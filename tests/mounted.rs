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
        let f = Fixture::new();
        let store = f.store();
        let mount = store.config.mount.clone();
        drop(store);
        let log = fs::File::create(f.state.join("test-daemon.log")).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_gtl"))
            .arg("--state-dir")
            .arg(&f.state)
            .args(["daemon", "run"])
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
