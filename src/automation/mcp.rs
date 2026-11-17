use super::{
    Action, Wire,
    cli::{self, Options},
    session,
};
use crate::config::Store;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn catalog() -> Value {
    let device = json!({"type":"string","description":"paired device name/id, or self"});
    let tools = vec![
        json!({"name":"worklink_devices","description":"List paired devices and connection state.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
        json!({"name":"worklink_browser","description":"Remote dedicated Edge/Chrome. action.kind: browser_status/open/tabs/snapshot/click/fill/press/text/wait/screenshot/close/ask. open:{url}; snapshot:{tab,offset?}; click:{tab,reference}; fill:{tab,reference,text}; press:{tab,reference,key}; text/wait:{tab,selector?,text?}; ask:{tab,prompt,input?,send?,answer?}. Get refs from snapshot. ask submits once and waits for a stable answer. Screenshots return artifact IDs only. Long text returns preview + result_id; read pages via worklink_result. Reuse id after uncertainty; never blindly repeat a submission.","inputSchema":operation_schema(&device)}),
        json!({"name":"worklink_desktop","description":"Windows UI Automation in logged-in unlocked owner session. action.kind: desktop_windows/launch/snapshot/invoke/fill/focus/screenshot. launch:{program:absolute path,args?}; snapshot/focus/screenshot:{window:numeric handle}; invoke:{reference}; fill:{reference,text}. Snapshot supplies short refs; ref_expired requires a fresh snapshot. No password fields. Screenshots are artifact IDs.","inputSchema":operation_schema(&device)}),
        json!({"name":"worklink_task","description":"Query or cancel an existing automation id. get supports long polling up to 30000ms. Match background to the original request. Cancellation does not undo an applied action.","inputSchema":{"type":"object","properties":{"device":device,"background":{"type":"boolean","default":false},"id":{"type":"string"},"op":{"enum":["get","cancel"]},"wait_ms":{"type":"integer","minimum":0,"maximum":30000}},"required":["device","id","op"],"additionalProperties":false}}),
        json!({"name":"worklink_result","description":"Read a text result by Unicode character offset (default 2000), search query snippets, or download an artifact to an absolute local path. Binary bytes never enter tool context. Results expire after 24h.","inputSchema":{"type":"object","properties":{"device":device,"background":{"type":"boolean","default":false},"id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":16000},"query":{"type":"string"},"download_path":{"type":"string"}},"required":["device","id"],"additionalProperties":false}}),
    ];
    json!({"tools":tools})
}
fn operation_schema(device: &Value) -> Value {
    json!({"type":"object","properties":{"device":device,"background":{"type":"boolean","default":false},"id":{"type":"string","description":"UUID; reuse exactly after uncertain submission"},"action":{"type":"object","properties":{"kind":{"type":"string"}},"required":["kind"],"additionalProperties":true},"timeout":{"type":"integer","default":120,"minimum":1,"maximum":3600},"wait_ms":{"type":"integer","default":1500,"minimum":0,"maximum":30000},"max_chars":{"type":"integer","default":4000,"minimum":512,"maximum":16000},"max_items":{"type":"integer","default":30,"minimum":1,"maximum":100}},"required":["device","action"],"additionalProperties":false})
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    device: String,
    #[serde(default)]
    background: bool,
    id: Option<String>,
    action: Action,
    #[serde(default = "timeout")]
    timeout: u64,
    #[serde(default = "wait")]
    wait_ms: u64,
    #[serde(default = "chars")]
    max_chars: usize,
    #[serde(default = "items")]
    max_items: usize,
}
fn timeout() -> u64 {
    120
}
fn wait() -> u64 {
    1500
}
fn chars() -> usize {
    4000
}
fn items() -> usize {
    30
}
async fn call(store: &Store, name: &str, args: Value) -> Result<Value> {
    if name == "worklink_devices" {
        let status = crate::client::local(store, crate::protocol::LocalRequest::Status).await?;
        let peers = status["peers"]
            .as_array()
            .context("invalid_device_status")?;
        let compact:Vec<Value>=peers.iter().take(20).map(|p|json!({"id":p["id"],"name":p["name"],"online":p["online"],"allow":p["allow"],"last_error":p["last_error"].as_str().map(|s|super::clip(s,200))})).collect();
        return Ok(
            json!({"id":status["id"],"name":status["name"],"version":status["version"],"peers":compact,"truncated":peers.len()>20}),
        );
    }
    if name == "worklink_browser" || name == "worklink_desktop" {
        let a: Operation = serde_json::from_value(args)?;
        ensure!(a.wait_ms <= 30000, "invalid_wait");
        ensure!(
            a.action.capability()
                == if name == "worklink_browser" {
                    "browser"
                } else {
                    "desktop"
                },
            "wrong_tool_action"
        );
        return cli::submit(
            store,
            &a.device,
            &Options {
                background: a.background,
                id: a.id,
                timeout: a.timeout,
                detach: false,
                max_chars: a.max_chars,
                max_items: a.max_items,
            },
            a.action,
            a.wait_ms,
        )
        .await;
    }
    let device = args["device"].as_str().context("device_required")?;
    let background = args["background"].as_bool().unwrap_or(false);
    let id = args["id"].as_str().context("id_required")?.to_string();
    let wire = match name {
        "worklink_task" => match args["op"].as_str() {
            Some("get") => Wire::Get {
                id,
                wait_ms: args["wait_ms"].as_u64().unwrap_or(0),
            },
            Some("cancel") => Wire::Cancel { id },
            _ => anyhow::bail!("invalid_op"),
        },
        "worklink_result" => {
            if let Some(path) = args["download_path"].as_str() {
                let path = std::path::Path::new(path);
                ensure!(path.is_absolute(), "absolute_path_required");
                return cli::download(store, device, background, &id, path).await;
            }
            Wire::Read {
                id,
                offset: args["offset"].as_u64().unwrap_or(0) as usize,
                limit: args["limit"].as_u64().unwrap_or(2000) as usize,
                query: args["query"].as_str().map(str::to_string),
            }
        }
        _ => anyhow::bail!("unknown_tool"),
    };
    session::call(store, device, background, wire).await
}
pub async fn run(store: &Store) -> Result<()> {
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    // 按字节限长读取，避免错误客户端在一行内无限灌入数据；stdout 只承载 MCP。
    loop {
        let mut bytes = Vec::new();
        loop {
            let available = input.fill_buf().await?;
            if available.is_empty() {
                break;
            }
            let end = available
                .iter()
                .position(|b| *b == b'\n')
                .map(|n| n + 1)
                .unwrap_or(available.len());
            ensure!(bytes.len() + end <= 256 * 1024, "mcp_input_too_large");
            bytes.extend_from_slice(&available[..end]);
            input.consume(end);
            if bytes.last() == Some(&b'\n') {
                break;
            }
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                output.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}\n").await?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let response = match request["method"].as_str() {
            Some("initialize") => {
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"WorkLink","version":env!("CARGO_PKG_VERSION")},"instructions":"Use compact snapshots first. Read long results by result_id and offset. Download screenshots explicitly. Keep task ids and background mode; never automatically repeat uncertain mutations."}})
            }
            Some("ping") => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            Some("tools/list") => json!({"jsonrpc":"2.0","id":id,"result":catalog()}),
            Some("tools/call") => {
                let result = call(
                    store,
                    request["params"]["name"].as_str().unwrap_or(""),
                    request["params"]
                        .get("arguments")
                        .cloned()
                        .unwrap_or(json!({})),
                )
                .await;
                let (value, error) = match result {
                    Ok(v) => {
                        let error = v.get("error").is_some();
                        (v, error)
                    }
                    Err(e) => (json!({"error":super::clip(&format!("{e:#}"),700)}), true),
                };
                json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":serde_json::to_string(&value)?}],"isError":error}})
            }
            _ => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
            }
        };
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        output.write_all(&bytes).await?;
        output.flush().await?;
    }
}
