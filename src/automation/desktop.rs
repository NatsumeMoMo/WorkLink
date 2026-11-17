use super::{Action, Budget, session::ContextTask};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

struct Worker {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    last: Instant,
}
pub struct Desktop {
    background: bool,
    worker: Mutex<Option<Worker>>,
}
impl Desktop {
    pub fn new(background: bool) -> Self {
        Self {
            background,
            worker: Mutex::new(None),
        }
    }
    fn start() -> Result<Worker> {
        let mut process = Command::new(crate::platform::open_terminal_program());
        process
            .args(crate::client::powershell(include_str!("desktop.ps1")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .creation_flags(0x08000000);
        let mut child = process.spawn().context("desktop_worker_failed")?;
        Ok(Worker {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            last: Instant::now(),
        })
    }
    pub async fn idle(&self) {
        if let Ok(mut slot) = self.worker.try_lock()
            && slot
                .as_ref()
                .is_some_and(|w| w.last.elapsed() > Duration::from_secs(60))
        {
            *slot = None;
        }
    }
    pub async fn execute(
        &self,
        action: &Action,
        budget: &Budget,
        ctx: &ContextTask,
    ) -> Result<Value> {
        ensure!(
            !self.background,
            "desktop_session_unavailable: --background 不提供桌面操作"
        );
        let mut slot = self.worker.lock().await;
        let mut worker = if let Some(mut worker) = slot.take() {
            if worker.child.try_wait()?.is_none() {
                worker
            } else {
                Self::start()?
            }
        } else {
            Self::start()?
        };
        let mut request = serde_json::to_value(action)?;
        request["peer"] = json!(ctx.peer);
        request["budget"] = serde_json::to_value(budget)?;
        if let Action::DesktopLaunch { program, args } = action {
            ensure!(
                std::path::Path::new(program).is_absolute()
                    && std::path::Path::new(program).is_file(),
                "invalid_program: 需要存在的本机绝对程序路径"
            );
            ensure!(args.len() <= 64, "too_many_arguments");
            request["arguments"] = json!(
                args.iter()
                    .map(|arg| crate::platform::quote_arg(arg))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        let artifact = if matches!(action, Action::DesktopScreenshot { .. }) {
            let (id, path) = ctx.results().allocate();
            request["output"] = json!(path);
            Some((id, path))
        } else {
            None
        };
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        // UIA 提供程序可能卡住；将它隔离在可结束的子进程，超时或取消不会耗尽服务线程。
        let operation = async {
            worker.input.write_all(&bytes).await?;
            worker.input.flush().await?;
            let mut line = String::new();
            let n = worker.output.read_line(&mut line).await?;
            ensure!(
                n > 0 && n <= 64 * 1024,
                "desktop_worker_disconnected: 请重新获取控件快照"
            );
            let value: Value = serde_json::from_str(&line)?;
            Ok::<_, anyhow::Error>(value)
        };
        let response = tokio::time::timeout(Duration::from_secs(15), operation)
            .await
            .context("desktop_unresponsive: UIA 超时，旧控件引用已失效")??;
        worker.last = Instant::now();
        *slot = Some(worker);
        if response["ok"] != true {
            bail!(
                "desktop_action_failed: {}",
                response["error"].as_str().unwrap_or("UIA 操作失败")
            );
        }
        if let Some((id, path)) = artifact {
            let result = ctx
                .results()
                .finish(&id, &ctx.peer, "desktop", "image/jpeg", false);
            if result.is_err() {
                let _ = std::fs::remove_file(path);
            }
            let mut result = result?;
            result["width"] = response["data"]["width"].clone();
            result["height"] = response["data"]["height"].clone();
            return Ok(result);
        }
        Ok(response["data"].clone())
    }
}
