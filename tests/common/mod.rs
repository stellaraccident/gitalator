#![allow(dead_code)]
use gitalator::{backend::git, db::Store};
use std::path::Path;
pub struct Fixture {
    pub temp: tempfile::TempDir,
    pub store: Store,
    pub repo: std::path::PathBuf,
}
impl Fixture {
    pub fn new() -> Self {
        std::fs::create_dir_all(".tmp").unwrap();
        let temp = tempfile::tempdir_in(".tmp").unwrap();
        let repo = temp.path().join("origin");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "Test"]).unwrap();
        git(&repo, &["config", "user.email", "test@localhost"]).unwrap();
        std::fs::write(repo.join("file"), b"base\n").unwrap();
        std::fs::create_dir_all(repo.join("nested")).unwrap();
        std::fs::write(repo.join("nested/hidden"), b"hidden\n").unwrap();
        commit(&repo);
        let store = Store::open(&temp.path().join("state")).unwrap();
        store
            .repo_add(
                "origin",
                std::fs::canonicalize(&repo).unwrap().to_str().unwrap(),
            )
            .unwrap();
        store
            .create(
                "a",
                "format=1\n[[mount]]\nid='root'\nrepo='origin'\npath=''\nrevision='main'\n",
            )
            .unwrap();
        let id = store.activate("a").unwrap();
        store.finish_activation(&id, false).unwrap();
        Self { temp, store, repo }
    }
}
pub fn commit(repo: &Path) {
    git(repo, &["add", "-A"]).unwrap();
    git(repo, &["commit", "-m", "test"]).unwrap();
}
