use crate::{
    cli::{Cmd, DaemonCmd},
    error,
    store::{absolute, Store},
    util::{atomic, hash, Lock},
    vfs::{Core, Fs},
    workspace::dispatch,
    Result,
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, IsTerminal, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn signal(_: i32) {
    STOP.store(true, Ordering::Release);
    crate::activity::SHUTDOWN.store(true, Ordering::Release);
}
pub fn request(root: &Path, cmd: &Cmd) -> Result<Value> {
    let mut s = UnixStream::connect(root.join("control.sock"))?;
    s.set_read_timeout(Some(Duration::from_secs(600)))?;
    s.write_all(&serde_json::to_vec(cmd)?)?;
    s.write_all(b"\n")?;
    let mut reader = BufReader::new(s);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(error("server disconnected before replying"));
        }
        let v: Value = serde_json::from_str(&line)?;
        if let Some(progress) = v.get("progress") {
            if std::io::stderr().is_terminal() {
                eprintln!(
                    "gtl [{}] {}: {} ({:.1}s)",
                    progress["id"],
                    progress["operation"].as_str().unwrap_or("operation"),
                    progress["stage"].as_str().unwrap_or("running"),
                    progress["elapsed_seconds"].as_f64().unwrap_or(0.0)
                );
            }
            continue;
        }
        if let Some(e) = v.get("error") {
            return Err(error(e.as_str().unwrap_or("service failure")));
        }
        return Ok(v["ok"].clone());
    }
}
pub fn run(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    let _lock = Lock::try_take(&root.join("daemon.lock"))?;
    crate::activity::init(&root)?;
    unsafe {
        libc::signal(libc::SIGTERM, signal as *const () as usize);
        libc::signal(libc::SIGINT, signal as *const () as usize);
    }
    let started = Instant::now();
    let startup = crate::activity::Task::new("Open workspace state");
    let startup_context = startup.enter();
    let startup_progress = StartupProgress::new();
    crate::activity::event("info", "Starting foreground filesystem server");
    let store = Store::open(&root)?;
    let mount = store.config.mount.clone();
    std::fs::create_dir_all(&mount)?;
    if std::fs::read_dir(&mount)?.next().is_some() {
        return Err(error("mount destination is not empty"));
    }
    let core = Core::new(Arc::new(Mutex::new(store)));
    crate::activity::stage("Attach FUSE mount");
    crate::activity::event("info", "Attach FUSE mount");
    let session = fuser::spawn_mount2(
        Fs::new(core.clone()),
        &mount,
        &[
            fuser::MountOption::FSName("gitalator".into()),
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::NoSuid,
            fuser::MountOption::NoDev,
        ],
    )?;
    *core.notifier.lock().unwrap() = Some(session.notifier());
    let socket = root.join("control.sock");
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    unsafe {
        libc::signal(libc::SIGTERM, signal as *const () as usize);
        libc::signal(libc::SIGINT, signal as *const () as usize);
    }
    let stopping = Arc::new(AtomicBool::new(false));
    let mut clients = vec![];
    let mut heartbeat = Instant::now();
    crate::activity::event(
        "info",
        &format!(
            "Ready in {:.2}s: mount {}; activity log {}",
            started.elapsed().as_secs_f64(),
            mount.display(),
            root.join("logs/activity.jsonl").display()
        ),
    );
    drop(startup_progress);
    drop(startup_context);
    drop(startup);
    while !stopping.load(Ordering::Acquire) && !STOP.load(Ordering::Acquire) {
        if heartbeat.elapsed() >= Duration::from_secs(5) {
            crate::activity::heartbeat();
            heartbeat = Instant::now();
        }
        match listener.accept() {
            Ok((s, _)) => {
                let core = core.clone();
                let stopping = stopping.clone();
                let mount = mount.clone();
                clients.push(std::thread::spawn(move || {
                    let r = serve(&s, core, &mount, &stopping);
                    let value = match r {
                        Ok(v) => json!({"ok":v}),
                        Err(e) => json!({"error":e.to_string()}),
                    };
                    let mut s = s;
                    let _ = writeln!(s, "{value}");
                }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(e.into()),
        }
        let mut i = 0;
        while i < clients.len() {
            if clients[i].is_finished() {
                let h = clients.swap_remove(i);
                let _ = h.join();
            } else {
                i += 1;
            }
        }
    }
    crate::activity::cancel_all();
    crate::activity::event("info", "Stopping: cancelling outstanding work");
    for client in clients {
        let _ = client.join();
    }
    if STOP.load(Ordering::Acquire) {
        let _ = Command::new("fusermount3").arg("-uz").arg(&mount).status();
    }
    drop(session);
    let _ = std::fs::remove_file(socket);
    crate::activity::event("info", "Filesystem server stopped");
    Ok(())
}
fn serve(stream: &UnixStream, core: Arc<Core>, mount: &Path, stop: &AtomicBool) -> Result<Value> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut _,
            &mut len,
        )
    } != 0
        || cred.uid != unsafe { libc::getuid() }
    {
        return Err(error("peer UID mismatch"));
    }
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream);
    let mut line = vec![];
    use std::io::Read;
    reader
        .by_ref()
        .take(1024 * 1024)
        .read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(error("invalid or oversized request"));
    }
    let cmd: Cmd = serde_json::from_slice(&line)?;
    match cmd {
        Cmd::Activity => Ok(crate::activity::snapshot()),
        Cmd::Cancel { id } => Ok(json!({"id":id,"cancelled":crate::activity::cancel(id)})),
        Cmd::Daemon {
            command: DaemonCmd::Status,
        } => Ok(
            json!({"running":true,"ready":true,"pid":std::process::id(),"mount":mount,"protocol":5}),
        ),
        Cmd::Daemon {
            command: DaemonCmd::Stop { lazy },
        } => {
            let out = Command::new("fusermount3")
                .arg(if lazy { "-uz" } else { "-u" })
                .arg(mount)
                .output()?;
            if !out.status.success() {
                return Err(error(format!(
                    "unmount failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                )));
            }
            stop.store(true, Ordering::Release);
            Ok(json!({"stopping":true}))
        }
        Cmd::Daemon { .. } => Err(error("lifecycle command must run in client")),
        cmd => {
            let task = Arc::new(crate::activity::Task::new(cmd.label()));
            let _context = task.enter();
            crate::activity::event("info", &format!("Begin {}", cmd.label()));
            let done = Arc::new(AtomicBool::new(false));
            let watch_done = done.clone();
            let watch_task = task.clone();
            let mut peer = stream.try_clone()?;
            peer.set_write_timeout(Some(Duration::from_secs(2)))?;
            writeln!(peer, "{}", json!({"progress":task.status()}))?;
            // A CLI killed by Ctrl-C closes its socket: cancel its server work.
            let watcher = std::thread::spawn(move || {
                let mut poll = libc::pollfd {
                    fd: peer.as_raw_fd(),
                    events: libc::POLLRDHUP,
                    revents: 0,
                };
                let mut progress = Instant::now();
                while !watch_done.load(Ordering::Acquire) {
                    if progress.elapsed() >= Duration::from_secs(1) {
                        if writeln!(peer, "{}", json!({"progress":watch_task.status()})).is_err() {
                            watch_task.cancel();
                            break;
                        }
                        progress = Instant::now();
                    }
                    let result = unsafe { libc::poll(&mut poll, 1, 50) };
                    if result > 0
                        && poll.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
                    {
                        watch_task.cancel();
                        break;
                    }
                }
            });
            let result = dispatch(&core, cmd);
            done.store(true, Ordering::Release);
            let _ = watcher.join();
            match &result {
                Ok(_) => crate::activity::event("info", "Completed"),
                Err(e) if crate::activity::interrupted(&**e) => {
                    crate::activity::event("info", "Cancelled")
                }
                Err(e) => crate::activity::event("error", &format!("Failed: {e}")),
            }
            result
        }
    }
}
pub fn unit_text(exe: &Path, root: &Path) -> String {
    fn quote(p: &Path) -> String {
        format!(
            "\"{}\"",
            p.to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$")
                .replace('\n', "\\n")
        )
    }
    format!("# Managed by gitalator\n[Unit]\nDescription=Gitalator depot workspaces\n[Service]\nType=simple\nUMask=0077\nWorkingDirectory=%h\nExecStart={} --state-dir {} daemon run\nExecStop={} --state-dir {} daemon stop --lazy\nRestart=on-failure\n[Install]\nWantedBy=default.target\n",quote(exe),quote(root),quote(exe),quote(root))
}
pub fn start(root: &Path, detached: bool) -> Result<Value> {
    if let Ok(v) = request(
        root,
        &Cmd::Daemon {
            command: DaemonCmd::Status,
        },
    ) {
        return Ok(v);
    }
    let root = absolute(root)?;
    std::fs::create_dir_all(&root)?;
    let _start = Lock::take(&root.join("start.lock"))?;
    {
        let _ownership = Lock::try_take(&root.join("daemon.lock"))?;
    }
    let exe = std::env::current_exe()?;
    let mut child = None;
    if detached {
        std::fs::create_dir_all(root.join("logs"))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("logs/daemon.log"))?;
        let mut cmd = Command::new(&exe);
        cmd.arg("--state-dir")
            .arg(&root)
            .args(["daemon", "run"])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .current_dir(&root);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        child = Some(cmd.spawn()?);
    } else {
        if !Command::new("systemctl")
            .args(["--user", "show", "--property=Version"])
            .output()?
            .status
            .success()
        {
            return Err(error(
                "no systemd user manager; explicitly use daemon start --detached",
            ));
        }
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".config"));
        let unit = format!(
            "gitalator-{}.service",
            &hash(root.as_os_str().as_encoded_bytes())[..16]
        );
        let path = config.join("systemd/user").join(&unit);
        if path.exists() && !std::fs::read_to_string(&path)?.starts_with("# Managed by gitalator\n")
        {
            return Err(error("refusing to overwrite unmanaged unit"));
        }
        atomic(&path, unit_text(&exe, &root).as_bytes())?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        for args in [
            vec!["--user", "daemon-reload"],
            vec!["--user", "start", &unit],
        ] {
            if !Command::new("systemctl").args(args).status()?.success() {
                return Err(error("systemctl failed"));
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(child) = child.as_mut() {
            if let Some(status) = child.try_wait()? {
                return Err(error(format!(
                    "daemon exited ({status}); inspect logs/daemon.log"
                )));
            }
        }
        if let Ok(v) = request(
            &root,
            &Cmd::Daemon {
                command: DaemonCmd::Status,
            },
        ) {
            return Ok(v);
        }
        if Instant::now() > deadline {
            return Err(error("daemon readiness timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// Wake immediately on success or failure; never leave a startup reporter behind.
struct StartupProgress {
    stop: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl StartupProgress {
    fn new() -> Self {
        let (stop, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            while matches!(
                receive.recv_timeout(Duration::from_secs(5)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                crate::activity::heartbeat();
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for StartupProgress {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
