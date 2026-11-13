mod common;
use common::*;
use gitalator::{
    cli::{Cmd, DaemonCmd, DepotCmd, WorkspaceCmd},
    daemon,
};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::symlink,
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};
struct Mounted {
    f: Fixture,
    child: Child,
    mount: PathBuf,
}
impl Mounted {
    fn new() -> Self {
        Self::configured(Fixture::new(), &[])
    }
    fn configured(f: Fixture, env: &[(&str, &std::ffi::OsStr)]) -> Self {
        let store = f.store();
        let mount = store.config.mount.clone();
        drop(store);
        let log = fs::File::create(f.state.join("test-daemon.log")).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_gtl"))
            .arg("--state-dir")
            .arg(&f.state)
            .args(["daemon", "run"])
            .envs(env.iter().copied())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon::request(
            &f.state,
            &Cmd::Daemon {
                command: DaemonCmd::Status,
            },
        )
        .is_err()
        {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "daemon {status}: {}",
                    fs::read_to_string(f.state.join("test-daemon.log")).unwrap()
                );
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("mount timeout");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Self { f, child, mount }
    }
    fn dev(&self) -> PathBuf {
        self.mount.join("dev")
    }
    fn request(&self, c: Cmd) -> serde_json::Value {
        daemon::request(&self.f.state, &c).unwrap()
    }
}
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = daemon::request(
            &self.f.state,
            &Cmd::Daemon {
                command: DaemonCmd::Stop { lazy: true },
            },
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn filesystem_roundtrip_and_discovery() {
    let m = Mounted::new();
    let root = m.dev();
    assert_eq!(fs::read(root.join("src/hello")).unwrap(), b"hello\n");
    let alias = m.f.temp.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(fs::read_link(alias.join(".depotgit"))
        .unwrap()
        .is_absolute());
    assert!(fs::read_link(alias.join("src/.mountgit"))
        .unwrap()
        .is_absolute());
    fs::write(root.join("src/hello"), b"changed").unwrap();
    assert_eq!(fs::read(root.join("other/hello")).unwrap(), b"hello\n");
    assert_eq!(
        fs::read(root.join(".depotgit/overlay/src/hello")).unwrap(),
        b"changed"
    );
    fs::rename(root.join("src/hello"), root.join("src/moved")).unwrap();
    assert!(!root.join("src/hello").exists());
    fs::remove_file(root.join("src/moved")).unwrap();
    fs::create_dir(root.join("output")).unwrap();
    fs::write(root.join("output/a"), b"one").unwrap();
    fs::remove_file(root.join("output/a")).unwrap();
    fs::remove_dir(root.join("output")).unwrap();
}
#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn real_compiler_executable_and_mmap() {
    let m = Mounted::new();
    let root = m.dev();
    fs::create_dir(root.join("out")).unwrap();
    fs::write(
        root.join("out/main.c"),
        b"#include <stdio.h>\nint main(void){puts(\"depot\");return 0;}\n",
    )
    .unwrap();
    let o = Command::new("cc")
        .arg(root.join("out/main.c"))
        .arg("-o")
        .arg(root.join("out/app"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let out = Command::new(root.join("out/app")).output().unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"depot\n");
    let script="import mmap,os,sys\np=sys.argv[1]\nwith open(p,'w+b') as f:\n f.write(b'abcdef');f.flush()\n with mmap.mmap(f.fileno(),0) as mm:\n  mm[1:3]=b'XY';mm.flush()\nwith open(p,'rb') as f: assert f.read()==b'aXYdef'\n";
    let output = Command::new("python3")
        .args(["-c", script])
        .arg(root.join("out/mapped"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn open_inode_survives_unlink_and_rename_overwrite() {
    let m = Mounted::new();
    let root = m.dev();
    fs::write(root.join("a"), b"old").unwrap();
    fs::write(root.join("b"), b"new").unwrap();
    let mut old = fs::File::open(root.join("a")).unwrap();
    fs::rename(root.join("b"), root.join("a")).unwrap();
    let mut bytes = vec![];
    old.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"old");
    assert_eq!(fs::read(root.join("a")).unwrap(), b"new");
    let mut open = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("a"))
        .unwrap();
    fs::remove_file(root.join("a")).unwrap();
    open.write_all(b"updated").unwrap();
    open.sync_all().unwrap();
    assert!(!root.join("a").exists());
}
#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn edit_lease_excludes_live_handles_and_other_workspaces_continue() {
    let m = Mounted::new();
    m.request(Cmd::Workspace {
        command: WorkspaceCmd::Fork {
            name: "second".into(),
            from: "dev".into(),
        },
    });
    let f = fs::File::open(m.dev().join("src/hello")).unwrap();
    assert!(daemon::request(
        &m.f.state,
        &Cmd::Depot {
            command: DepotCmd::Begin {
                workspace: "dev".into()
            }
        }
    )
    .is_err());
    drop(f);
    std::thread::sleep(Duration::from_millis(50));
    let info = m.request(Cmd::Depot {
        command: DepotCmd::Begin {
            workspace: "dev".into(),
        },
    });
    let work = PathBuf::from(info["worktree"].as_str().unwrap());
    fs::create_dir_all(work.join("overlay/src")).unwrap();
    fs::write(work.join("overlay/src/hello"), b"backing edit").unwrap();
    assert!(fs::read(m.dev().join("src/hello")).is_err());
    assert_eq!(
        fs::read(m.mount.join("second/src/hello")).unwrap(),
        b"hello\n"
    );
    m.request(Cmd::Depot {
        command: DepotCmd::End {
            workspace: "dev".into(),
            token: info["token"].as_str().unwrap().into(),
        },
    });
    assert_eq!(
        fs::read(m.dev().join("src/hello")).unwrap(),
        b"backing edit"
    );
}
#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn chmod_copyup_and_same_inode_reads() {
    use std::os::unix::fs::PermissionsExt;
    let m = Mounted::new();
    let p = m.dev().join("src/hello");
    let old = fs::File::open(&p).unwrap();
    old.set_permissions(fs::Permissions::from_mode(0o755))
        .unwrap();
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o111,
        0o111
    );
    assert_eq!(
        fs::metadata(m.dev().join("other/hello"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    drop(old);
    let mut reader = fs::File::open(&p).unwrap();
    fs::write(&p, b"after").unwrap();
    let mut bytes = vec![];
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"after");
}
#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn detached_lifecycle_restart_and_configured_mount() {
    let f = Fixture::new();
    let mut s = f.store();
    let mount = f.state.with_file_name("custom-mount");
    s.config.mount = mount.clone();
    gitalator::store::json_write(&f.state.join("config.json"), &s.config).unwrap();
    drop(s);
    let exe = env!("CARGO_BIN_EXE_gtl");
    let start = || {
        Command::new(exe)
            .arg("--state-dir")
            .arg(&f.state)
            .args(["daemon", "start", "--detached"])
            .output()
            .unwrap()
    };
    let out = start();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::write(mount.join("dev/src/hello"), b"durable dirty").unwrap();
    daemon::request(
        &f.state,
        &Cmd::Daemon {
            command: DaemonCmd::Stop { lazy: false },
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.state.join("control.sock").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = start();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(mount.join("dev/src/hello")).unwrap(),
        b"durable dirty"
    );
    daemon::request(
        &f.state,
        &Cmd::Daemon {
            command: DaemonCmd::Stop { lazy: true },
        },
    )
    .unwrap();
}
#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn backing_edit_can_change_file_type() {
    let m = Mounted::new();
    assert_eq!(fs::read(m.dev().join("src/hello")).unwrap(), b"hello\n");
    let info = m.request(Cmd::Depot {
        command: DepotCmd::Begin {
            workspace: "dev".into(),
        },
    });
    let work = PathBuf::from(info["worktree"].as_str().unwrap());
    fs::create_dir_all(work.join("overlay/src")).unwrap();
    symlink("dir/file", work.join("overlay/src/hello")).unwrap();
    m.request(Cmd::Depot {
        command: DepotCmd::End {
            workspace: "dev".into(),
            token: info["token"].as_str().unwrap().into(),
        },
    });
    assert_eq!(fs::read(m.dev().join("src/hello")).unwrap(), b"nested\n");
}

fn wait_for(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not exit after cancellation");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn delayed_mount() -> (Mounted, PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let old = gitalator::git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    for i in 0..20 {
        fs::write(
            f.component.join(format!("blocked-{i}")),
            format!("payload {i}"),
        )
        .unwrap();
    }
    commit(&f.component);
    let pin = gitalator::git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let map = fs::read_to_string(f.depot.join("depot.toml")).unwrap();
    fs::write(f.depot.join("depot.toml"), map.replace(&old, &pin)).unwrap();
    commit(&f.depot);
    gitalator::git::run(&f.component, &["config", "uploadpack.allowFilter", "true"]).unwrap();
    let dir = f.component.parent().unwrap().join("fake-bin");
    fs::create_dir(&dir).unwrap();
    let gate = dir.join("delay");
    let pid = dir.join("pid");
    fs::write(&gate, b"").unwrap();
    let real = Command::new("which").arg("git").output().unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    fs::write(
        dir.join("git"),
        format!(
            r#"#!/bin/sh
case " $* " in
  *" fetch "*)
    if [ -e "$GTL_TEST_GATE" ]; then
      echo $$ > "$GTL_TEST_PID"
      echo "test transfer waiting" >&2
      sleep 60
    fi
    ;;
esac
exec "{}" "$@"
"#,
            real.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(dir.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
    let m = Mounted::configured(
        f,
        &[
            ("PATH", std::ffi::OsStr::new(&path)),
            ("GTL_TEST_GATE", gate.as_os_str()),
            ("GTL_TEST_PID", pid.as_os_str()),
        ],
    );
    (m, gate, pid)
}

#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn interrupt_fetch_and_retry_without_blocking_other_workspaces() {
    let (m, gate, pid) = delayed_mount();
    m.request(Cmd::Workspace {
        command: WorkspaceCmd::Fork {
            name: "quiet".into(),
            from: "dev".into(),
        },
    });
    let mut client = Command::new("ls")
        .args(["-l"])
        .arg(m.dev().join("src/hello"))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&pid);
    let before = Instant::now();
    let activity = m.request(Cmd::Activity);
    assert!(activity
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["stage"].as_str().unwrap().contains("Fetch")));
    assert_eq!(
        fs::read(m.mount.join("quiet/build/config")).unwrap(),
        b"config\n"
    );
    assert!(before.elapsed() < Duration::from_secs(2));
    unsafe {
        libc::kill(client.id() as i32, libc::SIGINT);
    }
    assert!(!wait_exit(&mut client).success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let logs = fs::read_to_string(m.f.state.join("logs/activity.jsonl")).unwrap();
        if logs.contains("Cancelled backend process group") {
            assert!(logs.contains("FUSE interrupt"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "backend cancellation missing: {logs}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::remove_file(gate).unwrap();
    assert_eq!(fs::read(m.dev().join("src/hello")).unwrap(), b"hello\n");
    let expected: u64 =
        gitalator::git::text(&m.f.component, &["show", "-s", "--format=%ct", "HEAD"])
            .unwrap()
            .parse()
            .unwrap();
    assert_eq!(
        fs::metadata(m.dev().join("src/hello"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expected
    );
    assert_eq!(
        fs::metadata(m.dev().join("src/dir"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        expected
    );
}

#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn explicit_cancel_releases_lock_wait_and_cli_disconnect_cancels_fetch() {
    let (m, gate, pid) = delayed_mount();
    // Diff must hydrate this upper file to compare it to its base.
    let depot = fs::read_link(m.dev().join(".depotgit")).unwrap();
    m.request(Cmd::Depot {
        command: DepotCmd::Begin {
            workspace: "dev".into(),
        },
    });
    fs::create_dir_all(depot.join("overlay/src")).unwrap();
    fs::write(depot.join("overlay/src/hello"), b"edited").unwrap();
    let status = m.request(Cmd::Status {
        workspace: "dev".into(),
    });
    let token = status["record"]["edit_token"].as_str().unwrap().to_string();
    m.request(Cmd::Depot {
        command: DepotCmd::End {
            workspace: "dev".into(),
            token,
        },
    });
    let mut client = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&m.f.state)
        .args(["diff", "dev"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&pid);
    let mut waiter = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&m.f.state)
        .args(["validate", "dev"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let id = loop {
        let activities = m.request(Cmd::Activity);
        if let Some(a) = activities
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["operation"] == "validate dev")
        {
            break a["id"].as_u64().unwrap();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(m.request(Cmd::Cancel { id })["cancelled"], true);
    assert!(!wait_exit(&mut waiter).success());
    unsafe {
        libc::kill(client.id() as i32, libc::SIGINT);
    }
    assert!(!wait_exit(&mut client).success());
    fs::remove_file(gate).unwrap();
    // The disconnected diff must release the workspace/repository locks.
    let mut validate = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&m.f.state)
        .args(["validate", "dev"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_exit(&mut validate).success());
}

#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn shutdown_cancels_inflight_fetch() {
    let (mut m, _gate, pid) = delayed_mount();
    let mut client = Command::new("ls")
        .arg("-l")
        .arg(m.dev().join("src/hello"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&pid);
    unsafe {
        libc::kill(m.child.id() as i32, libc::SIGTERM);
    }
    assert!(wait_exit(&mut m.child).success());
    wait_exit(&mut client);
    let logs = fs::read_to_string(m.f.state.join("logs/activity.jsonl")).unwrap();
    assert!(logs.contains("Cancelled backend process group"));
    assert!(logs.contains("Filesystem server stopped"));
}

#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn long_listing_does_not_extract_payloads() {
    let m = Mounted::new();
    assert!(Command::new("ls")
        .args(["-ln", "--color=never"])
        .arg(m.dev().join("src"))
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap()
        .success());
    assert!(fs::read_dir(m.f.state.join("cache"))
        .unwrap()
        .next()
        .is_none());
    assert_eq!(fs::read(m.dev().join("src/hello")).unwrap(), b"hello\n");
    assert!(fs::read_dir(m.f.state.join("cache"))
        .unwrap()
        .next()
        .is_some());
}

#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn initialize_empty_workspace_and_write_first_file() {
    let m = Mounted::new();
    // Exercise the actual CLI, including the wire representation and JSON output.
    let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&m.f.state)
        .args(["workspace", "init", "example"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let root = std::path::Path::new(info["path"].as_str().unwrap());
    let mut names: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec![".depotgit", ".depotstate.json"]);
    fs::write(root.join("hello.txt"), b"first file\n").unwrap();
    assert_eq!(
        fs::read(root.join(".depotgit/overlay/hello.txt")).unwrap(),
        b"first file\n"
    );
    assert_eq!(
        m.request(Cmd::Diff {
            workspace: "example".into()
        })[0]["path"],
        "hello.txt"
    );
    m.request(Cmd::Workspace {
        command: WorkspaceCmd::Clone {
            name: "copy".into(),
            from: "example".into(),
        },
    });
    assert_eq!(
        fs::read(m.mount.join("copy/hello.txt")).unwrap(),
        b"first file\n"
    );
}

#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn add_root_mount_from_cwd_and_workspace_alias() {
    let m = Mounted::new();
    m.request(Cmd::Workspace {
        command: WorkspaceCmd::Init {
            name: "example".into(),
        },
    });
    let root = m.mount.join("example");
    let alias = m.f.component.parent().unwrap().join("workspace-alias");
    symlink(&root, &alias).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .current_dir(&alias)
        .args(["mount", "add"])
        .arg(&m.f.component)
        .args(["--at", ".", "--submodules", "none"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["workspace"], "example");
    assert_eq!(result["added"][0]["path"], "");
    assert_eq!(fs::read(root.join("hello")).unwrap(), b"hello\n");
    assert!(fs::read_link(root.join(".mountgit")).unwrap().is_absolute());
    let before = fs::read(root.join(".depotgit/depot.toml")).unwrap();
    let wrong = m.f.component.parent().unwrap().join("wrong-state");
    let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&wrong)
        .current_dir(&root)
        .args(["mount", "add"])
        .arg(&m.f.component)
        .args(["--at", "another"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!wrong.exists());
    assert_eq!(fs::read(root.join(".depotgit/depot.toml")).unwrap(), before);
}

#[test]
#[ignore = "requires /dev/fuse and mount capability"]
fn cancelling_mount_import_keeps_old_layout_available() {
    let (m, gate, pid) = delayed_mount();
    m.request(Cmd::Workspace {
        command: WorkspaceCmd::Init {
            name: "example".into(),
        },
    });
    let before = fs::read(m.mount.join("example/.depotgit/depot.toml")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gtl"));
    command
        .arg("--state-dir")
        .arg(&m.f.state)
        .args(["mount", "add"])
        .arg(&m.f.component)
        .args(["--at", ".", "--workspace", "example"])
        .stdout(std::process::Stdio::null());
    let mut client = command.spawn().unwrap();
    wait_for(&pid);
    assert_eq!(fs::read(m.dev().join("build/config")).unwrap(), b"config\n");
    assert!(m.request(Cmd::Status {
        workspace: "example".into()
    })["record"]["map"]["mounts"]
        .as_array()
        .unwrap()
        .is_empty());
    unsafe {
        libc::kill(client.id() as i32, libc::SIGINT);
    }
    assert!(!wait_exit(&mut client).success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let activities = m.request(Cmd::Activity);
        if !activities
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["operation"] == "mount add")
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read(m.mount.join("example/.depotgit/depot.toml")).unwrap(),
        before
    );
    fs::remove_file(gate).unwrap();
    let mut retry = command.spawn().unwrap();
    assert!(wait_exit(&mut retry).success());
    assert_eq!(fs::read(m.mount.join("example/hello")).unwrap(), b"hello\n");
}
