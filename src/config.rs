use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Permissions {
    pub ssh: bool,
    pub files: bool,
    pub exec: bool,
    pub system: bool,
    #[serde(default)]
    pub browser: bool,
    #[serde(default)]
    pub desktop: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            ssh: true,
            files: false,
            exec: false,
            system: false,
            browser: false,
            desktop: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub certificate: String,
    pub fingerprint: String,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub allow: Permissions,
    #[serde(default)]
    pub ssh_client: SshClient,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SshClient {
    pub user: Option<String>,
    pub identity_file: Option<String>,
    pub known_hosts: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub owner_sid: String,
    pub listen: Option<SocketAddr>,
    pub ssh_port: u16,
    pub peers: Vec<Peer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invitation {
    pub version: u32,
    pub id: String,
    pub name: String,
    pub certificate: String,
    pub fingerprint: String,
}

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn read(&self) -> Result<Config> {
        let data = fs::read(self.root.join("config.json"))
            .context("尚未初始化，请先执行 WorkLink init --name <设备名称>")?;
        let cfg: Config = serde_json::from_slice(&data).context("设备配置损坏")?;
        ensure!(cfg.version == 1, "不支持的配置版本");
        Ok(cfg)
    }

    pub fn update<T>(&self, f: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("config.lock"))?;
        lock.lock_exclusive()?;
        let mut cfg = self.read()?;
        let result = f(&mut cfg)?;
        self.write_json("config.json", &cfg)?;
        FileExt::unlock(&lock)?;
        Ok(result)
    }

    pub fn write_json(&self, name: impl AsRef<Path>, value: &impl Serialize) -> Result<()> {
        atomic_json(&self.root.join(name), value)
    }

    pub fn init(&self, name: String, listen: Option<SocketAddr>) -> Result<Config> {
        ensure!(
            !name.chars().any(char::is_control),
            "设备名称不能含控制字符"
        );
        fs::create_dir_all(&self.root)?;
        let init_lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("config.lock"))?;
        init_lock.lock_exclusive()?;
        ensure!(
            !name.trim().is_empty() && name.len() <= 128,
            "设备名称长度应为 1–128 字节"
        );
        ensure!(
            !self.root.join("config.json").exists(),
            "该目录已经初始化，未覆盖现有身份"
        );
        fs::create_dir_all(&self.root)?;
        let owner_sid = crate::platform::current_sid()?;
        crate::platform::protect_directory(&self.root, &owner_sid)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let key = rcgen::KeyPair::generate()?;
        let mut params = rcgen::CertificateParams::new(vec![server_name(&id)])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name.clone());
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let cert = params.self_signed(&key)?;
        // 服务和普通用户需要共用身份，因此使用机器范围 DPAPI 并以目录 ACL 限制读取。
        let private = zeroize::Zeroizing::new(key.serialize_der());
        fs::write(
            self.root.join("identity.key.dpapi"),
            crate::platform::seal(&private)?,
        )?;
        fs::write(self.root.join("identity.crt.pem"), cert.pem())?;
        let cfg = Config {
            version: 1,
            id,
            name,
            owner_sid,
            listen,
            ssh_port: 22,
            peers: vec![],
        };
        self.write_json("config.json", &cfg)?;
        fs::create_dir_all(self.root.join("jobs"))?;
        FileExt::unlock(&init_lock)?;
        Ok(cfg)
    }

    pub fn invitation(&self) -> Result<Invitation> {
        let cfg = self.read()?;
        let certificate = fs::read_to_string(self.root.join("identity.crt.pem"))?;
        let fingerprint = fingerprint(&certificate)?;
        Ok(Invitation {
            version: 1,
            id: cfg.id,
            name: cfg.name,
            certificate,
            fingerprint,
        })
    }

    pub fn add_peer(
        &self,
        input: &str,
        endpoint: Option<String>,
        expected_fingerprint: &str,
    ) -> Result<Peer> {
        let invite = Invitation::decode(input)?;
        ensure!(
            invite
                .fingerprint
                .eq_ignore_ascii_case(expected_fingerprint),
            "指纹不匹配，未添加设备"
        );
        if let Some(ep) = &endpoint {
            validate_endpoint(ep)?;
        }
        self.update(|cfg| {
            ensure!(cfg.id != invite.id, "不能添加自己");
            if let Some(old) = cfg.peers.iter().find(|p| p.id == invite.id) {
                ensure!(
                    old.fingerprint == invite.fingerprint,
                    "设备 ID 已存在，但身份发生变化；请先核实并撤销旧身份"
                );
                bail!("设备已经添加，请使用 peer endpoint 修改入口");
            }
            ensure!(
                !cfg.peers
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(&invite.name)),
                "设备名重复，请先重命名"
            );
            ensure!(cfg.peers.len() < 16, "设备数量已达上限");
            let peer = Peer {
                id: invite.id,
                name: invite.name,
                certificate: invite.certificate,
                fingerprint: invite.fingerprint,
                endpoint,
                allow: Permissions::default(),
                ssh_client: SshClient::default(),
            };
            cfg.peers.push(peer.clone());
            Ok(peer)
        })
    }

    pub fn peer(&self, name: &str) -> Result<Peer> {
        find_peer(&self.read()?, name).cloned()
    }

    pub fn pipe_name(&self) -> Result<String> {
        Ok(format!(r"\\.\pipe\worklink-{}", self.read()?.id))
    }
}

impl Invitation {
    pub fn encode(&self) -> Result<String> {
        Ok(format!(
            "worklink1:{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }

    pub fn decode(input: &str) -> Result<Self> {
        ensure!(input.len() <= 32_768, "邀请过长");
        let bytes = if let Some(code) = input.trim().strip_prefix("worklink1:") {
            URL_SAFE_NO_PAD.decode(code)?
        } else {
            input.as_bytes().to_vec()
        };
        let invite: Self = serde_json::from_slice(&bytes).context("邀请格式无效")?;
        ensure!(invite.version == 1, "不支持的邀请版本");
        ensure!(
            invite.id.len() == 32 && invite.id.bytes().all(|b| b.is_ascii_hexdigit()),
            "设备 ID 无效"
        );
        ensure!(
            !invite.name.trim().is_empty() && invite.name.len() <= 128,
            "设备名称无效"
        );
        ensure!(
            !invite.name.chars().any(char::is_control),
            "设备名称不能含控制字符"
        );
        ensure!(invite.certificate.len() <= 4096, "设备证书过长");
        ensure!(
            fingerprint(&invite.certificate)? == invite.fingerprint,
            "邀请证书与指纹不一致"
        );
        Ok(invite)
    }
}

pub fn find_peer<'a>(cfg: &'a Config, name: &str) -> Result<&'a Peer> {
    cfg.peers
        .iter()
        .find(|p| p.id == name || p.name.eq_ignore_ascii_case(name))
        .with_context(|| format!("未知设备：{name}"))
}

pub fn cert_der(pem: &str) -> Result<rustls::pki_types::CertificateDer<'static>> {
    let mut reader = std::io::Cursor::new(pem);
    let certs = rustls_pemfile::certs(&mut reader).collect::<std::io::Result<Vec<_>>>()?;
    ensure!(certs.len() == 1, "邀请必须恰好包含一张设备证书");
    Ok(certs.into_iter().next().unwrap())
}

pub fn fingerprint(pem: &str) -> Result<String> {
    Ok(hex::encode(Sha256::digest(cert_der(pem)?.as_ref())))
}

pub fn server_name(id: &str) -> String {
    format!("worklink-{id}")
}

pub fn validate_endpoint(value: &str) -> Result<()> {
    let (host, port) = value
        .rsplit_once(':')
        .context("入口应为 主机名:端口 或 IP:端口")?;
    ensure!(
        !host.is_empty() && !host.contains(['/', '\\', ' ', '\n', '\r']),
        "入口主机名无效"
    );
    ensure!(port.parse::<u16>().is_ok_and(|p| p > 0), "入口端口无效");
    Ok(())
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().context("没有父目录")?)?;
    serde_json::to_writer_pretty(&mut tmp, value)?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
