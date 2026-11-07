use crate::{
    cache::{Lock, Span},
    cli::{Cmd, DaemonCmd, DirTarget},
    config::Config,
    db::Store,
    error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
pub const PROTOCOL: u32 = 3;
static TERMINATE: AtomicBool = AtomicBool::new(false);
extern "C" fn signal(_: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Info {
    pub protocol: u32,
    pub pid: u32,
    pub uid: u32,
    pub executable: PathBuf,
    pub version: String,
    pub startup_mode: String,
    pub state_dir: PathBuf,
    pub mount: PathBuf,
    pub configured_mount: PathBuf,
}
pub fn status(s: &Store) -> Result<Value> {
    let info = s
        .runtime
        .daemon
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| error("not a daemon"))?;
    let mut value = serde_json::to_value(info)?;
    value["running"] = json!(true);
    value["ready"] = json!(s.runtime.ready.load(Ordering::Acquire));
    value["stopping"] = json!(s.runtime.draining.load(Ordering::Acquire));
    value["logs"] = json!({"operations":s.root.join("logs/operations.jsonl"),"daemon":s.root.join("logs/daemon.log")});
    Ok(value)
}
pub fn handle(s: &Store, command: DaemonCmd) -> Result<Value> {
    match command {
        DaemonCmd::Status => status(s),
        DaemonCmd::Reload => {
            let _lock = Lock::take(&s.root.join("config.lock"))?;
            let config = Config::load(&s.root)?;
            config.apply(s)?;
            crate::logging::message("gtl: configuration reloaded");
            Ok(json!({"reloaded":true,"config":config}))
        }
        DaemonCmd::Stop { lazy } => {
            if s.runtime.draining.swap(true, Ordering::AcqRel) {
                return Ok(json!({"stopping":true}));
            }
            while s.runtime.commands.load(Ordering::Acquire) > 0 {
                std::thread::sleep(Duration::from_millis(25));
            }
            let mount = s
                .runtime
                .daemon
                .lock()
                .unwrap()
                .as_ref()
                .ok_or_else(|| error("not a daemon"))?
                .mount
                .clone();
            let out = Command::new("fusermount3")
                .arg(if lazy { "-uz" } else { "-u" })
                .arg(&mount)
                .output();
            let out = match out {
                Ok(v) => v,
                Err(e) => {
                    s.runtime.draining.store(false, Ordering::Release);
                    return Err(e.into());
                }
            };
            if !out.status.success() {
                s.runtime.draining.store(false, Ordering::Release);
                return Err(error(format!(
                    "unmount failed: {}; leave the tree or explicitly use daemon stop --lazy",
                    String::from_utf8_lossy(&out.stderr)
                )));
            }
            s.runtime.ready.store(false, Ordering::Release);
            Ok(json!({"stopping":true}))
        }
        _ => Err(error("daemon lifecycle command must run locally")),
    }
}
pub fn directory(
    s: &Store,
    target: DirTarget,
    workspace: Option<String>,
    cwd: &Path,
) -> Result<Value> {
    let mount = mount_path(s)?;
    let (name, root) = if let Some(name) = workspace {
        let root = crate::namespace::workspace_path(&mount, &name);
        (name, root)
    } else {
        crate::namespace::context(&mount, cwd)?
    };
    s.head(&name)?;
    let path = match target {
        DirTarget::Top => crate::namespace::workspace_path(&mount, &name),
        DirTarget::Parent => {
            let cwd = std::fs::canonicalize(cwd)?;
            if !cwd.starts_with(&root) {
                return Err(error(
                    "parent requires a current directory inside the selected workspace",
                ));
            }
            if cwd == root {
                root
            } else {
                cwd.parent().unwrap().to_path_buf()
            }
        }
    };
    Ok(json!({"workspace":name,"path":path}))
}
fn probe(root: &Path) -> Result<Option<Value>> {
    crate::cli::request(
        root,
        &Cmd::Daemon {
            command: DaemonCmd::Status,
        },
    )
}
pub fn start(root: &Path, detached: bool) -> Result<Value> {
    Store::check_schema(root)?;
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    let _start = Lock::take(&root.join("start.lock"))?;
    // A previous daemon may still be draining. Never race its ownership lock.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match probe(&root) {
            Ok(Some(info)) if info["ready"] == true && info["stopping"] != true => return Ok(info),
            Err(e) if e.to_string().contains("protocol") => return Err(e),
            _ => {}
        }
        if let Ok(lock) = Lock::try_take(&root.join("daemon.lock")) {
            drop(lock);
            break;
        }
        if Instant::now() > deadline {
            return Err(error("daemon owns this store but is not ready (possibly still shutting down); inspect daemon status and logs"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    if detached {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(&executable);
        cmd.arg("--state-dir")
            .arg(&root)
            .args(["daemon", "run", "--mode", "detached"])
            .current_dir(
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or(root.clone()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        return wait_ready(&root, Some(&mut child));
    }
    // Probe the user manager without reading or importing its environment.
    let manager = Command::new("systemctl")
        .args(["--user", "show", "--property=Version"])
        .output();
    if !manager.is_ok_and(|o| o.status.success()) {
        return Err(error("systemd user manager unavailable; explicitly use: gtl daemon start --detached (plus your --state-dir, if any)"));
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(
            crate::ergonomics::default_root()
                .parent()
                .unwrap()
                .join(".config"),
        );
    let unit_dir = config_home.join("systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit = unit_name(&root);
    let body = unit_text(&executable, &root);
    let path = unit_dir.join(&unit);
    if path.exists() && !std::fs::read_to_string(&path)?.starts_with("# Managed by gitalator\n") {
        return Err(error("refusing to replace a user-authored systemd unit"));
    }
    crate::cache::atomic(&path, body.as_bytes())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["start", &unit])?;
    wait_ready(&root, None)
}
fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(error(format!(
            "systemd: {}",
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}
fn wait_ready(root: &Path, mut child: Option<&mut std::process::Child>) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(child) = child.as_mut() {
            if let Some(status) = child.try_wait()? {
                return Err(error(format!(
                    "daemon exited during startup ({status}); inspect {}",
                    root.join("logs/daemon.log").display()
                )));
            }
        }
        if let Ok(Some(info)) = probe(root) {
            if info["ready"] == true {
                return Ok(info);
            }
        }
        if Instant::now() > deadline {
            return Err(error(format!(
                "daemon readiness timed out; inspect {} and daemon status",
                root.join("logs/daemon.log").display()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn unit_arg(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}
pub fn unit_name(root: &Path) -> String {
    format!(
        "gitalator-{}.service",
        &crate::cache::hash(root.as_os_str().as_encoded_bytes())[..16]
    )
}
pub fn unit_text(exe: &Path, root: &Path) -> String {
    format!("# Managed by gitalator\n[Unit]\nDescription=Gitalator user workspace service\n\n[Service]\nType=simple\nUMask=0077\nWorkingDirectory=%h\nExecStart={} --state-dir {} daemon run --mode systemd\nExecStop={} --state-dir {} daemon stop --lazy\nRestart=on-failure\nRestartSec=2\nTimeoutStopSec=360\n\n[Install]\nWantedBy=default.target\n",unit_arg(exe),unit_arg(root),unit_arg(exe),unit_arg(root))
}
pub fn run(root: &Path, mount_override: Option<&Path>, mode: &str, recover: bool) -> Result<()> {
    Store::check_schema(root)?;
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    let _ownership = Lock::try_take(&root.join("daemon.lock"))?;
    let mut store = Store::open(&root)?;
    crate::logging::initialize(&root)?;
    let config = Config::load(&root)?;
    let configured_mount = config.mount_path(&root);
    let mount = mount_override
        .map(Path::to_path_buf)
        .unwrap_or(configured_mount.clone());
    if recover {
        recover_mount(&mount)?;
    }
    config.apply(&store)?;
    store.progress = Arc::new(|line| crate::logging::message(&format!("gtl: {line}")));
    store
        .runtime
        .activity
        .enabled
        .store(true, Ordering::Release);
    *store.runtime.daemon.lock().unwrap() = Some(Info {
        protocol: PROTOCOL,
        pid: std::process::id(),
        uid: unsafe { libc::geteuid() },
        executable: std::env::current_exe()?,
        version: env!("CARGO_PKG_VERSION").into(),
        startup_mode: mode.into(),
        state_dir: root.clone(),
        mount: mount.clone(),
        configured_mount,
    });
    let lifecycle = Span::new(&root, "daemon");
    crate::logging::message(&format!(
        "gtl: starting {mode} daemon pid {}; mount {}; logs {}",
        std::process::id(),
        mount.display(),
        root.join("logs").display()
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let server = crate::cli::server(store.clone(), stop.clone())?;
    TERMINATE.store(false, Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGTERM, signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, signal as *const () as libc::sighandler_t);
    }
    let report_stop = stop.clone();
    let runtime = store.runtime.clone();
    let signal_mount = mount.clone();
    let reporter = std::thread::spawn(move || {
        while !report_stop.load(Ordering::Relaxed) {
            runtime.activity.report();
            if TERMINATE.swap(false, Ordering::Relaxed) {
                runtime.draining.store(true, Ordering::Release);
                let _ = Command::new("fusermount3")
                    .arg("-uz")
                    .arg(&signal_mount)
                    .status();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let result = crate::vfs::mount(store, &mount);
    stop.store(true, Ordering::Release);
    let _ = server.join();
    let _ = reporter.join();
    lifecycle.event(
        if result.is_ok() { "stopped" } else { "failed" },
        result.as_ref().err().map(ToString::to_string).as_deref(),
    );
    if result.is_ok() {
        crate::logging::message("gtl: daemon stopped");
    }
    result
}
fn recover_mount(path: &Path) -> Result<()> {
    let disconnected = std::fs::metadata(path)
        .err()
        .is_some_and(|e| e.raw_os_error() == Some(libc::ENOTCONN))
        || std::fs::read_dir(path)
            .err()
            .is_some_and(|e| e.raw_os_error() == Some(libc::ENOTCONN));
    if !disconnected {
        return Ok(());
    }
    let canonical = path.to_string_lossy();
    let info = std::fs::read_to_string("/proc/self/mountinfo")?;
    let verified = info.lines().any(|l| {
        let f: Vec<_> = l.split_whitespace().collect();
        f.get(4).is_some_and(|p| decode_mount(p) == canonical) && l.contains(" - fuse gitalator ")
    });
    if !verified {
        return Err(error(
            "disconnected mount is not a verified gitalator mount; detach it explicitly",
        ));
    }
    let out = Command::new("fusermount3").arg("-uz").arg(path).output()?;
    if !out.status.success() {
        return Err(error("could not detach disconnected gitalator mount"));
    }
    Ok(())
}
fn decode_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

pub fn mount_path(s: &Store) -> Result<PathBuf> {
    if let Some(info) = s.runtime.daemon.lock().unwrap().as_ref() {
        return Ok(info.mount.clone());
    }
    Ok(Config::load(&s.root)?.mount_path(&s.root))
}
