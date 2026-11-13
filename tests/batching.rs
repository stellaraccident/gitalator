mod common;
use common::{commit, Fixture};
use gitalator::{git, model::Map};
use std::{fs, path::Path};

fn pin(f: &Fixture) {
    commit(&f.component);
    let head = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let mut map = Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    for m in &mut map.mounts {
        m.commit = head.clone();
    }
    fs::write(f.depot.join("depot.toml"), toml::to_string(&map).unwrap()).unwrap();
    commit(&f.depot);
    git::run(&f.component, &["config", "uploadpack.allowFilter", "true"]).unwrap();
}
fn empty_cache(state: &Path) -> bool {
    fs::read_dir(state.join("cache")).unwrap().next().is_none()
}

#[test]
fn large_listing_batches_attributes_and_extracts_only_opened_file() {
    let f = Fixture::new();
    for i in 0..600 {
        fs::write(
            f.component.join(format!("f{i:03}")),
            format!("{i:04}").repeat(512),
        )
        .unwrap();
    }
    pin(&f);
    let mut s = f.store();
    let arc = s.get("dev").unwrap();
    let w = arc.lock().unwrap();
    w.children("src").unwrap();
    assert_eq!(s.metadata.batches(), 0);
    for i in 0..600 {
        let e = w.attributes(&format!("src/f{i:03}")).unwrap();
        assert_eq!(e.size, 2048);
        assert!(e.physical.is_none());
    }
    assert_eq!(s.metadata.batches(), 3);
    assert!(empty_cache(&f.state));
    for i in 0..600 {
        assert_eq!(w.attributes(&format!("other/f{i:03}")).unwrap().size, 2048);
    }
    assert_eq!(s.metadata.batches(), 3);
    assert_eq!(
        fs::read(w.entry("src/f599").unwrap().physical.unwrap()).unwrap(),
        b"0599".repeat(512)
    );
    let repo = &w.base["src/f599"].repo;
    assert_eq!(
        fs::read_dir(f.state.join("cache").join(repo))
            .unwrap()
            .count(),
        1
    );
    drop(w);
    s.fork("clone", "dev", false).unwrap();
    let arc = s.get("clone").unwrap();
    assert_eq!(
        arc.lock().unwrap().attributes("src/f012").unwrap().size,
        2048
    );
    assert_eq!(s.metadata.batches(), 3);
}

#[test]
fn recursive_lookahead_is_bounded_and_respects_mount_boundaries() {
    let f = Fixture::new();
    for d in 0..40 {
        fs::create_dir(f.component.join(format!("d{d:02}"))).unwrap();
        for n in 0..40 {
            fs::write(
                f.component.join(format!("d{d:02}/f{n:02}")),
                format!("directory {d} file {n}"),
            )
            .unwrap();
        }
    }
    pin(&f);
    let mut map = Map::parse(&fs::read_to_string(f.depot.join("depot.toml")).unwrap()).unwrap();
    let mut nested = map.mounts[0].clone();
    nested.id = "nested".into();
    nested.path = "src/d01".into();
    nested.subtree = "d01".into();
    map.mounts.push(nested);
    fs::write(f.depot.join("depot.toml"), toml::to_string(&map).unwrap()).unwrap();
    commit(&f.depot);
    let s = f.store();
    let arc = s.get("dev").unwrap();
    let w = arc.lock().unwrap();
    w.children("src").unwrap();
    w.children("src/d00").unwrap();
    assert_eq!(s.metadata.batches(), 0);
    w.attributes("src/d00/f00").unwrap();
    assert!(empty_cache(&f.state));
    assert_eq!(s.metadata.batches(), 1);
    let resolved = w
        .base
        .iter()
        .filter(|(path, b)| path.starts_with("src/d") && s.metadata.get(&b.repo, &b.oid).is_some())
        .count();
    assert_eq!(resolved, 1024);
    for p in ["src/d01/f00", "src/d39/f00"] {
        let b = &w.base[p];
        assert!(s.metadata.get(&b.repo, &b.oid).is_none());
        assert_eq!(
            git::blob_sizes_local(
                &f.state.join("repos").join(&b.repo),
                std::slice::from_ref(&b.oid)
            )
            .unwrap(),
            vec![None]
        );
    }
    for n in 0..40 {
        w.attributes(&format!("src/d02/f{n:02}")).unwrap();
    }
    assert_eq!(s.metadata.batches(), 1);
}

#[test]
fn lfs_sizes_and_symlink_attributes_do_not_hydrate() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 987654321\n",
        "a".repeat(64)
    );
    fs::write(f.component.join("asset"), &pointer).unwrap();
    symlink(&pointer, f.component.join("pointer-link")).unwrap();
    pin(&f);
    let s = f.store();
    let arc = s.get("dev").unwrap();
    let w = arc.lock().unwrap();
    w.children("src").unwrap();
    assert_eq!(w.attributes("src/asset").unwrap().size, 987654321);
    assert_eq!(
        w.attributes("src/pointer-link").unwrap().size,
        pointer.len() as u64
    );
    assert!(empty_cache(&f.state));
    assert!(!f
        .state
        .join("repos")
        .join(&w.base["src/asset"].repo)
        .join("lfs")
        .exists());
    assert_eq!(
        w.entry("src/pointer-link").unwrap().bytes.unwrap(),
        pointer.as_bytes()
    );
}

#[test]
fn speculative_failure_falls_back_and_disables_directory_batching() {
    let f = Fixture::new();
    fs::write(f.component.join("bad"), "bad").unwrap();
    fs::write(f.component.join("second"), "second").unwrap();
    pin(&f);
    let s = f.store();
    let arc = s.get("dev").unwrap();
    let mut w = arc.lock().unwrap();
    w.base.get_mut("src/bad").unwrap().oid = "0".repeat(40);
    w.children("src").unwrap();
    assert_eq!(w.attributes("src/hello").unwrap().size, 6);
    assert!(w.disabled_batches.lock().unwrap().contains("src"));
    assert_eq!(s.metadata.batches(), 2);
    assert_eq!(w.attributes("src/second").unwrap().size, 6);
    assert_eq!(s.metadata.batches(), 3);
    assert!(empty_cache(&f.state));
}

#[test]
fn concurrent_workspaces_coalesce_metadata_fills() {
    let f = Fixture::new();
    for i in 0..100 {
        fs::write(f.component.join(format!("f{i:03}")), format!("body {i}")).unwrap();
    }
    pin(&f);
    let mut s = f.store();
    s.fork("second", "dev", false).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = ["dev", "second"]
        .into_iter()
        .map(|name| {
            let arc = s.get(name).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let w = arc.lock().unwrap();
                w.children("src").unwrap();
                barrier.wait();
                for i in 0..100 {
                    w.attributes(&format!("src/f{i:03}")).unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(s.metadata.batches(), 1);
    assert!(empty_cache(&f.state));
}
