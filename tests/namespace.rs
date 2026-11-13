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
    let mut s = Store::open(&f.state).unwrap();
    s.trees = Arc::new(Trees::with_budget(900));
    let ns = s.prepare_namespace(&map).unwrap();
    for _ in 0..3 {
        assert!(ns.lookup("src/dir/file").unwrap().is_some());
        assert!(ns.children("src").unwrap().contains_key("src/hello"));
        assert!(s.trees.stats()["resident_bytes"].as_u64().unwrap() <= 900);
        assert_eq!(s.trees.stats()["resident_trees"], 1);
    }
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

#[test]
fn concurrent_directory_misses_coalesce_and_cancellation_retries() {
    let f = Fixture::new();
    let s = Store::open(&f.state).unwrap();
    let map = Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    let ns = s.prepare_namespace(&map).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let ns = ns.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ns.lookup("src/dir/file").unwrap().unwrap().oid
            })
        })
        .collect();
    let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(s.trees.stats()["tree_objects_loaded"], 2);
    let task = gitalator::activity::Task::new("cancel tree lookup");
    task.cancel();
    {
        let _context = task.enter();
        assert!(gitalator::activity::interrupted(
            &*ns.children("src/dir").unwrap_err()
        ));
    }
    assert!(ns.children("src/dir").unwrap().contains_key("src/dir/file"));
}

#[test]
fn binary_tree_parser_supports_sha256() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repos/example");
    fs::create_dir_all(&repo).unwrap();
    gitalator::git::run(&repo, &["init", "--bare", "--object-format=sha256"]).unwrap();
    let blob = String::from_utf8(
        gitalator::git::input(&repo, &["hash-object", "-w", "--stdin"], Some(b"value")).unwrap(),
    )
    .unwrap()
    .trim()
    .to_string();
    let tree = String::from_utf8(
        gitalator::git::input(
            &repo,
            &["mktree"],
            Some(format!("100644 blob {blob}\tname\n").as_bytes()),
        )
        .unwrap(),
    )
    .unwrap()
    .trim()
    .to_string();
    let cache = Trees::with_budget(4096);
    let entries = cache.get(temp.path(), "example", &tree).unwrap();
    assert_eq!(entries["name"].oid, blob);
    assert_eq!(entries["name"].oid.len(), 64);
}
