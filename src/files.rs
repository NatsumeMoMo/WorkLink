use crate::{
    platform,
    protocol::{self, Reply},
    transport::Stream,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub size: u64,
    pub sha256: String,
    pub offset: u64,
}

pub fn hash_file(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(hex::encode(hash.finalize()))
}

fn regular(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && meta.file_attributes() & 0x400 == 0,
        "路径必须是普通文件，不能是重解析点"
    );
    Ok(())
}

pub async fn upload(
    stream: &mut Stream,
    owner: String,
    path: String,
    size: u64,
    sha256: String,
    overwrite: bool,
    system: bool,
) -> Result<()> {
    ensure!(
        sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "文件校验值无效"
    );
    let target = platform::absolute_file(&path)?;
    let partial = PathBuf::from(format!("{}.worklink-{}.part", target.display(), sha256));
    let t = target.clone();
    let p = partial.clone();
    let who = owner.clone();
    let (file, offset) = tokio::task::spawn_blocking(move || {
        platform::as_user(&who, system, || {
            ensure!(
                overwrite || !t.exists(),
                "目标文件已存在，请显式指定 --overwrite"
            );
            ensure!(t.parent().is_some_and(Path::is_dir), "目标目录不存在");
            if p.exists() {
                regular(&p)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&p)?;
            file.try_lock_exclusive().context("同一文件已有传输任务")?;
            let mut offset = file.metadata()?.len();
            if offset > size {
                file.set_len(0)?;
                offset = 0;
            }
            Ok((file, offset))
        })
    })
    .await??;
    protocol::write_json(
        stream,
        &Reply::ok(FileInfo {
            size,
            sha256: sha256.clone(),
            offset,
        }),
    )
    .await?;
    let mut file = tokio::fs::File::from_std(file);
    tokio::io::AsyncSeekExt::seek(&mut file, SeekFrom::Start(offset)).await?;
    let transferred = copy_bytes(stream, &mut file, size - offset).await;
    file.flush().await?;
    file.sync_all().await?;
    let mut file = file.into_std().await;
    transferred?;
    let t = target.clone();
    let p = partial.clone();
    tokio::task::spawn_blocking(move || {
        if hash_file(&mut file)? != sha256 {
            file.set_len(0)?;
            file.sync_all()?;
            anyhow::bail!("文件校验失败，已重置损坏的临时文件，请重新执行传输");
        }
        FileExt::unlock(&file)?;
        drop(file);
        platform::as_user(&owner, system, || {
            ensure!(overwrite || !t.exists(), "提交前发现目标已存在，未覆盖");
            if overwrite {
                fs::rename(&p, &t)?;
            } else {
                fs::hard_link(&p, &t)?;
                fs::remove_file(&p)?;
            }
            Ok(())
        })
    })
    .await??;
    protocol::write_json(
        stream,
        &Reply::ok(serde_json::json!({"path":path,"size":size,"verified":true})),
    )
    .await?;
    Ok(())
}

pub async fn download(
    stream: &mut Stream,
    owner: String,
    path: String,
    requested_offset: u64,
    expected_hash: Option<String>,
    system: bool,
) -> Result<()> {
    let source = platform::absolute_file(&path)?;
    let (file, info) = tokio::task::spawn_blocking(move || {
        platform::as_user(&owner, system, || {
            regular(&source)?;
            let mut file = File::open(&source)?;
            let size = file.metadata()?.len();
            let sha256 = hash_file(&mut file)?;
            let offset = if expected_hash.as_deref() == Some(&sha256) && requested_offset <= size {
                requested_offset
            } else {
                0
            };
            file.seek(SeekFrom::Start(offset))?;
            Ok((
                file,
                FileInfo {
                    size,
                    sha256,
                    offset,
                },
            ))
        })
    })
    .await??;
    protocol::write_json(stream, &Reply::ok(&info)).await?;
    let mut file = tokio::fs::File::from_std(file);
    copy_bytes(&mut file, stream, info.size - info.offset).await?;
    stream.shutdown().await?;
    Ok(())
}

pub async fn copy_bytes<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    input: &mut R,
    output: &mut W,
    mut remaining: u64,
) -> Result<u64> {
    let initial = remaining;
    let mut buffer = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let capacity = remaining.min(buffer.len() as u64) as usize;
        let n = tokio::time::timeout(
            Duration::from_secs(120),
            input.read(&mut buffer[..capacity]),
        )
        .await??;
        ensure!(n > 0, "传输中断，临时文件已保留，可重新执行以续传");
        tokio::time::timeout(Duration::from_secs(120), output.write_all(&buffer[..n])).await??;
        remaining -= n as u64;
    }
    output.flush().await?;
    Ok(initial)
}

pub fn local_partial(target: &Path) -> PathBuf {
    PathBuf::from(format!("{}.worklink.part", target.display()))
}

pub fn save_download_meta(target: &Path, info: &FileInfo) -> Result<()> {
    crate::config::atomic_json(
        &PathBuf::from(format!("{}.json", local_partial(target).display())),
        info,
    )
}

pub fn download_resume(target: &Path) -> (u64, Option<String>) {
    let part = local_partial(target);
    let data = fs::read(format!("{}.json", part.display()))
        .ok()
        .and_then(|v| serde_json::from_slice::<FileInfo>(&v).ok());
    match (fs::metadata(&part), data) {
        (Ok(meta), Some(info)) => (meta.len(), Some(info.sha256)),
        _ => (0, None),
    }
}
