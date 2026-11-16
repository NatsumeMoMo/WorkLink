use crate::{
    config::{Peer, Store},
    files::{self, FileInfo},
    jobs::JobRecord,
    protocol::{self, JobSpec, LocalRequest, Reply, Request},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom},
    path::Path,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient},
    process::Command,
};

pub async fn connect(store: &Store) -> Result<NamedPipeClient> {
    let name = store.pipe_name()?;
    for attempt in 0..30 {
        match ClientOptions::new()
            .security_qos_flags(windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION)
            .open(&name)
        {
            Ok(pipe) => return Ok(pipe),
            Err(error) if error.raw_os_error() == Some(231) && attempt < 29 => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => {
                return Err(error).context("无法连接本机后台，请启动 daemon 或安装并启动服务");
            }
        }
    }
    anyhow::bail!("本机后台繁忙")
}

pub async fn local(store: &Store, request: LocalRequest) -> Result<serde_json::Value> {
    let mut pipe = connect(store).await?;
    protocol::write_json(&mut pipe, &request).await?;
    tokio::time::timeout(
        Duration::from_secs(15),
        protocol::read_json::<_, Reply>(&mut pipe),
    )
    .await??
    .checked()
}

pub async fn open(
    store: &Store,
    peer: &str,
    request: Request,
) -> Result<(NamedPipeClient, serde_json::Value)> {
    let mut pipe = connect(store).await?;
    protocol::write_json(
        &mut pipe,
        &LocalRequest::Open {
            peer: peer.to_string(),
            request,
        },
    )
    .await?;
    let value = tokio::time::timeout(
        Duration::from_secs(120),
        protocol::read_json::<_, Reply>(&mut pipe),
    )
    .await??
    .checked()?;
    Ok((pipe, value))
}

pub async fn remote(store: &Store, peer: &str, request: Request) -> Result<serde_json::Value> {
    Ok(open(store, peer, request).await?.1)
}

pub async fn proxy(store: &Store, peer: &str) -> Result<()> {
    let (pipe, _) = open(store, peer, Request::Ssh).await?;
    let (mut read, mut write) = tokio::io::split(pipe);
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let to_remote = async {
        tokio::io::copy(&mut input, &mut write).await?;
        write.shutdown().await
    };
    let from_remote = async {
        tokio::io::copy(&mut read, &mut output).await?;
        output.flush().await
    };
    tokio::pin!(to_remote, from_remote);
    tokio::select! {
        result = &mut from_remote => { result?; }
        result = &mut to_remote => { result?; from_remote.await?; }
    }
    Ok(())
}

pub async fn job_get(store: &Store, peer: &str, id: &str, output: bool) -> Result<JobRecord> {
    let value = remote(store, peer, Request::JobGet { id: id.to_string() }).await?;
    let mut record: JobRecord = serde_json::from_value(value)?;
    if output {
        let (mut pipe, sizes) = open(store, peer, Request::JobLogs { id: id.to_string() }).await?;
        let stdout = sizes["stdout_size"].as_u64().context("缺少输出长度")?;
        let stderr = sizes["stderr_size"].as_u64().context("缺少输出长度")?;
        ensure!(
            stdout <= 12 * 1024 * 1024 && stderr <= 12 * 1024 * 1024,
            "输出长度超限"
        );
        let mut bytes = vec![0; (stdout + stderr) as usize];
        tokio::time::timeout(Duration::from_secs(60), pipe.read_exact(&mut bytes)).await??;
        record.stdout = String::from_utf8_lossy(&bytes[..stdout as usize]).into_owned();
        record.stderr = String::from_utf8_lossy(&bytes[stdout as usize..]).into_owned();
    }
    Ok(record)
}

pub fn powershell(command: &str) -> Vec<String> {
    let script = format!(
        "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false);$OutputEncoding=[Console]::OutputEncoding;$ProgressPreference='SilentlyContinue';{command}"
    );
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    vec![
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        STANDARD.encode(bytes),
    ]
}

pub struct NativeOptions {
    pub cwd: Option<String>,
    pub seconds: u64,
    pub system: bool,
    pub detach: bool,
    pub id: Option<String>,
}

pub async fn native_exec(
    store: &Store,
    peer: &str,
    command: &str,
    options: NativeOptions,
) -> Result<JobRecord> {
    let NativeOptions {
        cwd,
        seconds,
        system,
        detach,
        id,
    } = options;
    let job = JobSpec {
        id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        program: crate::platform::open_terminal_program(),
        args: powershell(command),
        cwd,
        timeout_secs: seconds,
        system,
    };
    let value = remote(store, peer, Request::JobSubmit { job }).await?;
    let record: JobRecord = serde_json::from_value(value)?;
    if detach {
        return Ok(record);
    }
    loop {
        let job = job_get(store, peer, &record.spec.id, false).await?;
        if !matches!(job.state.as_str(), "queued" | "running") {
            return job_get(store, peer, &record.spec.id, true).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

pub fn proxy_option(store: &Store, peer: &Peer) -> Result<String> {
    let exe = std::env::current_exe()?;
    let text = format!(
        "{} --data-dir {} proxy --device {}",
        crate::platform::quote_arg(&exe.to_string_lossy()),
        crate::platform::quote_arg(&store.root.to_string_lossy()),
        peer.id
    );
    Ok(format!("ProxyCommand={}", text.replace('%', "%%")))
}

pub fn ssh_config(store: &Store, peer: &Peer, user: Option<&str>) -> Result<String> {
    let user = user
        .or(peer.ssh_client.user.as_deref())
        .unwrap_or("YOUR_WINDOWS_USER");
    ensure!(!user.contains(['\0', '\n', '\r']), "SSH 用户名无效");
    let user = format!("\"{}\"", user.replace('\\', "\\\\").replace('"', "\\\""));
    let alias = format!("worklink-{}", peer.id);
    let proxy = proxy_option(store, peer)?
        .trim_start_matches("ProxyCommand=")
        .to_owned();
    let mut text = format!(
        "Host {alias}\n    HostName {alias}\n    HostKeyAlias {alias}\n    User {}\n    ProxyCommand {proxy}\n    ServerAliveInterval 20\n    ServerAliveCountMax 3\n",
        user
    );
    if let Some(path) = &peer.ssh_client.identity_file {
        text.push_str(&format!(
            "    IdentityFile \"{}\"\n",
            path.replace('\\', "/").replace('"', "\\\"")
        ));
    }
    if let Some(path) = &peer.ssh_client.known_hosts {
        text.push_str(&format!(
            "    UserKnownHostsFile \"{}\"\n",
            path.replace('\\', "/").replace('"', "\\\"")
        ));
    }
    Ok(text)
}

pub fn ssh_command(
    store: &Store,
    peer: &Peer,
    user: Option<&str>,
    batch: bool,
    sftp: bool,
) -> Result<Command> {
    let mut command = Command::new(if sftp { "sftp.exe" } else { "ssh.exe" });
    command.kill_on_drop(true);
    command.arg("-o").arg(proxy_option(store, peer)?);
    command
        .arg("-o")
        .arg(format!("HostKeyAlias=worklink-{}", peer.id));
    command.args([
        "-o",
        "ServerAliveInterval=20",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ConnectTimeout=15",
    ]);
    if batch {
        command.args(["-o", "BatchMode=yes"]);
    }
    if let Some(user) = user.or(peer.ssh_client.user.as_deref()) {
        ensure!(!user.contains(['\0', '\n', '\r']), "用户名无效");
        command.arg("-o").arg(format!("User={user}"));
    }
    if let Some(path) = &peer.ssh_client.identity_file {
        command.arg("-i").arg(path);
    }
    if let Some(path) = &peer.ssh_client.known_hosts {
        ensure!(
            !path.contains(['\n', '\r', '\0', '"']),
            "known_hosts 路径无效"
        );
        command.arg("-o").arg(format!(
            "UserKnownHostsFile=\"{}\"",
            path.replace('\\', "/")
        ));
    }
    Ok(command)
}

#[derive(Debug, Serialize)]
pub struct CommandOutput {
    pub backend: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
}

pub async fn ssh_exec(
    store: &Store,
    peer_name: &str,
    user: Option<&str>,
    command: &str,
    seconds: u64,
) -> Result<CommandOutput> {
    let peer = store.peer(peer_name)?;
    let mut process = ssh_command(store, &peer, user, true, false)?;
    process.arg(format!("worklink-{}", peer.id));
    process.arg("powershell.exe").args(powershell(command));
    capture(process, None, seconds).await
}

pub async fn capture(
    mut process: Command,
    stdin: Option<String>,
    seconds: u64,
) -> Result<CommandOutput> {
    use std::process::Stdio;
    process
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = process.spawn().context("无法启动系统 OpenSSH 客户端")?;
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let mut out_task = tokio::spawn(read_capped(out));
    let mut err_task = tokio::spawn(read_capped(err));
    let operation = async {
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(input.as_bytes()).await?;
            pipe.shutdown().await?;
        }
        let status = child.wait().await?;
        let (stdout, out_truncated) = (&mut out_task).await??;
        let (stderr, err_truncated) = (&mut err_task).await??;
        Ok(CommandOutput {
            backend: "ssh".into(),
            exit_code: status.code(),
            stdout,
            stderr,
            output_truncated: out_truncated || err_truncated,
        })
    };
    // 子进程退出后，代理仍可能持有输出句柄；超时必须包含 stdin 写入和输出收尾。
    let result = tokio::time::timeout(Duration::from_secs(seconds), operation).await;
    out_task.abort();
    err_task.abort();
    match result {
        Ok(result) => result,
        Err(error) => {
            let _ = child.kill().await;
            Err(error).context("SSH 操作超时；远端命令的最终状态需要核实")
        }
    }
}

async fn read_capped(mut input: impl AsyncRead + Unpin) -> Result<(String, bool)> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let n = input.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let keep = n.min((4 * 1024 * 1024usize).saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..keep]);
        truncated |= keep < n;
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

pub async fn native_put(
    store: &Store,
    peer: &str,
    source: &Path,
    target: &str,
    overwrite: bool,
    system: bool,
) -> Result<serde_json::Value> {
    let source = source.to_owned();
    let (file, size, sha256) = tokio::task::spawn_blocking(move || {
        let mut file = File::open(source)?;
        let size = file.metadata()?.len();
        let hash = files::hash_file(&mut file)?;
        Ok::<_, anyhow::Error>((file, size, hash))
    })
    .await??;
    let (mut pipe, data) = open(
        store,
        peer,
        Request::Upload {
            path: target.into(),
            size,
            sha256: sha256.clone(),
            overwrite,
            system,
        },
    )
    .await?;
    let info: FileInfo = serde_json::from_value(data)?;
    ensure!(
        info.offset <= size && info.sha256 == sha256,
        "远端续传信息无效"
    );
    let mut file = tokio::fs::File::from_std(file);
    tokio::io::AsyncSeekExt::seek(&mut file, SeekFrom::Start(info.offset)).await?;
    files::copy_bytes(&mut file, &mut pipe, size - info.offset).await?;
    pipe.shutdown().await?;
    let mut result = protocol::read_json::<_, Reply>(&mut pipe)
        .await?
        .checked()?;
    result["resumed_from"] = serde_json::json!(info.offset);
    Ok(result)
}

pub async fn native_get(
    store: &Store,
    peer: &str,
    source: &str,
    target: &Path,
    overwrite: bool,
    system: bool,
) -> Result<serde_json::Value> {
    ensure!(
        overwrite || !target.exists(),
        "目标文件已存在，请显式指定 --overwrite"
    );
    let (offset, sha256) = files::download_resume(target);
    let (mut pipe, data) = open(
        store,
        peer,
        Request::Download {
            path: source.into(),
            offset,
            sha256,
            system,
        },
    )
    .await?;
    let info: FileInfo = serde_json::from_value(data)?;
    ensure!(info.offset <= info.size, "远端文件信息无效");
    files::save_download_meta(target, &info)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(files::local_partial(target))?;
    fs2::FileExt::try_lock_exclusive(&file).context("目标已有下载任务")?;
    file.set_len(info.offset)?;
    file.seek(SeekFrom::Start(info.offset))?;
    let mut file = tokio::fs::File::from_std(file);
    files::copy_bytes(&mut pipe, &mut file, info.size - info.offset).await?;
    file.sync_all().await?;
    let file = file.into_std().await;
    finish_download(file, target, &info, overwrite)?;
    Ok(
        serde_json::json!({"path":target,"size":info.size,"sha256":info.sha256,"verified":true,"resumed_from":info.offset}),
    )
}

fn finish_download(mut file: File, target: &Path, info: &FileInfo, overwrite: bool) -> Result<()> {
    if files::hash_file(&mut file)? != info.sha256 {
        file.set_len(0)?;
        file.sync_all()?;
        anyhow::bail!("文件完整性校验失败，已重置临时文件，未提交目标文件");
    }
    fs2::FileExt::unlock(&file)?;
    drop(file);
    let part = files::local_partial(target);
    if overwrite {
        fs::rename(&part, target)?;
    } else {
        fs::hard_link(&part, target)?;
        fs::remove_file(&part)?;
    }
    let _ = fs::remove_file(format!("{}.json", part.display()));
    Ok(())
}

fn ps_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

pub async fn list_dir(
    store: &Store,
    name: &str,
    user: Option<&str>,
    path: &str,
    native: bool,
    system: bool,
) -> Result<serde_json::Value> {
    if native {
        return remote(
            store,
            name,
            Request::ListDir {
                path: path.into(),
                system,
            },
        )
        .await;
    }
    let script = format!(
        "$ErrorActionPreference='Stop';$items=@(Get-ChildItem -LiteralPath {} -Force|Select-Object -First 201);$entries=@($items|Select-Object -First 200|ForEach-Object{{@{{name=$_.Name;path=$_.FullName;directory=$_.PSIsContainer;size=$_.Length}}}});@{{entries=$entries;truncated=($items.Count -gt 200)}}|ConvertTo-Json -Depth 4 -Compress",
        ps_literal(path)
    );
    let result = ssh_exec(store, name, user, &script, 60).await?;
    ensure!(
        result.exit_code == Some(0),
        "读取目录失败：{}",
        result.stderr
    );
    Ok(serde_json::from_str(result.stdout.trim())?)
}

fn sftp_literal(text: &str) -> Result<String> {
    ensure!(!text.contains(['\n', '\r', '\0']), "文件路径含无效字符");
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "/").replace('"', "\\\"")
    ))
}

async fn sftp_batch(store: &Store, peer: &Peer, user: Option<&str>, script: String) -> Result<()> {
    let mut process = ssh_command(store, peer, user, true, true)?;
    process
        .args(["-b", "-"])
        .arg(format!("worklink-{}", peer.id));
    let output = capture(process, Some(script), 86400).await?;
    ensure!(
        output.exit_code == Some(0),
        "SFTP 失败：{}",
        output.stderr.trim()
    );
    Ok(())
}

pub async fn ssh_put(
    store: &Store,
    name: &str,
    user: Option<&str>,
    source: &Path,
    target: &str,
    overwrite: bool,
) -> Result<serde_json::Value> {
    let path = source.to_owned();
    let info = tokio::task::spawn_blocking(move || {
        let mut file = File::open(path)?;
        Ok::<_, anyhow::Error>(FileInfo {
            size: file.metadata()?.len(),
            sha256: files::hash_file(&mut file)?,
            offset: 0,
        })
    })
    .await??;
    let partial = format!("{target}.worklink-{}.part", info.sha256);
    let peer = store.peer(name)?;
    let preflight = format!(
        "$ErrorActionPreference='Stop';$t={};$p={};if((Test-Path -LiteralPath $t) -and ${}){{throw 'Target exists; specify --overwrite'}};$complete=$false;if(Test-Path -LiteralPath $p){{$n=(Get-Item -LiteralPath $p).Length;if($n -eq {}){{if((Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash -eq '{}'){{$complete=$true}}else{{Remove-Item -LiteralPath $p -Force}}}}elseif($n -gt {}){{Remove-Item -LiteralPath $p -Force}}}};if(!(Test-Path -LiteralPath $p)){{[IO.File]::WriteAllBytes($p,[byte[]]@())}};@{{complete=$complete}}|ConvertTo-Json -Compress",
        ps_literal(target),
        ps_literal(&partial),
        if overwrite { "false" } else { "true" },
        info.size,
        info.sha256,
        info.size
    );
    let check = ssh_exec(store, name, user, &preflight, 60).await?;
    ensure!(check.exit_code == Some(0), "上传检查失败：{}", check.stderr);
    let preflight: serde_json::Value = serde_json::from_str(check.stdout.trim())?;
    // OpenSSH 的 reput 会拒绝等长文件，完整临时文件先核验后直接进入提交阶段。
    if info.size > 0 && preflight["complete"] != true {
        sftp_batch(
            store,
            &peer,
            user,
            format!(
                "reput {} {}\n",
                sftp_literal(&source.to_string_lossy())?,
                sftp_literal(&partial)?
            ),
        )
        .await?;
    }
    let commit = format!(
        "$ErrorActionPreference='Stop';$p={};$t={};if((Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash -ne '{}'){{Remove-Item -LiteralPath $p -Force;throw 'SHA256 mismatch; corrupt partial reset'}};if(Test-Path -LiteralPath $t){{{} }}else{{[IO.File]::Move($p,$t)}}",
        ps_literal(&partial),
        ps_literal(target),
        info.sha256,
        if overwrite {
            // Windows PowerShell 会把 $null 转成空路径，NullString 才能保留真正的空备份参数。
            "[IO.File]::Replace($p,$t,[NullString]::Value)"
        } else {
            "throw 'Target exists; not overwritten'"
        }
    );
    let result = ssh_exec(store, name, user, &commit, 86400).await?;
    ensure!(
        result.exit_code == Some(0),
        "上传提交失败：{}",
        result.stderr
    );
    Ok(
        serde_json::json!({"path":target,"size":info.size,"sha256":info.sha256,"verified":true,"backend":"ssh"}),
    )
}

pub async fn ssh_get(
    store: &Store,
    name: &str,
    user: Option<&str>,
    source: &str,
    target: &Path,
    overwrite: bool,
) -> Result<serde_json::Value> {
    ensure!(
        overwrite || !target.exists(),
        "目标文件已存在，请显式指定 --overwrite"
    );
    let script = format!(
        "$ErrorActionPreference='Stop';$p={};$i=Get-Item -LiteralPath $p;@{{size=$i.Length;sha256=(Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash.ToLowerInvariant();offset=0}}|ConvertTo-Json -Compress",
        ps_literal(source)
    );
    let output = ssh_exec(store, name, user, &script, 86400).await?;
    ensure!(
        output.exit_code == Some(0),
        "下载检查失败：{}",
        output.stderr
    );
    let info: FileInfo = serde_json::from_str(output.stdout.trim()).context("远端文件信息无效")?;
    let (_, old_hash) = files::download_resume(target);
    let part = files::local_partial(target);
    if old_hash.as_deref() != Some(&info.sha256) && part.exists() {
        fs::remove_file(&part)?;
    }
    files::save_download_meta(target, &info)?;
    let peer = store.peer(name)?;
    if !part.exists() {
        File::create(&part)?;
    }
    let mut partial = OpenOptions::new().read(true).write(true).open(&part)?;
    let length = partial.metadata()?.len();
    let complete = length == info.size && files::hash_file(&mut partial)? == info.sha256;
    // reget 同样不能处理已等长的本机文件；完整数据直接提交，损坏或超长数据先清零。
    if !complete && length >= info.size {
        partial.set_len(0)?;
    }
    drop(partial);
    if !complete {
        sftp_batch(
            store,
            &peer,
            user,
            format!(
                "reget {} {}\n",
                sftp_literal(source)?,
                sftp_literal(&part.to_string_lossy())?
            ),
        )
        .await?;
    }
    let file = OpenOptions::new().read(true).write(true).open(&part)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    finish_download(file, target, &info, overwrite)?;
    Ok(
        serde_json::json!({"path":target,"size":info.size,"sha256":info.sha256,"verified":true,"backend":"ssh"}),
    )
}
