mod common;
use common::Fixture;
use gitalator::{model::Map, store::Store};
use std::{fs, time::Instant};

fn snapshot(
    ns: &gitalator::namespace::Namespace,
) -> std::collections::BTreeMap<String, gitalator::model::Base> {
    let mut result = std::collections::BTreeMap::new();
    let mut todo = vec![String::new()];
    while let Some(path) = todo.pop() {
        for (p, b) in ns.children(&path).unwrap() {
            if b.kind == gitalator::model::Kind::Directory {
                todo.push(p.clone());
            }
            result.insert(p, b);
        }
    }
    result
}
#[test]
fn startup_uses_lazy_namespace_without_legacy_index() {
    let f = Fixture::new();
    let s = f.store();
    assert!(!f.state.join("indexes").exists());
    assert_eq!(s.trees.stats()["tree_objects_loaded"], 1);
    let arc = s.get("dev").unwrap();
    let map = arc.lock().unwrap().record.map.clone();
    let mut moved = map.clone();
    moved.mounts[0].path = "relocated".into();
    let ns = s.prepare_namespace(&moved).unwrap();
    assert!(ns.lookup("relocated/hello").unwrap().is_some());
    assert!(ns.lookup("src/hello").unwrap().is_none());
    drop(s);
    let reopened = Store::open(&f.state).unwrap();
    assert_eq!(reopened.trees.stats()["tree_objects_loaded"], 0);
    assert!(reopened
        .get("dev")
        .unwrap()
        .lock()
        .unwrap()
        .namespace
        .lookup("src/hello")
        .unwrap()
        .is_some());
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
        let namespace = store.prepare_namespace(&map).unwrap();
        eprintln!(
            "layout pass {pass}: {} mounts, {} entries, {:.3}s",
            map.mounts.len(),
            namespace.children("").unwrap().len(),
            start.elapsed().as_secs_f64()
        );
    }
}

#[test]
fn parallel_index_matches_serial_with_nested_overrides() {
    let f = Fixture::new();
    let mut map = Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    let original = map.mounts[0].clone();
    fs::write(f.component.join("hello"), "new pin").unwrap();
    common::commit(&f.component);
    map.mounts[0].path = "".into();
    map.mounts[1].path = "dir".into();
    map.mounts[1].commit = gitalator::git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    for (id, path) in [("nested", "dir/nested"), ("sibling", "dir-other")] {
        let mut mount = original.clone();
        mount.id = id.into();
        mount.path = path.into();
        map.mounts.push(mount);
    }
    fs::write(f.depot.join("depot.toml"), toml::to_string(&map).unwrap()).unwrap();
    common::commit(&f.depot);
    let mut expected = None;
    for jobs in [1, 4] {
        let state = f.state.with_file_name(format!("state-{jobs}"));
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_gtl"))
            .arg("--state-dir")
            .arg(&state)
            .args(["--offline", "workspace", "create", "dev", "--from"])
            .arg(&f.depot)
            .env("GTL_INDEX_JOBS", jobs.to_string())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let store = Store::open(&state).unwrap();
        let arc = store.get("dev").unwrap();
        let w = arc.lock().unwrap();
        assert!(w.namespace.lookup("dir/file").unwrap().is_none());
        assert!(w.namespace.lookup("dir/hello").unwrap().is_some());
        assert!(w.namespace.lookup("dir/nested/hello").unwrap().is_some());
        assert!(w.namespace.lookup("dir-other/hello").unwrap().is_some());
        let actual = serde_json::to_value(snapshot(&w.namespace)).unwrap();
        if let Some(expected) = &expected {
            assert_eq!(&actual, expected);
        } else {
            expected = Some(actual);
        }
    }
}

#[test]
fn cancellation_stops_all_index_workers_without_publishing_cache() {
    use std::{os::unix::fs::PermissionsExt, process::Command, time::Duration};
    let f = Fixture::new();
    let mut map = Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    map.repositories.insert(
        "second".into(),
        gitalator::model::Repository {
            url: format!("file://{}", f.component.display()),
        },
    );
    map.mounts[1].repository = "second".into();
    fs::write(f.depot.join("depot.toml"), toml::to_string(&map).unwrap()).unwrap();
    common::commit(&f.depot);
    drop(f.store());
    fs::remove_dir_all(f.state.join("tree-cache")).unwrap();
    let fake = f.component.parent().unwrap().join("fake-index-bin");
    fs::create_dir(&fake).unwrap();
    let marker = fake.join("workers");
    let real = Command::new("which").arg("git").output().unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    fs::write(
        fake.join("git"),
        format!(
            r#"#!/bin/sh
case " $* " in
  *" cat-file --batch "*)
    echo $$ >> "$GTL_TEST_WORKERS"
    sleep 30
    ;;
esac
exec "{}" "$@"
"#,
            real.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(fake.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.state)
        .args(["--offline", "validate", "dev"])
        .env("GTL_INDEX_JOBS", "4")
        .env("GTL_TEST_WORKERS", &marker)
        .env(
            "PATH",
            format!("{}:{}", fake.display(), std::env::var("PATH").unwrap()),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_to_string(&marker)
        .unwrap_or_default()
        .lines()
        .count()
        < 2
    {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("parallel index workers did not start");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("index cancellation did not finish");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!f.state.join("tree-cache").exists());
}
