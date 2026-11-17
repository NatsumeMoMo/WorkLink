pub mod browser;
pub mod cli;
pub mod desktop;
pub mod mcp;
pub mod session;
pub mod store;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_chars() -> usize {
    4000
}
fn default_items() -> usize {
    30
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default = "default_chars")]
    pub max_chars: usize,
    #[serde(default = "default_items")]
    pub max_items: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            max_chars: 4000,
            max_items: 30,
        }
    }
}
impl Budget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (512..=16000).contains(&self.max_chars),
            "invalid_budget: max_chars 应为 512–16000"
        );
        ensure!(
            (1..=100).contains(&self.max_items),
            "invalid_budget: max_items 应为 1–100"
        );
        Ok(())
    }
    pub fn text_chars(&self) -> usize {
        self.max_chars.saturating_sub(800)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    BrowserStatus,
    BrowserOpen {
        url: String,
    },
    BrowserTabs,
    BrowserSnapshot {
        tab: String,
        #[serde(default)]
        offset: usize,
    },
    BrowserClick {
        tab: String,
        reference: String,
    },
    BrowserFill {
        tab: String,
        reference: String,
        text: String,
    },
    BrowserPress {
        tab: String,
        reference: String,
        key: String,
    },
    BrowserText {
        tab: String,
        selector: Option<String>,
    },
    BrowserWait {
        tab: String,
        selector: Option<String>,
        text: Option<String>,
    },
    BrowserScreenshot {
        tab: String,
    },
    BrowserClose {
        tab: Option<String>,
    },
    BrowserAsk {
        tab: String,
        prompt: String,
        input: Option<String>,
        send: Option<String>,
        answer: Option<String>,
    },
    DesktopWindows,
    DesktopLaunch {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    DesktopSnapshot {
        window: i64,
        #[serde(default)]
        offset: usize,
    },
    DesktopInvoke {
        reference: String,
    },
    DesktopFill {
        reference: String,
        text: String,
    },
    DesktopFocus {
        window: i64,
    },
    DesktopScreenshot {
        window: i64,
    },
}
impl Action {
    pub fn capability(&self) -> &'static str {
        match self {
            Self::DesktopWindows
            | Self::DesktopLaunch { .. }
            | Self::DesktopSnapshot { .. }
            | Self::DesktopInvoke { .. }
            | Self::DesktopFill { .. }
            | Self::DesktopFocus { .. }
            | Self::DesktopScreenshot { .. } => "desktop",
            _ => "browser",
        }
    }
    pub fn mutates(&self) -> bool {
        matches!(
            self,
            Self::BrowserOpen { .. }
                | Self::BrowserClick { .. }
                | Self::BrowserFill { .. }
                | Self::BrowserPress { .. }
                | Self::BrowserClose { .. }
                | Self::BrowserAsk { .. }
                | Self::DesktopLaunch { .. }
                | Self::DesktopInvoke { .. }
                | Self::DesktopFill { .. }
                | Self::DesktopFocus { .. }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Wire {
    Submit {
        id: String,
        action: Action,
        #[serde(default)]
        budget: Budget,
        #[serde(default = "default_timeout")]
        timeout: u64,
    },
    Get {
        id: String,
        #[serde(default)]
        wait_ms: u64,
    },
    Cancel {
        id: String,
    },
    Read {
        id: String,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_read_limit")]
        limit: usize,
        query: Option<String>,
    },
    Artifact {
        id: String,
        #[serde(default)]
        offset: u64,
    },
    Stop,
}
fn default_timeout() -> u64 {
    60
}
fn default_read_limit() -> usize {
    2000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    #[serde(default)]
    pub background: bool,
    pub state: String,
    pub phase: String,
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}
impl TaskView {
    pub fn terminal(&self) -> bool {
        !matches!(self.state.as_str(), "queued" | "running")
    }
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn clip(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}
pub fn valid_id(id: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok()
            && id.len() <= 36
            && id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'),
        "invalid_id: 需要 UUID"
    );
    Ok(())
}

pub fn permitted(store: &crate::config::Store, peer: &str, capability: &str) -> Result<()> {
    let cfg = store.read()?;
    if peer == cfg.id {
        return Ok(());
    }
    let paired = crate::config::find_peer(&cfg, peer)?;
    ensure!(
        if capability == "browser" {
            paired.allow.browser
        } else {
            paired.allow.desktop
        },
        "automation_permission_denied: {capability} 权限已关闭"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budgets_ids_and_old_permissions() {
        assert!(Budget::default().validate().is_ok());
        assert!(
            Budget {
                max_chars: 511,
                max_items: 1
            }
            .validate()
            .is_err()
        );
        assert!(valid_id("../../config").is_err());
        assert!(valid_id(&uuid::Uuid::new_v4().to_string()).is_ok());
        let old: crate::config::Permissions = serde_json::from_value(
            serde_json::json!({"ssh":true,"files":true,"exec":true,"system":false}),
        )
        .unwrap();
        assert!(!old.browser && !old.desktop);
        assert!(serde_json::from_value::<Action>(serde_json::json!({"kind":"browser_open","url":"https://example.com","script":"ignored?"})).is_err());
    }
}
