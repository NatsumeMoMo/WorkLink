mod client;
mod config;
mod daemon;
mod files;
mod jobs;
mod platform;
mod protocol;
mod service;
mod transport;
mod ui;

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use config::Store;
use protocol::{LocalRequest, Request};
use std::{net::SocketAddr, path::PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "WorkLink",
    version,
    about = "双向加密 SSH、文件传输与 Agent 远程管理工具"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "设备数据目录，默认 %ProgramData%\\WorkLink"
    )]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true, help = "输出 JSON，日志仍写入文件或 stderr")]
    json: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "127.0.0.1:41443")]
        listen: SocketAddr,
        #[arg(long)]
        outbound_only: bool,
    },
    Invite {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Peer {
        #[command(subcommand)]
        action: PeerCommand,
    },
    Config {
        #[arg(long)]
        listen: Option<SocketAddr>,
        #[arg(long)]
        outbound_only: bool,
        #[arg(long)]
        ssh_port: Option<u16>,
    },
    Daemon,
    Status,
    Devices,
    Shutdown,
    Proxy {
        #[arg(long)]
        device: String,
        #[arg(long, default_value = "ssh")]
        service: String,
    },
    Forward {
        device: String,
        #[arg(long, default_value = "127.0.0.1:22001")]
        bind: SocketAddr,
    },
    SshConfig {
        device: String,
        #[arg(long)]
        user: Option<String>,
    },
    Exec {
        device: String,
        #[arg(long, value_enum, default_value = "ssh")]
        backend: Backend,
        #[arg(long)]
        command: String,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 3600)]
        timeout: u64,
        #[arg(long)]
        system: bool,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        id: Option<String>,
    },
    Terminal {
        device: String,
        #[arg(long, value_enum, default_value = "ssh")]
        backend: Backend,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        system: bool,
        #[arg(long, default_value_t = 120)]
        cols: u16,
        #[arg(long, default_value_t = 32)]
        rows: u16,
    },
    Put {
        device: String,
        source: PathBuf,
        target: String,
        #[arg(long, value_enum, default_value = "ssh")]
        backend: Backend,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        system: bool,
    },
    Get {
        device: String,
        source: String,
        target: PathBuf,
        #[arg(long, value_enum, default_value = "ssh")]
        backend: Backend,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        system: bool,
    },
    Job {
        #[command(subcommand)]
        action: JobCommand,
    },
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    Gui,
    Tray,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Backend {
    Ssh,
    Native,
}

#[derive(Subcommand)]
enum PeerCommand {
    Add {
        #[arg(long, conflicts_with = "file")]
        invite: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        endpoint: Option<String>,
    },
    List,
    Remove {
        device: String,
    },
    Endpoint {
        device: String,
        endpoint: Option<String>,
    },
    Allow {
        device: String,
        #[arg(long)]
        ssh: Option<bool>,
        #[arg(long)]
        files: Option<bool>,
        #[arg(long)]
        exec: Option<bool>,
        #[arg(long)]
        system: Option<bool>,
    },
    Ssh {
        device: String,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        identity: Option<PathBuf>,
        #[arg(long)]
        known_hosts: Option<PathBuf>,
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum JobCommand {
    List {
        device: String,
    },
    Get {
        device: String,
        id: String,
        #[arg(long)]
        no_output: bool,
    },
    Cancel {
        device: String,
        id: String,
    },
}
#[derive(Subcommand)]
enum ServiceCommand {
    Install,
    Start,
    Stop,
    Uninstall,
    Status,
    #[command(hide = true)]
    Run,
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(32)
        .enable_all()
        .build()?)
}

pub fn logging(store: &Store) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("worklink")
        .max_log_files(7)
        .build(store.root.join("logs"))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(writer)
        .try_init();
    Ok(guard)
}

fn output(value: impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let gui = matches!(
        &cli.command,
        None | Some(Commands::Gui) | Some(Commands::Tray)
    );
    if gui && std::env::var_os("WORKLINK_SCREENSHOT").is_none() {
        platform::detach_console();
    }
    let json = cli.json;
    let code = match run(cli) {
        Ok(code) => code,
        Err(error) => {
            if gui && std::env::var_os("WORKLINK_SCREENSHOT").is_none() {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                        std::ptr::null_mut(),
                        platform::wide(format!("{error:#}")).as_ptr(),
                        platform::wide("WorkLink").as_ptr(),
                        windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                    );
                }
            }
            if json {
                let _ = output(serde_json::json!({"ok":false,"error":format!("{error:#}")}));
            } else {
                eprintln!("WorkLink: {error:#}");
            }
            1
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let root = cli.data_dir.unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into()))
            .join("WorkLink")
    });
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    let store = Store::new(root);
    match cli.command {
        None | Some(Commands::Gui) => {
            ui::run(store)?;
            Ok(0)
        }
        Some(Commands::Tray) => {
            ui::tray(store)?;
            Ok(0)
        }
        Some(Commands::Init {
            name,
            listen,
            outbound_only,
        }) => {
            output(store.init(name, if outbound_only { None } else { Some(listen) })?)?;
            Ok(0)
        }
        Some(Commands::Invite { out }) => {
            let invite = store.invitation()?;
            let code = invite.encode()?;
            if let Some(path) = out {
                std::fs::write(path, &code)?;
            }
            if cli.json {
                output(
                    serde_json::json!({"id":invite.id,"name":invite.name,"fingerprint":invite.fingerprint,"invite":code}),
                )?;
            } else {
                println!("{code}\n\nSHA-256: {}", invite.fingerprint);
            }
            Ok(0)
        }
        Some(Commands::Peer { action }) => {
            peer_command(&store, action)?;
            Ok(0)
        }
        Some(Commands::Config {
            listen,
            outbound_only,
            ssh_port,
        }) => {
            store.update(|cfg| {
                if outbound_only {
                    cfg.listen = None;
                } else if listen.is_some() {
                    cfg.listen = listen;
                }
                if let Some(port) = ssh_port {
                    ensure!(port > 0, "SSH 端口无效");
                    cfg.ssh_port = port;
                }
                Ok(())
            })?;
            output(store.read()?)?;
            Ok(0)
        }
        Some(Commands::SshConfig { device, user }) => {
            println!(
                "{}",
                client::ssh_config(&store, &store.peer(&device)?, user.as_deref())?
            );
            Ok(0)
        }
        Some(Commands::Service { action }) => {
            match action {
                ServiceCommand::Install => service::install(&store)?,
                ServiceCommand::Start => service::control("start")?,
                ServiceCommand::Stop => service::control("stop")?,
                ServiceCommand::Uninstall => service::control("uninstall")?,
                ServiceCommand::Status => {
                    output(service::query()?)?;
                    return Ok(0);
                }
                ServiceCommand::Run => {
                    service::dispatch(store)?;
                    return Ok(0);
                }
            }
            output(serde_json::json!({"ok":true}))?;
            Ok(0)
        }
        Some(command) => {
            let runtime = runtime()?;
            let result = runtime.block_on(async_run(store, command, cli.json));
            // 控制台读取和 Windows 子进程管道使用不可取消的阻塞线程；超时或远端关闭后必须允许 CLI 退出。
            runtime.shutdown_timeout(std::time::Duration::from_millis(200));
            result
        }
    }
}

fn peer_command(store: &Store, command: PeerCommand) -> Result<()> {
    match command {
        PeerCommand::Ssh {
            device,
            user,
            identity,
            known_hosts,
            clear,
        } => {
            let id = store.peer(&device)?.id;
            let absolute = |p: PathBuf| -> Result<String> {
                Ok(std::path::absolute(p)?.to_string_lossy().into_owned())
            };
            let identity = identity.map(absolute).transpose()?;
            let known_hosts = known_hosts.map(absolute).transpose()?;
            if let Some(u) = &user {
                ensure!(!u.contains(['\n', '\r', '\0']), "用户名无效");
            }
            store.update(|cfg| {
                let client = &mut cfg
                    .peers
                    .iter_mut()
                    .find(|p| p.id == id)
                    .unwrap()
                    .ssh_client;
                if clear {
                    *client = Default::default();
                } else {
                    if user.is_some() {
                        client.user = user;
                    }
                    if identity.is_some() {
                        client.identity_file = identity;
                    }
                    if known_hosts.is_some() {
                        client.known_hosts = known_hosts;
                    }
                }
                Ok(())
            })?;
            output(store.peer(&device)?)?;
        }
        PeerCommand::Add {
            invite,
            file,
            fingerprint,
            endpoint,
        } => {
            let input = match (invite, file) {
                (Some(code), _) => code,
                (_, Some(path)) => std::fs::read_to_string(path)?,
                _ => anyhow::bail!("请提供 --invite 或 --file"),
            };
            output(store.add_peer(&input, endpoint, &fingerprint)?)?;
        }
        PeerCommand::List => output(store.read()?.peers)?,
        PeerCommand::Remove { device } => {
            let id = store.peer(&device)?.id;
            store.update(|cfg| {
                cfg.peers.retain(|p| p.id != id);
                Ok(())
            })?;
            output(serde_json::json!({"ok":true}))?;
        }
        PeerCommand::Endpoint { device, endpoint } => {
            if let Some(ep) = &endpoint {
                config::validate_endpoint(ep)?;
            }
            let id = store.peer(&device)?.id;
            store.update(|cfg| {
                cfg.peers.iter_mut().find(|p| p.id == id).unwrap().endpoint = endpoint;
                Ok(())
            })?;
            output(store.peer(&device)?)?;
        }
        PeerCommand::Allow {
            device,
            ssh,
            files,
            exec,
            system,
        } => {
            let id = store.peer(&device)?.id;
            store.update(|cfg| {
                let peer = cfg.peers.iter_mut().find(|p| p.id == id).unwrap();
                if let Some(v) = ssh {
                    peer.allow.ssh = v;
                }
                if let Some(v) = files {
                    peer.allow.files = v;
                }
                if let Some(v) = exec {
                    peer.allow.exec = v;
                }
                if let Some(v) = system {
                    peer.allow.system = v;
                }
                Ok(())
            })?;
            output(store.peer(&device)?)?;
        }
    }
    Ok(())
}

async fn async_run(store: Store, command: Commands, json: bool) -> Result<i32> {
    match command {
        Commands::Daemon => {
            let _logging = logging(&store)?;
            let stop = CancellationToken::new();
            let signal = stop.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                signal.cancel();
            });
            daemon::run(store, stop).await?;
        }
        Commands::Status | Commands::Devices => {
            output(client::local(&store, LocalRequest::Status).await?)?;
        }
        Commands::Shutdown => {
            output(client::local(&store, LocalRequest::Shutdown).await?)?;
        }
        Commands::Proxy { device, service } => {
            ensure!(service == "ssh", "当前仅支持 ssh 转发目标");
            client::proxy(&store, &device).await?;
        }
        Commands::Forward { device, bind } => {
            ensure!(bind.ip().is_loopback(), "本机转发只允许绑定回环地址");
            let listener = tokio::net::TcpListener::bind(bind).await?;
            eprintln!("SSH 本机入口：{bind}，按 Ctrl+C 停止");
            let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(32));
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted?;
                        if let Ok(permit) = limit.clone().try_acquire_owned() {
                            let store = store.clone(); let device = device.clone();
                            tokio::spawn(async move { let _permit=permit; if let Ok((mut pipe,_))=client::open(&store,&device,Request::Ssh).await { let _=tokio::io::copy_bidirectional(&mut socket,&mut pipe).await; } });
                        }
                    }
                }
            }
        }
        Commands::Exec {
            device,
            backend,
            command,
            user,
            cwd,
            timeout,
            system,
            detach,
            id,
        } => {
            ensure!(timeout > 0 && timeout <= 86400, "超时应为 1–86400 秒");
            if backend == Backend::Native {
                let record = client::native_exec(
                    &store,
                    &device,
                    &command,
                    client::NativeOptions {
                        cwd,
                        seconds: timeout,
                        system,
                        detach,
                        id,
                    },
                )
                .await?;
                let code = if detach {
                    0
                } else {
                    record.exit_code.map(|c| c.min(255) as i32).unwrap_or(1)
                };
                if json {
                    output(&record)?;
                } else {
                    print!("{}", record.stdout);
                    eprint!("{}", record.stderr);
                    if let Some(error) = &record.error {
                        eprintln!("{error}");
                    }
                    if detach {
                        println!("{}", record.spec.id);
                    }
                }
                return Ok(code);
            } else {
                ensure!(
                    !system && !detach && id.is_none(),
                    "--system、--detach、--id 仅适用于显式选择的 native 后端"
                );
                let command = if let Some(cwd) = cwd {
                    format!(
                        "Set-Location -LiteralPath '{}';{command}",
                        cwd.replace('\'', "''")
                    )
                } else {
                    command
                };
                let result =
                    client::ssh_exec(&store, &device, user.as_deref(), &command, timeout).await?;
                let code = result.exit_code.unwrap_or(1);
                if json {
                    output(&result)?;
                } else {
                    print!("{}", result.stdout);
                    eprint!("{}", result.stderr);
                }
                return Ok(code);
            }
        }
        Commands::Put {
            device,
            source,
            target,
            backend,
            user,
            overwrite,
            system,
        } => {
            let result = if backend == Backend::Native {
                client::native_put(&store, &device, &source, &target, overwrite, system).await?
            } else {
                ensure!(!system, "SSH 权限由登录账户决定");
                client::ssh_put(
                    &store,
                    &device,
                    user.as_deref(),
                    &source,
                    &target,
                    overwrite,
                )
                .await?
            };
            output(result)?;
        }
        Commands::Get {
            device,
            source,
            target,
            backend,
            user,
            overwrite,
            system,
        } => {
            let result = if backend == Backend::Native {
                client::native_get(&store, &device, &source, &target, overwrite, system).await?
            } else {
                ensure!(!system, "SSH 权限由登录账户决定");
                client::ssh_get(
                    &store,
                    &device,
                    user.as_deref(),
                    &source,
                    &target,
                    overwrite,
                )
                .await?
            };
            output(result)?;
        }
        Commands::Job { action } => match action {
            JobCommand::List { device } => {
                output(client::remote(&store, &device, Request::JobList).await?)?
            }
            JobCommand::Get {
                device,
                id,
                no_output,
            } => output(client::job_get(&store, &device, &id, !no_output).await?)?,
            JobCommand::Cancel { device, id } => {
                output(client::remote(&store, &device, Request::JobCancel { id }).await?)?
            }
        },
        Commands::Terminal {
            device,
            backend,
            user,
            system,
            cols,
            rows,
        } => {
            platform::ensure_console();
            if backend == Backend::Ssh {
                ensure!(!system, "SSH 权限由登录账户决定");
                let peer = store.peer(&device)?;
                let mut cmd = client::ssh_command(&store, &peer, user.as_deref(), false, false)?;
                cmd.arg("-t").arg(format!("worklink-{}", peer.id));
                let status = cmd.status().await?;
                return Ok(status.code().unwrap_or(1));
            }
            let _console = platform::RawConsole::new()?;
            let (pipe, _) = client::open(
                &store,
                &device,
                Request::Terminal {
                    shell: None,
                    cols,
                    rows,
                    system,
                },
            )
            .await?;
            let (mut read, mut write) = tokio::io::split(pipe);
            let mut input = tokio::io::stdin();
            let mut out = tokio::io::stdout();
            tokio::select! { _=tokio::io::copy(&mut input,&mut write)=>{}, _=tokio::io::copy(&mut read,&mut out)=>{} }
        }
        _ => unreachable!(),
    }
    Ok(0)
}
