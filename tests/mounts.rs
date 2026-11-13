mod common;
use common::{commit, Fixture};
use gitalator::{cli::Submodules, git, store::Store};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn repo(path: &Path) {
    fs::create_dir(path).unwrap();
    git::run(path, &["init", "-b", "main"]).unwrap();
    git::run(path, &["config", "user.name", "Test"]).unwrap();
    git::run(path, &["config", "user.email", "test@localhost"]).unwrap();
    fs::write(path.join("file"), "initial").unwrap();
    commit(path);
}
fn gitlink(repo: &Path, path: &str, pin: &str, url: &str) {
    fs::write(
        repo.join(".gitmodules"),
        format!("[submodule \"child.path\"]\npath = {path}\nurl = {url}\nbranch = not-the-pin\n"),
    )
    .unwrap();
    git::run(repo, &["add", ".gitmodules"]).unwrap();
    git::run(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{pin},{path}"),
        ],
    )
    .unwrap();
    git::run(repo, &["commit", "-m", "link"]).unwrap();
}
fn graph() -> (Fixture, PathBuf, String) {
    let f = Fixture::new();
    let parent = f.component.parent().unwrap();
    let leaf = parent.join("leaf");
    repo(&leaf);
    let child = parent.join("child");
    repo(&child);
    let pin = git::text(&leaf, &["rev-parse", "HEAD"]).unwrap();
    gitlink(&child, "vendor/leaf", &pin, "../leaf");
    let pin = git::text(&child, &["rev-parse", "HEAD"]).unwrap();
    gitlink(&f.component, "deps/child", &pin, "../child");
    // The importer must honor the gitlink pin, not this newer branch tip.
    fs::write(child.join("file"), "new branch tip").unwrap();
    commit(&child);
    (f, child, pin)
}

#[test]
fn import_depths_exact_pins_and_unstaged_manifest() {
    for (mode, count) in [
        (Submodules::None, 1),
        (Submodules::FirstLevel, 2),
        (Submodules::Recursive, 3),
    ] {
        let (f, _child, pin) = graph();
        let mut s = Store::open(&f.state).unwrap();
        s.init("example").unwrap();
        let arc = s.get("example").unwrap();
        let before = {
            let w = arc.lock().unwrap();
            fs::write(w.upper().join("local"), "preserve").unwrap();
            git::run(&w.depot(), &["add", "overlay/local"]).unwrap();
            git::text(&w.depot(), &["write-tree"]).unwrap()
        };
        s.add_mount(
            "example",
            None,
            f.component.to_str().unwrap(),
            "",
            "HEAD",
            mode,
        )
        .unwrap();
        {
            let w = arc.lock().unwrap();
            assert_eq!(w.record.map.mounts.len(), count);
            assert_eq!(git::text(&w.depot(), &["write-tree"]).unwrap(), before);
            assert_eq!(
                git::text(&w.depot(), &["diff", "--name-only", "--", "depot.toml"]).unwrap(),
                "depot.toml"
            );
            assert_eq!(
                fs::read(w.entry("local").unwrap().physical.unwrap()).unwrap(),
                b"preserve"
            );
            assert_eq!(
                fs::read(w.entry("hello").unwrap().physical.unwrap()).unwrap(),
                b"hello\n"
            );
            if mode == Submodules::None {
                assert!(w.children("deps/child").unwrap().is_empty());
            } else {
                assert_eq!(w.record.map.mounts[1].commit, pin);
                assert_eq!(
                    fs::read(w.entry("deps/child/file").unwrap().physical.unwrap()).unwrap(),
                    b"initial"
                );
                if mode == Submodules::FirstLevel {
                    assert!(w.children("deps/child/vendor/leaf").unwrap().is_empty());
                } else {
                    assert_eq!(
                        fs::read(
                            w.entry("deps/child/vendor/leaf/file")
                                .unwrap()
                                .physical
                                .unwrap()
                        )
                        .unwrap(),
                        b"initial"
                    );
                }
            }
            w.validate().unwrap();
        }
        s.fork("live", "example", true).unwrap();
        assert_eq!(
            s.get("live")
                .unwrap()
                .lock()
                .unwrap()
                .record
                .map
                .mounts
                .len(),
            count
        );
        s.fork("committed", "example", false).unwrap();
        assert!(s
            .get("committed")
            .unwrap()
            .lock()
            .unwrap()
            .record
            .map
            .mounts
            .is_empty());
        drop(s);
        let s = Store::open(&f.state).unwrap();
        assert_eq!(
            s.get("example")
                .unwrap()
                .lock()
                .unwrap()
                .record
                .map
                .mounts
                .len(),
            count
        );
    }
}

#[test]
fn failed_import_and_collision_do_not_change_active_layout() {
    let (f, child, _) = graph();
    let mut s = Store::open(&f.state).unwrap();
    s.init("example").unwrap();
    let arc = s.get("example").unwrap();
    let before = fs::read(arc.lock().unwrap().depot().join("depot.toml")).unwrap();
    fs::rename(&child, child.with_extension("offline")).unwrap();
    assert!(s
        .add_mount(
            "example",
            None,
            f.component.to_str().unwrap(),
            "",
            "HEAD",
            Submodules::Recursive
        )
        .is_err());
    assert_eq!(
        fs::read(arc.lock().unwrap().depot().join("depot.toml")).unwrap(),
        before
    );
    assert!(arc.lock().unwrap().record.map.mounts.is_empty());
    // A free-standing overlay file may not collide with generated mount metadata.
    fs::write(arc.lock().unwrap().upper().join(".mountgit"), "preserve").unwrap();
    assert!(s
        .add_mount(
            "example",
            None,
            f.component.to_str().unwrap(),
            "",
            "HEAD",
            Submodules::None
        )
        .is_err());
    assert_eq!(
        fs::read(arc.lock().unwrap().depot().join("depot.toml")).unwrap(),
        before
    );
    fs::remove_file(arc.lock().unwrap().upper().join(".mountgit")).unwrap();
    s.add_mount(
        "example",
        None,
        f.component.to_str().unwrap(),
        "",
        "HEAD",
        Submodules::None,
    )
    .unwrap();
    assert!(s
        .add_mount(
            "example",
            None,
            f.component.to_str().unwrap(),
            "",
            "HEAD",
            Submodules::None
        )
        .is_err());
}

#[test]
fn layout_activation_replays_after_crash() {
    for point in ["layout-published", "layout-applied"] {
        let f = Fixture::new();
        let mut s = Store::open(&f.state).unwrap();
        s.init("example").unwrap();
        drop(s);
        let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
            .arg("--state-dir")
            .arg(&f.state)
            .args(["--offline", "mount", "add"])
            .arg(&f.component)
            .args(["--at", ".", "--workspace", "example"])
            .env("GTL_FAILPOINT", point)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(86),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for _ in 0..2 {
            let s = Store::open(&f.state).unwrap();
            let w = s.get("example").unwrap();
            let w = w.lock().unwrap();
            w.validate().unwrap();
            assert_eq!(w.record.map.mounts.len(), 1);
            assert!(!w.root.join("layout.json").exists());
            assert_eq!(w.attributes("hello").unwrap().size, 6);
        }
    }
}

#[test]
fn active_layout_can_be_checkpointed() {
    let f = Fixture::new();
    let mut s = Store::open(&f.state).unwrap();
    s.init("example").unwrap();
    s.add_mount(
        "example",
        None,
        f.component.to_str().unwrap(),
        "",
        "HEAD",
        Submodules::None,
    )
    .unwrap();
    let w = s.get("example").unwrap();
    let mut w = w.lock().unwrap();
    git::run(&w.depot(), &["config", "user.name", "Test"]).unwrap();
    git::run(&w.depot(), &["config", "user.email", "test@localhost"]).unwrap();
    git::run(&w.depot(), &["add", "depot.toml"]).unwrap();
    w.checkpoint("mount").unwrap();
    assert_eq!(
        git::text(&w.depot(), &["status", "--porcelain"]).unwrap(),
        ""
    );
}

fn flat_graph() -> (Fixture, Store) {
    let (f, _, _) = graph();
    let mut s = Store::open(&f.state).unwrap();
    s.init("example").unwrap();
    s.add_mount(
        "example",
        None,
        f.component.to_str().unwrap(),
        ".",
        "HEAD",
        Submodules::None,
    )
    .unwrap();
    (f, s)
}

#[test]
fn expand_in_steps_is_idempotent_and_preserves_overlay_and_index() {
    let (_f, s) = flat_graph();
    let arc = s.get("example").unwrap();
    let index = {
        let w = arc.lock().unwrap();
        fs::write(w.upper().join("hello"), "local").unwrap();
        git::run(&w.depot(), &["add", "overlay/hello"]).unwrap();
        git::text(&w.depot(), &["write-tree"]).unwrap()
    };
    let result = s
        .expand_submodules("example", None, ".", Submodules::FirstLevel)
        .unwrap();
    assert_eq!(result["added"].as_array().unwrap().len(), 1);
    assert_eq!(arc.lock().unwrap().record.map.mounts.len(), 2);
    s.expand_submodules("example", None, ".", Submodules::Recursive)
        .unwrap();
    let (generation, manifest) = {
        let w = arc.lock().unwrap();
        assert_eq!(w.record.map.mounts.len(), 3);
        assert_eq!(
            fs::read(w.entry("hello").unwrap().physical.unwrap()).unwrap(),
            b"local"
        );
        assert_eq!(git::text(&w.depot(), &["write-tree"]).unwrap(), index);
        (
            w.record.generation.clone(),
            fs::read(w.depot().join("depot.toml")).unwrap(),
        )
    };
    for depth in [Submodules::FirstLevel, Submodules::Recursive] {
        let result = s.expand_submodules("example", None, ".", depth).unwrap();
        assert_eq!(result["changed"], false);
        let w = arc.lock().unwrap();
        assert_eq!(w.record.generation, generation);
        assert_eq!(fs::read(w.depot().join("depot.toml")).unwrap(), manifest);
    }
}

#[test]
fn removal_preserves_descendants_overlay_and_reexpands_missing_children() {
    let (f, s) = flat_graph();
    s.expand_submodules("example", None, ".", Submodules::Recursive)
        .unwrap();
    let arc = s.get("example").unwrap();
    {
        let w = arc.lock().unwrap();
        fs::write(w.upper().join("hello"), "keep").unwrap();
    }
    // Remove the intermediate submodule but preserve its independently mounted leaf.
    s.remove_mount("example", None, "deps/child", false)
        .unwrap();
    {
        let w = arc.lock().unwrap();
        assert!(w.record.map.mounts[0].uninitialized_submodules);
        assert!(w.entry("deps/child/file").is_err());
        assert!(w.entry("deps/child/vendor/leaf/file").is_ok());
    }
    s.expand_submodules("example", None, ".", Submodules::Recursive)
        .unwrap();
    assert_eq!(arc.lock().unwrap().record.map.mounts.len(), 3);
    // Removing only the root preserves the complete child cone and local payload.
    s.remove_mount("example", None, ".", false).unwrap();
    {
        let w = arc.lock().unwrap();
        assert_eq!(w.record.map.mounts.len(), 2);
        assert_eq!(
            fs::read(w.entry("hello").unwrap().physical.unwrap()).unwrap(),
            b"keep"
        );
        assert!(w.entry("deps/child/file").is_ok());
        assert!(w.entry("dir/file").is_err());
    }
    // A similarly prefixed sibling is outside the removal cone.
    s.add_mount(
        "example",
        None,
        f.component.to_str().unwrap(),
        "deps/child-other",
        "HEAD",
        Submodules::None,
    )
    .unwrap();
    s.remove_mount("example", None, "deps/child", true).unwrap();
    let w = arc.lock().unwrap();
    assert_eq!(w.record.map.mounts.len(), 1);
    assert_eq!(w.record.map.mounts[0].path, "deps/child-other");
    assert!(w.entry("deps/child-other/hello").is_ok());
    w.validate().unwrap();
}

#[test]
fn removal_rejects_orphan_tombstones_without_changing_state() {
    let (_f, s) = flat_graph();
    let arc = s.get("example").unwrap();
    let before = {
        let w = arc.lock().unwrap();
        w.unlink("hello", false).unwrap();
        fs::read(w.depot().join("depot.toml")).unwrap()
    };
    let error = s
        .remove_mount("example", None, ".", true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("tombstone"), "{error}");
    let w = arc.lock().unwrap();
    assert_eq!(fs::read(w.depot().join("depot.toml")).unwrap(), before);
    assert!(w.marker("hello").unwrap().is_some());
    assert!(w.entry("hello").is_err());
}

#[test]
fn expansion_rejects_changed_pin_and_failed_fetch_atomically() {
    let (f, s) = flat_graph();
    let arc = s.get("example").unwrap();
    let before = fs::read(arc.lock().unwrap().depot().join("depot.toml")).unwrap();
    let child = f.component.parent().unwrap().join("child");
    let offline = child.with_extension("offline");
    fs::rename(&child, &offline).unwrap();
    assert!(s
        .expand_submodules("example", None, ".", Submodules::Recursive)
        .is_err());
    assert_eq!(
        fs::read(arc.lock().unwrap().depot().join("depot.toml")).unwrap(),
        before
    );
    fs::rename(&offline, &child).unwrap();
    // HEAD has advanced past the gitlink pin in graph().
    s.add_mount(
        "example",
        None,
        child.to_str().unwrap(),
        "deps/child",
        "HEAD",
        Submodules::None,
    )
    .unwrap();
    let before = fs::read(arc.lock().unwrap().depot().join("depot.toml")).unwrap();
    let error = s
        .expand_submodules("example", None, ".", Submodules::Recursive)
        .unwrap_err()
        .to_string();
    assert!(error.contains("conflict at deps/child"), "{error}");
    assert_eq!(
        fs::read(arc.lock().unwrap().depot().join("depot.toml")).unwrap(),
        before
    );
}

#[test]
fn removal_and_expansion_replay_published_layouts() {
    for operation in ["remove", "submodules"] {
        for point in ["layout-published", "layout-applied"] {
            let (f, s) = flat_graph();
            drop(s);
            let out = Command::new(env!("CARGO_BIN_EXE_gtl"))
                .arg("--state-dir")
                .arg(&f.state)
                .args([
                    "--offline",
                    "mount",
                    operation,
                    "--at",
                    ".",
                    "--workspace",
                    "example",
                ])
                .env("GTL_FAILPOINT", point)
                .output()
                .unwrap();
            assert_eq!(
                out.status.code(),
                Some(86),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            for _ in 0..2 {
                let s = Store::open(&f.state).unwrap();
                let arc = s.get("example").unwrap();
                let w = arc.lock().unwrap();
                w.validate().unwrap();
                assert_eq!(
                    w.record.map.mounts.len(),
                    if operation == "remove" { 0 } else { 2 }
                );
                assert!(!w.root.join("layout.json").exists());
            }
        }
    }
}

#[test]
fn expansion_of_subtree_mount_uses_repository_root_gitmodules() {
    let (f, _, _) = graph();
    let map =
        gitalator::model::Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap())
            .unwrap();
    let mut map = map;
    map.mounts.truncate(1);
    map.mounts[0].commit = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    map.mounts[0].subtree = "deps".into();
    map.mounts[0].uninitialized_submodules = true;
    fs::write(f.depot.join("depot.toml"), toml::to_string(&map).unwrap()).unwrap();
    commit(&f.depot);
    let s = f.store();
    s.expand_submodules("dev", None, "src", Submodules::Recursive)
        .unwrap();
    let arc = s.get("dev").unwrap();
    let w = arc.lock().unwrap();
    assert_eq!(w.record.map.mounts[0].id, "code");
    assert!(w.entry("src/child/vendor/leaf/file").is_ok());
}
