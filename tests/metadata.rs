mod common;
use common::*;
use gitalator::{
    metadata::{cached_only, Runtime},
    model::Kind,
    ops::View,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[test]
fn warm_metadata_has_no_database_git_or_payload_work() {
    let f = Fixture::new();
    let s = &f.store;
    s.runtime.managed.store(true, Ordering::Release);
    let view = View::Live("a".into());
    let warm = s.warmup("a", &[], true).unwrap();
    assert_eq!(warm["failures"], serde_json::json!([]));
    let state = s.view_snapshot(&view).unwrap();
    s.directory_metadata(&state, "").unwrap();
    let before = s.metrics();
    for _ in 0..100 {
        cached_only(|| {
            let state = s.view_snapshot(&view)?;
            let entries = s.directory_metadata(&state, "")?;
            assert!(entries.contains_key("nested"));
            let file = s.lookup_metadata(&state, "file")?;
            assert_eq!(s.exact_size(&file)?, 5);
            Ok(())
        })
        .unwrap();
    }
    let after = s.metrics();
    for counter in [
        "sql_leases",
        "sql_writes",
        "payload_reads",
        "metadata_fetches",
        "revision_replays",
        "directory_scans",
    ] {
        assert_eq!(before[counter], after[counter], "{counter}");
    }
    assert_eq!(after["payload_reads"], 0);
}

#[test]
fn sizes_distinguish_empty_unknown_and_lfs_without_payloads() {
    let f = Fixture::new();
    std::fs::write(f.repo.join("empty"), b"").unwrap();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 987654321\n",
        "a".repeat(64)
    );
    std::fs::write(f.repo.join("pointer"), pointer).unwrap();
    commit(&f.repo);
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
    let view = View::Live("a".into());
    let empty = f.store.entry(&view, "empty").unwrap();
    assert_eq!(f.store.known_size(&empty).unwrap(), None);
    assert_eq!(f.store.exact_size(&empty).unwrap(), 0);
    let ptr = f.store.entry(&view, "pointer").unwrap();
    assert_eq!(f.store.exact_size(&ptr).unwrap(), 987654321);
    assert_eq!(f.store.metrics()["payload_reads"], 0);
    f.store.gc(false, 0).unwrap();
    let reopened = gitalator::db::Store::open(&f.store.root).unwrap();
    assert_eq!(reopened.known_size(&empty).unwrap(), Some(0));
    assert_eq!(reopened.known_size(&ptr).unwrap(), Some(987654321));
}

#[test]
fn warmup_is_nonrecursive_and_rejects_escape_paths() {
    let f = Fixture::new();
    let result = f.store.warmup("a", &[], false).unwrap();
    assert_eq!(result["directories"], 1);
    assert_eq!(result["files"], 1);
    assert!(f.store.warmup("a", &["../outside".into()], true).is_err());
    assert!(f.store.warmup("a", &["/outside".into()], true).is_err());
    assert_eq!(
        f.store.warmup("a", &["missing".into()], false).unwrap()["failures"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn immutable_views_survive_publication_and_parent_time_changes() {
    let f = Fixture::new();
    f.store.runtime.managed.store(true, Ordering::Release);
    let view = View::Live("a".into());
    let old = f.store.view_snapshot(&view).unwrap();
    let old_parent = old.time("nested");
    f.store
        .write_file(&view, "nested/new", b"hello", 0o644, Kind::File)
        .unwrap();
    let new = f.store.view_snapshot(&view).unwrap();
    assert_ne!(old.revision, new.revision);
    assert!(new.time("nested") > old_parent);
    assert!(f.store.lookup_metadata(&old, "nested/new").is_err());
    assert_eq!(f.store.lookup_metadata(&new, "nested/new").unwrap().size, 5);
    assert_eq!(old.time("file"), new.time("file"));
}

#[test]
fn pinned_cache_is_bounded_and_evictable_after_release() {
    let runtime = Runtime::default();
    runtime.limit(256);
    let pinned = runtime.put("a".into(), vec![1u8; 128], 200).unwrap();
    assert!(runtime.put("b".into(), 2u64, 100).is_err());
    drop(pinned);
    assert_eq!(*runtime.put("b".into(), 2u64, 100).unwrap(), 2);
    assert!(runtime.get::<Vec<u8>>("a").is_none());
}

#[test]
fn publication_barrier_serializes_and_retries_stale_responses() {
    let runtime = Arc::new(Runtime::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    *runtime.invalidator.lock().unwrap() = Some(Arc::new(move || {
        c.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }));
    runtime.observe(1, 1);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let r = runtime.clone();
            scope.spawn(move || r.flush().unwrap());
        }
    });
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let mut tries = 0;
    gitalator::workers::complete(
        &runtime,
        || {
            tries += 1;
            if tries == 1 {
                runtime.observe(2, 2);
            }
            Ok(tries)
        },
        |result| assert_eq!(result.unwrap(), 2),
    );
}

#[test]
fn migration_preserves_v1_backup_and_checkpoint_replays() {
    let f = Fixture::new();
    let original = f.store.head("a").unwrap();
    // v1 authoritative tables are unchanged; remove only v2 derived schema.
    let c = f.store.conn().unwrap();
    let triggers: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'metadata_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for trigger in triggers {
        c.execute_batch(&format!("DROP TRIGGER {trigger}")).unwrap();
    }
    for table in [
        "metadata_entries",
        "metadata_trees",
        "metadata_sizes",
        "metadata_checkpoints",
        "revision_stamps",
        "metadata_epoch",
    ] {
        c.execute_batch(&format!("DROP TABLE {table}")).unwrap();
    }
    c.execute_batch("PRAGMA user_version=1").unwrap();
    drop(c);
    let s = gitalator::db::Store::open(&f.store.root).unwrap();
    let backup = rusqlite::Connection::open(s.root.join("schema-v1-backup.sqlite3")).unwrap();
    assert_eq!(
        backup
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(s.head("a").unwrap(), original);
    assert_eq!(s.read(&View::Live("a".into()), "file").unwrap(), b"base\n");
    // Force a checkpoint id without creating a thousand fsyncs.
    s.conn().unwrap().execute("INSERT INTO revisions(id,parent,map,delta) SELECT 1024,head,(SELECT map FROM revisions WHERE id=head),'[]' FROM workspaces WHERE name='a'", []).unwrap();
    s.revision_snapshot(1024).unwrap();
    let reopened = gitalator::db::Store::open(&s.root).unwrap();
    let snapshot = reopened.revision_snapshot(1024).unwrap();
    assert_eq!(snapshot.revision, 1024);
    assert_eq!(reopened.metrics()["revision_replays"], 0);
}

#[test]
fn workers_bound_queue_and_do_not_block_cached_operations() {
    use std::sync::{Condvar, Mutex};
    let runtime = Arc::new(Runtime::default());
    let workers = gitalator::workers::Workers::new(runtime.clone());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (started, receiver) = std::sync::mpsc::channel();
    for _ in 0..4 {
        let gate = gate.clone();
        let started = started.clone();
        assert!(workers
            .submit(Box::new(move || {
                started.send(()).unwrap();
                let (lock, wake) = &*gate;
                let mut ready = lock.lock().unwrap();
                while !*ready {
                    ready = wake.wait(ready).unwrap();
                }
            }))
            .is_ok());
    }
    for _ in 0..4 {
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
    for _ in 0..128 {
        assert!(workers.submit(Box::new(|| {})).is_ok());
    }
    assert!(workers.submit(Box::new(|| {})).is_err());
    runtime.put("hot".into(), 42u64, 128).unwrap();
    assert_eq!(*runtime.get::<u64>("hot").unwrap(), 42);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

#[test]
fn traversal_batching_requires_a_listing_and_respects_revision_changes() {
    let f = Fixture::new();
    let s = &f.store;
    s.runtime.managed.store(true, Ordering::Release);
    let view = View::Live("a".into());
    let file = s.entry(&view, "file").unwrap();
    let hidden = s.entry(&view, "nested/hidden").unwrap();
    assert_eq!(s.traversal_size(&view, "file", &file).unwrap(), 5);
    assert_eq!(s.known_size(&hidden).unwrap(), None);
    let state = s.view_snapshot(&view).unwrap();
    s.note_listing(&view, &state, "nested").unwrap();
    assert_eq!(s.known_size(&hidden).unwrap(), None);
    s.write_file(&view, "nested/new", b"new", 0o644, Kind::File)
        .unwrap();
    assert_ne!(s.view_snapshot(&view).unwrap().revision, state.revision);
    assert_eq!(
        s.traversal_size(&view, "nested/hidden", &hidden).unwrap(),
        7
    );
}

#[test]
fn unreadable_speculative_size_does_not_fail_requested_file() {
    let f = Fixture::new();
    // Recognizable but invalid LFS pointer: its logical size cannot be resolved.
    std::fs::write(
        f.repo.join("bad-pointer"),
        b"version https://git-lfs.github.com/spec/v1\nsize broken\n",
    )
    .unwrap();
    commit(&f.repo);
    let tx = f.store.activate("a").unwrap();
    f.store.finish_activation(&tx, false).unwrap();
    let s = &f.store;
    s.runtime.managed.store(true, Ordering::Release);
    let view = View::Live("a".into());
    let state = s.view_snapshot(&view).unwrap();
    s.note_listing(&view, &state, "").unwrap();
    let file = s.entry(&view, "file").unwrap();
    assert_eq!(s.traversal_size(&view, "file", &file).unwrap(), 5);
    let bad = s.entry(&view, "bad-pointer").unwrap();
    assert_eq!(s.known_size(&bad).unwrap(), None);
    assert!(s.traversal_size(&view, "bad-pointer", &bad).is_err());
    let log = std::fs::read_to_string(s.root.join("operations.jsonl")).unwrap();
    assert!(log.contains("falling back to requested file"));
}

#[test]
fn recursive_lookahead_bounds_directories_objects_and_depth() {
    for (files, expected) in [(1, 32), (40, 1024)] {
        let f = Fixture::new();
        for d in 0..40 {
            let dir = f.repo.join(format!("walk/d{d:02}"));
            std::fs::create_dir_all(&dir).unwrap();
            for n in 0..files {
                std::fs::write(dir.join(format!("f{n:02}")), format!("unique {d} {n}\n")).unwrap();
            }
        }
        std::fs::create_dir_all(f.repo.join("walk/d01/deep")).unwrap();
        std::fs::write(
            f.repo.join("walk/d01/deep/untouched"),
            b"no deep speculation\n",
        )
        .unwrap();
        commit(&f.repo);
        let tx = f.store.activate("a").unwrap();
        f.store.finish_activation(&tx, false).unwrap();
        let s = &f.store;
        s.runtime.managed.store(true, Ordering::Release);
        let view = View::Live("a".into());
        let state = s.view_snapshot(&view).unwrap();
        s.note_listing(&view, &state, "walk").unwrap();
        s.note_listing(&view, &state, "walk/d00").unwrap();
        let entry = s.entry(&view, "walk/d00/f00").unwrap();
        assert_eq!(s.known_size(&entry).unwrap(), None);
        assert_eq!(s.traversal_size(&view, "walk/d00/f00", &entry).unwrap(), 11);
        let mut known = 0;
        for d in 0..40 {
            for n in 0..files {
                let e = s.entry(&view, &format!("walk/d{d:02}/f{n:02}")).unwrap();
                known += usize::from(s.known_size(&e).unwrap().is_some());
            }
        }
        assert_eq!(known, expected);
        let deep = s.entry(&view, "walk/d01/deep/untouched").unwrap();
        assert_eq!(s.known_size(&deep).unwrap(), None);
    }
}
