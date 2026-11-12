#![allow(dead_code)]
use gitalator::{git, store::Store};
use std::{fs, path::PathBuf};
pub struct Fixture {
    pub temp: tempfile::TempDir,
    pub depot: PathBuf,
    pub component: PathBuf,
    pub state: PathBuf,
}
impl Fixture {
    pub fn new() -> Self {
        fs::create_dir_all(".tmp").unwrap();
        let temp = tempfile::tempdir_in(".tmp").unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let component = root.join("component");
        let depot = root.join("depot");
        for p in [&component, &depot] {
            fs::create_dir(p).unwrap();
            git::run(p, &["init", "-b", "main"]).unwrap();
            git::run(p, &["config", "user.name", "Test"]).unwrap();
            git::run(p, &["config", "user.email", "test@localhost"]).unwrap();
        }
        fs::create_dir(component.join("dir")).unwrap();
        fs::write(component.join("hello"), b"hello\n").unwrap();
        fs::write(component.join("dir/file"), b"nested\n").unwrap();
        commit(&component);
        let pin = git::text(&component, &["rev-parse", "HEAD"]).unwrap();
        fs::write(depot.join("depot.toml"),format!("format=1\n[repositories.code]\nurl={:?}\n[[mounts]]\nid='code'\nrepository='code'\ncommit='{pin}'\npath='src'\n[[mounts]]\nid='other'\nrepository='code'\ncommit='{pin}'\npath='other'\n",component.to_str().unwrap())).unwrap();
        fs::create_dir_all(depot.join("overlay/build")).unwrap();
        fs::write(depot.join("overlay/build/config"), b"config\n").unwrap();
        commit(&depot);
        Self {
            temp,
            depot,
            component,
            state: root.join("state"),
        }
    }
    pub fn store(&self) -> Store {
        let mut s = Store::open(&self.state).unwrap();
        s.create("dev", self.depot.to_str().unwrap(), "main")
            .unwrap();
        let w = s.get("dev").unwrap();
        let w = w.lock().unwrap();
        git::run(&w.depot(), &["config", "user.name", "Test"]).unwrap();
        git::run(&w.depot(), &["config", "user.email", "test@localhost"]).unwrap();
        drop(w);
        s
    }
}
pub fn commit(p: &std::path::Path) {
    git::run(p, &["add", "-A"]).unwrap();
    git::run(p, &["commit", "-m", "fixture"]).unwrap();
}
