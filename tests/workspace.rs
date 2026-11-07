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
#[test]
fn import_reconciles_advanced_workspace_through_conflicts() {
    let f = Fixture::new();
    let s = &f.store;
    let v = View::Live("a".into());
    s.write_file(&v, "file", b"exported\n", 0o644, Kind::File)
        .unwrap();
    let export = f.temp.path().join("export");
    s.export("a", &export).unwrap();
    std::fs::write(export.join("root/file"), b"review edit\n").unwrap();
    s.write_file(&v, "file", b"local edit\n", 0o644, Kind::File)
        .unwrap();
    let value = gitalator::cli::dispatch(
        s,
        gitalator::cli::Cmd::Import {
            workspace: "a".into(),
            export,
        },
    )
    .unwrap();
    assert_eq!(value["state"], "CONFLICTED");
    let id = value["transaction"].as_str().unwrap();
    s.write_file(
        &View::Transaction(id.into()),
        "file",
        b"resolved import\n",
        0o644,
        Kind::File,
    )
    .unwrap();
    s.resolve_conflict(id, "root", "file", false).unwrap();
    s.finish_activation(id, false).unwrap();
    assert_eq!(s.read(&v, "file").unwrap(), b"resolved import\n");
}
#[test]
fn bundle_reproduces_export_tree_without_original_state() {
    let f = Fixture::new();
    f.store
        .write_file(
            &View::Live("a".into()),
            "file",
            b"bundled\n",
            0o644,
            Kind::File,
        )
        .unwrap();
    let export = f.temp.path().join("export");
    f.store.export("a", &export).unwrap();
    let bundle = f.temp.path().join("bundle");
    gitalator::workflow::bundle(&export, &bundle).unwrap();
    let receiver = f.temp.path().join("receiver");
    std::fs::create_dir(&receiver).unwrap();
    git(
        &receiver,
        &[
            "clone",
            std::fs::canonicalize(bundle.join("root.bundle"))
                .unwrap()
                .to_str()
                .unwrap(),
            ".",
        ],
    )
    .unwrap();
    assert_eq!(std::fs::read(receiver.join("file")).unwrap(), b"bundled\n");
}

#[test]
fn import_new_directories_and_review_plan_are_local() {
    let f = Fixture::new();
    let s = &f.store;
    let v = View::Live("a".into());
    s.write_file(&v, "file", b"checkpoint\n", 0o644, Kind::File)
        .unwrap();
    let export = f.temp.path().join("export");
    s.export("a", &export).unwrap();
    let before = git(&f.repo, &["show-ref"]).unwrap();
    let plan = gitalator::workflow::review(s, &export, false, None).unwrap();
    assert_eq!(plan["publish"], false);
    assert_eq!(git(&f.repo, &["show-ref"]).unwrap(), before);
    std::fs::create_dir_all(export.join("root/new/deep")).unwrap();
    std::fs::write(export.join("root/new/deep/file"), b"new\n").unwrap();
    gitalator::cli::dispatch(
        s,
        gitalator::cli::Cmd::Import {
            workspace: "a".into(),
            export,
        },
    )
    .unwrap();
    assert_eq!(s.entry(&v, "new").unwrap().kind, Kind::Directory);
    assert_eq!(s.read(&v, "new/deep/file").unwrap(), b"new\n");
}

#[test]
fn layout_branches_keep_independent_ancestry_and_tags_resolve() {
    let f = Fixture::new();
    let s = &f.store;
    git(&f.repo, &["tag", "v1"]).unwrap();
    s.create(
        "b",
        &s.draft("a").unwrap().replace("'main'", "'refs/tags/v1'"),
    )
    .unwrap();
    let tx = s.activate("b").unwrap();
    s.finish_activation(&tx, false).unwrap();
    let layouts = s.root.join("layouts");
    assert_eq!(
        git(&layouts, &["rev-list", "--count", "b"]).unwrap(),
        b"1\n"
    );
    let tx = s.activate("a").unwrap();
    s.finish_activation(&tx, false).unwrap();
    assert_eq!(
        git(&layouts, &["rev-list", "--count", "a"]).unwrap(),
        b"2\n"
    );
    assert_eq!(
        git(&layouts, &["rev-list", "--count", "b"]).unwrap(),
        b"1\n"
    );
}
