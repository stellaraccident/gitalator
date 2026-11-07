mod common;
use common::*;
use gitalator::{db::Store, model::Kind, ops::View};
#[test]
fn crash_child() {
    let Ok(root) = std::env::var("GTL_CRASH_STATE") else {
        return;
    };
    let s = Store::open(std::path::Path::new(&root)).unwrap();
    match std::env::var("GTL_CRASH_ACTION").unwrap().as_str() {
        "write" => s
            .write_file(
                &View::Live("a".into()),
                "file",
                b"crashed\n",
                0o644,
                Kind::File,
            )
            .unwrap(),
        "snapshot" => {
            s.snapshot("a").unwrap();
        }
        "activate" => {
            s.finish_activation(&std::env::var("GTL_CRASH_TX").unwrap(), false)
                .unwrap();
        }
        _ => panic!(),
    }
}
fn crash(f: &Fixture, point: &str, action: &str, tx: &str) {
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--nocapture"])
        .env("GTL_CRASH_STATE", &f.store.root)
        .env("GTL_FAILPOINT", point)
        .env("GTL_CRASH_ACTION", action)
        .env("GTL_CRASH_TX", tx)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
#[test]
fn interrupted_payload_and_revision_preserve_old_view() {
    for point in ["payload-after-publish", "revision-before-commit"] {
        let f = Fixture::new();
        let old = f.store.head("a").unwrap();
        crash(&f, point, "write", "");
        let s = Store::open(&f.store.root).unwrap();
        assert_eq!(s.head("a").unwrap(), old);
        assert_eq!(s.read(&View::Live("a".into()), "file").unwrap(), b"base\n");
    }
}
#[test]
fn interrupted_snapshot_is_not_published() {
    let f = Fixture::new();
    let old = f.store.snapshots("a").unwrap();
    crash(&f, "snapshot-before-commit", "snapshot", "");
    assert_eq!(
        Store::open(&f.store.root).unwrap().snapshots("a").unwrap(),
        old
    );
}
#[test]
fn activation_publication_is_atomic() {
    for point in ["activation-before-commit", "activation-after-commit"] {
        let f = Fixture::new();
        let old = f.store.head("a").unwrap();
        std::fs::write(f.repo.join("file"), b"new\n").unwrap();
        commit(&f.repo);
        let tx = f.store.activate("a").unwrap();
        crash(&f, point, "activate", &tx);
        let s = Store::open(&f.store.root).unwrap();
        if point == "activation-before-commit" {
            assert_eq!(s.head("a").unwrap(), old);
            assert!(s.frozen("a").unwrap());
            s.finish_activation(&tx, false).unwrap();
        } else {
            assert_ne!(s.head("a").unwrap(), old);
            assert!(!s.frozen("a").unwrap());
        }
        assert_eq!(s.read(&View::Live("a".into()), "file").unwrap(), b"new\n");
    }
}
