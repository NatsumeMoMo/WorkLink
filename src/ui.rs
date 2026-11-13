use crate::{
    client,
    config::{Invitation, Store},
    protocol::LocalRequest,
};
use anyhow::{Context, Result};
use eframe::egui::{self, Color32, RichText, Vec2};
use std::{
    fs::OpenOptions,
    os::windows::process::CommandExt,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const BG: Color32 = Color32::from_rgb(14, 19, 29);
const CARD: Color32 = Color32::from_rgb(23, 31, 46);
const BLUE: Color32 = Color32::from_rgb(102, 161, 255);
const GREEN: Color32 = Color32::from_rgb(76, 209, 160);
const MUTED: Color32 = Color32::from_rgb(151, 165, 185);
const TITLE: &str = "WorkLink · 远程工作台";

pub fn launch(store: &Store, args: &[&str]) -> Result<()> {
    std::process::Command::new(std::env::current_exe()?)
        .arg("--data-dir")
        .arg(&store.root)
        .args(args)
        .creation_flags(0x08000000)
        .spawn()?;
    Ok(())
}

pub fn run(store: Store) -> Result<()> {
    let tracing_capture = std::env::var_os("WORKLINK_SCREENSHOT").is_some();
    if tracing_capture {
        eprintln!("GUI entry");
    }
    std::fs::create_dir_all(&store.root)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(store.root.join("gui.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        unsafe {
            let window = FindWindowW(std::ptr::null(), crate::platform::wide(TITLE).as_ptr());
            if !window.is_null() {
                ShowWindow(window, SW_RESTORE);
                SetForegroundWindow(window);
            }
        }
        return Ok(());
    }
    let _ = launch(&store, &["tray"]);
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1140.0, 780.0])
            .with_min_inner_size([900.0, 620.0])
            .with_icon(egui::IconData {
                rgba: icon_pixels(64),
                width: 64,
                height: 64,
            }),
        ..Default::default()
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            if tracing_capture {
                eprintln!("GUI context created");
            }
            configure(&cc.egui_ctx);
            if tracing_capture {
                eprintln!("GUI fonts configured");
            }
            Ok(Box::new(App::new(store, cc.egui_ctx.clone())?))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI 启动失败：{e}"))?;
    drop(lock);
    Ok(())
}

fn configure(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut fonts = egui::FontDefinitions::default();
    if let Some(bytes) = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyhl.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
    ]
    .into_iter()
    .find_map(|p| std::fs::read(p).ok())
    {
        fonts
            .font_data
            .insert("Chinese".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "Chinese".into());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("Chinese".into());
    }
    ctx.set_fonts(fonts);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = Color32::from_rgb(9, 14, 23);
    visuals.selection.bg_fill = Color32::from_rgb(43, 78, 124);
    visuals.widgets.inactive.bg_fill = CARD;
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(38, 53, 76);
    ctx.set_visuals(visuals);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(12.0, 12.0);
        s.spacing.button_padding = Vec2::new(14.0, 8.0);
        s.text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        s.text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
    });
}

enum Event {
    Status(Result<serde_json::Value, String>),
    Result(Result<serde_json::Value, String>),
    Files(Result<serde_json::Value, String>),
}
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Devices,
    Files,
    Tasks,
    Pair,
    Settings,
}

struct App {
    store: Store,
    rt: Arc<tokio::runtime::Runtime>,
    ctx: egui::Context,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    tab: Tab,
    status: Option<serde_json::Value>,
    next_status: Instant,
    status_pending: bool,
    busy: bool,
    message: String,
    output: String,
    initialized: bool,
    name: String,
    listen: String,
    outbound: bool,
    peer: usize,
    user: String,
    native: bool,
    system: bool,
    invite: String,
    fingerprint: String,
    endpoint: String,
    endpoint_drafts: std::collections::HashMap<String, String>,
    ssh_drafts: std::collections::HashMap<String, (String, String, String)>,
    command: String,
    remote_dir: String,
    remote_file: String,
    local_file: String,
    entries: Vec<serde_json::Value>,
    overwrite: bool,
    job_id: String,
    screenshot: Option<std::path::PathBuf>,
    screenshot_frames: u32,
}

impl App {
    fn new(store: Store, ctx: egui::Context) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let cfg = store.read().ok();
        Ok(Self {
            store,
            rt: Arc::new(crate::runtime()?),
            ctx,
            tx,
            rx,
            tab: Tab::Devices,
            status: None,
            next_status: Instant::now(),
            status_pending: false,
            busy: false,
            message: String::new(),
            output: String::new(),
            initialized: cfg.is_some(),
            name: cfg.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| {
                std::env::var("COMPUTERNAME").unwrap_or_else(|_| "My PC".into())
            }),
            listen: cfg
                .as_ref()
                .and_then(|c| c.listen)
                .map(|a| a.to_string())
                .unwrap_or_else(|| "127.0.0.1:41443".into()),
            outbound: cfg.as_ref().is_some_and(|c| c.listen.is_none()),
            peer: 0,
            user: String::new(),
            native: false,
            system: false,
            invite: String::new(),
            fingerprint: String::new(),
            endpoint: String::new(),
            endpoint_drafts: Default::default(),
            ssh_drafts: Default::default(),
            command: "Get-ComputerInfo | Select-Object CsName, WindowsProductName".into(),
            remote_dir: "C:\\".into(),
            remote_file: String::new(),
            local_file: String::new(),
            entries: vec![],
            overwrite: false,
            job_id: String::new(),
            screenshot: std::env::var_os("WORKLINK_SCREENSHOT").map(Into::into),
            screenshot_frames: 0,
        })
    }
    fn message(&mut self, result: Result<()>) {
        self.message = match result {
            Ok(()) => "操作完成".into(),
            Err(e) => format!("{e:#}"),
        };
        self.next_status = Instant::now();
    }
    fn poll(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Status(result) => {
                    self.status_pending = false;
                    self.status = result.ok();
                }
                Event::Result(result) => {
                    self.busy = false;
                    match result {
                        Ok(value) => {
                            self.message = if value["exit_code"].as_i64().is_some_and(|c| c != 0) {
                                format!("执行结束，退出码 {}", value["exit_code"])
                            } else if let Some(state) = value["state"].as_str() {
                                format!("任务状态：{state}")
                            } else {
                                "操作完成".into()
                            };
                            self.output = serde_json::to_string_pretty(&value).unwrap_or_default();
                            if let Some(id) = value["spec"]["id"].as_str() {
                                self.job_id = id.into();
                            }
                        }
                        Err(error) => self.message = error,
                    }
                }
                Event::Files(result) => {
                    self.busy = false;
                    match result {
                        Ok(value) => {
                            self.entries = value["entries"].as_array().cloned().unwrap_or_default();
                            self.message = if value["truncated"] == true {
                                "仅显示前 200 项；可缩小目录范围或使用 SFTP 客户端".into()
                            } else {
                                String::new()
                            };
                        }
                        Err(error) => self.message = error,
                    }
                }
            }
        }
        if self.initialized && !self.status_pending && Instant::now() >= self.next_status {
            self.status_pending = true;
            self.next_status = Instant::now() + Duration::from_secs(3);
            let store = self.store.clone();
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.rt.spawn(async move {
                let result = client::local(&store, LocalRequest::Status)
                    .await
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Event::Status(result));
                ctx.request_repaint();
            });
        }
    }
    fn task(
        &mut self,
        future: impl std::future::Future<Output = Result<serde_json::Value>> + Send + 'static,
    ) {
        self.busy = true;
        self.message = "正在处理…".into();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let result = future.await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Result(result));
            ctx.request_repaint();
        });
    }
    fn cli_task(&mut self, args: Vec<String>) {
        let store = self.store.clone();
        self.task(async move {
            let mut cmd = tokio::process::Command::new(std::env::current_exe()?);
            cmd.arg("--data-dir")
                .arg(store.root)
                .arg("--json")
                .args(args)
                .creation_flags(0x08000000);
            // 子进程独立完成任务，窗口关闭不会中断传输或远端操作。
            let result = client::capture(cmd, None, 86400).await?;
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&result.stdout) {
                if result.exit_code != Some(0) && value["ok"] == false {
                    anyhow::bail!("{}", value["error"].as_str().unwrap_or("操作失败"));
                }
                return Ok(value);
            }
            anyhow::bail!(
                "{}",
                if result.stderr.is_empty() {
                    result.stdout
                } else {
                    result.stderr
                }
            )
        });
    }
    fn selected(&self) -> Option<crate::config::Peer> {
        self.store.read().ok()?.peers.get(self.peer).cloned()
    }
    fn selector(&mut self, ui: &mut egui::Ui) {
        let peers = self.store.read().map(|c| c.peers).unwrap_or_default();
        if self.peer >= peers.len() {
            self.peer = 0;
        }
        ui.horizontal(|ui| {
            ui.label("目标设备");
            egui::ComboBox::from_id_salt("target")
                .selected_text(peers.get(self.peer).map_or("请选择", |p| p.name.as_str()))
                .width(180.0)
                .show_ui(ui, |ui| {
                    for (i, p) in peers.iter().enumerate() {
                        ui.selectable_value(&mut self.peer, i, &p.name);
                    }
                });
            ui.label("SSH 用户");
            ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(160.0));
        });
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.native, false, "SSH / SFTP · 默认");
            ui.selectable_value(&mut self.native, true, "原生兜底");
            if self.native {
                ui.checkbox(&mut self.system, "以 SYSTEM 执行");
            } else {
                self.system = false;
            }
        });
    }
    fn init(&mut self, ui: &mut egui::Ui) {
        ui.add_space(32.0);
        ui.heading("连接你的两台电脑");
        ui.label(RichText::new("先为这台电脑创建身份，再交换邀请完成配对。").color(MUTED));
        ui.add_space(18.0);
        field(ui, "设备名称", &mut self.name);
        field(ui, "本机监听地址", &mut self.listen);
        ui.checkbox(&mut self.outbound, "这台电脑只主动连接，不监听入站连接");
        ui.label(
            RichText::new("使用 frp 的电脑保留监听；NAT4 电脑可以选择只主动连接。").color(MUTED),
        );
        if primary(ui, "创建设备身份").clicked() {
            let result = (|| {
                let address = if self.outbound {
                    None
                } else {
                    Some(self.listen.parse().context("监听地址无效")?)
                };
                self.store.init(self.name.clone(), address)?;
                self.initialized = true;
                self.tab = Tab::Pair;
                Ok(())
            })();
            self.message(result);
        }
    }
    fn devices(&mut self, ui: &mut egui::Ui) {
        title(
            ui,
            "设备",
            "一条连接，两个方向。SSH 客户端与 Agent 使用同一条加密隧道。",
        );
        let online = self
            .status
            .as_ref()
            .and_then(|v| v["peers"].as_array())
            .map_or(0, |p| p.iter().filter(|p| p["online"] == true).count());
        ui.horizontal(|ui| {
            metric(ui, "在线设备", &online.to_string());
            metric(
                ui,
                "后台",
                if self.status.is_some() {
                    "运行中"
                } else {
                    "未连接"
                },
            );
            metric(ui, "通道保护", "TLS 1.3 · 双向认证");
        });
        ui.add_space(12.0);
        if self.status.is_none() {
            ui.horizontal(|ui| {
                if primary(ui, "启动用户后台").clicked() {
                    self.message(launch(&self.store, &["daemon"]));
                }
                if ui.button("安装后台服务…").clicked() {
                    self.message(elevate(&self.store, &["service", "install"]));
                }
            });
            ui.label(
                RichText::new("无人登录时连接，需要在设置中安装 Windows 后台服务。").color(MUTED),
            );
        }
        let peers = self.store.read().map(|c| c.peers).unwrap_or_default();
        if peers.is_empty() {
            ui.add_space(24.0);
            card().show(ui, |ui| {
                ui.set_min_width(ui.available_width() - 4.0);
                ui.set_min_height(210.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(24.0);
                    ui.label(RichText::new("连接你的另一台电脑").size(23.0).strong());
                    ui.label(
                        RichText::new("交换设备邀请，核对指纹，然后由 B 主动连接 A 的公网入口。")
                            .color(MUTED),
                    );
                    ui.add_space(10.0);
                    if primary(ui, "添加第一台设备").clicked() {
                        self.tab = Tab::Pair;
                    }
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("SSH 终端  ·  双向文件传输  ·  Agent 命令接口")
                            .small()
                            .color(MUTED),
                    );
                });
            });
        }
        for (i, peer) in peers.iter().enumerate() {
            let status = self
                .status
                .as_ref()
                .and_then(|v| v["peers"].as_array())
                .and_then(|p| p.iter().find(|p| p["id"] == peer.id))
                .cloned();
            let online = status.as_ref().is_some_and(|s| s["online"] == true);
            card().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if online { "●" } else { "○" }).color(if online {
                            GREEN
                        } else {
                            MUTED
                        }),
                    );
                    ui.label(RichText::new(&peer.name).size(20.0).strong());
                    ui.label(
                        RichText::new(if online { "已连接" } else { "等待连接" })
                            .color(if online { GREEN } else { MUTED }),
                    );
                });
                ui.label(
                    RichText::new(
                        peer.endpoint
                            .as_deref()
                            .unwrap_or("通过对方主动建立的连接访问"),
                    )
                    .color(MUTED),
                );
                ui.horizontal(|ui| {
                    if primary(ui, "打开 SSH 终端").clicked() {
                        self.peer = i;
                        self.open_terminal(false);
                    }
                    if ui.button("文件管理").clicked() {
                        self.peer = i;
                        self.tab = Tab::Files;
                    }
                    if ui.button("运行命令").clicked() {
                        self.peer = i;
                        self.tab = Tab::Tasks;
                    }
                    if ui.button("复制 SSH 配置").clicked() {
                        match client::ssh_config(
                            &self.store,
                            peer,
                            if self.user.is_empty() {
                                None
                            } else {
                                Some(&self.user)
                            },
                        ) {
                            Ok(text) => self.ctx.copy_text(text),
                            Err(e) => self.message = error_text(e),
                        }
                    }
                });
                if let Some(error) = status.as_ref().and_then(|s| s["last_error"].as_str()) {
                    ui.label(RichText::new(error).small().color(MUTED));
                }
                egui::CollapsingHeader::new("连接入口与访问授权")
                    .id_salt(&peer.id)
                    .show(ui, |ui| {
                        ui.label("以下权限决定这台设备可以访问本机的哪些能力。");
                        let mut updated = peer.clone();
                        let endpoint = self
                            .endpoint_drafts
                            .entry(peer.id.clone())
                            .or_insert_with(|| updated.endpoint.clone().unwrap_or_default());
                        ui.horizontal(|ui| {
                            ui.label("对方公网入口");
                            ui.text_edit_singleline(endpoint);
                            if ui.button("保存入口").clicked() {
                                updated.endpoint = if endpoint.trim().is_empty() {
                                    None
                                } else {
                                    Some(endpoint.trim().into())
                                };
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.checkbox(&mut updated.allow.ssh, "SSH 转发");
                            ui.checkbox(&mut updated.allow.files, "原生文件");
                            ui.checkbox(&mut updated.allow.exec, "原生命令/终端");
                            ui.checkbox(&mut updated.allow.system, "SYSTEM");
                        });
                        if updated.endpoint != peer.endpoint
                            || serde_json::to_string(&updated.allow).ok()
                                != serde_json::to_string(&peer.allow).ok()
                        {
                            let id = peer.id.clone();
                            let result = self.store.update(|cfg| {
                                if let Some(ep) = &updated.endpoint {
                                    crate::config::validate_endpoint(ep)?;
                                }
                                *cfg.peers.iter_mut().find(|p| p.id == id).unwrap() = updated;
                                Ok(())
                            });
                            self.message(result);
                        }
                        ui.separator();
                        ui.label("本机连接对方时使用的 SSH 设置");
                        let draft = self.ssh_drafts.entry(peer.id.clone()).or_insert_with(|| {
                            (
                                peer.ssh_client.user.clone().unwrap_or_default(),
                                peer.ssh_client.identity_file.clone().unwrap_or_default(),
                                peer.ssh_client.known_hosts.clone().unwrap_or_default(),
                            )
                        });
                        field(ui, "SSH 用户名", &mut draft.0);
                        field(ui, "本机私钥路径（可空）", &mut draft.1);
                        field(ui, "known_hosts（可空）", &mut draft.2);
                        if ui.button("保存 SSH 设置").clicked() {
                            let option = |s: &str| {
                                if s.trim().is_empty() {
                                    None
                                } else {
                                    Some(s.trim().to_owned())
                                }
                            };
                            let client = crate::config::SshClient {
                                user: option(&draft.0),
                                identity_file: option(&draft.1),
                                known_hosts: option(&draft.2),
                            };
                            let id = peer.id.clone();
                            let result = self.store.update(|cfg| {
                                cfg.peers
                                    .iter_mut()
                                    .find(|p| p.id == id)
                                    .context("设备已经移除")?
                                    .ssh_client = client;
                                Ok(())
                            });
                            self.message(result);
                        }
                        ui.label(
                            RichText::new(format!("设备指纹：{}", peer.fingerprint))
                                .small()
                                .color(MUTED),
                        );
                        if ui
                            .button(RichText::new("撤销设备信任").color(Color32::LIGHT_RED))
                            .clicked()
                        {
                            let id = peer.id.clone();
                            let result = self.store.update(|cfg| {
                                cfg.peers.retain(|p| p.id != id);
                                Ok(())
                            });
                            self.message(result);
                        }
                    });
            });
            ui.add_space(8.0);
        }
    }
    fn open_terminal(&mut self, native: bool) {
        if let Some(peer) = self.selected() {
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.arg("--data-dir")
                .arg(&self.store.root)
                .args(["terminal", &peer.id]);
            if !self.user.is_empty() {
                cmd.args(["--user", &self.user]);
            }
            if native {
                cmd.args(["--backend", "native"]);
                if self.system {
                    cmd.arg("--system");
                }
            }
            let result = cmd
                .creation_flags(0x00000010)
                .spawn()
                .map(|_| ())
                .map_err(Into::into);
            self.message(result);
        }
    }
    fn files(&mut self, ui: &mut egui::Ui) {
        title(
            ui,
            "文件",
            "浏览远程目录，上传或下载文件；完成前校验完整 SHA-256。",
        );
        self.selector(ui);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("远程目录");
            ui.add(egui::TextEdit::singleline(&mut self.remote_dir).desired_width(480.0));
            if ui
                .add_enabled(!self.busy, egui::Button::new("刷新"))
                .clicked()
            {
                self.refresh_files();
            }
            if ui.button("上一级").clicked() {
                if let Some(parent) = std::path::Path::new(&self.remote_dir).parent() {
                    self.remote_dir = parent.to_string_lossy().into_owned();
                }
                self.refresh_files();
            }
        });
        card().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("files")
                .max_height(220.0)
                .show(ui, |ui| {
                    if self.entries.is_empty() {
                        ui.label(
                            RichText::new(
                                "点击刷新读取目录。双击文件夹进入，点击文件选择下载路径。",
                            )
                            .color(MUTED),
                        );
                    }
                    for entry in self.entries.clone() {
                        let directory = entry["directory"] == true;
                        ui.horizontal(|ui| {
                            let response = ui.selectable_label(
                                self.remote_file == entry["path"].as_str().unwrap_or_default(),
                                format!(
                                    "{}  {}",
                                    if directory { "▸" } else { "·" },
                                    entry["name"].as_str().unwrap_or_default()
                                ),
                            );
                            if response.clicked() {
                                self.remote_file =
                                    entry["path"].as_str().unwrap_or_default().into();
                            }
                            if response.double_clicked() && directory {
                                self.remote_dir = entry["path"].as_str().unwrap_or_default().into();
                                self.refresh_files();
                            }
                            if !directory {
                                ui.label(
                                    RichText::new(format_bytes(
                                        entry["size"].as_u64().unwrap_or(0),
                                    ))
                                    .small()
                                    .color(MUTED),
                                );
                            }
                        });
                    }
                });
        });
        ui.add_space(8.0);
        field(ui, "本机文件", &mut self.local_file);
        ui.horizontal(|ui| {
            if ui.button("选择上传文件…").clicked()
                && let Some(path) = pick_file(false)
            {
                self.local_file = path.to_string_lossy().into_owned();
                if let Some(name) = path.file_name() {
                    self.remote_file = std::path::Path::new(&self.remote_dir)
                        .join(name)
                        .to_string_lossy()
                        .into_owned();
                }
            }
            if ui.button("选择下载保存位置…").clicked()
                && let Some(path) = pick_file(true)
            {
                self.local_file = path.to_string_lossy().into_owned();
            }
        });
        field(ui, "远程文件", &mut self.remote_file);
        ui.checkbox(&mut self.overwrite, "允许覆盖已有目标文件");
        ui.add_enabled_ui(!self.busy, |ui| {
            ui.horizontal(|ui| {
                if primary(ui, "上传 →").clicked() {
                    self.file_action(true);
                }
                if ui.button("← 下载").clicked() {
                    self.file_action(false);
                }
                ui.label(RichText::new("临时文件支持重新执行续传").color(MUTED));
            });
        });
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在传输或校验，关闭窗口后任务仍继续运行。");
            });
        }
    }
    fn refresh_files(&mut self) {
        if self.busy {
            return;
        }
        if let Some(peer) = self.selected() {
            self.busy = true;
            let store = self.store.clone();
            let name = peer.id;
            let user = self.user.clone();
            let path = self.remote_dir.clone();
            let native = self.native;
            let system = self.system;
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.rt.spawn(async move {
                let result = client::list_dir(
                    &store,
                    &name,
                    if user.is_empty() { None } else { Some(&user) },
                    &path,
                    native,
                    system,
                )
                .await
                .map_err(error_text);
                let _ = tx.send(Event::Files(result));
                ctx.request_repaint();
            });
        }
    }
    fn file_action(&mut self, put: bool) {
        if let Some(peer) = self.selected() {
            let mut args = vec![if put { "put" } else { "get" }.into(), peer.id];
            if put {
                args.extend([self.local_file.clone(), self.remote_file.clone()]);
            } else {
                args.extend([self.remote_file.clone(), self.local_file.clone()]);
            }
            args.extend([
                "--backend".into(),
                if self.native { "native" } else { "ssh" }.into(),
            ]);
            if !self.user.is_empty() {
                args.extend(["--user".into(), self.user.clone()]);
            }
            if self.overwrite {
                args.push("--overwrite".into());
            }
            if self.system {
                args.push("--system".into());
            }
            self.cli_task(args);
        }
    }
    fn tasks(&mut self, ui: &mut egui::Ui) {
        title(
            ui,
            "终端与任务",
            "默认通过 SSH 执行；原生兜底提供可查询的持久任务。",
        );
        self.selector(ui);
        ui.label("PowerShell 命令");
        ui.add(
            egui::TextEdit::multiline(&mut self.command)
                .font(egui::TextStyle::Monospace)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        ui.add_enabled_ui(!self.busy, |ui| {
            ui.horizontal(|ui| {
                if primary(ui, "执行命令").clicked()
                    && let Some(peer) = self.selected()
                {
                    let mut args = vec![
                        "exec".into(),
                        peer.id,
                        "--command".into(),
                        self.command.clone(),
                        "--backend".into(),
                        if self.native { "native" } else { "ssh" }.into(),
                    ];
                    if !self.user.is_empty() {
                        args.extend(["--user".into(), self.user.clone()]);
                    }
                    if self.system {
                        args.push("--system".into());
                    }
                    self.cli_task(args);
                }
                if ui.button("打开交互终端").clicked() {
                    self.open_terminal(self.native);
                }
                if self.busy {
                    ui.spinner();
                }
            });
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("原生任务 ID");
            ui.add(egui::TextEdit::singleline(&mut self.job_id).desired_width(330.0));
            if ui
                .add_enabled(!self.busy, egui::Button::new("查询"))
                .clicked()
                && let Some(peer) = self.selected()
            {
                self.cli_task(vec![
                    "job".into(),
                    "get".into(),
                    peer.id,
                    self.job_id.clone(),
                ]);
            }
            if ui
                .add_enabled(!self.busy, egui::Button::new("取消任务"))
                .clicked()
                && let Some(peer) = self.selected()
            {
                self.cli_task(vec![
                    "job".into(),
                    "cancel".into(),
                    peer.id,
                    self.job_id.clone(),
                ]);
            }
        });
        ui.horizontal(|ui| {
            ui.label("执行结果");
            if ui.small_button("复制").clicked() {
                self.ctx.copy_text(self.output.clone());
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("output")
            .max_height(300.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.output)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(12),
                );
            });
    }
    fn pair(&mut self, ui: &mut egui::Ui) {
        title(
            ui,
            "配对设备",
            "两端分别导出邀请、核对指纹并添加对方。邀请中只有公开证书。",
        );
        card().show(ui, |ui| {
            ui.label(RichText::new("这台电脑的邀请").strong());
            if let Ok(invite) = self.store.invitation() {
                ui.label(&invite.name);
                ui.label(
                    RichText::new(format!("SHA-256  {}", invite.fingerprint))
                        .monospace()
                        .small(),
                );
                ui.horizontal(|ui| {
                    if primary(ui, "复制本机邀请").clicked()
                        && let Ok(code) = invite.encode()
                    {
                        self.ctx.copy_text(code);
                        self.message = "已复制邀请，请在另一台电脑导入".into();
                    }
                    if ui.button("复制指纹").clicked() {
                        self.ctx.copy_text(invite.fingerprint.clone());
                    }
                });
            }
        });
        ui.add_space(12.0);
        ui.label("粘贴对方邀请");
        ui.add(
            egui::TextEdit::multiline(&mut self.invite)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        if let Ok(invite) = Invitation::decode(&self.invite) {
            ui.label(RichText::new(format!("对方设备：{}", invite.name)).color(BLUE));
        }
        field(ui, "已核对的对方指纹", &mut self.fingerprint);
        field(ui, "对方公网入口（可空）", &mut self.endpoint);
        ui.label(
            RichText::new("B 填 A 的 frp 公网地址和端口；A 可以留空，等待 B 主动连接。")
                .color(MUTED),
        );
        if primary(ui, "信任并添加设备").clicked() {
            let endpoint = if self.endpoint.trim().is_empty() {
                None
            } else {
                Some(self.endpoint.trim().into())
            };
            let result = self
                .store
                .add_peer(&self.invite, endpoint, self.fingerprint.trim())
                .map(|_| ());
            if result.is_ok() {
                self.invite.clear();
                self.fingerprint.clear();
                self.endpoint.clear();
                self.tab = Tab::Devices;
            }
            self.message(result);
        }
    }
    fn settings(&mut self, ui: &mut egui::Ui) {
        title(ui, "设置", "连接可以在无人登录时保持，界面只在需要时运行。");
        card().show(ui, |ui| {
            ui.label(RichText::new("后台与开机启动").strong());
            ui.label(
                "用户后台在当前账户下运行；Windows 服务以 SYSTEM 运行，并按设备授权处理请求。",
            );
            ui.horizontal(|ui| {
                if ui.button("启动用户后台").clicked() {
                    self.message(launch(&self.store, &["daemon"]));
                }
                if ui.button("安装并启动服务…").clicked() {
                    self.message(elevate(&self.store, &["service", "install"]));
                }
                if ui.button("启动服务…").clicked() {
                    self.message(elevate(&self.store, &["service", "start"]));
                }
            });
            ui.horizontal(|ui| {
                if ui.button("停止服务…").clicked() {
                    self.message(elevate(&self.store, &["service", "stop"]));
                }
                if ui.button("卸载服务…").clicked() {
                    self.message(elevate(&self.store, &["service", "uninstall"]));
                }
                if ui.button("登录时启动托盘").clicked() {
                    self.message(startup(&self.store, true));
                }
                if ui.button("取消托盘自启").clicked() {
                    self.message(startup(&self.store, false));
                }
            });
            ui.label(
                RichText::new(
                    "服务操作会触发 Windows 管理员确认。用户后台与服务使用同一身份，不能同时运行。",
                )
                .small()
                .color(MUTED),
            );
        });
        ui.add_space(8.0);
        card().show(ui, |ui| {
            ui.label(RichText::new("本机网络").strong());
            field(ui, "监听地址", &mut self.listen);
            ui.checkbox(&mut self.outbound, "只主动连接");
            if ui.button("保存网络设置").clicked() {
                let result = (|| {
                    let address = if self.outbound {
                        None
                    } else {
                        Some(self.listen.parse().context("监听地址无效")?)
                    };
                    self.store.update(|cfg| {
                        cfg.listen = address;
                        Ok(())
                    })?;
                    Ok(())
                })();
                self.message(result);
                self.message.push_str("；监听地址变更需要重启后台");
            }
            ui.label(
                RichText::new("frp 使用 TCP 透传到本机 127.0.0.1:41443；公网只需一个映射端口。")
                    .color(MUTED),
            );
        });
        ui.add_space(8.0);
        ui.label(format!("数据目录：{}", self.store.root.display()));
        if ui.button("打开数据与日志目录").clicked() {
            let result = std::process::Command::new("explorer.exe")
                .arg(&self.store.root)
                .spawn()
                .map(|_| ())
                .map_err(Into::into);
            self.message(result);
        }
        ui.label(
            RichText::new("关闭窗口后保留原生托盘，释放 GUI 资源；双击托盘重新打开。").color(MUTED),
        );
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(path) = &self.screenshot {
            eprintln!("GUI frame {}", self.screenshot_frames);
            for event in ctx.input(|i| i.events.clone()) {
                if let egui::Event::Screenshot { image, .. } = event {
                    let pixels = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                    if let Some(buffer) = image::RgbaImage::from_raw(
                        image.size[0] as u32,
                        image.size[1] as u32,
                        pixels,
                    ) {
                        let _ = buffer.save(path);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            self.screenshot_frames += 1;
            if self.screenshot_frames == 4 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        self.poll();
        ctx.request_repaint_after(Duration::from_secs(1));
        egui::SidePanel::left("nav")
            .exact_width(190.0)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(18, 25, 38))
                    .inner_margin(20),
            )
            .show(ctx, |ui| {
                ui.add_space(12.0);
                ui.label(RichText::new("W  WorkLink").size(24.0).strong().color(BLUE));
                ui.label(RichText::new("你的设备，始终相连").small().color(MUTED));
                ui.add_space(36.0);
                for (tab, label) in [
                    (Tab::Devices, "设备"),
                    (Tab::Files, "文件"),
                    (Tab::Tasks, "终端与任务"),
                    (Tab::Pair, "配对"),
                    (Tab::Settings, "设置"),
                ] {
                    ui.add_enabled_ui(self.initialized, |ui| {
                        if ui
                            .add_sized(
                                [146.0, 40.0],
                                egui::Button::new(label).selected(self.tab == tab),
                            )
                            .clicked()
                        {
                            self.tab = tab;
                        }
                    });
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(format!("v{}  ·  Windows 11", env!("CARGO_PKG_VERSION")))
                            .small()
                            .color(MUTED),
                    );
                    ui.label(RichText::new(&self.name).color(Color32::WHITE));
                });
            });
        egui::TopBottomPanel::bottom("footer")
            .frame(egui::Frame::new().fill(CARD).inner_margin(12))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if self.status.is_some() {
                            "● 后台已连接"
                        } else {
                            "○ 后台未连接"
                        })
                        .color(if self.status.is_some() {
                            GREEN
                        } else {
                            MUTED
                        }),
                    );
                    if self.busy {
                        ui.spinner();
                    }
                    if !self.message.is_empty() {
                        ui.label(&self.message);
                    }
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(28))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if !self.initialized {
                        self.init(ui);
                    } else {
                        match self.tab {
                            Tab::Devices => self.devices(ui),
                            Tab::Files => self.files(ui),
                            Tab::Tasks => self.tasks(ui),
                            Tab::Pair => self.pair(ui),
                            Tab::Settings => self.settings(ui),
                        }
                    }
                });
            });
    }
}

fn title(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(28.0).strong());
    ui.label(RichText::new(subtitle).color(MUTED));
    ui.add_space(16.0);
}
fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(12)
        .inner_margin(18)
}
fn metric(ui: &mut egui::Ui, label: &str, value: &str) {
    card().show(ui, |ui| {
        ui.vertical(|ui| {
            ui.set_min_width(165.0);
            ui.label(RichText::new(label).color(MUTED));
            ui.label(RichText::new(value).size(20.0).strong());
        });
    });
}
fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::from_rgb(10, 20, 35))).fill(BLUE))
}
fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.add_sized([155.0, 24.0], egui::Label::new(label));
        ui.add(egui::TextEdit::singleline(value).desired_width(520.0));
    });
}
fn error_text(error: anyhow::Error) -> String {
    format!("{error:#}")
}
fn format_bytes(value: u64) -> String {
    if value < 1024 {
        format!("{value} B")
    } else if value < 1024 * 1024 {
        format!("{:.1} KB", value as f64 / 1024.0)
    } else {
        format!("{:.1} MB", value as f64 / 1024.0 / 1024.0)
    }
}

fn pick_file(save: bool) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    unsafe {
        let mut buffer = vec![0u16; 32768];
        let filter = crate::platform::wide("所有文件\0*.*\0");
        let mut info: OPENFILENAMEW = std::mem::zeroed();
        info.lStructSize = std::mem::size_of_val(&info) as u32;
        info.hwndOwner = FindWindowW(std::ptr::null(), crate::platform::wide(TITLE).as_ptr());
        info.lpstrFilter = filter.as_ptr();
        info.lpstrFile = buffer.as_mut_ptr();
        info.nMaxFile = buffer.len() as u32;
        info.Flags = OFN_EXPLORER
            | OFN_PATHMUSTEXIST
            | OFN_NOCHANGEDIR
            | if save {
                OFN_OVERWRITEPROMPT
            } else {
                OFN_FILEMUSTEXIST
            };
        if (if save {
            GetSaveFileNameW(&mut info)
        } else {
            GetOpenFileNameW(&mut info)
        }) == 0
        {
            return None;
        }
        let length = buffer.iter().position(|c| *c == 0)?;
        Some(String::from_utf16_lossy(&buffer[..length]).into())
    }
}

fn icon_pixels(size: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let i = (y * size + x) * 4;
            let nx = x as f32 / size as f32;
            let ny = y as f32 / size as f32;
            let stroke = (ny > 0.25 && ny < 0.73)
                && ((nx - (0.19 + (ny - 0.25) * 0.3)).abs() < 0.045
                    || (nx - (0.49 - (ny - 0.25) * 0.25)).abs() < 0.045
                    || (nx - (0.51 + (ny - 0.25) * 0.25)).abs() < 0.045
                    || (nx - (0.81 - (ny - 0.25) * 0.3)).abs() < 0.045);
            rgba[i..i + 4].copy_from_slice(if stroke {
                &[235, 245, 255, 255]
            } else {
                &[47, 100, 184, 255]
            });
        }
    }
    rgba
}

pub fn tray(store: Store) -> Result<()> {
    std::fs::create_dir_all(&store.root)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(store.root.join("tray.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(());
    }
    use tray_icon::{
        TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    };
    let menu = Menu::new();
    let open = MenuItem::new("打开 WorkLink", true, None);
    let start = MenuItem::new("启动用户后台", true, None);
    let exit = MenuItem::new("退出托盘（后台继续运行）", true, None);
    menu.append_items(&[&open, &start, &PredefinedMenuItem::separator(), &exit])?;
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(tray_icon::Icon::from_rgba(icon_pixels(32), 32, 32)?)
        .with_tooltip("WorkLink · 双击打开远程工作台")
        .with_menu_on_left_click(false)
        .build()?;
    unsafe {
        let mut msg = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == open.id() {
                    let _ = launch(&store, &["gui"]);
                } else if event.id == start.id() {
                    let _ = launch(&store, &["daemon"]);
                } else if event.id == exit.id() {
                    return Ok(());
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    let _ = launch(&store, &["gui"]);
                }
            }
        }
    }
    Ok(())
}

fn elevate(store: &Store, args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let params = format!(
        "--data-dir {} {}",
        crate::platform::quote_arg(&store.root.to_string_lossy()),
        args.join(" ")
    );
    unsafe {
        let result = windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            crate::platform::wide("runas").as_ptr(),
            crate::platform::wide(exe).as_ptr(),
            crate::platform::wide(params).as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        anyhow::ensure!(result as isize > 32, "管理员操作未启动或被取消");
    }
    Ok(())
}

fn startup(store: &Store, enabled: bool) -> Result<()> {
    let mut command = std::process::Command::new("reg.exe");
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    if enabled {
        let value = format!(
            "{} --data-dir {} tray",
            crate::platform::quote_arg(&std::env::current_exe()?.to_string_lossy()),
            crate::platform::quote_arg(&store.root.to_string_lossy())
        );
        command.args([
            "add", key, "/v", "WorkLink", "/t", "REG_SZ", "/d", &value, "/f",
        ]);
    } else {
        command.args(["delete", key, "/v", "WorkLink", "/f"]);
    }
    let status = command
        .creation_flags(0x08000000)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    anyhow::ensure!(status.success(), "无法更新托盘登录启动项");
    Ok(())
}
