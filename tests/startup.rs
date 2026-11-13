mod common;
use common::Fixture;
use gitalator::{model::Map, store::Store};
use std::{fs, time::Instant};

#[test]
fn index_cache_is_disposable_and_keyed_by_complete_layout() {
    let f = Fixture::new();
    let s = f.store();
    let map = s.get("dev").unwrap().lock().unwrap().record.map.clone();
    let expected = serde_json::to_value(s.load_base(&map).unwrap()).unwrap();
    let cache = fs::read_dir(f.state.join("indexes"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&cache, b"truncated cache").unwrap();
    assert_eq!(
        serde_json::to_value(s.load_base(&map).unwrap()).unwrap(),
        expected
    );
    assert_eq!(
        serde_json::to_value(s.load_base(&map).unwrap()).unwrap(),
        expected
    );
    let mut moved = map.clone();
    moved.mounts[0].path = "relocated".into();
    let base = s.load_base(&moved).unwrap();
    assert!(base.contains_key("relocated/hello"));
    assert!(!base.contains_key("src/hello"));
    drop(s);
    let reopened = Store::open(&f.state).unwrap();
    assert_eq!(
        serde_json::to_value(&reopened.get("dev").unwrap().lock().unwrap().base).unwrap(),
        expected
    );
}

#[test]
fn cached_index_does_not_bypass_overlay_validation() {
    let f = Fixture::new();
    let s = f.store();
    let w = s.get("dev").unwrap();
    let upper = w.lock().unwrap().upper();
    fs::create_dir_all(upper.join("src")).unwrap();
    fs::write(upper.join("src/.mountgit"), "collision").unwrap();
    drop(s);
    let error = Store::open(&f.state)
        .err()
        .expect("invalid overlay must be rejected");
    assert!(error.to_string().contains("invalid overlay path"));
}

// Run against a real layout without opening/changing its workspace records.
// Only derived index caches and normal shared Git repository configuration change.
#[test]
#[ignore]
fn benchmark_layout_index() {
    let state = std::path::PathBuf::from(std::env::var_os("GTL_BENCH_STATE").unwrap());
    let manifest = std::env::var_os("GTL_BENCH_MANIFEST").unwrap();
    let map = Map::parse(&fs::read_to_string(manifest).unwrap()).unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    store.root = state;
    for pass in 1..=2 {
        let start = Instant::now();
        let base = store.load_base(&map).unwrap();
        eprintln!(
            "layout pass {pass}: {} mounts, {} entries, {:.3}s",
            map.mounts.len(),
            base.len(),
            start.elapsed().as_secs_f64()
        );
    }
}
