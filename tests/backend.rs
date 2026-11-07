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
    use std::{sync::mpsc, time::Duration};
    let f = Fixture::new();
    let other = f.temp.path().join("other-origin");
    git(
        f.temp.path(),
        &[
            "clone",
            "--bare",
            f.repo.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    )
    .unwrap();
    f.store
        .repo_add(
            "independent",
            std::fs::canonicalize(&other).unwrap().to_str().unwrap(),
        )
        .unwrap();
    let held = f.store.repo_lock("origin").unwrap();
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

#[test]
fn activation_fetches_only_requested_refs_and_defers_blobs() {
    use gitalator::{db::Store, model::Map};
    let f = Fixture::new();
    git(&f.repo, &["config", "uploadpack.allowFilter", "true"]).unwrap();
    git(&f.repo, &["branch", "unrelated"]).unwrap();
    git(&f.repo, &["tag", "unrequested"]).unwrap();
    let pin = String::from_utf8(git(&f.repo, &["rev-parse", "HEAD"]).unwrap())
        .unwrap()
        .trim()
        .to_string();
    let blob = String::from_utf8(git(&f.repo, &["rev-parse", "HEAD:file"]).unwrap())
        .unwrap()
        .trim()
        .to_string();
    let s = Store::open(&f.temp.path().join("lazy-state")).unwrap();
    s.repo_add(
        "source",
        &format!(
            "file://{}",
            std::fs::canonicalize(&f.repo).unwrap().display()
        ),
    )
    .unwrap();
    let mut map =
        Map::parse("format=1\n[[mount]]\nid='root'\nrepo='source'\npath=''\nrevision='main'\n")
            .unwrap();
    s.resolve_map(&map).unwrap();
    let refs = String::from_utf8(
        git(
            &s.repo_path("source").unwrap(),
            &["for-each-ref", "--format=%(refname)"],
        )
        .unwrap(),
    )
    .unwrap();
    assert!(refs.contains("refs/remotes/origin/main"));
    assert!(!refs.contains("unrelated"));
    assert!(!refs.contains("unrequested"));
    let mut check = std::process::Command::new("git");
    check
        .arg("-C")
        .arg(s.repo_path("source").unwrap())
        .args(["cat-file", "-e", &blob])
        .env("GIT_NO_LAZY_FETCH", "1");
    assert!(
        !execute_command(&mut check, None, std::time::Duration::from_secs(10))
            .unwrap()
            .status
            .success()
    );
    // Fetch an exact commit that was never covered by the selected branch fetch.
    std::fs::write(f.repo.join("file"), b"new pin\n").unwrap();
    commit(&f.repo);
    map.mount[0].revision = String::from_utf8(git(&f.repo, &["rev-parse", "HEAD"]).unwrap())
        .unwrap()
        .trim()
        .to_string();
    let resolved = s.resolve_map(&map).unwrap();
    assert_eq!(
        resolved.mount[0].commit.as_ref(),
        Some(&map.mount[0].revision)
    );
    let entry = s.tree(&resolved.mount[0]).unwrap().remove("file").unwrap();
    assert_eq!(s.bytes(&entry).unwrap(), b"new pin\n");
    // Cached immutable pins can be activated without contacting the origin.
    std::fs::rename(&f.repo, f.temp.path().join("offline-origin")).unwrap();
    map.mount[0].revision = pin;
    s.resolve_map(&map).unwrap();
}

#[test]
fn legacy_cached_git_directories_are_searchable() {
    let f = Fixture::new();
    let (map, _) = f.store.view_state(&View::Live("a".into())).unwrap();
    let mut tree = f.store.tree(&map.mount[0]).unwrap();
    assert_eq!(tree["nested"].mode, 0o755);
    tree.get_mut("nested").unwrap().mode = 0;
    f.store
        .conn()
        .unwrap()
        .execute(
            "UPDATE tree_cache SET data=?1",
            [serde_json::to_string(&tree).unwrap()],
        )
        .unwrap();
    let reopened = gitalator::db::Store::open(&f.store.root).unwrap();
    assert_eq!(
        reopened
            .entry(&View::Live("a".into()), "nested")
            .unwrap()
            .mode,
        0o755
    );
}

#[test]
fn metadata_fetch_repairs_missing_promised_blobs_and_coalesces_with_reads() {
    use gitalator::{db::Store, model::Map};
    let f = Fixture::new();
    git(&f.repo, &["config", "uploadpack.allowFilter", "true"]).unwrap();
    let s = Store::open(&f.temp.path().join("missing-blobs")).unwrap();
    s.repo_add(
        "source",
        &format!(
            "file://{}",
            std::fs::canonicalize(&f.repo).unwrap().display()
        ),
    )
    .unwrap();
    let map =
        Map::parse("format=1\n[[mount]]\nid='root'\nrepo='source'\npath=''\nrevision='main'\n")
            .unwrap();
    let resolved = s.resolve_map(&map).unwrap();
    let entries = s.tree(&resolved.mount[0]).unwrap();
    let blobs: Vec<_> = entries
        .values()
        .filter(|e| e.kind == Kind::File)
        .cloned()
        .collect();
    // These commits are already present, while their blobs are promised/missing.
    for e in &blobs {
        let oid = e.content.rsplit(':').next().unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(s.repo_path("source").unwrap())
            .args(["cat-file", "--batch-check"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                writeln!(child.stdin.take().unwrap(), "{oid}")?;
                child.wait_with_output()
            })
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("missing"));
    }
    // Exercise a repository configured for ordinary skipping negotiation.
    git(
        &s.repo_path("source").unwrap(),
        &["config", "fetch.negotiationAlgorithm", "skipping"],
    )
    .unwrap();
    std::thread::scope(|scope| {
        let a = scope.spawn(|| s.resolve_sizes(&blobs));
        let b = scope.spawn(|| s.bytes(&entries["file"]));
        a.join().unwrap().unwrap();
        assert_eq!(b.join().unwrap().unwrap(), b"base\n");
    });
    assert_eq!(s.known_size(&entries["file"]).unwrap(), Some(5));
    assert_eq!(s.known_size(&entries["nested/hidden"]).unwrap(), Some(7));
    let log = std::fs::read_to_string(s.root.join("operations.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(records
        .iter()
        .any(|r| r["operation"] == "fetch-blobs:source" && r["status"] == "succeeded"));
    assert!(!records
        .iter()
        .any(|r| r["operation"] == "fetch-blobs:source" && r["status"] == "failed"));
    // Once known, offline repeats do not fetch or log backend work.
    std::fs::rename(&f.repo, f.temp.path().join("offline")).unwrap();
    let before = s.metrics();
    s.resolve_sizes(&blobs).unwrap();
    assert_eq!(s.metrics()["metadata_fetches"], before["metadata_fetches"]);
}
