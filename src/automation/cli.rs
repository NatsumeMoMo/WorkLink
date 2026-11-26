use super::{Action, Budget, TaskView, Wire, session};
use crate::config::Store;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Args)]
pub struct Options {
    #[arg(long, global = true)]
    pub background: bool,
    #[arg(long, global = true)]
    pub id: Option<String>,
    #[arg(long, global = true, default_value_t = 120)]
    pub timeout: u64,
    #[arg(long, global = true)]
    pub detach: bool,
    #[arg(long, global = true, default_value_t = 4000)]
    pub max_chars: usize,
    #[arg(long, global = true, default_value_t = 30)]
    pub max_items: usize,
}
#[derive(Args)]
pub struct BrowserCli {
    pub device: String,
    #[command(flatten)]
    pub options: Options,
    #[command(subcommand)]
    pub action: BrowserCommand,
}
#[derive(Subcommand)]
pub enum BrowserCommand {
    Status,
    Open {
        url: String,
    },
    Tabs,
    Snapshot {
        tab: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    Click {
        tab: String,
        reference: String,
    },
    Fill {
        tab: String,
        reference: String,
        text: String,
    },
    Press {
        tab: String,
        reference: String,
        key: String,
    },
    Text {
        tab: String,
        #[arg(long)]
        selector: Option<String>,
    },
    Wait {
        tab: String,
        #[arg(long)]
        selector: Option<String>,
        #[arg(long)]
        text: Option<String>,
    },
    Screenshot {
        tab: String,
    },
    Close {
        tab: Option<String>,
    },
    Ask {
        tab: String,
        prompt: String,
        #[arg(long)]
        input: Option<String>,
        #[arg(long)]
        send: Option<String>,
        #[arg(long)]
        answer: Option<String>,
    },
}
impl From<BrowserCommand> for Action {
    fn from(value: BrowserCommand) -> Self {
        match value {
            BrowserCommand::Status => Self::BrowserStatus,
            BrowserCommand::Open { url } => Self::BrowserOpen { url },
            BrowserCommand::Tabs => Self::BrowserTabs,
            BrowserCommand::Snapshot { tab, offset } => Self::BrowserSnapshot { tab, offset },
            BrowserCommand::Click { tab, reference } => Self::BrowserClick { tab, reference },
            BrowserCommand::Fill {
                tab,
                reference,
                text,
            } => Self::BrowserFill {
                tab,
                reference,
                text,
            },
            BrowserCommand::Press {
                tab,
                reference,
                key,
            } => Self::BrowserPress {
                tab,
                reference,
                key,
            },
            BrowserCommand::Text { tab, selector } => Self::BrowserText { tab, selector },
            BrowserCommand::Wait {
                tab,
                selector,
                text,
            } => Self::BrowserWait {
                tab,
                selector,
                text,
            },
            BrowserCommand::Screenshot { tab } => Self::BrowserScreenshot { tab },
            BrowserCommand::Close { tab } => Self::BrowserClose { tab },
            BrowserCommand::Ask {
                tab,
                prompt,
                input,
                send,
                answer,
            } => Self::BrowserAsk {
                tab,
                prompt,
                input,
                send,
                answer,
            },
        }
    }
}
#[derive(Args)]
pub struct DesktopCli {
    pub device: String,
    #[command(flatten)]
    pub options: Options,
    #[command(subcommand)]
    pub action: DesktopCommand,
}
#[derive(Subcommand)]
pub enum DesktopCommand {
    Windows,
    Launch {
        program: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    Snapshot {
        window: i64,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    Invoke {
        reference: String,
    },
    Fill {
        reference: String,
        text: String,
    },
    Focus {
        window: i64,
    },
    Screenshot {
        window: i64,
    },
    /// 在已打开的普通 Edge 中使用现有登录态向 ChatGPT 提问
    AskChatgpt {
        #[arg(
            long,
            conflicts_with = "prompt_file",
            required_unless_present = "prompt_file"
        )]
        prompt: Option<String>,
        #[arg(long, value_name = "UTF8_FILE")]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        window: Option<i64>,
        #[arg(long)]
        conversation: Option<String>,
    },
}
impl TryFrom<DesktopCommand> for Action {
    type Error = anyhow::Error;

    fn try_from(value: DesktopCommand) -> Result<Self> {
        Ok(match value {
            DesktopCommand::Windows => Self::DesktopWindows,
            DesktopCommand::Launch { program, args } => Self::DesktopLaunch { program, args },
            DesktopCommand::Snapshot { window, offset } => Self::DesktopSnapshot { window, offset },
            DesktopCommand::Invoke { reference } => Self::DesktopInvoke { reference },
            DesktopCommand::Fill { reference, text } => Self::DesktopFill { reference, text },
            DesktopCommand::Focus { window } => Self::DesktopFocus { window },
            DesktopCommand::Screenshot { window } => Self::DesktopScreenshot { window },
            DesktopCommand::AskChatgpt {
                prompt,
                prompt_file,
                model,
                window,
                conversation,
            } => {
                let prompt = if let Some(path) = prompt_file {
                    ensure!(std::fs::metadata(&path)?.len() <= 32771, "prompt_too_large");
                    std::fs::read_to_string(path)?
                        .trim_start_matches('\u{feff}')
                        .to_owned()
                } else {
                    prompt.context("prompt_required")?
                };
                Self::DesktopEdgeAsk {
                    prompt,
                    model,
                    window,
                    conversation,
                }
            }
        })
    }
}
#[derive(Args)]
pub struct ToolCli {
    pub device: String,
    #[arg(long, global = true)]
    pub background: bool,
    #[command(subcommand)]
    pub action: ToolCommand,
}
#[derive(Subcommand)]
pub enum ToolCommand {
    Get {
        id: String,
        #[arg(long, default_value_t = 0)]
        wait_ms: u64,
    },
    Cancel {
        id: String,
    },
    Read {
        id: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 2000)]
        limit: usize,
        #[arg(long)]
        query: Option<String>,
    },
    Download {
        id: String,
        path: PathBuf,
    },
    Stop,
}
pub fn output(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

pub async fn submit(
    store: &Store,
    device: &str,
    options: &Options,
    action: Action,
    wait_ms: u64,
) -> Result<Value> {
    let id = options
        .id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    super::valid_id(&id)?;
    let budget = Budget {
        max_chars: options.max_chars,
        max_items: options.max_items,
    };
    budget.validate()?;
    // 提交响应丢失时仍返回原 ID，调用者才能查询结果，避免盲目重做有副作用的操作。
    let result = session::call(
        store,
        device,
        options.background,
        Wire::Submit {
            id: id.clone(),
            action,
            budget,
            timeout: options.timeout,
        },
    )
    .await;
    let mut value = match result {
        Ok(v) => v,
        Err(e) => {
            return Ok(
                json!({"id":id,"background":options.background,"state":"submission_unknown","error":{"message":super::clip(&format!("{e:#}"),600),"hint":"query this id before retrying; reuse the same id and parameters"}}),
            );
        }
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let view: TaskView = serde_json::from_value(value.clone())?;
        if view.terminal() || options.detach || tokio::time::Instant::now() >= deadline {
            return Ok(value);
        }
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis()
            .min(30000) as u64;
        match session::call(
            store,
            device,
            options.background,
            Wire::Get {
                id: id.clone(),
                wait_ms: remaining,
            },
        )
        .await
        {
            Ok(v) => value = v,
            Err(e) => {
                value["poll_error"] = json!(super::clip(&format!("{e:#}"), 500));
                return Ok(value);
            }
        }
    }
}
fn exit_code(value: &Value) -> i32 {
    if matches!(
        value["state"].as_str(),
        Some("failed" | "cancelled" | "timed_out" | "interrupted" | "submission_unknown")
    ) {
        1
    } else {
        0
    }
}
pub async fn browser(store: &Store, args: BrowserCli) -> Result<i32> {
    let value = submit(
        store,
        &args.device,
        &args.options,
        args.action.into(),
        args.options.timeout.saturating_add(15).saturating_mul(1000),
    )
    .await?;
    output(&value)?;
    Ok(exit_code(&value))
}
pub async fn desktop(store: &Store, args: DesktopCli) -> Result<i32> {
    let value = submit(
        store,
        &args.device,
        &args.options,
        args.action.try_into()?,
        args.options.timeout.saturating_add(15).saturating_mul(1000),
    )
    .await?;
    output(&value)?;
    Ok(exit_code(&value))
}

pub async fn download(
    store: &Store,
    device: &str,
    background: bool,
    id: &str,
    path: &Path,
) -> Result<Value> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    let mut offset = 0u64;
    let mut hash = Sha256::new();
    let mut expected = None;
    loop {
        let value = session::call(
            store,
            device,
            background,
            Wire::Artifact {
                id: id.into(),
                offset,
            },
        )
        .await?;
        let size = value["bytes"].as_u64().context("invalid_artifact")?;
        let digest = value["sha256"]
            .as_str()
            .context("invalid_artifact")?
            .to_string();
        ensure!(
            size <= super::store::MAX_RESULT_BYTES as u64 && value["offset"] == offset,
            "invalid_artifact"
        );
        if let Some((old_size, ref old_hash)) = expected {
            ensure!(old_size == size && old_hash == &digest, "artifact_changed");
        } else {
            expected = Some((size, digest.clone()));
        }
        let bytes = STANDARD.decode(value["data"].as_str().context("invalid_artifact")?)?;
        ensure!(
            bytes.len() <= super::store::CHUNK_BYTES && offset + bytes.len() as u64 <= size,
            "invalid_artifact"
        );
        ensure!(!bytes.is_empty() || offset == size, "artifact_stalled");
        file.write_all(&bytes)?;
        hash.update(&bytes);
        offset += bytes.len() as u64;
        if offset == size {
            ensure!(
                hex::encode(hash.finalize()) == digest,
                "artifact_checksum_mismatch"
            );
            file.as_file().sync_all()?;
            file.persist_noclobber(path)
                .context("output_exists_or_unwritable")?;
            return Ok(json!({"path":std::path::absolute(path)?,"bytes":size,"sha256":digest}));
        }
    }
}
pub async fn tool(store: &Store, args: ToolCli) -> Result<i32> {
    let wire = match args.action {
        ToolCommand::Get { id, wait_ms } => Wire::Get { id, wait_ms },
        ToolCommand::Cancel { id } => Wire::Cancel { id },
        ToolCommand::Read {
            id,
            offset,
            limit,
            query,
        } => Wire::Read {
            id,
            offset,
            limit,
            query,
        },
        ToolCommand::Stop => Wire::Stop,
        ToolCommand::Download { id, path } => {
            output(&download(store, &args.device, args.background, &id, &path).await?)?;
            return Ok(0);
        }
    };
    let value = session::call(store, &args.device, args.background, wire).await?;
    output(&value)?;
    Ok(exit_code(&value))
}
