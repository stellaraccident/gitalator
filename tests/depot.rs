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
#[test]
fn live_clone_preserves_index_dirty_and_untracked_without_sharing_branch() {
    use gitalator::git;
    use std::io::Write;
    let f = Fixture::new();
    let mut s = f.store();
    {
        let w = s.get("dev").unwrap();
        let w = w.lock().unwrap();
        w.open_file("src/hello", libc::O_RDWR | libc::O_TRUNC)
            .unwrap()
            .write_all(b"staged")
            .unwrap();
        git::run(&w.depot(), &["add", "overlay/src/hello"]).unwrap();
        std::fs::write(w.upper().join("src/hello"), b"unstaged").unwrap();
        w.create_file("untracked", 0o644)
            .unwrap()
            .write_all(b"extra")
            .unwrap();
    }
    s.fork("copy", "dev", true).unwrap();
    let a = s.get("dev").unwrap();
    let b = s.get("copy").unwrap();
    let a = a.lock().unwrap();
    let b = b.lock().unwrap();
    assert_eq!(a.record.depot_repo, b.record.depot_repo);
    assert_ne!(a.record.branch, b.record.branch);
    assert_eq!(
        git::run(&b.depot(), &["show", ":overlay/src/hello"]).unwrap(),
        b"staged"
    );
    assert_eq!(
        std::fs::read(b.upper().join("src/hello")).unwrap(),
        b"unstaged"
    );
    assert_eq!(
        std::fs::read(b.upper().join("untracked")).unwrap(),
        b"extra"
    );
    std::fs::write(b.upper().join("src/hello"), b"independent").unwrap();
    assert_eq!(
        std::fs::read(a.upper().join("src/hello")).unwrap(),
        b"unstaged"
    );
}
#[test]
fn checkpoint_normalizes_index_but_preserves_unstaged_contents() {
    use gitalator::git;
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    w.copy_up("src/hello", false).unwrap();
    git::run(&w.depot(), &["add", "overlay/src/hello"]).unwrap();
    std::fs::write(w.upper().join("src/hello"), b"unstaged").unwrap();
    std::fs::write(w.depot().join("note"), b"checkpoint").unwrap();
    git::run(&w.depot(), &["add", "note"]).unwrap();
    let commit = w.checkpoint("normalize").unwrap();
    assert_eq!(
        gitalator::git::text(&w.depot(), &["rev-parse", "HEAD"]).unwrap(),
        commit
    );
    assert!(git::run(&w.depot(), &["show", "HEAD:overlay/src/hello"]).is_err());
    assert_eq!(
        std::fs::read(w.upper().join("src/hello")).unwrap(),
        b"unstaged"
    );
    assert!(!w.record.editing);
}
#[test]
fn invalid_edit_session_stays_suspended_across_restart() {
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    let t = w.begin().unwrap();
    let original = std::fs::read(w.depot().join("depot.toml")).unwrap();
    std::fs::write(w.depot().join("depot.toml"), "invalid").unwrap();
    assert!(w.end(&t).is_err());
    drop(w);
    drop(s);
    let s = Store::open(&f.state).unwrap();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    assert!(w.entry("src/hello").is_err());
    std::fs::write(w.depot().join("depot.toml"), original).unwrap();
    w.end(&t).unwrap();
    assert!(w.entry("src/hello").is_ok());
}
#[test]
fn equal_only_checkpoint_collapses_without_empty_commit() {
    use gitalator::git;
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    let before = git::text(&w.depot(), &["rev-parse", "HEAD"]).unwrap();
    w.copy_up("src/hello", false).unwrap();
    git::run(&w.depot(), &["add", "overlay/src/hello"]).unwrap();
    assert_eq!(w.checkpoint("normalize").unwrap(), before);
    assert!(!w.upper().join("src/hello").exists());
    assert!(!w.record.editing);
}
#[test]
fn committed_depot_reconstructs_in_fresh_state() {
    use gitalator::git;
    use std::io::Write;
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    w.open_file("src/hello", libc::O_RDWR | libc::O_TRUNC)
        .unwrap()
        .write_all(b"patch")
        .unwrap();
    w.unlink("src/dir/file", false).unwrap();
    w.create_file("extra", 0o755)
        .unwrap()
        .write_all(b"new")
        .unwrap();
    git::run(&w.depot(), &["add", "-A"]).unwrap();
    let pin = w.checkpoint("source state").unwrap();
    let bare = w.state_root().join("repos").join(&w.record.depot_repo);
    drop(w);
    let mut restored = Store::open(&f.state.with_file_name("restored")).unwrap();
    restored
        .create("fresh", bare.to_str().unwrap(), &pin)
        .unwrap();
    let w = restored.get("fresh").unwrap();
    let w = w.lock().unwrap();
    assert_eq!(
        std::fs::read(w.entry("src/hello").unwrap().physical.unwrap()).unwrap(),
        b"patch"
    );
    assert!(w.entry("src/dir/file").is_err());
    assert_eq!(w.entry("extra").unwrap().mode & 0o111, 0o111);
    assert!(w.entry("other/dir/file").is_ok());
}
