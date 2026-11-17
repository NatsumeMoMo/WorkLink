use super::{Action, Budget, clip, session::ContextTask, store::MAX_RESULT_BYTES};
use crate::config::Store;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    process::{Child, Command},
    sync::Mutex,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[derive(Clone)]
struct Endpoint {
    url: String,
}
struct State {
    endpoint: Option<Endpoint>,
    child: Option<Child>,
}
pub struct Browser {
    store: Store,
    background: bool,
    state: Mutex<State>,
    tabs: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}
struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    id: u64,
    session: Option<String>,
    context: Option<i64>,
}
impl Cdp {
    async fn connect(endpoint: &Endpoint) -> Result<Self> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(8 * 1024 * 1024))
            .max_frame_size(Some(8 * 1024 * 1024));
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(&endpoint.url, Some(config), false)
                .await
                .context("browser_disconnected: 无法连接浏览器")?;
        Ok(Self {
            socket,
            id: 0,
            session: None,
            context: None,
        })
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        let id = self.id;
        let mut message = json!({"id":id,"method":method,"params":params});
        if let Some(session) = &self.session {
            message["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let frame = self
                    .socket
                    .next()
                    .await
                    .context("browser_disconnected: 浏览器连接已关闭")??;
                match frame {
                    Message::Text(text) => {
                        let reply: Value = serde_json::from_str(&text)?;
                        if reply["id"].as_u64() == Some(id) {
                            ensure!(
                                reply.get("error").is_none(),
                                "browser_protocol_error: {}",
                                clip(&reply["error"]["message"].to_string(), 400)
                            );
                            return Ok(reply["result"].clone());
                        }
                    }
                    Message::Ping(bytes) => self.socket.send(Message::Pong(bytes)).await?,
                    Message::Close(_) => bail!("browser_disconnected: 浏览器连接已关闭"),
                    _ => {}
                }
            }
        })
        .await
        .context("browser_unresponsive: 浏览器 15 秒内未响应")?
    }
    async fn page(endpoint: &Endpoint, tab: &str) -> Result<Self> {
        ensure!(
            !tab.is_empty()
                && tab.len() < 128
                && tab.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "invalid_tab"
        );
        let mut cdp = Self::connect(endpoint).await?;
        let info = cdp
            .request("Target.getTargetInfo", json!({"targetId":tab}))
            .await?;
        ensure!(
            info["targetInfo"]["type"] == "page",
            "invalid_tab: 仅支持普通页面"
        );
        let attached = cdp
            .request(
                "Target.attachToTarget",
                json!({"targetId":tab,"flatten":true}),
            )
            .await?;
        cdp.session = Some(
            attached["sessionId"]
                .as_str()
                .context("browser_protocol_error")?
                .into(),
        );
        cdp.request("Page.enable", json!({})).await?;
        cdp.reset_world().await?;
        Ok(cdp)
    }
    async fn reset_world(&mut self) -> Result<()> {
        let tree = self.request("Page.getFrameTree", json!({})).await?;
        // 隔离脚本环境，避免网页覆盖原生对象，并让短引用在多次工具调用之间保留。
        let world=self.request("Page.createIsolatedWorld",json!({"frameId":tree["frameTree"]["frame"]["id"],"worldName":"WorkLinkAutomation"})).await?;
        self.context = world["executionContextId"].as_i64();
        Ok(())
    }
    async fn evaluate(&mut self, expression: String) -> Result<Value> {
        let mut params = json!({"expression":expression,"returnByValue":true,"awaitPromise":true,"timeout":10000});
        if let Some(context) = self.context {
            params["contextId"] = json!(context);
        }
        let reply = self.request("Runtime.evaluate", params).await?;
        if let Some(error) = reply.get("exceptionDetails") {
            let message = error["exception"]["description"]
                .as_str()
                .or(error["text"].as_str())
                .unwrap_or("page_script_failed");
            bail!(
                "page_action_failed: {}",
                clip(message.lines().next().unwrap_or(message), 500)
            );
        }
        Ok(reply["result"]["value"].clone())
    }
    async fn page_op(&mut self, args: Value) -> Result<Value> {
        let safe = matches!(
            args["op"].as_str(),
            Some("snapshot" | "text" | "wait" | "ask_probe" | "ask_refs")
        );
        let expression = format!("({})({args})", include_str!("page.js"));
        // 导航会替换脚本上下文；只读探测可重新附加，有副作用的输入和点击不能重放。
        for attempt in 0..4 {
            match self.evaluate(expression.clone()).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let text = format!("{error:#}");
                    if !safe
                        || attempt == 3
                        || !(text.contains("Cannot find context")
                            || text.contains("context was destroyed")
                            || text.contains("Cannot find default execution context"))
                    {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    self.reset_world().await?;
                }
            }
        }
        unreachable!()
    }
    async fn click(&mut self, reference: &str) -> Result<()> {
        let p = self.page_op(json!({"op":"point","ref":reference})).await?;
        self.request(
            "Input.dispatchMouseEvent",
            json!({"type":"mouseMoved","x":p["x"],"y":p["y"]}),
        )
        .await?;
        for kind in ["mousePressed", "mouseReleased"] {
            self.request(
                "Input.dispatchMouseEvent",
                json!({"type":kind,"x":p["x"],"y":p["y"],"button":"left","clickCount":1}),
            )
            .await?;
        }
        Ok(())
    }
    async fn fill(&mut self, reference: &str, text: &str) -> Result<()> {
        ensure!(text.len() <= 32768, "input_too_large: text 最多 32 KiB");
        let focused = self
            .page_op(json!({"op":"focus","ref":reference,"editable":true}))
            .await?;
        ensure!(focused["focused"] == true, "focus_failed");
        self.request("Input.dispatchKeyEvent",json!({"type":"rawKeyDown","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65})).await?;
        self.request("Input.dispatchKeyEvent",json!({"type":"keyUp","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65})).await?;
        self.request("Input.insertText", json!({"text":text}))
            .await?;
        Ok(())
    }
}

impl Browser {
    pub fn new(store: Store, background: bool) -> Self {
        Self {
            store,
            background,
            state: Mutex::new(State {
                endpoint: None,
                child: None,
            }),
            tabs: Mutex::new(HashMap::new()),
        }
    }
    fn profile(&self) -> PathBuf {
        self.store.root.join(if self.background {
            "automation/profiles/background"
        } else {
            "automation/profiles/interactive"
        })
    }
    fn executable() -> Result<PathBuf> {
        let mut paths = vec![];
        if let Some(custom) = std::env::var_os("WORKLINK_BROWSER") {
            paths.push(custom.into());
        }
        for key in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(key) {
                for path in [
                    "Microsoft/Edge/Application/msedge.exe",
                    "Google/Chrome/Application/chrome.exe",
                ] {
                    paths.push(PathBuf::from(&base).join(path));
                }
            }
        }
        paths
            .into_iter()
            .find(|p| p.is_absolute() && p.is_file())
            .context("browser_not_installed: 需要 Microsoft Edge 或 Chrome")
    }
    async fn existing(&self) -> Option<Endpoint> {
        let text = tokio::fs::read_to_string(self.profile().join("DevToolsActivePort"))
            .await
            .ok()?;
        let mut lines = text.lines();
        let port: u16 = lines.next()?.parse().ok()?;
        let path = lines.next()?;
        if !path.starts_with("/devtools/browser/")
            || !path
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/-".contains(&c))
        {
            return None;
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?;
        let reply = client
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .await
            .ok()?;
        if reply.content_length().is_some_and(|n| n > 16384) {
            return None;
        }
        let mut reply = reply;
        let mut bytes = Vec::new();
        while let Some(chunk) = reply.chunk().await.ok()? {
            if bytes.len() + chunk.len() > 16384 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        let version: Value = serde_json::from_slice(&bytes).ok()?;
        let ws = version["webSocketDebuggerUrl"].as_str()?;
        let expected = format!("ws://127.0.0.1:{port}{path}");
        if ws != expected {
            return None;
        }
        Some(Endpoint { url: expected })
    }
    async fn endpoint(&self, launch: bool) -> Result<Option<Endpoint>> {
        let mut state = self.state.lock().await;
        if let Some(endpoint) = self.existing().await {
            state.endpoint = Some(endpoint.clone());
            return Ok(Some(endpoint));
        }
        state.endpoint = None;
        if !launch {
            return Ok(None);
        }
        std::fs::create_dir_all(self.profile())?;
        let mut command = Command::new(Self::executable()?);
        command.arg(format!("--user-data-dir={}", self.profile().display()));
        command.args([
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--window-size=1280,900",
        ]);
        if self.background {
            command.arg("--headless=new");
        }
        command
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x08000000);
        state.child = Some(command.spawn().context("browser_start_failed")?);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            if let Some(endpoint) = self.existing().await {
                state.endpoint = Some(endpoint.clone());
                return Ok(Some(endpoint));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        bail!("browser_start_failed: 15 秒内未获得调试入口；请检查浏览器策略或专用配置是否被占用")
    }
    pub async fn execute(
        &self,
        action: &Action,
        budget: &Budget,
        ctx: &ContextTask,
    ) -> Result<Value> {
        if matches!(action, Action::BrowserStatus) {
            return Ok(
                json!({"running":self.endpoint(false).await?.is_some(),"background":self.background,"helper_ready":true,"profile":"dedicated_persistent","screenshots":"on_demand"}),
            );
        }
        if let Action::BrowserOpen { url } = action {
            let parsed = reqwest::Url::parse(url).context("invalid_url")?;
            ensure!(
                matches!(parsed.scheme(), "http" | "https")
                    && parsed.username().is_empty()
                    && parsed.password().is_none(),
                "invalid_url: 仅允许无内嵌凭据的 http/https URL"
            );
            let endpoint = self.endpoint(true).await?.unwrap();
            let mut cdp = Cdp::connect(&endpoint).await?;
            let targets = cdp.request("Target.getTargets", json!({})).await?;
            let pages: Vec<&Value> = targets["targetInfos"]
                .as_array()
                .context("browser_protocol_error")?
                .iter()
                .filter(|v| v["type"] == "page")
                .collect();
            ensure!(
                pages.len() < 8,
                "browser_tab_limit: 最多保留 8 个页面，请先关闭不用的页面"
            );
            let target = cdp
                .request("Target.createTarget", json!({"url":url}))
                .await?;
            let tab = target["targetId"]
                .as_str()
                .context("browser_protocol_error")?
                .to_string();
            // 建页立即返回 ID，慢网页的就绪条件交给 wait，避免加载时间触发重复建页。
            return Ok(json!({"tab":tab,"url":clip(url,512),"loading":true}));
        }
        let endpoint = self.endpoint(false).await?;
        if matches!(action, Action::BrowserTabs) {
            let Some(endpoint) = endpoint else {
                return Ok(json!({"running":false,"tabs":[]}));
            };
            let mut cdp = Cdp::connect(&endpoint).await?;
            let reply = cdp.request("Target.getTargets", json!({})).await?;
            let mut tabs = vec![];
            let mut used = 100;
            for page in reply["targetInfos"]
                .as_array()
                .context("browser_protocol_error")?
                .iter()
                .filter(|p| p["type"] == "page")
            {
                let item = json!({"tab":page["targetId"],"title":clip(page["title"].as_str().unwrap_or(""),100),"url":clip(page["url"].as_str().unwrap_or(""),400)});
                if used + item.to_string().chars().count() > budget.max_chars
                    || tabs.len() >= budget.max_items
                {
                    break;
                }
                used += item.to_string().chars().count();
                tabs.push(item);
            }
            return Ok(json!({"running":true,"tabs":tabs}));
        }
        if let Action::BrowserClose { tab } = action {
            if let Some(endpoint) = endpoint {
                let mut cdp = Cdp::connect(&endpoint).await?;
                if let Some(tab) = tab {
                    cdp.request("Target.closeTarget", json!({"targetId":tab}))
                        .await?;
                } else {
                    let _ = cdp.request("Browser.close", json!({})).await;
                }
            }
            return Ok(json!({"closed":true}));
        }
        let endpoint = endpoint.context("browser_not_running: 请先 browser open")?;
        let tab = match action {
            Action::BrowserSnapshot { tab, .. }
            | Action::BrowserClick { tab, .. }
            | Action::BrowserFill { tab, .. }
            | Action::BrowserPress { tab, .. }
            | Action::BrowserText { tab, .. }
            | Action::BrowserWait { tab, .. }
            | Action::BrowserScreenshot { tab, .. }
            | Action::BrowserAsk { tab, .. } => tab,
            _ => bail!("unsupported_browser_action"),
        };
        let lock = self
            .tabs
            .lock()
            .await
            .entry(tab.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock
            .try_lock()
            .context("browser_tab_busy: 该页已有操作进行中，请查询原任务")?;
        let mut cdp = Cdp::page(&endpoint, tab).await?;
        match action{
            Action::BrowserSnapshot{offset,..}=>cdp.page_op(json!({"op":"snapshot","tab":tab,"id":uuid::Uuid::new_v4().simple().to_string()[..8],"offset":offset,"items":budget.max_items,"chars":budget.max_chars})).await,
            Action::BrowserClick{reference,..}=>{cdp.click(reference).await?;Ok(json!({"clicked":true,"tab":tab}))},
            Action::BrowserFill{reference,text,..}=>{cdp.fill(reference,text).await?;Ok(json!({"filled":true,"tab":tab}))},
            Action::BrowserPress{reference,key,..}=>{
                cdp.page_op(json!({"op":"focus","ref":reference})).await?;
                let (key,code,vk,modifiers)=match key.as_str(){"Enter"=>("Enter","Enter",13,0),"Ctrl+Enter"=>("Enter","Enter",13,2),"Tab"=>("Tab","Tab",9,0),"Escape"=>("Escape","Escape",27,0),"ArrowDown"=>("ArrowDown","ArrowDown",40,0),"ArrowUp"=>("ArrowUp","ArrowUp",38,0),_=>bail!("unsupported_key: 支持 Enter/Ctrl+Enter/Tab/Escape/ArrowDown/ArrowUp")};
                for kind in ["rawKeyDown","keyUp"]{cdp.request("Input.dispatchKeyEvent",json!({"type":kind,"key":key,"code":code,"windowsVirtualKeyCode":vk,"modifiers":modifiers})).await?;}
                Ok(json!({"pressed":true,"tab":tab}))
            }
            Action::BrowserText{selector,..}=>{
                let reply=cdp.page_op(json!({"op":"text","selector":selector})).await?;
                let mut result=ctx.results().save_text(&ctx.peer,"browser",reply["text"].as_str().unwrap_or(""),budget)?;
                if reply["source_truncated"]==true{result["source_truncated"]=json!(true);}Ok(result)
            }
            Action::BrowserWait{selector,text,..}=>{
                ensure!(selector.is_some()||text.is_some(),"invalid_wait: 需要 selector 或 text");
                loop { if cdp.page_op(json!({"op":"wait","selector":selector,"text":text})).await?["ready"]==true{return Ok(json!({"ready":true,"tab":tab}));}tokio::time::sleep(Duration::from_millis(500)).await; }
            }
            Action::BrowserScreenshot{..}=>{
                let reply=cdp.request("Page.captureScreenshot",json!({"format":"jpeg","quality":65,"captureBeyondViewport":false,"fromSurface":true})).await?;
                let bytes=STANDARD.decode(reply["data"].as_str().context("browser_protocol_error")?)?;
                let image=image::load_from_memory(&bytes)?.thumbnail(1280,900);
                let mut jpeg=vec![];image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg,65).encode_image(&image)?;
                ensure!(jpeg.len()<=MAX_RESULT_BYTES,"screenshot_too_large");
                let mut result=ctx.results().save_bytes(&ctx.peer,"browser","image/jpeg",&jpeg)?;
                result["width"]=json!(image.width());result["height"]=json!(image.height());Ok(result)
            }
            Action::BrowserAsk{prompt,input,send,answer,..}=>{
                ensure!(!prompt.trim().is_empty()&&prompt.len()<=32768,"invalid_prompt: prompt 应为 1–32768 字节");
                let args=|op:&str|json!({"op":op,"input":input,"send":send,"answer":answer,"id":ctx.id});
                let before=cdp.page_op(args("ask_probe")).await?;
                ensure!(before["input_ready"]==true,"login_or_input_required: 请在目标浏览器登录，或指定 input 选择器");
                ensure!(before["draft"].as_str().unwrap_or("").is_empty(),"draft_not_empty: 保留了原有草稿，请先处理草稿");
                ensure!(before["generating"]!=true,"conversation_busy: 上一条回答仍在生成");
                let refs=cdp.page_op(args("ask_refs")).await?;
                cdp.fill(refs["input"].as_str().unwrap(),prompt).await?;
                let deadline=tokio::time::Instant::now()+Duration::from_secs(10);
                loop{
                    if cdp.page_op(args("ask_probe")).await?["send_ready"]==true{break;}
                    ensure!(tokio::time::Instant::now()<deadline,"send_not_ready: 请指定 send 选择器或检查登录状态");tokio::time::sleep(Duration::from_millis(200)).await;
                }
                let refs=cdp.page_op(args("ask_refs")).await?;
                // 在发送前保存阶段；崩溃发生在点击边界时，也不会在重连后自动再次发送。
                ctx.phase("submission_started").await?;
                cdp.click(refs["send"].as_str().unwrap()).await?;
                ctx.phase("waiting_for_answer").await?;
                let baseline=before["count"].as_u64().unwrap_or(0);let mut previous=String::new();let mut stable=0;
                loop{
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let probe=cdp.page_op(args("ask_probe")).await?;let text=probe["text"].as_str().unwrap_or("");
                    if probe["count"].as_u64().unwrap_or(0)>baseline&&!text.trim().is_empty()&&probe["generating"]!=true{
                        if previous==text{stable+=1;}else{stable=0;previous=text.into();}
                        if stable>=5{let mut result=ctx.results().save_text(&ctx.peer,"browser",text,budget)?;result["tab"]=json!(tab);return Ok(result);}
                    }else{stable=0;}
                }
            }
            _=>bail!("unsupported_browser_action"),
        }
    }
}
