mod common;
use common::*;
use gitalator::{model::Map, store::Store};
#[test]
fn linked_depot_and_exact_layout_survive_restart() {
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    assert!(w.depot().join(".git").is_file());
    assert_eq!(w.base["src/hello"].oid, w.base["other/hello"].oid);
    assert!(!w.upper().join("src/hello").exists());
    drop(w);
    drop(s);
    assert!(Store::open(&f.state).unwrap().get("dev").is_ok());
}
#[test]
fn old_state_is_rejected_without_modification() {
    let f = Fixture::new();
    std::fs::create_dir(&f.state).unwrap();
    std::fs::write(f.state.join("state.sqlite3"), b"preserve").unwrap();
    assert!(Store::open(&f.state).is_err());
    assert_eq!(
        std::fs::read(f.state.join("state.sqlite3")).unwrap(),
        b"preserve"
    );
}
#[test]
fn map_rejects_floating_pins_and_escaping_paths() {
    assert!(Map::parse(
        "format=1\n[[mounts]]\nid='x'\nrepository='x'\ncommit='main'\npath='../x'\n"
    )
    .is_err());
}
#[test]
fn sparse_edits_deletions_recreation_and_aliases() {
    use std::io::Write;
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    assert_eq!(
        std::fs::read(w.entry("src/hello").unwrap().physical.unwrap()).unwrap(),
        b"hello\n"
    );
    assert!(!w.upper().join("src/hello").exists());
    w.open_file("src/hello", libc::O_RDWR | libc::O_TRUNC)
        .unwrap()
        .write_all(b"edit")
        .unwrap();
    assert_eq!(
        std::fs::read(w.entry("other/hello").unwrap().physical.unwrap()).unwrap(),
        b"hello\n"
    );
    w.unlink("src/hello", false).unwrap();
    assert!(w.entry("src/hello").is_err());
    assert!(w.tombs().join("src/hello").is_file());
    w.create_file("src/hello", 0o644)
        .unwrap()
        .write_all(b"new")
        .unwrap();
    assert!(!w.tombs().join("src/hello").exists());
    w.revert("src/hello").unwrap();
    assert_eq!(
        std::fs::read(w.entry("src/hello").unwrap().physical.unwrap()).unwrap(),
        b"hello\n"
    );
    w.validate().unwrap();
}
#[test]
fn opaque_directory_and_rename() {
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    w.unlink("src/dir/file", false).unwrap();
    w.unlink("src/dir", true).unwrap();
    w.mkdir("src/dir", 0o755).unwrap();
    assert!(w.children("src/dir").unwrap().is_empty());
    w.create_file("src/dir/new", 0o644).unwrap();
    w.rename("src/dir/new", "src/new").unwrap();
    assert!(w.entry("src/new").is_ok());
    w.validate().unwrap();
}
#[test]
fn generated_links_are_absolute_and_mount_specific() {
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    for p in [".depotgit", "src/.mountgit", "other/.mountgit"] {
        assert!(w.entry(p).unwrap().bytes.unwrap().starts_with(b"/"));
    }
    let a: serde_json::Value =
        serde_json::from_slice(&w.entry("src/.mountstate.json").unwrap().bytes.unwrap()).unwrap();
    assert_eq!(a["path"], "src");
    assert!(w.unlink("src/.mountcommit", false).is_err());
}
#[test]
fn backing_symlink_cannot_escape() {
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    w.symlink("escape", std::path::Path::new("/tmp")).unwrap();
    assert!(w.create_file("escape/unsafe", 0o644).is_err());
}
