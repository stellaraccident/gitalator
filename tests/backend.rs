mod common;
use common::*;
use gitalator::{backend::*, model::Kind, ops::View};
#[test]
fn deadline_kills_backend() {
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("10");
    let now = std::time::Instant::now();
    assert!(
        execute_command(&mut cmd, None, std::time::Duration::from_millis(50))
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    assert!(now.elapsed() < std::time::Duration::from_secs(2));
}
#[test]
fn cache_eviction_preserves_workspace_and_offline_read() {
    let f = Fixture::new();
    let s = &f.store;
    let v = View::Live("a".into());
    assert_eq!(s.read(&v, "file").unwrap(), b"base\n");
    s.write_file(&v, "file", b"base\n", 0o644, Kind::File)
        .unwrap();
    s.gc(false, 0).unwrap();
    assert_eq!(s.read(&v, "file").unwrap(), b"base\n");
    std::fs::rename(&f.repo, f.temp.path().join("unavailable")).unwrap();
    assert_eq!(s.read(&View::Base("a".into()), "file").unwrap(), b"base\n");
}
#[test]
fn concurrent_hydration_is_coalesced() {
    let f = Fixture::new();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let s = &f.store;
            scope.spawn(move || {
                assert_eq!(s.read(&View::Live("a".into()), "file").unwrap(), b"base\n")
            });
        }
    });
    let count: i64 = f
        .store
        .conn()
        .unwrap()
        .query_row("SELECT count(*) FROM objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
#[test]
fn metadata_enumeration_does_not_hydrate() {
    let f = Fixture::new();
    let s = &f.store;
    s.entries(&View::Live("a".into())).unwrap();
    assert_eq!(
        s.conn()
            .unwrap()
            .query_row("SELECT count(*) FROM objects", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn local_lfs_payload_hydration() {
    let f = Fixture::new();
    git(&f.repo, &["lfs", "install", "--local", "--skip-repo"]).unwrap();
    git(&f.repo, &["lfs", "track", "*.bin"]).unwrap();
    std::fs::write(f.repo.join("payload.bin"), b"local-lfs-payload").unwrap();
    commit(&f.repo);
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
    assert_eq!(
        f.store
            .read(&View::Live("a".into()), "payload.bin")
            .unwrap(),
        b"local-lfs-payload"
    );
    f.store
        .write_file(
            &View::Live("a".into()),
            "file",
            b"export me\n",
            0o644,
            Kind::File,
        )
        .unwrap();
    let export = f.temp.path().join("export");
    f.store.export("a", &export).unwrap();
    assert_eq!(
        std::fs::read(export.join("root/payload.bin")).unwrap(),
        b"local-lfs-payload"
    );
    let bundle = f.temp.path().join("bundle");
    gitalator::workflow::bundle(&export, &bundle).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(bundle.join("gitalator-export.json")).unwrap())
            .unwrap();
    assert_eq!(
        manifest["repositories"][0]["lfs_payloads"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn repository_locks_serialize_shared_work_without_blocking_other_repos() {
    use gitalator::cache::Lock;
    use std::{sync::mpsc, time::Duration};
    let f = Fixture::new();
    f.store
        .repo_add(
            "independent",
            std::fs::canonicalize(&f.repo).unwrap().to_str().unwrap(),
        )
        .unwrap();
    let held = Lock::take(&f.store.root.join("locks/repo-origin")).unwrap();
    let (send, recv) = mpsc::channel();
    std::thread::scope(|scope| {
        let s = &f.store;
        let send_other = send.clone();
        scope.spawn(move || {
            s.fetch("independent").unwrap();
            send_other.send("other").unwrap();
        });
        scope.spawn(move || {
            s.fetch("origin").unwrap();
            send.send("same").unwrap();
        });
        assert_eq!(recv.recv_timeout(Duration::from_secs(10)).unwrap(), "other");
        assert!(recv.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        assert_eq!(recv.recv_timeout(Duration::from_secs(10)).unwrap(), "same");
    });
    let log = std::fs::read_to_string(f.store.root.join("operations.jsonl")).unwrap();
    assert!(log
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .any(|v| v["operation"] == "fetch:independent"
            && v["status"] == "succeeded"
            && v["timestamp_ms"].is_number()));
}
