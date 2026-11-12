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
    io::{BufRead, BufReader, Write},
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
}
pub fn request(root: &Path, cmd: &Cmd) -> Result<Value> {
    let mut s = UnixStream::connect(root.join("control.sock"))?;
    s.set_read_timeout(Some(Duration::from_secs(600)))?;
    s.write_all(&serde_json::to_vec(cmd)?)?;
    s.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    let v: Value = serde_json::from_str(&line)?;
    if let Some(e) = v.get("error") {
        return Err(error(e.as_str().unwrap_or("service failure")));
    }
    Ok(v["ok"].clone())
}
pub fn run(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    let _lock = Lock::try_take(&root.join("daemon.lock"))?;
    let store = Store::open(&root)?;
    let mount = store.config.mount.clone();
    std::fs::create_dir_all(&mount)?;
    if std::fs::read_dir(&mount)?.next().is_some() {
        return Err(error("mount destination is not empty"));
    }
    let core = Core::new(Arc::new(Mutex::new(store)));
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
    while !stopping.load(Ordering::Acquire) && !STOP.load(Ordering::Acquire) {
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
    for client in clients {
        let _ = client.join();
    }
    if STOP.load(Ordering::Acquire) {
        let _ = Command::new("fusermount3").arg("-uz").arg(&mount).status();
    }
    drop(session);
    let _ = std::fs::remove_file(socket);
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
        Cmd::Daemon {
            command: DaemonCmd::Status,
        } => Ok(
            json!({"running":true,"ready":true,"pid":std::process::id(),"mount":mount,"protocol":4}),
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
        cmd => dispatch(&core, cmd),
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
        Store::open(&root)?;
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
