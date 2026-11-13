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
#[test]
fn repeated_pins_nested_subtree_and_ordinary_dotfiles() {
    use gitalator::{git, model::Mount};
    let f = Fixture::new();
    std::fs::write(f.component.join("hello"), b"new pin").unwrap();
    std::fs::write(f.component.join("dir/.mountcommit"), b"source dotfile").unwrap();
    commit(&f.component);
    let mut map =
        Map::parse(&std::fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    map.mounts[1].commit = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    map.mounts.push(Mount {
        uninitialized_submodules: false,
        id: "nested".into(),
        repository: "code".into(),
        commit: map.mounts[0].commit.clone(),
        path: "src/dir".into(),
        subtree: "dir".into(),
    });
    std::fs::write(f.depot.join("depot.toml"), toml::to_string(&map).unwrap()).unwrap();
    commit(&f.depot);
    let s = f.store();
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    assert_ne!(
        w.entry("src/.mountcommit").unwrap().bytes,
        w.entry("other/.mountcommit").unwrap().bytes
    );
    assert!(w.entry("src/dir/hello").is_err());
    assert_eq!(
        std::fs::read(w.entry("other/dir/.mountcommit").unwrap().physical.unwrap()).unwrap(),
        b"source dotfile"
    );
}
#[test]
fn type_replacements_are_reproducible() {
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    w.unlink("src/hello", false).unwrap();
    w.mkdir("src/hello", 0o755).unwrap();
    w.create_file("src/hello/child", 0o644).unwrap();
    w.validate().unwrap();
    w.unlink("src/hello/child", false).unwrap();
    w.unlink("src/hello", true).unwrap();
    w.symlink("src/hello", std::path::Path::new("dir/file"))
        .unwrap();
    w.validate().unwrap();
    assert_eq!(
        std::fs::read_link(w.upper().join("src/hello")).unwrap(),
        std::path::Path::new("dir/file")
    );
}
#[test]
fn invalid_staged_tombstone_is_rejected_before_commit() {
    use gitalator::git;
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    w.unlink("src/hello", false).unwrap();
    git::run(&w.depot(), &["add", "tombstones"]).unwrap();
    let bad = b"{\"format\":99}";
    let oid = String::from_utf8(
        git::input(&w.depot(), &["hash-object", "-w", "--stdin"], Some(bad)).unwrap(),
    )
    .unwrap();
    git::run(
        &w.depot(),
        &[
            "update-index",
            "--cacheinfo",
            &format!("100644,{},tombstones/src/hello", oid.trim()),
        ],
    )
    .unwrap();
    assert!(w.checkpoint("invalid").is_err());
    assert!(!w.record.editing);
}
#[test]
fn clone_excludes_ignored_outputs_and_removal_preserves_branch() {
    use gitalator::git;
    let f = Fixture::new();
    let mut s = f.store();
    {
        let w = s.get("dev").unwrap();
        let w = w.lock().unwrap();
        std::fs::write(w.depot().join(".gitignore"), "overlay/out/\n").unwrap();
        w.mkdir("out", 0o755).unwrap();
        w.create_file("out/obj", 0o644).unwrap();
    }
    s.fork("copy", "dev", true).unwrap();
    {
        let w = s.get("copy").unwrap();
        let w = w.lock().unwrap();
        assert!(!w.upper().join("out/obj").exists());
    }
    assert!(s.remove("dev").is_err());
    s.fork("clean", "dev", false).unwrap();
    let (repo, branch) = {
        let w = s.get("clean").unwrap();
        let w = w.lock().unwrap();
        (w.record.depot_repo.clone(), w.record.branch.clone())
    };
    s.remove("clean").unwrap();
    assert!(git::run(&s.root.join("repos").join(repo), &["rev-parse", &branch]).is_ok());
}
#[test]
fn local_lfs_payload_and_equal_checkpoint() {
    use gitalator::git;
    let f = Fixture::new();
    git::run(&f.component, &["lfs", "install", "--local", "--skip-repo"]).unwrap();
    git::run(&f.component, &["lfs", "track", "*.dat"]).unwrap();
    std::fs::write(f.component.join("asset.dat"), b"expanded LFS payload\n").unwrap();
    commit(&f.component);
    let pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let mut map =
        Map::parse(&std::fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    for m in &mut map.mounts {
        m.commit = pin.clone();
    }
    std::fs::write(f.depot.join("depot.toml"), toml::to_string(&map).unwrap()).unwrap();
    commit(&f.depot);
    let s = f.store();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    assert_eq!(
        std::fs::read(w.entry("src/asset.dat").unwrap().physical.unwrap()).unwrap(),
        b"expanded LFS payload\n"
    );
    w.copy_up("src/asset.dat", false).unwrap();
    git::run(&w.depot(), &["add", "overlay/src/asset.dat"]).unwrap();
    w.checkpoint("normalize LFS").unwrap();
    assert!(!w.upper().join("src/asset.dat").exists());
}
#[test]
fn checkpoint_does_not_normalize_away_opaque_children() {
    use gitalator::git;
    use std::io::Write;
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let mut w = w.lock().unwrap();
    w.unlink("src/dir/file", false).unwrap();
    w.unlink("src/dir", true).unwrap();
    w.mkdir("src/dir", 0o755).unwrap();
    w.create_file("src/dir/file", 0o644)
        .unwrap()
        .write_all(b"nested\n")
        .unwrap();
    git::run(&w.depot(), &["add", "-A"]).unwrap();
    w.checkpoint("preserve explicit child").unwrap();
    assert_eq!(
        git::run(&w.depot(), &["show", "HEAD:overlay/src/dir/file"]).unwrap(),
        b"nested\n"
    );
    assert!(w.entry("src/dir/file").is_ok());
}
