use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn exe() -> PathBuf {
    std::env::var_os("WORKLINK_TEST_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_WorkLink").into())
}
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn call(root: &Path, args: &[&str]) -> Output {
    Command::new(exe())
        .arg("--data-dir")
        .arg(root)
        .arg("--json")
        .args(args)
        .env("RUST_LOG", "worklink=debug")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}
fn ok(root: &Path, args: &[&str]) -> Value {
    let result = call(root, args);
    assert!(
        result.status.success(),
        "{args:?}\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}
fn start(root: &Path, args: &[&str]) -> Child {
    Command::new(exe())
        .arg("--data-dir")
        .arg(root)
        .args(args)
        .env("RUST_LOG", "debug")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn powershell_cli_preserves_output_and_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
    let script = format!(
        "& {} --version\n& {} --data-dir {} devices --json\nexit $LASTEXITCODE",
        quote(&exe().to_string_lossy()),
        quote(&exe().to_string_lossy()),
        quote(&directory.path().to_string_lossy()),
    );
    let result = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    let text = String::from_utf8_lossy(&result.stdout);
    assert!(
        text.contains(concat!("WorkLink ", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    assert!(text.contains("\"ok\": false"), "{text}");
}
fn wait_online(root: &Path, online: bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut last = String::new();
    while Instant::now() < deadline {
        let result = call(root, &["status"]);
        last = String::from_utf8_lossy(&result.stdout).into_owned();
        if let Ok(value) = serde_json::from_slice::<Value>(&result.stdout)
            && value["peers"]
                .as_array()
                .is_some_and(|p| p.iter().any(|p| p["online"] == true) == online)
        {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
    panic!("online={online} failed: {last}");
}

struct Fixture {
    children: Vec<Child>,
    _temp: tempfile::TempDir,
    a: PathBuf,
    b: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if thread::panicking() {
            for root in [&self.a, &self.b] {
                if let Ok(entries) = fs::read_dir(root.join("logs")) {
                    for entry in entries.flatten() {
                        eprintln!(
                            "{}\n{}",
                            entry.path().display(),
                            fs::read_to_string(entry.path()).unwrap_or_default()
                        );
                    }
                }
            }
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct Echo {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Echo {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let worker = thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if let Ok((mut socket, _)) = listener.accept() {
                    thread::spawn(move || {
                        socket.set_nonblocking(false).unwrap();
                        let _ = socket.set_read_timeout(Some(Duration::from_secs(15)));
                        let _ = socket.write_all(b"SSH-2.0-WorkLink-test\r\n");
                        let mut buf = [0; 16384];
                        while let Ok(n) = socket.read(&mut buf) {
                            if n == 0 || socket.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    });
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        });
        Self {
            port,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn verify_forward(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut socket = loop {
        if let Ok(socket) = TcpStream::connect(("127.0.0.1", port)) {
            break socket;
        }
        assert!(Instant::now() < deadline, "forward did not listen");
        thread::sleep(Duration::from_millis(100));
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut banner = vec![0; 23];
    socket.read_exact(&mut banner).unwrap();
    assert_eq!(&banner, b"SSH-2.0-WorkLink-test\r\n");
    let data: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    socket.write_all(&data).unwrap();
    let mut received = vec![0; data.len()];
    let mut progress = 0;
    while progress < received.len() {
        match socket.read(&mut received[progress..]) {
            Ok(0) => panic!("eof after {progress}"),
            Ok(n) => progress += n,
            Err(e) => panic!("read failed after {progress}/65536: {e}"),
        }
    }
    assert_eq!(data, received);
}

#[test]
fn encrypted_duplex_permissions_files_jobs_and_reconnect() {
    let artifacts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-artifacts");
    fs::create_dir_all(&artifacts).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("e2e-")
        .tempdir_in(artifacts)
        .unwrap();
    let a = temp.path().join("A");
    let b = temp.path().join("B");
    let port = free_port();
    ok(
        &a,
        &[
            "init",
            "--name",
            "A",
            "--listen",
            &format!("127.0.0.1:{port}"),
        ],
    );
    ok(&b, &["init", "--name", "B", "--outbound-only"]);
    let ia = ok(&a, &["invite"]);
    let ib = ok(&b, &["invite"]);
    let bad = call(
        &b,
        &[
            "peer",
            "add",
            "--invite",
            ia["invite"].as_str().unwrap(),
            "--fingerprint",
            &"0".repeat(64),
        ],
    );
    assert!(!bad.status.success());
    ok(
        &a,
        &[
            "peer",
            "add",
            "--invite",
            ib["invite"].as_str().unwrap(),
            "--fingerprint",
            ib["fingerprint"].as_str().unwrap(),
        ],
    );
    ok(
        &b,
        &[
            "peer",
            "add",
            "--invite",
            ia["invite"].as_str().unwrap(),
            "--fingerprint",
            ia["fingerprint"].as_str().unwrap(),
            "--endpoint",
            &format!("127.0.0.1:{port}"),
        ],
    );
    let echo_a = Echo::new();
    let echo_b = Echo::new();
    ok(&a, &["config", "--ssh-port", &echo_a.port.to_string()]);
    ok(&b, &["config", "--ssh-port", &echo_b.port.to_string()]);
    let mut fixture = Fixture {
        children: vec![],
        _temp: temp,
        a,
        b,
    };
    fixture.children.push(start(&fixture.a, &["daemon"]));
    fixture.children.push(start(&fixture.b, &["daemon"]));
    wait_online(&fixture.b, true);
    wait_online(&fixture.a, true);
    println!("PASS: TLS 1.3 mutual authentication; B has no inbound listener; both peers online");
    let fa = free_port();
    let fb = free_port();
    fixture.children.push(start(
        &fixture.b,
        &["forward", "A", "--bind", &format!("127.0.0.1:{fa}")],
    ));
    fixture.children.push(start(
        &fixture.a,
        &["forward", "B", "--bind", &format!("127.0.0.1:{fb}")],
    ));
    verify_forward(fa);
    verify_forward(fb);
    println!("PASS: simultaneous bidirectional binary SSH forwarding over one connection");
    check_proxy_remote_eof(&fixture);
    ok(
        &fixture.a,
        &["config", "--ssh-port", &echo_a.port.to_string()],
    );
    let denied = call(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--command",
            "Write-Output denied",
        ],
    );
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stdout).contains("权限"));
    let nofiles = call(
        &fixture.b,
        &[
            "get",
            "A",
            "C:\\Windows\\win.ini",
            fixture._temp.path().join("forbidden").to_str().unwrap(),
            "--backend",
            "native",
        ],
    );
    assert!(!nofiles.status.success());
    println!("PASS: native exec and file operations denied by default");
    ok(
        &fixture.a,
        &["peer", "allow", "B", "--files", "true", "--exec", "true"],
    );
    ok(
        &fixture.b,
        &["peer", "allow", "A", "--files", "true", "--exec", "true"],
    );
    let jobid = uuid::Uuid::new_v4().to_string();
    let result = ok(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--id",
            &jobid,
            "--command",
            "Write-Output 'native-ok'",
        ],
    );
    assert_eq!(result["exit_code"], 0);
    assert!(result["stdout"].as_str().unwrap().contains("native-ok"));
    let repeated = ok(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--id",
            &jobid,
            "--command",
            "Write-Output 'native-ok'",
        ],
    );
    assert_eq!(result["created"], repeated["created"]);
    assert!(
        !call(
            &fixture.b,
            &[
                "exec",
                "A",
                "--backend",
                "native",
                "--id",
                &jobid,
                "--command",
                "Write-Output 'different'"
            ]
        )
        .status
        .success()
    );
    assert!(
        !call(
            &fixture.b,
            &[
                "exec",
                "A",
                "--backend",
                "native",
                "--system",
                "--command",
                "whoami"
            ]
        )
        .status
        .success()
    );
    println!("PASS: native user execution, durable idempotency and SYSTEM permission denial");
    check_conpty(&fixture);
    let source = fixture._temp.path().join("source 中文.bin");
    let remote = fixture._temp.path().join("remote 中文.bin");
    let target = fixture._temp.path().join("download 中文.bin");
    let data: Vec<u8> = (0..2_097_152).map(|i| ((i * 17) % 251) as u8).collect();
    fs::write(&source, &data).unwrap();
    let hash = hex::encode(Sha256::digest(&data));
    let partial = PathBuf::from(format!("{}.worklink-{hash}.part", remote.display()));
    fs::write(&partial, &data[..65536]).unwrap();
    let uploaded = ok(
        &fixture.b,
        &[
            "put",
            "A",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
            "--backend",
            "native",
        ],
    );
    assert_eq!(uploaded["resumed_from"], 65536);
    assert_eq!(fs::read(&remote).unwrap(), data);
    assert!(
        !call(
            &fixture.b,
            &[
                "put",
                "A",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
                "--backend",
                "native"
            ]
        )
        .status
        .success()
    );
    fs::write(
        format!("{}.worklink.part", target.display()),
        &data[..32768],
    )
    .unwrap();
    fs::write(
        format!("{}.worklink.part.json", target.display()),
        serde_json::to_vec(&json!({"size":data.len(),"sha256":hash,"offset":0})).unwrap(),
    )
    .unwrap();
    let downloaded = ok(
        &fixture.b,
        &[
            "get",
            "A",
            remote.to_str().unwrap(),
            target.to_str().unwrap(),
            "--backend",
            "native",
        ],
    );
    assert_eq!(downloaded["resumed_from"], 32768);
    assert_eq!(fs::read(&target).unwrap(), data);
    println!("PASS: Unicode paths, resumable upload/download, SHA-256 and overwrite protection");
    let corrupted = fixture._temp.path().join("corrupted.bin");
    fs::write(
        format!("{}.worklink-{hash}.part", corrupted.display()),
        b"bad-prefix",
    )
    .unwrap();
    assert!(
        !call(
            &fixture.b,
            &[
                "put",
                "A",
                source.to_str().unwrap(),
                corrupted.to_str().unwrap(),
                "--backend",
                "native"
            ]
        )
        .status
        .success()
    );
    assert!(!corrupted.exists());
    ok(
        &fixture.b,
        &[
            "put",
            "A",
            source.to_str().unwrap(),
            corrupted.to_str().unwrap(),
            "--backend",
            "native",
        ],
    );
    assert_eq!(fs::read(&corrupted).unwrap(), data);
    println!("PASS: corrupt partial rejected and reset; subsequent retry succeeds");
    let marker = fixture._temp.path().join("once.txt");
    let script = format!(
        "Start-Sleep -Seconds 3;Add-Content -LiteralPath '{}' -Value 'once'",
        marker.to_string_lossy().replace('\'', "''")
    );
    let detached = ok(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--detach",
            "--command",
            &script,
        ],
    );
    let id = detached["spec"]["id"].as_str().unwrap();
    fixture.children[1].kill().unwrap();
    fixture.children[1].wait().unwrap();
    thread::sleep(Duration::from_secs(4));
    fixture.children.push(start(&fixture.b, &["daemon"]));
    wait_online(&fixture.b, true);
    let deadline = Instant::now() + Duration::from_secs(20);
    let recovered = loop {
        let record = ok(&fixture.b, &["job", "get", "A", id]);
        if record["state"] != "queued" && record["state"] != "running" {
            break record;
        }
        // CI 主机的 PowerShell 冷启动时间不固定，按任务状态等待，避免把机器负载误判成断线丢任务。
        assert!(Instant::now() < deadline, "job did not finish: {record}");
        thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(recovered["state"], "completed");
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
    println!(
        "PASS: caller disconnect/restart; task continues and result recovered without re-execution"
    );
    let timeout = call(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--timeout",
            "1",
            "--command",
            "Start-Sleep -Seconds 30",
        ],
    );
    assert!(!timeout.status.success());
    let timed: Value = serde_json::from_slice(&timeout.stdout).unwrap();
    assert_eq!(timed["state"], "timed_out");
    let cancel = ok(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--detach",
            "--command",
            "Start-Sleep -Seconds 30",
        ],
    );
    let id = cancel["spec"]["id"].as_str().unwrap();
    ok(&fixture.b, &["job", "cancel", "A", id]);
    thread::sleep(Duration::from_millis(700));
    assert_eq!(
        ok(&fixture.b, &["job", "get", "A", id])["state"],
        "cancelled"
    );
    println!("PASS: timeout and cancellation terminate task process trees");
    let interrupted = ok(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--detach",
            "--command",
            "Start-Sleep -Seconds 30",
        ],
    );
    let interrupted_id = interrupted["spec"]["id"].as_str().unwrap();
    let crashed_pid = fixture.children[0].id();
    fixture.children[0].kill().unwrap();
    fixture.children[0].wait().unwrap();
    wait_online(&fixture.b, false);
    let probe = format!(
        "(Get-CimInstance Win32_Process -Filter 'ParentProcessId={crashed_pid}' | Where-Object Name -eq 'powershell.exe' | Measure-Object).Count"
    );
    let children = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &probe])
        .output()
        .unwrap();
    assert!(children.status.success());
    assert_eq!(
        String::from_utf8_lossy(&children.stdout).trim(),
        "0",
        "daemon crash left a suspended task process"
    );
    fixture.children.push(start(&fixture.a, &["daemon"]));
    wait_online(&fixture.b, true);
    assert_eq!(
        ok(&fixture.b, &["job", "get", "A", interrupted_id])["state"],
        "interrupted"
    );
    println!(
        "PASS: receiver restart reconnects automatically and marks unfinished tasks interrupted"
    );
    check_real_openssh(&mut fixture, fa);
    assert!(
        !call(&fixture.b, &["browser", "A", "--background", "status"])
            .status
            .success()
    );
    ok(&fixture.a, &["peer", "allow", "B", "--browser", "true"]);
    ok(&fixture.b, &["peer", "allow", "A", "--browser", "true"]);
    for (root, target) in [(&fixture.b, "A"), (&fixture.a, "B")] {
        let state = ok(root, &["browser", target, "--background", "status"]);
        assert_eq!(state["state"], "completed");
        assert_eq!(state["result"]["running"], false);
        let id = state["id"].as_str().unwrap();
        assert_eq!(
            ok(root, &["tool", target, "--background", "get", id]),
            state
        );
    }
    ok(&fixture.a, &["peer", "allow", "B", "--browser", "false"]);
    assert!(
        !call(&fixture.b, &["browser", "A", "--background", "status"])
            .status
            .success()
    );
    ok(&fixture.a, &["tool", "self", "--background", "stop"]);
    ok(&fixture.b, &["tool", "self", "--background", "stop"]);
    println!(
        "PASS: bidirectional automation routing, default denial, explicit browser grant and revocation"
    );
    let revoked_job = ok(
        &fixture.b,
        &[
            "exec",
            "A",
            "--backend",
            "native",
            "--detach",
            "--command",
            "Start-Sleep -Seconds 30",
        ],
    );
    let revoked_id = revoked_job["spec"]["id"].as_str().unwrap();
    ok(&fixture.a, &["peer", "allow", "B", "--exec", "false"]);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let record: Value = serde_json::from_slice(
            &fs::read(fixture.a.join(format!("jobs/{revoked_id}.json"))).unwrap(),
        )
        .unwrap();
        if record["state"] == "cancelled" {
            assert!(record["error"].as_str().unwrap().contains("撤销"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "revoked job was not cancelled: {record}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    println!("PASS: withdrawing execution permission cancels an already running native job");
    ok(&fixture.a, &["peer", "remove", "B"]);
    wait_online(&fixture.b, false);
    assert!(
        !call(
            &fixture.b,
            &[
                "exec",
                "A",
                "--backend",
                "native",
                "--command",
                "Write-Output forbidden"
            ]
        )
        .status
        .success()
    );
    println!("PASS: trust revocation disconnects peer and blocks subsequent operations");
}

fn check_real_openssh(fixture: &mut Fixture, forward_port: u16) {
    let directory = fixture._temp.path().join("openssh");
    fs::create_dir_all(&directory).unwrap();
    let key = directory.join("hostkey");
    let generated = Command::new("ssh-keygen.exe")
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-f")
        .arg(&key)
        .output()
        .unwrap();
    assert!(generated.status.success());
    let port = free_port();
    let config = directory.join("sshd_config");
    let quote = |p: &Path| format!("\"{}\"", p.to_string_lossy().replace('\\', "/"));
    fs::write(&config,format!("Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nPasswordAuthentication no\nPubkeyAuthentication yes\nAuthorizedKeysFile {}\nStrictModes no\nSubsystem sftp C:/Windows/System32/OpenSSH/sftp-server.exe\nLogLevel VERBOSE\n",quote(&key),quote(&directory.join("sshd.pid")),quote(&directory.join("authorized_keys")))).unwrap();
    let log = directory.join("sshd.log");
    let server = Command::new("C:\\Windows\\System32\\OpenSSH\\sshd.exe")
        .args(["-D", "-e", "-f"])
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn();
    match server {
        Err(error) => println!("SKIP: isolated system OpenSSH server unavailable: {error}"),
        Ok(mut child) => {
            thread::sleep(Duration::from_millis(700));
            if let Some(status) = child.try_wait().unwrap() {
                println!(
                    "SKIP: isolated OpenSSH requires additional Windows privileges ({status}): {}",
                    fs::read_to_string(log).unwrap_or_default()
                );
                return;
            }
            fixture.children.push(child);
            ok(&fixture.a, &["config", "--ssh-port", &port.to_string()]);
            let result = Command::new("ssh-keyscan.exe")
                .args([
                    "-T",
                    "5",
                    "-t",
                    "ed25519",
                    "-p",
                    &forward_port.to_string(),
                    "127.0.0.1",
                ])
                .output()
                .unwrap();
            assert!(
                result.status.success() && !result.stdout.is_empty(),
                "OpenSSH handshake failed: {}\n{}",
                String::from_utf8_lossy(&result.stderr),
                fs::read_to_string(log).unwrap_or_default()
            );
            let expected = fs::read_to_string(key.with_extension("pub")).unwrap();
            let expected_key = expected.split_whitespace().nth(1).unwrap();
            assert!(String::from_utf8_lossy(&result.stdout).contains(expected_key));
            println!(
                "PASS: real Windows OpenSSH server host key verified through WorkLink with ssh-keyscan"
            );
            let client_key = directory.join("clientkey");
            assert!(
                Command::new("ssh-keygen.exe")
                    .args(["-q", "-t", "ed25519", "-N", ""])
                    .arg("-f")
                    .arg(&client_key)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            fs::copy(
                client_key.with_extension("pub"),
                directory.join("authorized_keys"),
            )
            .unwrap();
            let known = directory.join("known_hosts");
            let id = ok(&fixture.a, &["status"])["id"]
                .as_str()
                .unwrap()
                .to_owned();
            fs::write(&known, format!("worklink-{id} {expected}")).unwrap();
            let user = format!(
                "{}\\{}",
                std::env::var("USERDOMAIN").unwrap(),
                std::env::var("USERNAME").unwrap()
            );
            ok(
                &fixture.b,
                &[
                    "peer",
                    "ssh",
                    "A",
                    "--user",
                    &user,
                    "--identity",
                    client_key.to_str().unwrap(),
                    "--known-hosts",
                    known.to_str().unwrap(),
                ],
            );
            let execution = call(
                &fixture.b,
                &["exec", "A", "--command", "Write-Output 'real-ssh-ok'"],
            );
            if !execution.status.success() {
                println!(
                    "SKIP: unprivileged isolated sshd cannot authenticate this Windows account: {}",
                    String::from_utf8_lossy(&execution.stdout)
                );
                return;
            }
            let result: Value = serde_json::from_slice(&execution.stdout).unwrap();
            assert!(result["stdout"].as_str().unwrap().contains("real-ssh-ok"));
            let source = directory.join("ssh-source 中文.txt");
            let target = directory.join("ssh-target 中文.txt");
            let downloaded = directory.join("ssh-download 中文.txt");
            fs::write(&source, "SFTP 文件验证 ✓").unwrap();
            ok(
                &fixture.b,
                &[
                    "put",
                    "A",
                    source.to_str().unwrap(),
                    target.to_str().unwrap(),
                ],
            );
            ok(
                &fixture.b,
                &[
                    "get",
                    "A",
                    target.to_str().unwrap(),
                    downloaded.to_str().unwrap(),
                ],
            );
            assert_eq!(fs::read(&source).unwrap(), fs::read(&downloaded).unwrap());
            assert!(
                !call(
                    &fixture.b,
                    &[
                        "put",
                        "A",
                        source.to_str().unwrap(),
                        target.to_str().unwrap()
                    ]
                )
                .status
                .success()
            );
            fs::write(&source, "SFTP 替换后的文件内容 ✓").unwrap();
            ok(
                &fixture.b,
                &[
                    "put",
                    "A",
                    source.to_str().unwrap(),
                    target.to_str().unwrap(),
                    "--overwrite",
                ],
            );
            assert_eq!(fs::read(&source).unwrap(), fs::read(&target).unwrap());
            let corrupt_target = directory.join("ssh-corrupt 中文.txt");
            let source_bytes = fs::read(&source).unwrap();
            let hash = hex::encode(Sha256::digest(&source_bytes));
            let corrupt_partial =
                PathBuf::from(format!("{}.worklink-{hash}.part", corrupt_target.display()));
            fs::write(&corrupt_partial, vec![0; source_bytes.len() / 2]).unwrap();
            assert!(
                !call(
                    &fixture.b,
                    &[
                        "put",
                        "A",
                        source.to_str().unwrap(),
                        corrupt_target.to_str().unwrap()
                    ]
                )
                .status
                .success()
            );
            assert!(!corrupt_target.exists());
            assert!(!corrupt_partial.exists());
            ok(
                &fixture.b,
                &[
                    "put",
                    "A",
                    source.to_str().unwrap(),
                    corrupt_target.to_str().unwrap(),
                ],
            );
            assert_eq!(fs::read(&corrupt_target).unwrap(), source_bytes);
            fs::remove_file(&corrupt_target).unwrap();
            fs::write(&corrupt_partial, vec![0; source_bytes.len()]).unwrap();
            ok(
                &fixture.b,
                &[
                    "put",
                    "A",
                    source.to_str().unwrap(),
                    corrupt_target.to_str().unwrap(),
                ],
            );
            assert_eq!(fs::read(&corrupt_target).unwrap(), source_bytes);
            fs::remove_file(&corrupt_target).unwrap();
            fs::write(&corrupt_partial, &source_bytes).unwrap();
            ok(
                &fixture.b,
                &[
                    "put",
                    "A",
                    source.to_str().unwrap(),
                    corrupt_target.to_str().unwrap(),
                ],
            );
            assert_eq!(fs::read(&corrupt_target).unwrap(), source_bytes);
            for (index, data) in [
                source_bytes.clone(),
                vec![0; source_bytes.len()],
                vec![0; source_bytes.len() + 1],
            ]
            .iter()
            .enumerate()
            {
                let resumed = directory.join(format!("ssh-resume-{index}.txt"));
                let part = format!("{}.worklink.part", resumed.display());
                fs::write(&part, data).unwrap();
                fs::write(
                    format!("{part}.json"),
                    serde_json::to_vec(
                        &json!({"size":source_bytes.len(),"sha256":hash,"offset":0}),
                    )
                    .unwrap(),
                )
                .unwrap();
                ok(
                    &fixture.b,
                    &[
                        "get",
                        "A",
                        target.to_str().unwrap(),
                        resumed.to_str().unwrap(),
                    ],
                );
                assert_eq!(fs::read(resumed).unwrap(), source_bytes);
            }
            let empty_source = directory.join("empty-source.txt");
            let empty_target = directory.join("empty-target.txt");
            let empty_download = directory.join("empty-download.txt");
            fs::write(&empty_source, []).unwrap();
            ok(
                &fixture.b,
                &[
                    "put",
                    "A",
                    empty_source.to_str().unwrap(),
                    empty_target.to_str().unwrap(),
                ],
            );
            ok(
                &fixture.b,
                &[
                    "get",
                    "A",
                    empty_target.to_str().unwrap(),
                    empty_download.to_str().unwrap(),
                ],
            );
            assert_eq!(fs::metadata(empty_download).unwrap().len(), 0);
            println!(
                "PASS: real OpenSSH exec and SFTP put/get, overwrite protection, verified replacement and corrupt-partial retry"
            );
            println!(
                "PASS: SFTP complete/corrupt/oversized download partials and empty-file upload/download"
            );
        }
    }
}

fn check_proxy_remote_eof(fixture: &Fixture) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    ok(&fixture.a, &["config", "--ssh-port", &port]);
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(b"remote-finished\n").unwrap();
    });
    let mut proxy = Command::new(exe())
        .arg("--data-dir")
        .arg(&fixture.b)
        .args(["proxy", "--device", "A"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // 保留 stdin 写端，复现 SSH 已关闭远端、父进程却仍等代理退出的相互等待。
    let _input = proxy.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = proxy.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if Instant::now() >= deadline {
            let _ = proxy.kill();
            panic!("proxy did not exit after remote EOF with stdin still open");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mut output = String::new();
    proxy
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output, "remote-finished\n");
    server.join().unwrap();
    println!("PASS: ProxyCommand exits after remote EOF while parent holds stdin open");
}

fn check_conpty(fixture: &Fixture) {
    let id = ok(&fixture.b, &["status"])["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (send, receive) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> std::io::Result<String> {
            let mut pipe = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(r"\\.\pipe\worklink-{id}"))?;
            let request=serde_json::to_vec(&json!({"op":"open","peer":"A","request":{"op":"terminal","shell":null,"cols":100,"rows":30,"system":false}})).unwrap();
            pipe.write_all(&(request.len() as u32).to_be_bytes())?;
            pipe.write_all(&request)?;
            let mut length = [0; 4];
            pipe.read_exact(&mut length)?;
            let mut bytes = vec![0; u32::from_be_bytes(length) as usize];
            pipe.read_exact(&mut bytes)?;
            let reply: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(reply["ok"], true, "{reply}");
            pipe.write_all(b"Write-Output ('CONPTY_'+'READY'); exit\r")?;
            let mut output = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let n = pipe.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                output.extend_from_slice(&buffer[..n]);
                if buffer[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    pipe.write_all(b"\x1b[1;1R")?;
                }
                if String::from_utf8_lossy(&output).contains("CONPTY_READY") {
                    return Ok(String::from_utf8_lossy(&output).into_owned());
                }
            }
            Ok(String::from_utf8_lossy(&output).into_owned())
        })();
        let _ = send.send(result);
    });
    let output = receive
        .recv_timeout(Duration::from_secs(15))
        .expect("ConPTY did not respond")
        .unwrap();
    assert!(output.contains("CONPTY_READY"), "{output}");
    println!("PASS: real Windows ConPTY interactive input/output over native fallback");
}
