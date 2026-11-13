use crate::{
    config::{self, Peer, Store},
    protocol::{self, Reply, Request},
};
use anyhow::{Context, Result, bail, ensure};
use futures::future::poll_fn;
use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    task::Poll,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{RwLock, Semaphore, mpsc, oneshot},
    time::{sleep, timeout},
};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use tokio_util::{
    compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt},
    sync::CancellationToken,
};

pub type Stream = Compat<yamux::Stream>;
type OpenSender = oneshot::Sender<Result<yamux::Stream>>;

#[derive(Clone)]
pub struct Link {
    sender: mpsc::Sender<OpenSender>,
    pub cancel: CancellationToken,
    pub serial: String,
    pub since: u64,
    preferred: bool,
    allow: crate::config::Permissions,
    fingerprint: String,
}

impl Link {
    pub async fn open(&self) -> Result<Stream> {
        let (send, recv) = oneshot::channel();
        timeout(Duration::from_secs(10), self.sender.send(send)).await??;
        Ok(timeout(Duration::from_secs(10), recv).await???.compat())
    }
}

pub struct Incoming {
    pub peer: String,
    pub stream: Stream,
    pub cancel: CancellationToken,
}

#[derive(Serialize)]
pub struct PeerStatus {
    #[serde(flatten)]
    pub peer: Peer,
    pub online: bool,
    pub connected_since: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone)]
pub struct Manager {
    store: Store,
    links: Arc<RwLock<HashMap<String, Link>>>,
    errors: Arc<RwLock<HashMap<String, String>>>,
    incoming: mpsc::Sender<Incoming>,
    stop: CancellationToken,
}

impl Manager {
    pub fn new(store: Store, incoming: mpsc::Sender<Incoming>, stop: CancellationToken) -> Self {
        Self {
            store,
            incoming,
            stop,
            links: Default::default(),
            errors: Default::default(),
        }
    }

    pub async fn status(&self) -> Result<Vec<PeerStatus>> {
        let cfg = self.store.read()?;
        let links = self.links.read().await;
        let errors = self.errors.read().await;
        Ok(cfg
            .peers
            .into_iter()
            .map(|peer| {
                let link = links.get(&peer.id);
                PeerStatus {
                    online: link.is_some(),
                    connected_since: link.map(|l| l.since),
                    last_error: errors.get(&peer.id).cloned(),
                    peer,
                }
            })
            .collect())
    }

    pub async fn open(&self, name: &str) -> Result<Stream> {
        let peer = self.store.peer(name)?;
        let link = self
            .links
            .read()
            .await
            .get(&peer.id)
            .cloned()
            .context("设备离线，尚未建立连接")?;
        link.open().await
    }

    pub async fn run(&self) -> Result<()> {
        let cfg = self.store.read()?;
        let listener = if let Some(address) = cfg.listen {
            Some(
                TcpListener::bind(address)
                    .await
                    .with_context(|| format!("无法监听 {address}"))?,
            )
        } else {
            None
        };
        let handshakes = Arc::new(Semaphore::new(16));
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let mut attempts: HashMap<String, (Instant, u32, Option<String>)> = HashMap::new();
        let pending = Arc::new(tokio::sync::Mutex::new(
            std::collections::HashSet::<String>::new(),
        ));
        tracing::info!(device = %cfg.name, listen = ?cfg.listen, "connection manager ready");
        loop {
            tokio::select! {
                _ = self.stop.cancelled() => break,
                accepted = async { match &listener { Some(l) => l.accept().await, None => std::future::pending().await } } => {
                    match accepted {
                        Ok((socket, _)) => {
                            if let Ok(permit) = handshakes.clone().try_acquire_owned() {
                                let manager = self.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(error) = manager.accept(socket).await { tracing::debug!(%error, "incoming connection rejected"); }
                                });
                            }
                        }
                        Err(error) => { tracing::warn!(%error, "accept failed"); sleep(Duration::from_millis(300)).await; }
                    }
                }
                _ = interval.tick() => {
                    let cfg = self.store.read()?;
                    {
                        let mut links = self.links.write().await;
                        links.retain(|id, link| {
                            let paired=cfg.peers.iter().find(|p|p.id==*id);
                            // 增加权限可以立即生效，收回权限才需要关闭已有会话。
                            let keep=paired.is_some_and(|p|p.fingerprint==link.fingerprint&&(!link.allow.ssh||p.allow.ssh)&&(!link.allow.files||p.allow.files)&&(!link.allow.exec||p.allow.exec)&&(!link.allow.system||p.allow.system));
                            if keep{link.allow=paired.unwrap().allow.clone();}
                            if !keep { link.cancel.cancel(); }
                            keep
                        });
                    }
                    for peer in cfg.peers {
                        if peer.endpoint.is_none() || self.links.read().await.contains_key(&peer.id) || pending.lock().await.contains(&peer.id) { continue; }
                        let entry = attempts.entry(peer.id.clone()).or_insert((Instant::now(), 0, peer.endpoint.clone()));
                        if entry.2 != peer.endpoint { *entry = (Instant::now(), 0, peer.endpoint.clone()); }
                        if Instant::now() < entry.0 { continue; }
                        let seconds = 1u64 << entry.1.min(5);
                        entry.0 = Instant::now() + Duration::from_millis(seconds * 1000 + rand::random_range(0..800));
                        entry.1 = entry.1.saturating_add(1);
                        pending.lock().await.insert(peer.id.clone());
                        let manager = self.clone();
                        let pending = pending.clone();
                        tokio::spawn(async move {
                            if let Err(error) = manager.connect(&peer).await {
                                manager.errors.write().await.insert(peer.id.clone(), format!("{error:#}"));
                            }
                            pending.lock().await.remove(&peer.id);
                        });
                    }
                    attempts.retain(|id, _| !self.links.try_read().is_ok_and(|l| l.contains_key(id)));
                }
            }
        }
        for link in self.links.write().await.drain().map(|(_, l)| l) {
            link.cancel.cancel();
        }
        Ok(())
    }

    async fn connect(&self, peer: &Peer) -> Result<()> {
        let (cert, key) = identity(&self.store)?;
        let mut roots = RootCertStore::empty();
        roots.add(config::cert_der(&peer.certificate)?)?;
        let mut tls = ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![cert], key)?;
        tls.alpn_protocols = vec![b"worklink/1".to_vec()];
        let endpoint = peer.endpoint.as_deref().context("未配置入口")?;
        let socket = timeout(Duration::from_secs(8), TcpStream::connect(endpoint)).await??;
        socket.set_nodelay(true)?;
        let name = ServerName::try_from(config::server_name(&peer.id))?;
        let stream = timeout(
            Duration::from_secs(protocol::HANDSHAKE_SECONDS),
            TlsConnector::from(Arc::new(tls)).connect(name, socket),
        )
        .await??;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(b"worklink/1"),
            "协议版本不匹配"
        );
        let remote = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .context("未收到设备证书")?;
        ensure!(
            remote.as_ref() == config::cert_der(&peer.certificate)?.as_ref(),
            "设备证书与配对记录不符"
        );
        self.establish(peer.id.clone(), TlsStream::Client(stream), true)
            .await
    }

    async fn accept(&self, socket: TcpStream) -> Result<()> {
        socket.set_nodelay(true)?;
        let cfg = self.store.read()?;
        if cfg.peers.is_empty() {
            bail!("尚未配对设备");
        }
        let mut roots = RootCertStore::empty();
        for peer in &cfg.peers {
            roots.add(config::cert_der(&peer.certificate)?)?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
        let (cert, key) = identity(&self.store)?;
        let mut tls = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![cert], key)?;
        tls.alpn_protocols = vec![b"worklink/1".to_vec()];
        let stream = timeout(
            Duration::from_secs(protocol::HANDSHAKE_SECONDS),
            TlsAcceptor::from(Arc::new(tls)).accept(socket),
        )
        .await??;
        ensure!(
            stream.get_ref().1.alpn_protocol() == Some(b"worklink/1"),
            "协议版本不匹配"
        );
        let remote = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .context("未收到设备证书")?;
        let peer = cfg
            .peers
            .iter()
            .find(|p| config::cert_der(&p.certificate).is_ok_and(|c| c.as_ref() == remote.as_ref()))
            .context("设备证书未配对")?;
        self.establish(peer.id.clone(), TlsStream::Server(stream), false)
            .await
    }

    async fn establish(
        &self,
        peer: String,
        tls: TlsStream<TcpStream>,
        outbound: bool,
    ) -> Result<()> {
        let cfg = self.store.read()?;
        ensure!(cfg.peers.iter().any(|p| p.id == peer), "设备已撤销");
        let preferred = (cfg.id < peer) == outbound;
        let paired = crate::config::find_peer(&cfg, &peer)?;
        let remote = tls
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .context("缺少设备证书")?;
        ensure!(
            remote.as_ref() == crate::config::cert_der(&paired.certificate)?.as_ref(),
            "配对身份在握手期间发生变化"
        );
        let cancel = self.stop.child_token();
        let (sender, mut receiver) = mpsc::channel::<OpenSender>(32);
        let link = Link {
            sender,
            cancel: cancel.clone(),
            serial: uuid::Uuid::new_v4().to_string(),
            since: now(),
            preferred,
            allow: paired.allow.clone(),
            fingerprint: paired.fingerprint.clone(),
        };
        {
            let mut links = self.links.write().await;
            if let Some(old) = links.get(&peer) {
                // 双方使用同一优先级规则，避免同时拨号时互相关闭对方选中的连接。
                if old.preferred || !preferred {
                    return Ok(());
                }
                old.cancel.cancel();
            }
            links.insert(peer.clone(), link.clone());
        }
        self.errors.write().await.remove(&peer);
        let mut config = yamux::Config::default();
        config.set_max_num_streams(protocol::MAX_STREAMS);
        config.set_max_connection_receive_window(Some(protocol::MAX_STREAMS * 256 * 1024));
        let mut connection = yamux::Connection::new(
            tls.compat(),
            config,
            if outbound {
                yamux::Mode::Client
            } else {
                yamux::Mode::Server
            },
        );
        let mut queued = VecDeque::<OpenSender>::new();
        let incoming = self.incoming.clone();
        let peer_id = peer.clone();
        let active = link.clone();
        let manager = self.clone();
        tracing::info!(device = %peer, outbound, "device connected");
        tokio::spawn(async move {
            let heartbeat = active.clone();
            let heart_cancel = active.cancel.clone();
            let heart = tokio::spawn(async move {
                loop {
                    tokio::select! { _ = heart_cancel.cancelled() => break, _ = sleep(Duration::from_secs(20)) => {} }
                    let result = timeout(Duration::from_secs(10), async {
                        let mut stream = heartbeat.open().await?;
                        protocol::write_json(&mut stream, &Request::Ping).await?;
                        protocol::read_json::<_, Reply>(&mut stream)
                            .await?
                            .checked()?;
                        Ok::<_, anyhow::Error>(())
                    })
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        heart_cancel.cancel();
                        break;
                    }
                }
            });
            let result: Result<()> = async {
                loop {
                    let next = poll_fn(|cx| {
                        while queued.len() < 32 {
                            match receiver.poll_recv(cx) { Poll::Ready(Some(request)) => queued.push_back(request), _ => break }
                        }
                        while !queued.is_empty() {
                            match connection.poll_new_outbound(cx) {
                                Poll::Ready(result) => { let request = queued.pop_front().unwrap(); let _ = request.send(result.map_err(Into::into)); }
                                Poll::Pending => break,
                            }
                        }
                        connection.poll_next_inbound(cx)
                    });
                    let stream = tokio::select! { _ = active.cancel.cancelled() => break, stream = next => stream };
                    match stream {
                        Some(Ok(stream)) => {
                            let request = Incoming { peer: peer_id.clone(), stream: stream.compat(), cancel: active.cancel.clone() };
                            if incoming.try_send(request).is_err() { tracing::warn!("incoming stream queue full"); }
                        }
                        Some(Err(error)) => return Err(error.into()),
                        None => break,
                    }
                }
                Ok(())
            }.await;
            active.cancel.cancel();
            heart.abort();
            let mut links = manager.links.write().await;
            if links
                .get(&peer_id)
                .is_some_and(|l| l.serial == active.serial)
            {
                links.remove(&peer_id);
            }
            if let Err(error) = result {
                manager
                    .errors
                    .write()
                    .await
                    .insert(peer_id.clone(), error.to_string());
            }
            tracing::info!(device = %peer_id, "device disconnected");
        });
        Ok(())
    }
}

fn identity(
    store: &Store,
) -> Result<(
    rustls::pki_types::CertificateDer<'static>,
    PrivateKeyDer<'static>,
)> {
    let cert = config::cert_der(&std::fs::read_to_string(
        store.root.join("identity.crt.pem"),
    )?)?;
    let key = zeroize::Zeroizing::new(crate::platform::unseal(&std::fs::read(
        store.root.join("identity.key.dpapi"),
    )?)?);
    Ok((
        cert,
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
    ))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
