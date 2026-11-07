mod common;
use common::*;
use gitalator::{backend::git, db::Store, ergonomics::canonical_remote, model::Kind, ops::View};
#[test]
fn remote_aliases_share_storage_and_transport_stays_distinct() {
    let f = Fixture::new();
    f.store.repo_add("alias", f.repo.to_str().unwrap()).unwrap();
    assert_eq!(
        f.store.repo_path("alias").unwrap(),
        f.store.repo_path("origin").unwrap()
    );
    assert_eq!(
        canonical_remote("https://GITHUB.COM/ROCm/TheRock.git/").unwrap(),
        canonical_remote("https://github.com/ROCm/TheRock").unwrap()
    );
    assert_ne!(
        canonical_remote("https://github.com/ROCm/TheRock.git").unwrap(),
        canonical_remote("ssh://git@github.com/ROCm/TheRock.git").unwrap()
    );
    assert!(canonical_remote("https://user:secret@example.com/repo").is_err());
}
#[test]
fn like_is_clean_clone_preserves_edits_and_draft() {
    let f = Fixture::new();
    let s = &f.store;
    s.write_file(
        &View::Live("a".into()),
        "file",
        b"edited",
        0o644,
        Kind::File,
    )
    .unwrap();
    s.set_draft("a", "invalid staged draft").unwrap();
    s.new_workspace("fresh", None, Some("a"), None, None, None, false)
        .unwrap();
    s.new_workspace("copy", None, None, Some("a"), None, None, false)
        .unwrap();
    assert_eq!(
        s.read(&View::Live("fresh".into()), "file").unwrap(),
        b"base\n"
    );
    assert_eq!(
        s.read(&View::Live("copy".into()), "file").unwrap(),
        b"edited"
    );
    assert_eq!(s.draft("copy").unwrap(), "invalid staged draft");
    assert!(gitalator::model::Map::parse(&s.draft("fresh").unwrap()).is_ok());
    s.write_file(
        &View::Live("copy".into()),
        "file",
        b"independent",
        0o644,
        Kind::File,
    )
    .unwrap();
    assert_eq!(s.read(&View::Live("a".into()), "file").unwrap(), b"edited");
}
#[test]
fn superrepo_creation_includes_root_and_immediate_submodules() {
    let f = Fixture::new();
    let superrepo = f.temp.path().join("super");
    std::fs::create_dir(&superrepo).unwrap();
    git(&superrepo, &["init", "-b", "main"]).unwrap();
    git(&superrepo, &["config", "user.name", "Test"]).unwrap();
    git(&superrepo, &["config", "user.email", "test@localhost"]).unwrap();
    git(
        &superrepo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            std::fs::canonicalize(&f.repo).unwrap().to_str().unwrap(),
            "child",
        ],
    )
    .unwrap();
    std::fs::write(superrepo.join("root-file"), b"root").unwrap();
    commit(&superrepo);
    let s = Store::open(&f.temp.path().join("user-state")).unwrap();
    s.new_workspace(
        "dev",
        Some(superrepo.to_str().unwrap()),
        None,
        None,
        Some("project"),
        None,
        false,
    )
    .unwrap();
    assert_eq!(
        s.read(&View::Live("dev".into()), "project/root-file")
            .unwrap(),
        b"root"
    );
    assert_eq!(
        s.read(&View::Live("dev".into()), "project/child/file")
            .unwrap(),
        b"base\n"
    );
    assert_eq!(s.state(s.head("dev").unwrap()).unwrap().0.mount.len(), 2);
    assert_eq!(s.expand_submodules("dev", &[], false).unwrap(), 0);
    assert!(s
        .new_workspace(
            "broken",
            Some(superrepo.to_str().unwrap()),
            None,
            None,
            None,
            Some("missing"),
            false
        )
        .is_err());
    assert!(!s.names().unwrap().contains(&"broken".to_string()));
}
