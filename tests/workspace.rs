mod common;
use common::*;
use gitalator::{backend::git, model::Kind, ops::View};
#[test]
fn lazy_read_edit_snapshot_export() {
    let f = Fixture::new();
    let s = &f.store;
    let v = View::Live("a".into());
    let count: i64 = s
        .conn()
        .unwrap()
        .query_row("SELECT count(*) FROM objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(s.read(&v, "file").unwrap(), b"base\n");
    let n = s.snapshot("a").unwrap();
    s.write_file(&v, "file", b"ours\n", 0o644, Kind::File)
        .unwrap();
    assert_eq!(
        s.read(&View::Snapshot("a".into(), n, false), "file")
            .unwrap(),
        b"base\n"
    );
    assert_eq!(s.read(&View::Base("a".into()), "file").unwrap(), b"base\n");
    let dest = f.temp.path().join("export");
    s.export("a", &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("root/file")).unwrap(), b"ours\n");
    assert!(
        String::from_utf8(git(&dest.join("root"), &["status", "--porcelain"]).unwrap())
            .unwrap()
            .is_empty()
    );
    s.restore("a", n).unwrap();
    assert_eq!(s.read(&v, "file").unwrap(), b"base\n");
}
#[test]
fn activation_conflict_abort_and_resume() {
    let f = Fixture::new();
    let s = &f.store;
    let v = View::Live("a".into());
    s.write_file(&v, "file", b"ours\n", 0o644, Kind::File)
        .unwrap();
    std::fs::write(f.repo.join("file"), b"theirs\n").unwrap();
    commit(&f.repo);
    let id = s.activate("a").unwrap();
    assert_eq!(s.conflict_list(&id).unwrap().len(), 1);
    assert!(s.write_file(&v, "file", b"bad", 0o644, Kind::File).is_err());
    s.set_draft("a", "unfinished newer draft").unwrap();
    s.write_file(
        &View::Transaction(id.clone()),
        "file",
        b"resolved\n",
        0o644,
        Kind::File,
    )
    .unwrap();
    s.resolve_conflict(&id, "root", "file", false).unwrap();
    s.finish_activation(&id, false).unwrap();
    assert_eq!(s.draft("a").unwrap(), "unfinished newer draft");
    assert_eq!(s.read(&v, "file").unwrap(), b"resolved\n");
    assert_eq!(
        s.read(&View::Base("a".into()), "file").unwrap(),
        b"theirs\n"
    );
    assert!(s.activate("a").is_err());
}
#[test]
fn nested_mount_does_not_fall_through_or_delete_parent() {
    let f = Fixture::new();
    let s = &f.store;
    let draft=format!("{}\n[[mount]]\nid='child'\nrepo='origin'\npath='nested'\nsubtree='nested'\nrevision='main'",s.draft("a").unwrap());
    s.set_draft("a", &draft).unwrap();
    let id = s.activate("a").unwrap();
    s.finish_activation(&id, false).unwrap();
    let v = View::Live("a".into());
    assert!(s.mutate(&v, "nested", None).is_err());
    s.mutate(&v, "nested/hidden", None).unwrap();
    assert!(s.read(&v, "nested/hidden").is_err());
    assert_eq!(
        s.read(&View::Base("a".into()), "nested/hidden").unwrap(),
        b"hidden\n"
    );
}
#[test]
fn activation_abort_preserves_original() {
    let f = Fixture::new();
    let s = &f.store;
    let old = s.head("a").unwrap();
    let id = s.activate("a").unwrap();
    s.finish_activation(&id, true).unwrap();
    assert_eq!(s.head("a").unwrap(), old);
    assert!(!s.frozen("a").unwrap());
}
