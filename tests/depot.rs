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
