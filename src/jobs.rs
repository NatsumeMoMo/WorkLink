use crate::{
    config::{Store, atomic_json},
    platform,
    protocol::JobSpec,
    transport::now,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobRecord {
    pub spec: JobSpec,
    pub owner_peer: String,
    pub state: String,
    pub created: u64,
    pub finished: Option<u64>,
    pub exit_code: Option<u32>,
    pub error: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
}

#[derive(Clone)]
pub struct Jobs {
    store: Store,
    running: Arc<Mutex<HashMap<String, CancellationToken>>>,
    admission: Arc<Mutex<()>>,
    slots: Arc<Semaphore>,
    stop: CancellationToken,
}

impl Jobs {
    pub fn new(store: Store, stop: CancellationToken) -> Result<Self> {
        fs::create_dir_all(store.root.join("jobs"))?;
        for entry in fs::read_dir(store.root.join("jobs"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "json")
                && let Ok(mut job) = serde_json::from_slice::<JobRecord>(&fs::read(&path)?)
                && matches!(job.state.as_str(), "running" | "queued")
            {
                // 后台重启时无法确认外部副作用，保留中断状态，绝不自动重跑。
                job.state = "interrupted".into();
                job.error = Some("后台在任务完成前退出；请核实结果后再提交新任务".into());
                job.finished = Some(now());
                atomic_json(&path, &job)?;
            }
        }
        Ok(Self {
            store,
            running: Default::default(),
            admission: Default::default(),
            slots: Arc::new(Semaphore::new(8)),
            stop,
        })
    }

    pub async fn submit(&self, owner: String, spec: JobSpec) -> Result<JobRecord> {
        validate_id(&spec.id)?;
        ensure!(
            spec.timeout_secs > 0 && spec.timeout_secs <= 86400,
            "任务超时应为 1–86400 秒"
        );
        ensure!(
            spec.args.len() <= 256 && spec.program.len() <= 32768,
            "命令过长"
        );
        ensure!(
            serde_json::to_vec(&spec)?.len() <= 24000,
            "编码后的命令超过 24 KB 限制"
        );
        let _admission = self.admission.lock().await;
        if self.path(&spec.id)?.exists() {
            let old = self.get(&owner, &spec.id, false)?;
            ensure!(old.spec == spec, "任务 ID 已用于不同命令");
            return Ok(old);
        }
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .context("并发任务已达上限，请稍后重试")?;
        self.prune()?;
        let record = JobRecord {
            spec: spec.clone(),
            owner_peer: owner,
            state: "queued".into(),
            created: now(),
            finished: None,
            exit_code: None,
            error: None,
            stdout: String::new(),
            stderr: String::new(),
            output_truncated: false,
        };
        self.store
            .write_json(format!("jobs/{}.json", spec.id), &record)?;
        let cancel = self.stop.child_token();
        self.running
            .lock()
            .await
            .insert(spec.id.clone(), cancel.clone());
        let jobs = self.clone();
        let job = record.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = jobs.execute(job, cancel).await {
                tracing::error!(%error, "job state persistence failed");
            }
            jobs.running.lock().await.remove(&spec.id);
        });
        Ok(record)
    }

    async fn execute(&self, mut record: JobRecord, cancel: CancellationToken) -> Result<()> {
        let cfg = self.store.read()?;
        let spec = record.spec.clone();
        let stdout_path = self.store.root.join(format!("jobs/{}.stdout", spec.id));
        let stderr_path = self.store.root.join(format!("jobs/{}.stderr", spec.id));
        let spawned = tokio::task::spawn_blocking(move || {
            platform::spawn_process(&spec, &cfg.owner_sid, None)
        })
        .await?;
        match spawned {
            Err(error) => {
                record.state = "failed".into();
                record.error = Some(format!("{error:#}"));
            }
            Ok(mut child) => {
                record.state = "running".into();
                self.store
                    .write_json(format!("jobs/{}.json", record.spec.id), &record)?;
                child.input.take();
                let out_file = File::create(&stdout_path)?;
                let err_file = File::create(&stderr_path)?;
                let output = child.output.take().unwrap();
                let error = child.error.take().unwrap();
                let out = tokio::task::spawn_blocking(move || drain(output, out_file));
                let err = tokio::task::spawn_blocking(move || drain(error, err_file));
                let deadline = tokio::time::sleep(Duration::from_secs(record.spec.timeout_secs));
                tokio::pin!(deadline);
                let mut next_policy = std::time::Instant::now() + Duration::from_secs(1);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => { child.terminate(); record.state = "cancelled".into(); break; }
                        _ = &mut deadline => { child.terminate(); record.state = "timed_out".into(); break; }
                        _ = tokio::time::sleep(Duration::from_millis(100)) => {
                            if std::time::Instant::now()>=next_policy{
                                next_policy=std::time::Instant::now()+Duration::from_secs(1);
                                let cfg=self.store.read()?;
                                if !cfg.peers.iter().any(|p|p.id==record.owner_peer&&p.allow.exec&&(!record.spec.system||p.allow.system)){
                                    child.terminate();record.state="cancelled".into();record.error=Some("发起设备的执行权限已撤销".into());break;
                                }
                            }
                            if let Some(code) = child.exit_code()? { record.exit_code = Some(code); record.state = "completed".into(); break; }
                        }
                    }
                }
                // 根进程退出后关闭 Job Object，防止后台子进程一直占着输出管道。
                child.terminate();
                drop(child);
                record.output_truncated = out.await?? | err.await??;
            }
        }
        record.finished = Some(now());
        self.store
            .write_json(format!("jobs/{}.json", record.spec.id), &record)?;
        Ok(())
    }

    pub fn get(&self, owner: &str, id: &str, output: bool) -> Result<JobRecord> {
        let mut record: JobRecord =
            serde_json::from_slice(&fs::read(self.path(id)?).context("未找到任务")?)?;
        ensure!(record.owner_peer == owner, "该任务属于其他设备");
        if output {
            record.stdout = read_output(self.store.root.join(format!("jobs/{id}.stdout")))?;
            record.stderr = read_output(self.store.root.join(format!("jobs/{id}.stderr")))?;
        }
        Ok(record)
    }

    pub fn list(&self, owner: &str) -> Result<Vec<JobRecord>> {
        let mut records = vec![];
        for entry in fs::read_dir(self.store.root.join("jobs"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "json")
                && let Ok(record) = serde_json::from_slice::<JobRecord>(&fs::read(path)?)
                && record.owner_peer == owner
            {
                records.push(record);
            }
        }
        records.sort_by_key(|r| std::cmp::Reverse(r.created));
        records.truncate(100);
        Ok(records)
    }

    pub async fn cancel(&self, owner: &str, id: &str) -> Result<JobRecord> {
        let record = self.get(owner, id, false)?;
        if let Some(cancel) = self.running.lock().await.get(id) {
            cancel.cancel();
        }
        Ok(record)
    }

    pub async fn shutdown(&self) {
        let wait = async {
            loop {
                if self.running.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        if tokio::time::timeout(Duration::from_secs(8), wait)
            .await
            .is_err()
        {
            tracing::warn!(
                "job shutdown timed out; unfinished records will be marked interrupted on restart"
            );
        }
    }

    fn path(&self, id: &str) -> Result<std::path::PathBuf> {
        validate_id(id)?;
        Ok(self.store.root.join(format!("jobs/{id}.json")))
    }

    fn prune(&self) -> Result<()> {
        let mut completed = vec![];
        for entry in fs::read_dir(self.store.root.join("jobs"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "json")
                && let Ok(record) = serde_json::from_slice::<JobRecord>(&fs::read(&path)?)
                && record.finished.is_some()
            {
                completed.push(record);
            }
        }
        completed.sort_by_key(|r| r.created);
        let remove_count = completed.len().saturating_sub(199);
        for record in completed.into_iter().take(remove_count) {
            for suffix in ["json", "stdout", "stderr"] {
                let path = self
                    .store
                    .root
                    .join(format!("jobs/{}.{suffix}", record.spec.id));
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "任务 ID 必须是 UUID");
    ensure!(
        id.len() <= 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "任务 ID 无效"
    );
    Ok(())
}

fn drain(mut input: File, mut output: File) -> Result<bool> {
    let mut total = 0;
    let mut buffer = [0u8; 8192];
    loop {
        let n = match input.read(&mut buffer) {
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(109) => 0,
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            break;
        }
        let keep = n.min(OUTPUT_LIMIT.saturating_sub(total));
        output.write_all(&buffer[..keep])?;
        total = total.saturating_add(n);
    }
    output.sync_all()?;
    Ok(total > OUTPUT_LIMIT)
}

fn read_output(path: std::path::PathBuf) -> Result<String> {
    match File::open(path) {
        Ok(file) => {
            let mut bytes = vec![];
            file.take(OUTPUT_LIMIT as u64).read_to_end(&mut bytes)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}
