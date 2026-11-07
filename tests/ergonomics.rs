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

// Add gitlinks directly: fixtures do not need checked-out submodules.
fn superrepo(parent: &std::path::Path, name: &str, child: &std::path::Path) -> std::path::PathBuf {
    let repo = parent.join(name);
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "user.name", "Test"]).unwrap();
    git(&repo, &["config", "user.email", "test@localhost"]).unwrap();
    let oid = String::from_utf8(git(child, &["rev-parse", "HEAD"]).unwrap()).unwrap();
    std::fs::write(
        repo.join(".gitmodules"),
        format!(
            "[submodule \"child\"]\npath = child\nurl = ../{}\n",
            child.file_name().unwrap().to_str().unwrap()
        ),
    )
    .unwrap();
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},child", oid.trim()),
        ],
    )
    .unwrap();
    git(&repo, &["add", ".gitmodules"]).unwrap();
    git(&repo, &["commit", "-m", "superrepo"]).unwrap();
    repo
}
#[test]
fn nested_expansion_is_pinned_idempotent_and_rejects_staged_changes() {
    let f = Fixture::new();
    let middle = superrepo(f.temp.path(), "middle", &f.repo);
    let top = superrepo(f.temp.path(), "top", &middle);
    let s = &f.store;
    s.new_workspace(
        "dev",
        Some(top.to_str().unwrap()),
        None,
        None,
        Some("project"),
        None,
        false,
    )
    .unwrap();
    assert_eq!(s.state(s.head("dev").unwrap()).unwrap().0.mount.len(), 2);
    // Changing the origin after creation must not refresh the superrepo's active pin.
    std::fs::write(top.join("new"), b"not pinned").unwrap();
    git(&top, &["add", "new"]).unwrap();
    git(&top, &["commit", "-m", "new"]).unwrap();
    assert_eq!(s.expand_submodules("dev", &[], false).unwrap(), 1);
    assert_eq!(
        s.read(&View::Live("dev".into()), "project/child/child/file")
            .unwrap(),
        b"base\n"
    );
    assert!(s.read(&View::Live("dev".into()), "project/new").is_err());
    let head = s.head("dev").unwrap();
    assert_eq!(s.expand_submodules("dev", &[], true).unwrap(), 0);
    assert_eq!(s.head("dev").unwrap(), head);
    s.set_draft("dev", "invalid draft").unwrap();
    assert!(s.expand_submodules("dev", &[], true).is_err());
    assert_eq!(s.head("dev").unwrap(), head);
    s.new_workspace(
        "recursive",
        Some(top.to_str().unwrap()),
        None,
        None,
        Some("project"),
        None,
        true,
    )
    .unwrap();
    assert_eq!(
        s.expand_submodules("recursive", &["project".into()], true)
            .unwrap(),
        2
    );
    // User edits in a not-yet-mounted submodule cone cannot be silently hidden.
    s.new_workspace(
        "edited",
        Some(top.to_str().unwrap()),
        None,
        None,
        Some("project"),
        None,
        true,
    )
    .unwrap();
    s.write_file(
        &View::Live("edited".into()),
        "project/child/local",
        b"precious",
        0o644,
        Kind::File,
    )
    .unwrap();
    let head = s.head("edited").unwrap();
    assert!(s.expand_submodules("edited", &[], false).is_err());
    assert_eq!(s.head("edited").unwrap(), head);
}
