mod common;
use common::*;
use gitalator::backend::git;
use std::{
    io::{Read, Seek, Write},
    path::PathBuf,
    process::{Child, Command},
    time::Duration,
};
struct Mount {
    child: Child,
    path: PathBuf,
}
impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.path)
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn mounted_workspace_roundtrip() {
    let f = Fixture::new();
    let p = f.temp.path().join("mount");
    std::fs::create_dir_all(&p).unwrap();
    let log = std::fs::File::create(f.temp.path().join("fuse.log")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .args([
            "--state-dir",
            f.store.root.to_str().unwrap(),
            "mount",
            p.to_str().unwrap(),
        ])
        .stderr(log)
        .spawn()
        .unwrap();
    let _mount = Mount {
        child,
        path: p.clone(),
    };
    let file = p.join("workspaces/a/file");
    for _ in 0..100 {
        if file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        file.exists(),
        "FUSE mount failed: {}",
        std::fs::read_to_string(f.temp.path().join("fuse.log")).unwrap()
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"base\n");
    // Backing Git directories must be searchable, not mode 000 from tree mode.
    assert_eq!(
        std::fs::read(p.join("workspaces/a/nested/hidden")).unwrap(),
        b"hidden\n"
    );
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    assert_eq!(
        before,
        std::fs::metadata(&file).unwrap().modified().unwrap()
    );
    let num = command(&f, &["snapshot", "create", "a"])["number"]
        .as_i64()
        .unwrap();
    std::fs::write(&file, b"edited\n").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"edited\n");
    assert_eq!(
        std::fs::read(p.join(format!("snapshots/a/{num}/file"))).unwrap(),
        b"base\n"
    );
    assert_eq!(std::fs::read(p.join("readonly/a/file")).unwrap(), b"base\n");
    assert!(std::fs::write(p.join("readonly/a/file"), b"bad").is_err());
    let depot = p.join("workspaces/a");
    std::fs::create_dir(depot.join("newdir")).unwrap();
    std::fs::write(depot.join("newdir/new"), b"hello").unwrap();
    std::fs::rename(depot.join("newdir/new"), depot.join("newdir/renamed")).unwrap();
    assert_eq!(
        std::fs::read(depot.join("newdir/renamed")).unwrap(),
        b"hello"
    );
    std::os::unix::fs::symlink("file", depot.join("link")).unwrap();
    assert_eq!(std::fs::read(depot.join("link")).unwrap(), b"edited\n");
    let mut open = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    open.seek(std::io::SeekFrom::Start(0)).unwrap();
    open.write_all(b"EDITED").unwrap();
    open.sync_all().unwrap();
    std::fs::remove_file(&file).unwrap();
    open.seek(std::io::SeekFrom::Start(0)).unwrap();
    let mut data = vec![];
    open.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"EDITED\n");
    let tmp = p.join("control/workspaces/a/.map-save");
    std::fs::write(&tmp, b"invalid draft").unwrap();
    std::fs::rename(&tmp, p.join("control/workspaces/a/VERSION_MAP")).unwrap();
    assert_eq!(f.store.draft("a").unwrap(), "invalid draft");
    assert!(p.join("readonly/a/file").exists());
    let cpath = std::ffi::CString::new(depot.join("newdir/renamed").to_str().unwrap()).unwrap();
    let cname = std::ffi::CString::new("user.ws.digest.sha256").unwrap();
    let mut digest = [0u8; 32];
    let n = unsafe {
        libc::getxattr(
            cpath.as_ptr(),
            cname.as_ptr(),
            digest.as_mut_ptr().cast(),
            32,
        )
    };
    assert_eq!(n, 32);
    use sha2::{Digest, Sha256};
    assert_eq!(digest.as_slice(), Sha256::digest(b"hello").as_slice());
}
#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn activation_preserves_open_read_generation() {
    let f = Fixture::new();
    let p = f.temp.path().join("mount");
    std::fs::create_dir_all(&p).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .args([
            "--state-dir",
            f.store.root.to_str().unwrap(),
            "mount",
            p.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    let _m = Mount {
        child,
        path: p.clone(),
    };
    let file = p.join("workspaces/a/file");
    for _ in 0..100 {
        if file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut old = std::fs::File::open(&file).unwrap();
    let mut old_writer = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
    std::fs::write(f.repo.join("file"), b"new generation\n").unwrap();
    commit(&f.repo);
    assert_eq!(command(&f, &["activate", "a"])["state"], "COMMITTED");
    let mut bytes = vec![];
    old.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"base\n");
    assert_eq!(std::fs::read(&file).unwrap(), b"new generation\n");
    assert!(old_writer.write_all(b"stale").is_err());
    let output = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .args([
            "--state-dir",
            f.store.root.to_str().unwrap(),
            "--json",
            "status",
            "a",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["frozen"], false);
    let temporary = p.join("control/workspaces/a/.save");
    std::fs::write(&temporary, b"bad").unwrap();
    assert!(std::fs::rename(
        &temporary,
        p.join("control/workspaces/a/ACTIVE_VERSION_MAP")
    )
    .is_err());
}

#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn workspace_listing_does_not_load_repository_trees() {
    let f = Fixture::new();
    f.store
        .conn()
        .unwrap()
        .execute("UPDATE tree_cache SET data='invalid json'", [])
        .unwrap();
    let p = f.temp.path().join("mount");
    std::fs::create_dir_all(&p).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .arg("mount")
        .arg(&p)
        .spawn()
        .unwrap();
    let _mount = Mount {
        child,
        path: p.clone(),
    };
    let workspace = p.join("control/workspaces/a");
    for _ in 0..100 {
        if workspace.is_dir() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for _ in 0..2 {
        let result = Command::new("ls")
            .arg("-l")
            .arg(&workspace)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let listing = String::from_utf8(result.stdout).unwrap();
        assert!(listing.contains("VERSION_MAP"));
        assert!(listing.contains("ACTIVE_VERSION_MAP"));
        assert!(p.join("workspaces/a").is_dir());
        assert!(p.join("readonly/a").is_dir());
    }
}

fn command(f: &Fixture, args: &[&str]) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn start_mount(f: &Fixture) -> Mount {
    let path = f.temp.path().join("mount");
    std::fs::create_dir(&path).unwrap();
    let log = std::fs::File::create(f.temp.path().join("fuse.log")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .arg("mount")
        .arg(&path)
        .stderr(log)
        .spawn()
        .unwrap();
    let mount = Mount { child, path };
    for _ in 0..100 {
        if mount.path.join("workspaces/a").is_dir() {
            return mount;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "mount failed: {}",
        std::fs::read_to_string(f.temp.path().join("fuse.log")).unwrap()
    );
}

#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn directory_cookies_rewind_and_rename_unlink_identity() {
    use std::{
        ffi::{CStr, CString},
        os::unix::fs::MetadataExt,
    };
    let f = Fixture::new();
    std::fs::create_dir(f.repo.join("large")).unwrap();
    for i in 0..10000 {
        std::fs::write(f.repo.join(format!("large/f{i:05}")), b"x").unwrap();
    }
    commit(&f.repo);
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
    let mount = start_mount(&f);
    let depot = mount.path.join("workspaces/a");
    let large = depot.join("large");
    let path = CString::new(large.to_str().unwrap()).unwrap();
    unsafe {
        let dir = libc::opendir(path.as_ptr());
        assert!(!dir.is_null());
        let first = libc::readdir(dir);
        assert!(!first.is_null());
        let mut names = vec![CStr::from_ptr((*first).d_name.as_ptr())
            .to_string_lossy()
            .into_owned()];
        std::fs::write(large.join("new"), b"new").unwrap();
        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() {
                break;
            }
            names.push(
                CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(names.len(), 10002);
        assert!(!names.iter().any(|n| n == "new"));
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        libc::rewinddir(dir);
        let mut refreshed = vec![];
        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() {
                break;
            }
            refreshed.push(
                CStr::from_ptr((*entry).d_name.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        libc::closedir(dir);
        assert_eq!(refreshed.len(), 10003);
        assert!(refreshed.iter().any(|n| n == "new"));
    }
    let file = depot.join("file");
    let mut first = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    let second = std::fs::File::open(&file).unwrap();
    let ino = first.metadata().unwrap().ino();
    let moved = depot.join("moved");
    std::fs::rename(&file, &moved).unwrap();
    assert_eq!(std::fs::metadata(&moved).unwrap().ino(), ino);
    std::fs::remove_file(&moved).unwrap();
    first.set_len(2).unwrap();
    assert_eq!(second.metadata().unwrap().len(), 2);
    std::fs::write(&moved, b"replacement").unwrap();
    first.write_all(b"XY").unwrap();
    first.rewind().unwrap();
    let mut bytes = vec![];
    first.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"XY");
    assert_eq!(std::fs::read(&moved).unwrap(), b"replacement");
    assert_ne!(std::fs::metadata(&moved).unwrap().ino(), ino);
    assert_eq!(
        std::fs::metadata(large.join("..")).unwrap().ino(),
        std::fs::metadata(&depot).unwrap().ino()
    );
}

#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn restore_same_map_invalidates_immediately_and_rejects_stale_writers() {
    let f = Fixture::new();
    let mount = start_mount(&f);
    let path = mount.path.join("workspaces/a/file");
    let snapshot = command(&f, &["snapshot", "create", "a"])["number"]
        .as_i64()
        .unwrap()
        .to_string();
    std::fs::write(&path, b"modified").unwrap();
    let mut stale = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert_eq!(stale.metadata().unwrap().len(), 8);
    command(&f, &["restore", "a", &snapshot]);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 5);
    assert_eq!(std::fs::read(&path).unwrap(), b"base\n");
    assert!(stale.write_all(b"bad").is_err());
    assert!(stale.set_len(1).is_err());
    let mut old = vec![];
    stale.read_to_end(&mut old).unwrap();
    assert_eq!(old, b"modified");
    let duplicate = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .arg("mount")
        .arg(f.temp.path().join("second"))
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already owns"));
}

#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn lazy_fetch_has_default_console_activity_and_detailed_log() {
    let mut f = Fixture::new();
    git(&f.repo, &["config", "uploadpack.allowFilter", "true"]).unwrap();
    f.store = gitalator::db::Store::open(&f.temp.path().join("lazy-state")).unwrap();
    f.store
        .repo_add(
            "source",
            &format!(
                "file://{}",
                std::fs::canonicalize(&f.repo).unwrap().display()
            ),
        )
        .unwrap();
    f.store
        .create(
            "a",
            "format=1\n[[mount]]\nid='root'\nrepo='source'\npath=''\nrevision='main'\n",
        )
        .unwrap();
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
    let mount = start_mount(&f);
    let file = mount.path.join("workspaces/a/file");
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 5);
    let console = std::fs::read_to_string(f.temp.path().join("fuse.log")).unwrap();
    assert!(console.contains("logs "), "{console}");
    assert!(console.contains("gtl: backend:"), "{console}");
    let logpath = f.store.root.join("logs/operations.jsonl");
    let before = std::fs::read_to_string(&logpath).unwrap();
    assert!(before
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .any(|r| r["operation"] == "fetch-blobs:source" && r["status"] == "succeeded"));
    for _ in 0..10 {
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 5);
    }
    let after = std::fs::read_to_string(&logpath).unwrap();
    assert_eq!(
        before.matches("fetch-blobs:source").count(),
        after.matches("fetch-blobs:source").count()
    );
}

#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn recursive_du_batches_sizes_without_fetching_for_name_listings() {
    let mut f = Fixture::new();
    std::fs::create_dir(f.repo.join("walk")).unwrap();
    for i in 0..300 {
        std::fs::write(
            f.repo.join(format!("walk/file-{i:04}")),
            format!("unique content {i}\n"),
        )
        .unwrap();
    }
    commit(&f.repo);
    git(&f.repo, &["config", "uploadpack.allowFilter", "true"]).unwrap();
    f.store = gitalator::db::Store::open(&f.temp.path().join("lazy-state")).unwrap();
    f.store
        .repo_add(
            "source",
            &format!(
                "file://{}",
                std::fs::canonicalize(&f.repo).unwrap().display()
            ),
        )
        .unwrap();
    f.store
        .create(
            "a",
            "format=1\n[[mount]]\nid='root'\nrepo='source'\npath=''\nrevision='main'\n",
        )
        .unwrap();
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
    let mount = start_mount(&f);
    let directory = mount.path.join("workspaces/a/walk");
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 300);
    assert_eq!(command(&f, &["metrics"])["metadata_fetches"], 0);
    let du = Command::new("du")
        .arg("-h")
        .arg(&directory)
        .output()
        .unwrap();
    assert!(
        du.status.success(),
        "{}",
        String::from_utf8_lossy(&du.stderr)
    );
    assert_eq!(command(&f, &["metrics"])["metadata_fetches"], 300);
    let count_fetches = || {
        std::fs::read_to_string(f.store.root.join("logs/operations.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|r| r["operation"] == "fetch-blobs:source" && r["status"] == "start")
            .count()
    };
    assert_eq!(
        count_fetches(),
        2,
        "300 unique files should require two batches, not 300 fetches"
    );
    assert!(Command::new("du")
        .arg("-h")
        .arg(&directory)
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(count_fetches(), 2);
}

#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn disconnected_mount_reports_recovery_and_restarts_without_delay() {
    let f = Fixture::new();
    let mut mount = start_mount(&f);
    mount.child.kill().unwrap();
    mount.child.wait().unwrap();
    let attempt = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .arg("mount")
        .arg(&mount.path)
        .output()
        .unwrap();
    assert!(!attempt.status.success());
    let stderr = String::from_utf8_lossy(&attempt.stderr);
    assert!(stderr.contains("disconnected FUSE mount"), "{stderr}");
    assert!(!stderr.contains("File exists"), "{stderr}");
    assert!(!stderr.contains("mount stopped"), "{stderr}");
    assert!(!f.store.root.join("control.sock").exists());
    assert!(Command::new("fusermount3")
        .arg("-uz")
        .arg(&mount.path)
        .status()
        .unwrap()
        .success());
    // Reuse the same mount directory immediately, without an arbitrary sleep.
    let log = std::fs::File::create(f.temp.path().join("restart.log")).unwrap();
    mount.child = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .arg("mount")
        .arg(&mount.path)
        .stderr(log)
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if mount.path.join("workspaces/a").is_dir() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "restart failed: {}",
        std::fs::read_to_string(f.temp.path().join("restart.log")).unwrap()
    );
}

#[test]
#[ignore = "requires real /dev/fuse mount; run explicitly"]
fn recursive_du_combines_small_sibling_directories() {
    let mut f = Fixture::new();
    for d in 0..40 {
        let dir = f.repo.join(format!("walk/d{d:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..4 {
            std::fs::write(dir.join(format!("file{n}")), format!("distinct {d} {n}\n")).unwrap();
        }
    }
    commit(&f.repo);
    git(&f.repo, &["config", "uploadpack.allowFilter", "true"]).unwrap();
    f.store = gitalator::db::Store::open(&f.temp.path().join("lazy-state")).unwrap();
    f.store
        .repo_add(
            "source",
            &format!(
                "file://{}",
                std::fs::canonicalize(&f.repo).unwrap().display()
            ),
        )
        .unwrap();
    f.store
        .create(
            "a",
            "format=1\n[[mount]]\nid='root'\nrepo='source'\npath=''\nrevision='main'\n",
        )
        .unwrap();
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
    let mount = start_mount(&f);
    let directory = mount.path.join("workspaces/a/walk");
    for dir in std::fs::read_dir(&directory).unwrap() {
        assert_eq!(std::fs::read_dir(dir.unwrap().path()).unwrap().count(), 4);
    }
    assert_eq!(command(&f, &["metrics"])["metadata_fetches"], 0);
    let result = Command::new("du")
        .arg("-h")
        .arg(&directory)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(command(&f, &["metrics"])["metadata_fetches"], 160);
    let log = std::fs::read_to_string(f.store.root.join("logs/operations.jsonl")).unwrap();
    let fetches = log
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|r| r["operation"] == "fetch-blobs:source" && r["status"] == "start")
        .count();
    assert_eq!(
        fetches, 2,
        "40 directories should use two fetches, bounded to 32 directories each"
    );
}
