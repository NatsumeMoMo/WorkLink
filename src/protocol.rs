use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_HEADER: usize = 256 * 1024;
pub const MAX_STREAMS: usize = 64;
pub const HANDSHAKE_SECONDS: u64 = 12;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Ssh,
    JobSubmit {
        job: JobSpec,
    },
    JobGet {
        id: String,
    },
    JobLogs {
        id: String,
    },
    JobCancel {
        id: String,
    },
    JobList,
    Upload {
        path: String,
        size: u64,
        sha256: String,
        overwrite: bool,
        system: bool,
    },
    Download {
        path: String,
        offset: u64,
        sha256: Option<String>,
        system: bool,
    },
    ListDir {
        path: String,
        system: bool,
    },
    Terminal {
        shell: Option<String>,
        cols: u16,
        rows: u16,
        system: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JobSpec {
    pub id: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_secs: u64,
    pub system: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

impl Reply {
    pub fn ok(data: impl Serialize) -> Self {
        Self {
            ok: true,
            error: None,
            data: serde_json::to_value(data).unwrap_or_default(),
        }
    }
    pub fn error(error: impl ToString) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            data: serde_json::Value::Null,
        }
    }
    pub fn checked(self) -> Result<serde_json::Value> {
        ensure!(self.ok, "{}", self.error.as_deref().unwrap_or("请求失败"));
        Ok(self.data)
    }
}

pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(io: &mut W, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_HEADER, "消息头超过限制");
    io.write_u32(bytes.len() as u32).await?;
    io.write_all(&bytes).await?;
    io.flush().await?;
    Ok(())
}

pub async fn read_json<R: AsyncRead + Unpin, T: DeserializeOwned>(io: &mut R) -> Result<T> {
    let length = io.read_u32().await? as usize;
    ensure!(length > 0 && length <= MAX_HEADER, "消息头长度无效");
    let mut bytes = vec![0; length];
    io.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LocalRequest {
    Status,
    Open { peer: String, request: Request },
    Shutdown,
}
