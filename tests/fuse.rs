mod common;
use common::*;
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
    let file = p.join("workspaces/a/depot/file");
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
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    assert_eq!(
        before,
        std::fs::metadata(&file).unwrap().modified().unwrap()
    );
    let num = f.store.snapshot("a").unwrap();
    std::fs::write(&file, b"edited\n").unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"edited\n");
    assert_eq!(
        std::fs::read(p.join(format!("snapshots/a/{num}/depot/file"))).unwrap(),
        b"base\n"
    );
    assert_eq!(
        std::fs::read(p.join("workspaces/a/READONLY/file")).unwrap(),
        b"base\n"
    );
    assert!(std::fs::write(p.join("workspaces/a/READONLY/file"), b"bad").is_err());
    let depot = p.join("workspaces/a/depot");
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
    let tmp = p.join("workspaces/a/.map-save");
    std::fs::write(&tmp, b"invalid draft").unwrap();
    std::fs::rename(&tmp, p.join("workspaces/a/VERSION_MAP")).unwrap();
    assert_eq!(f.store.draft("a").unwrap(), "invalid draft");
    assert!(p.join("workspaces/a/READONLY/file").exists());
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
    let file = p.join("workspaces/a/depot/file");
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
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
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
    let temporary = p.join("workspaces/a/.save");
    std::fs::write(&temporary, b"bad").unwrap();
    assert!(std::fs::rename(&temporary, p.join("workspaces/a/ACTIVE_VERSION_MAP")).is_err());
}
