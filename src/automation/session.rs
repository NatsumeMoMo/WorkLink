use super::{
    Action, Budget, TaskView, Wire, browser::Browser, desktop::Desktop, now, permitted,
    store::Results, valid_id,
};
use crate::{
    config::Store,
    platform,
    protocol::{self, Reply},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fs, sync::Arc, time::Duration};
use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    sync::{Mutex, Notify, Semaphore},
};
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize)]
struct Envelope {
    peer: String,
    request: Wire,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    owner: String,
    #[serde(default)]
    scope: String,
    hash: String,
    capability: String,
    mutates: bool,
    #[serde(flatten)]
    view: TaskView,
}
pub struct Host {
    pub results: Results,
    scope: String,
    background: bool,
    browser: Browser,
    desktop: Desktop,
    tasks: Mutex<HashMap<String, CancellationToken>>,
    changed: Notify,
    slots: Arc<Semaphore>,
    stop: CancellationToken,
}
#[derive(Clone)]
pub struct ContextTask {
    host: Arc<Host>,
    pub id: String,
    pub peer: String,
    pub capability: String,
}
impl ContextTask {
    pub async fn phase(&self, phase: &str) -> Result<()> {
        let mut record = self.host.load(&self.id)?;
        record.view.phase = phase.into();
        self.host.save(&record)?;
        Ok(())
    }
    pub fn results(&self) -> &Results {
        &self.host.results
    }
}
impl Host {
    fn path(&self, id: &str) -> std::path::PathBuf {
        self.results
            .store
            .root
            .join(format!("automation/tasks/{id}.json"))
    }
    fn load(&self, id: &str) -> Result<Record> {
        valid_id(id)?;
        Ok(serde_json::from_slice(
            &fs::read(self.path(id)).context("task_not_found: 任务不存在或已清理")?,
        )?)
    }
    fn save(&self, record: &Record) -> Result<()> {
        self.results
            .store
            .write_json(format!("automation/tasks/{}.json", record.view.id), record)?;
        self.changed.notify_waiters();
        Ok(())
    }
    fn owned(&self, peer: &str, id: &str) -> Result<Record> {
        let record = self.load(id)?;
        ensure!(record.owner == peer, "task_not_found: 无权查询此任务");
        ensure!(
            record.scope == self.scope,
            "task_mode_mismatch: 请使用原任务的 background 模式与用户会话"
        );
        permitted(&self.results.store, peer, &record.capability)?;
        Ok(record)
    }
    fn prune_tasks(&self, recover: bool) -> Result<()> {
        let mut finished = Vec::new();
        for entry in fs::read_dir(self.results.store.root.join("automation/tasks"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Ok(mut record) = serde_json::from_slice::<Record>(&fs::read(path)?)
            {
                if record.scope != self.scope {
                    continue;
                }
                if recover && !record.view.terminal() {
                    record.view.state = "interrupted".into();
                    record.view.error = Some(
                        json!({"code":"helper_restarted","message":"助手已重启；先确认原操作结果，不自动重放","may_have_applied":record.mutates}),
                    );
                    self.save(&record)?;
                }
                if record.view.terminal() {
                    finished.push(record);
                }
            }
        }
        // 保留完整去重窗口；容量不足时拒绝新任务，不能提前丢掉已执行操作的 ID。
        for record in &finished {
            if now().saturating_sub(record.view.created) > super::store::TTL {
                let _ = fs::remove_file(self.path(&record.view.id));
            }
        }
        ensure!(
            finished
                .iter()
                .filter(|r| now().saturating_sub(r.view.created) <= super::store::TTL)
                .count()
                < 10000,
            "task_capacity_reached: 24 小时内最多保留 10000 个任务，请等待旧任务过期"
        );
        Ok(())
    }
    async fn handle(self: &Arc<Self>, peer: &str, wire: Wire) -> Result<Value> {
        match wire {
            Wire::Submit {
                id,
                action,
                budget,
                timeout,
            } => {
                valid_id(&id)?;
                budget.validate()?;
                ensure!(
                    (1..=3600).contains(&timeout),
                    "invalid_timeout: timeout 应为 1–3600 秒"
                );
                let encoded = serde_json::to_vec(&action)?;
                ensure!(
                    encoded.len() <= 64 * 1024,
                    "input_too_large: 操作参数超过 64 KiB"
                );
                permitted(&self.results.store, peer, action.capability())?;
                let hash = hex::encode(Sha256::digest(serde_json::to_vec(&(
                    &action, &budget, timeout,
                ))?));
                // 持久化 ID 在执行前落盘；重连重试只能取回原任务，不能重复点击或发送问题。
                let mut running = self.tasks.lock().await;
                let registry = fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(self.results.store.root.join("automation/tasks.lock"))?;
                fs2::FileExt::lock_exclusive(&registry)?;
                if self.path(&id).exists() {
                    let record = self.owned(peer, &id)?;
                    ensure!(record.hash == hash, "id_conflict: 同一任务 ID 对应不同参数");
                    return Ok(serde_json::to_value(record.view)?);
                }
                let slot = self
                    .slots
                    .clone()
                    .try_acquire_owned()
                    .context("automation_busy: 最多同时执行 8 个任务")?;
                self.prune_tasks(false)?;
                let record = Record {
                    owner: peer.into(),
                    scope: self.scope.clone(),
                    hash,
                    capability: action.capability().into(),
                    mutates: action.mutates(),
                    view: TaskView {
                        id: id.clone(),
                        background: self.background,
                        state: "queued".into(),
                        phase: "queued".into(),
                        created: now(),
                        result: None,
                        error: None,
                    },
                };
                self.save(&record)?;
                let cancel = CancellationToken::new();
                running.insert(id.clone(), cancel.clone());
                let host = self.clone();
                let ctx = ContextTask {
                    host: self.clone(),
                    id: id.clone(),
                    peer: peer.into(),
                    capability: record.capability.clone(),
                };
                tokio::spawn(async move {
                    let _slot = slot;
                    let result = host.execute(&ctx, action, budget, timeout, cancel).await;
                    if let Ok(mut current) = host.load(&ctx.id) {
                        match result {
                            Ok(value) => {
                                current.view.state = "completed".into();
                                current.view.phase = "completed".into();
                                current.view.result = Some(value);
                            }
                            Err(error) => {
                                let message = super::clip(&format!("{error:#}"), 600);
                                let code = message.split(':').next().unwrap_or("automation_failed");
                                let code = if code.len() <= 64
                                    && code.bytes().all(|c| c.is_ascii_lowercase() || c == b'_')
                                {
                                    code
                                } else {
                                    "automation_failed"
                                };
                                current.view.state =
                                    if code == "cancelled" || code == "permission_revoked" {
                                        "cancelled"
                                    } else if code == "timed_out" {
                                        "timed_out"
                                    } else {
                                        "failed"
                                    }
                                    .into();
                                current.view.error = Some(
                                    json!({"code":code,"message":message,"may_have_applied":current.mutates&&!matches!(current.view.phase.as_str(),"queued"|"starting"|"preparing_edge")}),
                                );
                            }
                        }
                        let _ = host.save(&current);
                    }
                    host.tasks.lock().await.remove(&ctx.id);
                });
                Ok(serde_json::to_value(record.view)?)
            }
            Wire::Get { id, wait_ms } => {
                ensure!(wait_ms <= 30000, "invalid_wait: wait_ms 不得超过 30000");
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let record = self.owned(peer, &id)?;
                if record.view.terminal() || wait_ms == 0 {
                    return Ok(serde_json::to_value(record.view)?);
                }
                let _ = tokio::time::timeout(Duration::from_millis(wait_ms), notified).await;
                Ok(serde_json::to_value(self.owned(peer, &id)?.view)?)
            }
            Wire::Cancel { id } => {
                let record = self.owned(peer, &id)?;
                if let Some(cancel) = self.tasks.lock().await.get(&id) {
                    cancel.cancel();
                }
                Ok(serde_json::to_value(record.view)?)
            }
            Wire::Read {
                id,
                offset,
                limit,
                query,
            } => self
                .results
                .read(peer, &id, offset, limit, query.as_deref()),
            Wire::Artifact { id, offset } => self.results.chunk(peer, &id, offset),
            Wire::Stop => {
                ensure!(
                    peer == self.results.store.read()?.id,
                    "permission_denied: 仅本机所有者可停止助手"
                );
                Ok(json!({"stopping":true}))
            }
        }
    }
    async fn execute(
        self: &Arc<Self>,
        ctx: &ContextTask,
        action: Action,
        budget: Budget,
        seconds: u64,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let mut record = self.load(&ctx.id)?;
        record.view.state = "running".into();
        record.view.phase = "starting".into();
        self.save(&record)?;
        let operation = async {
            if action.mutates() && !matches!(action, Action::DesktopEdgeAsk { .. }) {
                ctx.phase("applying").await?;
            }
            if action.capability() == "browser" {
                self.browser.execute(&action, &budget, ctx).await
            } else {
                self.desktop.execute(&action, &budget, ctx).await
            }
        };
        let deadline = tokio::time::sleep(Duration::from_secs(seconds));
        tokio::pin!(deadline, operation);
        let mut permissions = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                value=&mut operation=>return value,
                _=&mut deadline=>anyhow::bail!("timed_out: 操作超时；变更操作的最终状态需要确认"),
                _=cancel.cancelled()=>anyhow::bail!("cancelled: 已取消等待，已发生的网页或桌面操作不会回滚"),
                _=self.stop.cancelled()=>anyhow::bail!("interrupted: 用户助手正在退出"),
                _=permissions.tick()=>if permitted(&self.results.store,&ctx.peer,&ctx.capability).is_err(){anyhow::bail!("permission_revoked: 权限已撤回，任务已停止")}
            }
        }
    }
}

fn pipe_name(store: &Store, background: bool) -> Result<String> {
    let cfg = store.read()?;
    let suffix = if background {
        "background".to_string()
    } else {
        platform::active_owner_session(&cfg.owner_sid)?
            .context("session_unavailable: 配置所有者尚未登录到活动桌面；可显式使用 --background")?
            .to_string()
    };
    Ok(format!(r"\\.\pipe\worklink-{}-session-{suffix}", cfg.id))
}
async fn connect(name: &str, owner: &str) -> Result<NamedPipeClient> {
    let pipe = ClientOptions::new()
        .security_qos_flags(windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION)
        .open(name)?;
    let mut pid = 0;
    unsafe {
        use std::os::windows::io::AsRawHandle;
        platform::wincheck(
            windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(
                pipe.as_raw_handle(),
                &mut pid,
            ),
        )?;
    }
    ensure!(
        platform::process_sid(pid)? == owner,
        "helper_identity_mismatch: 用户助手身份不匹配"
    );
    Ok(pipe)
}
pub async fn forward(store: &Store, peer: &str, background: bool, request: Wire) -> Result<Value> {
    let cfg = store.read()?;
    let name = pipe_name(store, background)?;
    let mut pipe = match connect(&name, &cfg.owner_sid).await {
        Ok(pipe) => pipe,
        Err(error)
            if matches!(
                error
                    .downcast_ref::<std::io::Error>()
                    .and_then(|e| e.raw_os_error()),
                Some(2 | 231)
            ) =>
        {
            let store = store.clone();
            let owner = cfg.owner_sid.clone();
            tokio::task::spawn_blocking(move || {
                platform::launch_session_helper(&store, &owner, background)
            })
            .await??;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                match connect(&name, &cfg.owner_sid).await {
                    Ok(pipe) => break pipe,
                    Err(error)
                        if tokio::time::Instant::now() < deadline
                            && matches!(
                                error
                                    .downcast_ref::<std::io::Error>()
                                    .and_then(|e| e.raw_os_error()),
                                Some(2 | 231)
                            ) =>
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await
                    }
                    Err(error) => {
                        return Err(error).context("helper_unavailable: 用户助手无法启动");
                    }
                }
            }
        }
        Err(error) => return Err(error),
    };
    protocol::write_json(
        &mut pipe,
        &Envelope {
            peer: peer.into(),
            request,
        },
    )
    .await?;
    tokio::time::timeout(
        Duration::from_secs(35),
        protocol::read_json::<_, Reply>(&mut pipe),
    )
    .await??
    .checked()
}
fn server(name: &str, owner: &str, first: bool) -> Result<NamedPipeServer> {
    let mut security = platform::PipeSecurity::new(owner)?;
    Ok(unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut security.attrs as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES)
                    .cast(),
            )?
    })
}
pub async fn run(store: Store, background: bool) -> Result<()> {
    let cfg = store.read()?;
    ensure!(
        platform::current_sid()? == cfg.owner_sid,
        "helper_wrong_user: 用户助手必须以配置所有者运行"
    );
    let name = pipe_name(&store, background)?;
    let results = Results::new(store.clone())?;
    let lock_path = store.root.join(format!(
        "automation/session-{}.lock",
        if background {
            "background".into()
        } else {
            platform::session_id()?.to_string()
        }
    ));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(());
    }
    let stop = CancellationToken::new();
    let scope = if background {
        "background".into()
    } else {
        format!("session-{}", platform::session_id()?)
    };
    let host = Arc::new(Host {
        results: results.clone(),
        scope,
        background,
        browser: Browser::new(store.clone(), background),
        desktop: Desktop::new(background),
        tasks: Mutex::new(HashMap::new()),
        changed: Notify::new(),
        slots: Arc::new(Semaphore::new(8)),
        stop: stop.clone(),
    });
    host.prune_tasks(true)?;
    let _logging = crate::logging(&store)?;
    tracing::info!(background, "user session helper ready");
    let mut pipe = server(&name, &cfg.owner_sid, true)?;
    let connections = Arc::new(Semaphore::new(32));
    let mut handlers = tokio::task::JoinSet::new();
    let mut maintenance = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _=stop.cancelled()=>break,
            _=maintenance.tick()=>host.desktop.idle().await,
            _=tokio::signal::ctrl_c()=>{stop.cancel();break;},
            Some(_)=handlers.join_next(),if !handlers.is_empty()=>{},
            result=pipe.connect()=>{
                result?;let connected=pipe;pipe=server(&name,&cfg.owner_sid,false)?;
                if let Ok(slot)=connections.clone().try_acquire_owned(){
                    let host=host.clone();handlers.spawn(async move {
                        let _slot=slot;let mut pipe=connected;
                        let result=async {
                            let envelope:Envelope=tokio::time::timeout(Duration::from_secs(10),protocol::read_json(&mut pipe)).await??;
                            host.handle(&envelope.peer,envelope.request).await
                        }.await;
                        let stopping=result.as_ref().is_ok_and(|v|v["stopping"]==true);
                        let reply=match result{Ok(value)=>Reply::ok(value),Err(error)=>Reply::error(format!("{error:#}"))};
                        let _=protocol::write_json(&mut pipe,&reply).await;
                        // 停止请求先回执，再退出监听，避免主循环提前取消正在发送回执的连接。
                        if stopping{host.stop.cancel();}
                    });
                }
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !host.tasks.lock().await.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    handlers.shutdown().await;
    Ok(())
}

pub async fn call(store: &Store, device: &str, background: bool, request: Wire) -> Result<Value> {
    if device.eq_ignore_ascii_case("self") {
        forward(store, &store.read()?.id, background, request).await
    } else {
        crate::client::remote(
            store,
            device,
            crate::protocol::Request::Automation {
                background,
                request,
            },
        )
        .await
    }
}
