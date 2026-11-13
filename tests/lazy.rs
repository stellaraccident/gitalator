mod common;
use common::{commit, Fixture};
use gitalator::git;
use std::fs;

#[test]
fn filtered_fetch_listing_demand_batch_and_offline_reopen() {
    let f = Fixture::new();
    git::run(&f.component, &["config", "uploadpack.allowFilter", "true"]).unwrap();
    fs::write(f.component.join("sibling"), b"sibling\n").unwrap();
    commit(&f.component);
    let pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let map = fs::read_to_string(f.depot.join("depot.toml")).unwrap();
    let old = git::text(&f.component, &["rev-parse", "HEAD~1"]).unwrap();
    fs::write(
        f.depot.join("depot.toml"),
        map.replace(&old, &pin).replace(
            f.component.to_str().unwrap(),
            &format!("file://{}", f.component.display()),
        ),
    )
    .unwrap();
    commit(&f.depot);
    let s = f.store();
    let arc = s.get("dev").unwrap();
    let w = arc.lock().unwrap();
    let paths = ["src/hello", "src/sibling", "src/dir/file"];
    let ids: Vec<_> = paths.iter().map(|p| w.base[*p].oid.clone()).collect();
    let repo = f.state.join("repos").join(&w.base[paths[0]].repo);
    assert_eq!(git::blob_sizes_local(&repo, &ids).unwrap(), vec![None; 3]);
    w.children("src").unwrap();
    assert_eq!(git::blob_sizes_local(&repo, &ids).unwrap(), vec![None; 3]);
    assert_eq!(
        fs::read(w.base_file("src/hello").unwrap()).unwrap(),
        b"hello\n"
    );
    assert_eq!(
        git::blob_sizes_local(&repo, &ids).unwrap(),
        vec![Some(6), Some(8), None]
    );
    // A second mount shares fetched blobs, but not upper-layer modifications.
    assert_eq!(
        fs::read(w.base_file("other/sibling").unwrap()).unwrap(),
        b"sibling\n"
    );
    drop(w);
    drop(arc);
    drop(s);
    fs::rename(&f.component, f.component.with_extension("offline")).unwrap();
    let s = gitalator::store::Store::open(&f.state).unwrap();
    let arc = s.get("dev").unwrap();
    let w = arc.lock().unwrap();
    assert_eq!(
        fs::read(w.base_file("src/hello").unwrap()).unwrap(),
        b"hello\n"
    );
    assert!(w.base_file("src/dir/file").is_err());
    assert_eq!(
        git::blob_sizes_local(&repo, &ids).unwrap(),
        vec![Some(6), Some(8), None]
    );
}

#[test]
fn local_probe_loose_packed_and_alternate_objects() {
    for format in ["sha1", "sha256"] {
        for version in ["1", "2"] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            git::run(&repo, &["init", &format!("--object-format={format}")]).unwrap();
            git::run(&repo, &["config", "user.name", "Test"]).unwrap();
            git::run(&repo, &["config", "user.email", "test@localhost"]).unwrap();
            fs::write(repo.join("file"), b"payload").unwrap();
            commit(&repo);
            let id = git::text(&repo, &["rev-parse", "HEAD:file"]).unwrap();
            let absent = "0".repeat(id.len());
            let ids = vec![absent, id.clone(), id];
            assert_eq!(
                git::blob_sizes_local(&repo, &ids).unwrap(),
                vec![None, Some(7), Some(7)]
            );
            git::run(&repo, &["config", "pack.indexVersion", version]).unwrap();
            git::run(&repo, &["repack", "-ad"]).unwrap();
            git::run(&repo, &["prune-packed"]).unwrap();
            assert_eq!(
                git::blob_sizes_local(&repo, &ids).unwrap(),
                vec![None, Some(7), Some(7)]
            );
            let alternate = temp.path().join("alternate");
            fs::create_dir(&alternate).unwrap();
            git::run(&alternate, &["init", &format!("--object-format={format}")]).unwrap();
            fs::write(
                alternate.join(".git/objects/info/alternates"),
                format!("{}\n", repo.join(".git/objects").display()),
            )
            .unwrap();
            assert_eq!(
                git::blob_sizes_local(&alternate, &ids).unwrap(),
                vec![None, Some(7), Some(7)]
            );
        }
    }
}

#[test]
#[ignore = "read-only probe of an externally supplied large partial repository"]
fn large_repository_missing_probe() {
    let repo = std::path::PathBuf::from(std::env::var("GTL_PROBE_REPO").unwrap());
    let oid = std::env::var("GTL_PROBE_OID").unwrap();
    let now = std::time::Instant::now();
    let sizes = git::blob_sizes_local(&repo, &[oid]).unwrap();
    eprintln!("large-repository probe: {:?}, {sizes:?}", now.elapsed());
    assert!(now.elapsed() < std::time::Duration::from_secs(2));
}
