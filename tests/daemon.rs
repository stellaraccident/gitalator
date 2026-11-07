mod common;
use common::*;
use gitalator::{
    cli::{self, Cmd, DaemonCmd},
    config::Config,
};
use std::{
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};
fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}
fn ok(root: &Path, args: &[&str]) -> serde_json::Value {
    let out = invoke(root, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn explicit_start_required_and_offline_administration_works() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let out = invoke(&root, &["init"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("daemon is not running"));
    assert!(!root.exists());
    assert_eq!(ok(&root, &["daemon", "status"])["running"], false);
    assert!(!root.exists());
    assert_eq!(ok(&root, &["--offline", "init"])["schema"], 3);
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .env("HOME", &home)
        .args(["--offline", "init"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(home.join(".gitalator/state.sqlite3").exists());
}
#[test]
fn configuration_validates_before_applying() {
    let f = Fixture::new();
    Config::update(&f.store, |c| {
        c.snapshot_idle_seconds = 7;
        c.snapshot_max_seconds = 20;
    })
    .unwrap();
    let old = std::fs::read(f.store.root.join("config.toml")).unwrap();
    assert!(Config::update(&f.store, |c| c.snapshot_max_seconds = 1).is_err());
    assert_eq!(
        std::fs::read(f.store.root.join("config.toml")).unwrap(),
        old
    );
    assert_eq!(f.store.setting("snapshot_idle_seconds", 0).unwrap(), 7);
    std::fs::write(f.store.root.join("config.toml"), "unknown_field=1").unwrap();
    assert!(Config::load(&f.store.root).is_err());
    assert_eq!(f.store.setting("snapshot_idle_seconds", 0).unwrap(), 7);
}
#[test]
fn unit_has_explicit_executable_root_and_no_environment_import() {
    let unit = gitalator::daemon::unit_text(Path::new("/a b/gtl"), Path::new("/home/user/a%$b"));
    assert!(unit.contains(
        "ExecStart=\"/a b/gtl\" --state-dir \"/home/user/a%%$$b\" daemon run --mode systemd"
    ));
    assert!(unit.contains("ExecStop="));
    assert!(unit.contains("daemon stop --lazy"));
    assert!(!unit.contains("Environment"));
}
#[test]
fn manager_unavailable_does_not_silently_detach() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(tmp.path().join("state"))
        .env("PATH", tmp.path())
        .args(["daemon", "start"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--detached"));
    assert!(!tmp.path().join("state/control.sock").exists());
}
struct Running {
    root: std::path::PathBuf,
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = invoke(&self.root, &["daemon", "stop", "--lazy"]);
    }
}
fn wait_stopped(root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(lock) = gitalator::cache::Lock::try_take(&root.join("daemon.lock")) {
            drop(lock);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not release ownership"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
#[test]
#[ignore = "requires local /dev/fuse mounts"]
fn detached_lifecycle_workspace_navigation_reload_and_recovery() {
    let f = Fixture::new();
    let root = &f.store.root;
    let first = ok(root, &["--json", "daemon", "start", "--detached"]);
    let _running = Running { root: root.clone() };
    assert_eq!(first["ready"], true);
    assert_eq!(first["startup_mode"], "detached");
    assert_eq!(first["uid"], unsafe { libc::geteuid() });
    let again = ok(root, &["daemon", "start", "--detached", "--json"]);
    assert_eq!(first["pid"], again["pid"]);
    let offline = invoke(root, &["--offline", "workspace", "list"]);
    assert!(!offline.status.success());
    let out = invoke(root, &["new-workspace", "fresh", "--like", "a"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = std::path::PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    assert_eq!(std::fs::read(path.join("file")).unwrap(), b"base\n");
    assert_eq!(ok(root, &["status", "fresh"])["draft_differs"], false);
    std::fs::write(path.join("file"), "edits").unwrap();
    ok(
        root,
        &["--json", "new-workspace", "copy", "--clone", "fresh"],
    );
    assert_eq!(
        std::fs::read(root.join("mount/workspaces/copy/file")).unwrap(),
        b"edits"
    );
    let link = f.temp.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let nav = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(root)
        .current_dir(link.join("nested"))
        .args(["dir", "top"])
        .output()
        .unwrap();
    assert!(nav.status.success());
    assert_eq!(
        String::from_utf8(nav.stdout).unwrap().trim(),
        path.to_str().unwrap()
    );
    let nav = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(root)
        .current_dir(&path)
        .args(["dir", "parent"])
        .output()
        .unwrap();
    assert!(nav.status.success());
    assert_eq!(
        String::from_utf8(nav.stdout).unwrap().trim(),
        path.to_str().unwrap()
    );
    std::fs::write(
        root.join("config.toml"),
        "snapshot_idle_seconds=3\nsnapshot_max_seconds=20\n",
    )
    .unwrap();
    assert_eq!(
        ok(root, &["daemon", "reload"])["config"]["snapshot_idle_seconds"],
        3
    );
    std::fs::write(
        root.join("config.toml"),
        "mount='different'\nsnapshot_idle_seconds=4\n",
    )
    .unwrap();
    let out = invoke(root, &["daemon", "reload"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("restart"));
    assert_eq!(f.store.setting("snapshot_idle_seconds", 0).unwrap(), 3);
    // Rejected config must not break navigation through the running mount.
    std::fs::write(root.join("config.toml"), "invalid TOML!").unwrap();
    let top = invoke(root, &["dir", "top", "--workspace", "fresh"]);
    assert!(top.status.success());
    std::fs::remove_file(root.join("config.toml")).unwrap();
    // Keep a directory fd open, making an ordinary unmount fail without losing service.
    let busy = std::fs::File::open(&path).unwrap();
    let out = invoke(root, &["daemon", "stop"]);
    assert!(!out.status.success());
    assert_eq!(ok(root, &["daemon", "status"])["stopping"], false);
    drop(busy);
    ok(root, &["daemon", "stop"]);
    wait_stopped(root);
    assert!(!root.join("control.sock").exists());
    let restart = ok(root, &["daemon", "start", "--detached", "--json"]);
    assert_ne!(first["pid"], restart["pid"]);
    unsafe {
        libc::kill(restart["pid"].as_i64().unwrap() as i32, libc::SIGKILL);
    }
    wait_stopped(root);
    let recovered = ok(root, &["daemon", "start", "--detached", "--json"]);
    assert_eq!(recovered["ready"], true);
    assert_eq!(std::fs::read(path.join("file")).unwrap(), b"edits");
    assert!(std::fs::read_to_string(root.join("logs/daemon.log"))
        .unwrap()
        .contains("configuration reloaded"));
    ok(root, &["daemon", "stop", "--lazy"]);
    wait_stopped(root);
}
#[test]
fn protocol_rejects_unversioned_clients() {
    use std::io::{Read, Write};
    let f = Fixture::new();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = cli::server(f.store.clone(), stop.clone()).unwrap();
    let mut client =
        std::os::unix::net::UnixStream::connect(f.store.root.join("control.sock")).unwrap();
    client.write_all(b"{\"command\":\"Init\"}").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut out = String::new();
    client.read_to_string(&mut out).unwrap();
    assert!(out.contains("incompatible client protocol"));
    stop.store(true, std::sync::atomic::Ordering::Release);
    server.join().unwrap();
    assert!(cli::request(
        &f.store.root,
        &Cmd::Daemon {
            command: DaemonCmd::Status
        }
    )
    .unwrap()
    .is_none());
}

#[test]
#[ignore = "requires local /dev/fuse mounts; uses a fake systemd user manager"]
fn systemd_start_writes_private_unit_and_waits_for_readiness() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bin = f.temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let config = f.temp.path().join("config");
    // The fake manager executes the exact generated ExecStart, without touching
    // the host's manager, home, login configuration, or user services.
    let script = r#"#!/usr/bin/python3
import os, pathlib, shlex, subprocess, sys
config = pathlib.Path(os.environ['XDG_CONFIG_HOME'])
with (config.parent / 'systemctl.calls').open('a') as f:
    f.write(' '.join(sys.argv[1:]) + '\n')
if sys.argv[2] == 'start':
    text = (config / 'systemd/user' / sys.argv[3]).read_text()
    command = next(l[len('ExecStart='):] for l in text.splitlines() if l.startswith('ExecStart='))
    args = shlex.split(command.replace('%%', '%').replace('$$', '$'))
    subprocess.Popen(args, start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
"#;
    std::fs::write(bin.join("systemctl"), script).unwrap();
    std::fs::set_permissions(
        bin.join("systemctl"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin",
                std::fs::canonicalize(&bin).unwrap().display()
            ),
        )
        .env(
            "XDG_CONFIG_HOME",
            std::fs::canonicalize(f.temp.path()).unwrap().join("config"),
        )
        .args(["--json", "daemon", "start"])
        .output()
        .unwrap();
    let _running = Running {
        root: f.store.root.clone(),
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["startup_mode"], "systemd");
    assert_eq!(info["ready"], true);
    let unit = config
        .join("systemd/user")
        .join(gitalator::daemon::unit_name(&f.store.root));
    assert!(unit.exists());
    let calls = std::fs::read_to_string(f.temp.path().join("systemctl.calls")).unwrap();
    assert!(calls.contains("daemon-reload"));
    assert!(!calls.contains("enable"));
    ok(&f.store.root, &["daemon", "stop"]);
    wait_stopped(&f.store.root);
}
