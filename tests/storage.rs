use gitalator::{cache::Cache, db::Store, model::*};
fn temp() -> tempfile::TempDir {
    std::fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}
#[test]
fn content_integrity_and_dedup() {
    let d = temp();
    let c = Cache::new(d.path().join("cache")).unwrap();
    let id = c.put(b"hello").unwrap();
    assert_eq!(id, c.put(b"hello").unwrap());
    assert_eq!(c.get(&id).unwrap(), b"hello");
    std::fs::write(c.root.join("objects").join(&id), b"bad").unwrap();
    assert!(c.get(&id).is_err());
    assert!(c.put(b"hello").is_err());
    assert!(c.get("../../etc/passwd").is_err());
}
#[test]
fn snapshots_survive_reopen_and_share_revision() {
    let d = temp();
    let s = Store::open(d.path()).unwrap();
    s.create("a", "invalid draft").unwrap();
    let key = s.cache.put(b"one").unwrap();
    s.apply(
        "a",
        vec![Delta {
            mount: "m".into(),
            path: "a".into(),
            entry: Some(Entry {
                kind: Kind::File,
                mode: 0o644,
                content: key.clone(),
                size: 3,
            }),
        }],
    )
    .unwrap();
    let n = s.snapshot("a").unwrap();
    let old = s.head("a").unwrap();
    s.apply(
        "a",
        vec![Delta {
            mount: "m".into(),
            path: "a".into(),
            entry: None,
        }],
    )
    .unwrap();
    drop(s);
    let s = Store::open(d.path()).unwrap();
    assert_eq!(
        s.snapshot_revision("a", n).unwrap(),
        (old, "invalid draft".into())
    );
    assert!(s.state(old).unwrap().1[&("m".into(), "a".into())].is_some());
    assert!(s.state(s.head("a").unwrap()).unwrap().1[&("m".into(), "a".into())].is_none());
    assert_eq!(s.cache.get(&key).unwrap(), b"one");
    assert_eq!(s.snapshot("a").unwrap(), 2);
}
#[test]
fn failed_transaction_does_not_leave_revision() {
    let d = temp();
    let s = Store::open(d.path()).unwrap();
    s.create("a", "").unwrap();
    assert!(s.create("a", "").is_err());
    let count: i64 = s
        .conn()
        .unwrap()
        .query_row("SELECT count(*) FROM revisions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
#[test]
fn concurrent_cache_writers() {
    let d = temp();
    let c = Cache::new(d.path().join("cache")).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let c = &c;
            scope.spawn(move || {
                assert_eq!(c.get(&c.put(b"shared").unwrap()).unwrap(), b"shared");
            });
        }
    });
    assert_eq!(
        std::fs::read_dir(c.root.join("objects")).unwrap().count(),
        1
    );
}
