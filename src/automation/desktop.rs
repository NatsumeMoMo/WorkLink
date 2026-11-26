use super::{Action, Budget, session::ContextTask};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
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
    edge_chat: Mutex<()>,
}
impl Desktop {
    pub fn new(background: bool) -> Self {
        Self {
            background,
            worker: Mutex::new(None),
            edge_chat: Mutex::new(()),
        }
    }
    fn start() -> Result<Worker> {
        Worker::start(include_str!("desktop.ps1"))
    }
    async fn ask_edge(&self, action: &Action, budget: &Budget, ctx: &ContextTask) -> Result<Value> {
        let Action::DesktopEdgeAsk {
            prompt,
            model,
            window,
            conversation,
        } = action
        else {
            unreachable!()
        };
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= 32768 && !prompt.contains('\0'),
            "invalid_prompt: 问题应为 1–32768 个 UTF-8 字节"
        );
        ensure!(
            model.as_ref().is_none_or(|m| !m.trim().is_empty()
                && m.chars().count() <= 80
                && !m.contains(char::is_control)),
            "invalid_model: 模型名称应为 1–80 个字符"
        );
        ensure!(
            window.is_none_or(|w| w > 0),
            "invalid_window: 需要有效的窗口句柄"
        );
        let _guard = self
            .edge_chat
            .try_lock()
            .context("edge_chat_busy: 此设备已有一项 Edge 对话任务，请查询原任务")?;
        ctx.phase("preparing_edge").await?;
        let code = format!(
            "{}\n{}",
            include_str!("edge-chat.ps1"),
            include_str!("desktop.ps1")
        );
        let mut worker = Worker::start_pipe_script(&code).await?;
        worker.edge_request(json!({"kind":"edge_prepare","task":ctx.id,"window":window,"conversation":conversation,"model":model}), 40, 65536).await?;
        super::permitted(&ctx.results().store, &ctx.peer, "desktop")?;
        // 持久化提交边界后才允许 UIA 点击发送，响应丢失时只能查原任务。
        ctx.phase("submission_started").await?;
        let sent = worker
            .edge_request(json!({"kind":"edge_send","prompt":prompt}), 20, 65536)
            .await?;
        ctx.phase("waiting_for_answer").await?;
        let completed = loop {
            let reply = worker
                .edge_request(json!({"kind":"edge_probe"}), 18, 65536)
                .await?;
            if reply["finished"] == true {
                break worker
                    .edge_request(json!({"kind":"edge_result"}), 18, 4 * 1024 * 1024)
                    .await?;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        let answer = completed["answer"]
            .as_str()
            .context("edge_answer_unavailable")?;
        let mut saved = ctx
            .results()
            .save_text(&ctx.peer, "desktop", answer, budget)?;
        if completed["truncated"] == true {
            saved["source_truncated"] = json!(true);
        }
        saved["model"] = sent["model"].clone();
        saved["conversation"] = completed["conversation"].clone();
        saved["window"] = completed["window"].clone();
        Ok(saved)
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
        if matches!(action, Action::DesktopEdgeAsk { .. }) {
            return self.ask_edge(action, budget, ctx).await;
        }
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

impl Worker {
    fn start(script: &str) -> Result<Self> {
        let mut process = Command::new(crate::platform::open_terminal_program());
        process
            .args(crate::client::powershell(script))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .creation_flags(0x08000000);
        let mut child = process.spawn().context("desktop_worker_failed")?;
        Ok(Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            last: Instant::now(),
        })
    }

    async fn start_pipe_script(script: &str) -> Result<Self> {
        // 避开 Windows 命令行长度限制。脚本来自 EXE 内置资源，请求仍以 JSON 与代码分开传递。
        let mut worker = Self::start(
            "$ErrorActionPreference='Stop'; [Console]::InputEncoding=[Text.UTF8Encoding]::new($false); \
            $scriptText=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String([Console]::ReadLine())); \
            . ([ScriptBlock]::Create($scriptText))",
        )?;
        let encoded = STANDARD.encode(script);
        worker.input.write_all(encoded.as_bytes()).await?;
        worker.input.write_all(b"\n").await?;
        worker.input.flush().await?;
        // 等 PowerShell 初始化完标准输入后再发 JSON，避免编码重设丢掉已经缓冲的第一条请求。
        let mut ready = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(20),
            (&mut worker.output).take(128).read_until(b'\n', &mut ready),
        )
        .await
        .context("edge_worker_start_timeout: Edge UI Automation 进程启动超时")??;
        ensure!(
            ready == b"WORKLINK_EDGE_READY\r\n" || ready == b"WORKLINK_EDGE_READY\n",
            "edge_worker_failed: Edge UI Automation 未完成初始化"
        );
        Ok(worker)
    }

    async fn edge_request(
        &mut self,
        request: Value,
        seconds: u64,
        max_bytes: u64,
    ) -> Result<Value> {
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        let response = tokio::time::timeout(Duration::from_secs(seconds), async {
            self.input.write_all(&bytes).await?;
            self.input.flush().await?;
            let mut line = Vec::new();
            (&mut self.output)
                .take(max_bytes)
                .read_until(b'\n', &mut line)
                .await?;
            ensure!(
                line.last() == Some(&b'\n'),
                "edge_worker_disconnected: Edge UI Automation 响应中断或超限"
            );
            Ok::<Value, anyhow::Error>(serde_json::from_slice(&line)?)
        })
        .await
        .context("edge_unresponsive: Edge UI Automation 在规定时间内没有响应")??;
        if response["ok"] != true {
            bail!(
                "{}",
                response["error"].as_str().unwrap_or("edge_action_failed")
            );
        }
        Ok(response["data"].clone())
    }
}
