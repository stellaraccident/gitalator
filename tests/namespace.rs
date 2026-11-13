mod common;
use common::{commit, Fixture};
use gitalator::{model::Map, store::Store, trees::Trees};
use std::{fs, sync::Arc};

#[test]
fn immediate_trees_are_lazy_shared_and_disposable() {
    let f = Fixture::new();
    let map = Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    let s = Store::open(&f.state).unwrap();
    let ns = s.prepare_namespace(&map).unwrap();
    assert_eq!(s.trees.stats()["tree_objects_loaded"], 1);
    assert_eq!(ns.children("src").unwrap().len(), 2);
    assert_eq!(s.trees.stats()["tree_objects_loaded"], 1);
    assert!(ns.lookup("src/dir/file").unwrap().is_some());
    assert_eq!(s.trees.stats()["tree_objects_loaded"], 2);
    assert!(ns.lookup("other/dir/file").unwrap().is_some());
    assert_eq!(s.trees.stats()["tree_objects_loaded"], 2);
    let base = ns.lookup("src").unwrap().unwrap();
    let disk = f
        .state
        .join("tree-cache/v1/trees")
        .join(&base.repo)
        .join(format!("{}.json", base.oid));
    fs::write(disk, b"truncated").unwrap();
    let mut s = Store::open(&f.state).unwrap();
    s.trees = Arc::new(Trees::with_budget(1));
    let ns = s.prepare_namespace(&map).unwrap();
    assert!(ns.lookup("src/hello").unwrap().is_some());
    assert_eq!(s.trees.stats()["resident_bytes"], 0);
    assert_eq!(s.trees.stats()["tree_objects_loaded"], 1);
}

#[test]
fn invalid_untouched_directory_is_deferred_but_full_validation_finds_it() {
    use std::os::unix::ffi::OsStrExt;
    let f = Fixture::new();
    fs::write(
        f.component
            .join("dir")
            .join(std::ffi::OsStr::from_bytes(b"invalid-\xff")),
        b"data",
    )
    .unwrap();
    commit(&f.component);
    let pin = gitalator::git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let mut map = Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    for m in &mut map.mounts {
        m.commit = pin.clone();
    }
    let s = Store::open(&f.state).unwrap();
    let ns = s.prepare_namespace(&map).unwrap();
    assert!(ns.lookup("src/hello").unwrap().is_some());
    let error = ns.children("src/dir").unwrap_err().to_string();
    assert!(error.contains("src/dir"), "{error}");
    assert!(ns.validate_full().is_err());
}
