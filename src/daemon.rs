use crate::{
    config::Store,
    files,
    jobs::Jobs,
    platform,
    protocol::{self, LocalRequest, Reply, Request},
    transport::{Incoming, Manager, Stream},
};
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        windows::named_pipe::{NamedPipeServer, ServerOptions},
    },
    sync::{Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;

pub async fn run(store: Store, stop: CancellationToken) -> Result<()> {
    let cfg = store.read()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.root.join("daemon.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("该设备后台已经运行")?;
    let jobs = Jobs::new(store.clone(), stop.clone())?;
    let (sender, mut incoming) = mpsc::channel::<Incoming>(64);
    let manager = Manager::new(store.clone(), sender, stop.clone());
    let engine = manager.clone();
    let engine_stop = stop.clone();
    let engine_task = tokio::spawn(async move {
        let result = engine.run().await;
        engine_stop.cancel();
        result
    });
    let pipe = create_pipe(&store, &cfg.owner_sid, true)?;
    let local_store = store.clone();
    let local_manager = manager.clone();
    let local_stop = stop.clone();
    let local_cancel = stop.clone();
    let local_task = tokio::spawn(async move {
        let result = local_server(local_store, local_manager, local_stop, pipe).await;
        local_cancel.cancel();
        result
    });
    let slots = Arc::new(Semaphore::new(64));
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            Some(result) = handlers.join_next(), if !handlers.is_empty() => { if let Err(error) = result { tracing::warn!(%error, "request task stopped"); } }
            Some(mut incoming) = incoming.recv() => {
                if let Ok(permit) = slots.clone().try_acquire_owned() {
                    let store = store.clone();
                    let jobs = jobs.clone();
                    handlers.spawn(async move {
                        let _permit = permit;
                        tokio::select! {
                            _ = incoming.cancel.cancelled() => {},
                            result = serve(&store, &jobs, &incoming.peer, &mut incoming.stream) => {
                                if let Err(error) = result {
                                    tracing::debug!(device = %incoming.peer, %error, "request failed");
                                    let _ = protocol::write_json(&mut incoming.stream, &Reply::error(format!("{error:#}"))).await;
                                }
                            }
                        }
                        let _ = incoming.stream.shutdown().await;
                    });
                }
            }
        }
    }
    stop.cancel();
    handlers.shutdown().await;
    tokio::time::timeout(Duration::from_secs(5), local_task).await???;
    engine_task.await??;
    jobs.shutdown().await;
    drop(lock);
    Ok(())
}

async fn serve(store: &Store, jobs: &Jobs, peer_id: &str, stream: &mut Stream) -> Result<()> {
    let request: Request =
        tokio::time::timeout(Duration::from_secs(15), protocol::read_json(stream)).await??;
    let cfg = store.read()?;
    let peer = crate::config::find_peer(&cfg, peer_id)?;
    static FILE_TRANSFERS: Semaphore = Semaphore::const_new(4);
    let _file_slot = if matches!(&request, Request::Upload { .. } | Request::Download { .. }) {
        Some(
            FILE_TRANSFERS
                .try_acquire()
                .context("文件传输已达并发上限")?,
        )
    } else {
        None
    };
    match request {
        Request::Automation {
            background,
            request,
        } => {
            ensure!(
                peer.allow.browser || peer.allow.desktop,
                "automation_permission_denied: 请先授予 browser 或 desktop 权限"
            );
            let value =
                crate::automation::session::forward(store, peer_id, background, request).await?;
            protocol::write_json(stream, &Reply::ok(value)).await?;
        }
        Request::Ping => {
            protocol::write_json(stream, &Reply::ok(serde_json::json!({"version":1}))).await?
        }
        Request::Ssh => {
            ensure!(peer.allow.ssh, "该设备未获 SSH 转发权限");
            let mut ssh = tokio::time::timeout(
                Duration::from_secs(5),
                TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, cfg.ssh_port)),
            )
            .await??;
            ssh.set_nodelay(true)?;
            protocol::write_json(stream, &Reply::ok(())).await?;
            if let Err(error) = tokio::io::copy_bidirectional(stream, &mut ssh).await {
                tracing::debug!(%error, "ssh stream ended");
            }
        }
        Request::JobSubmit { job } => {
            ensure!(peer.allow.exec, "该设备未获原生命令执行权限");
            ensure!(!job.system || peer.allow.system, "该设备未获 SYSTEM 权限");
            let record = jobs.submit(peer_id.to_string(), job).await?;
            protocol::write_json(stream, &Reply::ok(record)).await?;
        }
        Request::JobGet { id } => {
            ensure!(peer.allow.exec, "该设备未获原生命令执行权限");
            protocol::write_json(stream, &Reply::ok(jobs.get(peer_id, &id, false)?)).await?;
        }
        Request::JobLogs { id } => {
            ensure!(peer.allow.exec, "该设备未获原生命令执行权限");
            let record = jobs.get(peer_id, &id, true)?;
            protocol::write_json(stream, &Reply::ok(serde_json::json!({"stdout_size":record.stdout.len(),"stderr_size":record.stderr.len()}))).await?;
            if stream.write_all(record.stdout.as_bytes()).await.is_ok() {
                let _ = stream.write_all(record.stderr.as_bytes()).await;
            }
        }
        Request::JobList => {
            ensure!(peer.allow.exec, "该设备未获原生命令执行权限");
            let list: Vec<_> = jobs.list(peer_id)?.into_iter().map(|r| serde_json::json!({"id":r.spec.id,"state":r.state,"created":r.created,"finished":r.finished,"exit_code":r.exit_code,"system":r.spec.system})).collect();
            protocol::write_json(stream, &Reply::ok(list)).await?;
        }
        Request::JobCancel { id } => {
            ensure!(peer.allow.exec, "该设备未获原生命令执行权限");
            protocol::write_json(stream, &Reply::ok(jobs.cancel(peer_id, &id).await?)).await?;
        }
        Request::Upload {
            path,
            size,
            sha256,
            overwrite,
            system,
        } => {
            ensure!(peer.allow.files, "该设备未获原生文件访问权限");
            ensure!(!system || peer.allow.system, "该设备未获 SYSTEM 权限");
            files::upload(stream, cfg.owner_sid, path, size, sha256, overwrite, system).await?;
        }
        Request::Download {
            path,
            offset,
            sha256,
            system,
        } => {
            ensure!(peer.allow.files, "该设备未获原生文件访问权限");
            ensure!(!system || peer.allow.system, "该设备未获 SYSTEM 权限");
            files::download(stream, cfg.owner_sid, path, offset, sha256, system).await?;
        }
        Request::ListDir { path, system } => {
            ensure!(peer.allow.files, "该设备未获原生文件访问权限");
            ensure!(!system || peer.allow.system, "该设备未获 SYSTEM 权限");
            let owner = cfg.owner_sid;
            let value = tokio::task::spawn_blocking(move || platform::as_user(&owner, system, || {
                let directory=platform::absolute_file(&path)?;
                let mut entries=vec![];
                for entry in std::fs::read_dir(directory)?.take(201) {
                    let entry=entry?; let meta=entry.metadata()?;
                    entries.push(serde_json::json!({"name":entry.file_name().to_string_lossy(),"path":entry.path().to_string_lossy(),"directory":meta.is_dir(),"size":meta.len()}));
                }
                let truncated=entries.len()>200; entries.truncate(200);
                Ok(serde_json::json!({"entries":entries,"truncated":truncated}))
            })).await??;
            protocol::write_json(stream, &Reply::ok(value)).await?;
        }
        Request::Terminal {
            shell,
            cols,
            rows,
            system,
        } => {
            static TERMINALS: Semaphore = Semaphore::const_new(4);
            let _terminal = TERMINALS.try_acquire().context("交互终端已达并发上限")?;
            ensure!(peer.allow.exec, "该设备未获原生命令执行权限");
            ensure!(!system || peer.allow.system, "该设备未获 SYSTEM 权限");
            terminal(stream, cfg.owner_sid, shell, cols, rows, system).await?;
        }
    }
    Ok(())
}

async fn terminal(
    stream: &mut Stream,
    owner: String,
    shell: Option<String>,
    cols: u16,
    rows: u16,
    system: bool,
) -> Result<()> {
    let spec = crate::protocol::JobSpec {
        id: uuid::Uuid::new_v4().to_string(),
        program: shell.unwrap_or_else(platform::open_terminal_program),
        args: vec!["-NoLogo".into(), "-NoProfile".into(), "-NoExit".into()],
        cwd: None,
        timeout_secs: 86400,
        system,
    };
    let mut child = tokio::task::spawn_blocking(move || {
        platform::spawn_process(&spec, &owner, Some((cols, rows)))
    })
    .await??;
    protocol::write_json(
        stream,
        &Reply::ok(serde_json::json!({"terminal":"conpty","cols":cols,"rows":rows})),
    )
    .await?;
    let mut input = child.input.take().unwrap();
    let mut output = child.output.take().unwrap();
    let (send, mut receive) = mpsc::channel::<Vec<u8>>(4);
    let reader = tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match output.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if send.blocking_send(buffer[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let (mut net_read, mut net_write) = tokio::io::split(stream);
    let mut buffer = [0u8; 8192];
    let mut closing = None;
    loop {
        tokio::select! {
            chunk = receive.recv() => { match chunk { Some(bytes) => if net_write.write_all(&bytes).await.is_err() { break; }, None => break } }
            result = net_read.read(&mut buffer) => {
                match result { Ok(0) | Err(_) => break, Ok(n) => if input.write_all(&buffer[..n]).is_err() { break; } }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)), if closing.is_none() => {
                if let Some(code)=child.exit_code()? {
                    tracing::debug!(code,"interactive shell exited");
                    // ConPTY 在关闭时才排空最后一批输出，需要一边关闭一边继续读管道。
                    let console=child.console.take();
                    closing=Some(tokio::task::spawn_blocking(move||drop(console)));
                }
            }
        }
    }
    child.terminate();
    drop(input);
    drop(receive);
    if let Some(console) = child.console.take() {
        closing = Some(tokio::task::spawn_blocking(move || drop(console)));
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
    if let Some(task) = closing {
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
    Ok(())
}

fn create_pipe(store: &Store, owner: &str, first: bool) -> Result<NamedPipeServer> {
    let mut security = platform::PipeSecurity::new(owner)?;
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .max_instances(64);
    Ok(unsafe {
        options.create_with_security_attributes_raw(
            store.pipe_name()?,
            (&mut security.attrs as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast(),
        )?
    })
}

async fn local_server(
    store: Store,
    manager: Manager,
    stop: CancellationToken,
    mut pipe: NamedPipeServer,
) -> Result<()> {
    let owner = store.read()?.owner_sid;
    let limit = Arc::new(Semaphore::new(64));
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            Some(_) = handlers.join_next(), if !handlers.is_empty() => {},
            result = pipe.connect() => {
                result?;
                let next = create_pipe(&store, &owner, false)?;
                let mut client = std::mem::replace(&mut pipe, next);
                let manager = manager.clone();
                let store = store.clone();
                let stop = stop.clone();
                if let Ok(permit) = limit.clone().try_acquire_owned() {
                    handlers.spawn(async move {
                        let _permit = permit;
                        tokio::select! {
                            _ = stop.cancelled() => {},
                            result = local_request(&store, &manager, &stop, &mut client) => {
                                if let Err(error) = result { let _ = protocol::write_json(&mut client, &Reply::error(format!("{error:#}"))).await; }
                            }
                        }
                    });
                }
            }
        }
    }
    handlers.shutdown().await;
    Ok(())
}

async fn local_request(
    store: &Store,
    manager: &Manager,
    stop: &CancellationToken,
    client: &mut NamedPipeServer,
) -> Result<()> {
    let request: LocalRequest =
        tokio::time::timeout(Duration::from_secs(10), protocol::read_json(client)).await??;
    match request {
        LocalRequest::Status => {
            let cfg = store.read()?;
            protocol::write_json(client, &Reply::ok(serde_json::json!({"id":cfg.id,"name":cfg.name,"listen":cfg.listen,"ssh_port":cfg.ssh_port,"peers":manager.status().await?,"version":env!("CARGO_PKG_VERSION")}))).await?;
        }
        LocalRequest::Shutdown => {
            protocol::write_json(client, &Reply::ok(())).await?;
            stop.cancel();
        }
        LocalRequest::Open { peer, request } => {
            let mut remote = manager.open(&peer).await?;
            protocol::write_json(&mut remote, &request).await?;
            // 远端响应直接转交调用者，避免命令失败被本地代理错误地当作成功。
            let (mut local_read, mut local_write) = tokio::io::split(client);
            let (mut remote_read, mut remote_write) = tokio::io::split(remote);
            let to_remote = async {
                tokio::io::copy(&mut local_read, &mut remote_write).await?;
                remote_write.shutdown().await
            };
            let to_local = async {
                tokio::io::copy(&mut remote_read, &mut local_write).await?;
                local_write.flush().await
            };
            tokio::pin!(to_remote, to_local);
            // 命名管道没有 TCP 式半关闭；远端结束时必须释放整条本地管道，调用者才能收到 EOF。
            tokio::select! {_=&mut to_local=>{},result=&mut to_remote=>{if result.is_ok(){let _=to_local.await;}}}
        }
    }
    Ok(())
}
