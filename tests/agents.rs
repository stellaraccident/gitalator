use gitalator::{git, store::Store};
use std::{fs, process::Command};

fn cli(state: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .output()
        .unwrap()
}
#[test]
fn guides_are_available_without_creating_state() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("absent");
    for args in [vec!["agents"], vec!["agents", "workspace-create"]] {
        let out = cli(&state, &args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let guide = String::from_utf8(out.stdout).unwrap();
        assert!(guide.starts_with("# "));
        assert!(guide.contains("workspace"));
        assert!(!state.exists());
    }
    let out = cli(&state, &["agents", "workspace-create"]);
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("gtl workspace init example"));
}

#[test]
fn init_needs_no_identity_or_remote_and_survives_restart_and_fork() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let out = cli(&state, &["--offline", "workspace", "init", "example"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["path"], state.join("mount/example").to_str().unwrap());
    let depot = std::path::Path::new(info["depot_worktree"].as_str().unwrap());
    let bare = std::path::Path::new(info["depot_repository"].as_str().unwrap());
    assert_eq!(git::text(bare, &["remote"]).unwrap(), "");
    assert_eq!(git::text(depot, &["status", "--porcelain"]).unwrap(), "");
    assert_eq!(
        git::text(depot, &["ls-tree", "--name-only", "HEAD"]).unwrap(),
        "depot.toml"
    );
    assert!(git::run(bare, &["config", "--local", "--get", "user.name"]).is_err());
    let mut s = Store::open(&state).unwrap();
    let w = s.get("example").unwrap();
    {
        let w = w.lock().unwrap();
        assert!(w.record.map.mounts.is_empty());
        assert!(w.record.map.repositories.is_empty());
        assert!(w.base.is_empty());
        assert_eq!(
            w.children("").unwrap(),
            vec![".depotgit", ".depotstate.json"]
        );
    }
    let before = fs::read_dir(state.join("repos")).unwrap().count();
    assert!(s.init("example").is_err());
    assert!(s.init("../escape").is_err());
    assert_eq!(fs::read_dir(state.join("repos")).unwrap().count(), before);
    s.fork("clean", "example", false).unwrap();
    s.fork("live", "example", true).unwrap();
    let source = w.lock().unwrap();
    for name in ["clean", "live"] {
        let target = s.get(name).unwrap();
        let target = target.lock().unwrap();
        assert_eq!(source.record.depot_repo, target.record.depot_repo);
        assert_ne!(source.record.branch, target.record.branch);
    }
    assert_eq!(fs::read_dir(state.join("repos")).unwrap().count(), 1);
}
