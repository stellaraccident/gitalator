mod common;
use common::Fixture;
use gitalator::{git, store::Store, util::hash};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

fn forest(repo: &Path, links: &[(String, String, String)]) {
    let mut config = String::new();
    let mut index = String::new();
    for (n, (path, url, pin)) in links.iter().enumerate() {
        config.push_str(&format!(
            "[submodule \"child.{n}\"]\npath = {path}\nurl = {url}\n"
        ));
        index.push_str(&format!("160000 {pin}\t{path}\n"));
    }
    fs::write(repo.join(".gitmodules"), config).unwrap();
    git::run(repo, &["add", ".gitmodules"]).unwrap();
    git::input(
        repo,
        &["update-index", "--index-info"],
        Some(index.as_bytes()),
    )
    .unwrap();
    git::run(repo, &["commit", "-m", "forest"]).unwrap();
}
fn command(f: &Fixture, jobs: usize, expand: bool) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_gtl"));
    c.arg("--state-dir")
        .arg(&f.state)
        .args(["--offline", "mount"]);
    if expand {
        c.args(["submodules", "--depth", "recursive"]);
    } else {
        c.arg("add")
            .arg(&f.component)
            .args(["--submodules", "recursive"]);
    }
    c.args(["--at", ".", "--workspace", "example"])
        .env("GTL_SUBMODULE_JOBS", jobs.to_string());
    c
}
fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn ssh(f: &Fixture, body: &str) -> (String, std::path::PathBuf) {
    let helper = f.component.parent().unwrap().join("ssh-helper");
    let report = helper.with_extension("report");
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\n",
            report.display()
        ),
    )
    .unwrap();
    (format!("sh '{}'", helper.display()), report)
}

#[test]
fn failed_remote_is_probed_once_cached_pins_work_and_next_invocation_retries() {
    let f = Fixture::new();
    let pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let url = "ssh://example.invalid/denied";
    let mut links: Vec<_> = (0..128)
        .map(|i| {
            (
                format!("deps/m{i:03}"),
                url.into(),
                format!("{:040x}", i + 1),
            )
        })
        .collect();
    links.push(("zz-cached".into(), url.into(), pin.clone()));
    forest(&f.component, &links);
    let mut s = Store::open(&f.state).unwrap();
    s.init("example").unwrap();
    // An accessible local commit under the denied remote must still be usable.
    let id = s.repo(url).unwrap();
    git::run(
        &f.state.join("repos").join(id),
        &["fetch", f.component.to_str().unwrap(), &pin],
    )
    .unwrap();
    let (helper, report) = ssh(&f, "echo 'Permission denied (publickey).' >&2\nexit 255");
    for expand in [false, true] {
        let result = success(
            command(&f, 32, expand)
                .arg("--skip-unavailable")
                .env("GIT_SSH_COMMAND", &helper)
                .env("GIT_SSH_VARIANT", "ssh")
                .output()
                .unwrap(),
        );
        assert_eq!(result["skipped"].as_array().unwrap().len(), 128);
        assert_eq!(result["complete"], false);
        for (i, item) in result["skipped"].as_array().unwrap().iter().enumerate() {
            assert!(item["retry_command"]
                .as_str()
                .unwrap()
                .contains(&format!("{:040x}", i + 1)));
        }
        let store = Store::open(&f.state).unwrap();
        assert_eq!(
            store
                .get("example")
                .unwrap()
                .lock()
                .unwrap()
                .record
                .map
                .mounts
                .len(),
            2
        );
    }
    assert_eq!(fs::read_to_string(report).unwrap().lines().count(), 2);
}

#[test]
fn missing_refs_remain_pin_specific_and_distinct_remotes_run_concurrently() {
    let f = Fixture::new();
    let mut links: Vec<_> = (0..32)
        .map(|i| {
            (
                format!("m{i:03}"),
                format!("ssh://example.invalid/repo{}", i / 2),
                format!("{:040x}", i + 1),
            )
        })
        .collect();
    links.extend((0..32).map(|i| {
        (
            format!("z{i:03}"),
            "ssh://example.invalid/repo0".into(),
            format!("{:040x}", 1),
        )
    }));
    forest(&f.component, &links);
    Store::open(&f.state).unwrap().init("example").unwrap();
    let (helper, report) = ssh(
        &f,
        "sleep 0.3\necho 'fatal: remote error: upload-pack: not our ref' >&2\nexit 1",
    );
    let start = Instant::now();
    let result = success(
        command(&f, 32, false)
            .arg("--skip-unavailable")
            .env("GIT_SSH_COMMAND", helper)
            .env("GIT_SSH_VARIANT", "ssh")
            .output()
            .unwrap(),
    );
    assert_eq!(result["skipped"].as_array().unwrap().len(), 64);
    assert_eq!(fs::read_to_string(report).unwrap().lines().count(), 32);
    assert!(
        start.elapsed() < Duration::from_secs(7),
        "serialized failures: {:?}",
        start.elapsed()
    );
}

#[test]
fn hot_inventory_has_no_per_mount_git_and_corrupt_cache_is_rebuilt() {
    let f = Fixture::new();
    let pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let leaf = f.component.parent().unwrap().join("leaf");
    git::run(
        f.component.parent().unwrap(),
        &[
            "clone",
            "--bare",
            f.component.to_str().unwrap(),
            leaf.to_str().unwrap(),
        ],
    )
    .unwrap();
    forest(
        &f.component,
        &(0..128)
            .map(|i| {
                (
                    format!("dir{i:03}/leaf"),
                    leaf.to_string_lossy().into(),
                    pin.clone(),
                )
            })
            .collect::<Vec<_>>(),
    );
    Store::open(&f.state).unwrap().init("example").unwrap();
    success(command(&f, 32, false).output().unwrap());
    let before = Store::open(&f.state)
        .unwrap()
        .get("example")
        .unwrap()
        .lock()
        .unwrap()
        .record
        .map
        .clone();
    let trace = f.component.parent().unwrap().join("trace");
    fs::write(&trace, "").unwrap();
    let result = success(
        command(&f, 32, true)
            .env("GIT_TRACE", &trace)
            .output()
            .unwrap(),
    );
    assert_eq!(result["changed"], false);
    let trace = fs::read_to_string(trace).unwrap();
    assert!(!trace.contains("ls-tree"));
    assert!(!trace.contains("cat-file"), "{trace}");
    assert!(!trace.contains("config --no-includes"));
    let directory = f
        .state
        .join("submodule-cache/v1")
        .join(hash(leaf.to_str().unwrap().as_bytes()));
    for file in fs::read_dir(directory).unwrap() {
        fs::write(file.unwrap().path(), b"broken").unwrap();
    }
    success(command(&f, 1, true).output().unwrap());
    let after = Store::open(&f.state)
        .unwrap()
        .get("example")
        .unwrap()
        .lock()
        .unwrap()
        .record
        .map
        .clone();
    assert_eq!(before, after);
}

#[test]
fn malformed_config_is_fatal_and_undeclared_and_stale_links_are_not_imported() {
    let f = Fixture::new();
    let pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    forest(
        &f.component,
        &[(
            "undeclared".into(),
            "ssh://example.invalid/unused".into(),
            pin,
        )],
    );
    fs::write(
        f.component.join(".gitmodules"),
        "[submodule \"stale\"]\npath = no-such-path\nurl = ../missing\n",
    )
    .unwrap();
    // Preserve the gitlink while changing only the configuration.
    git::run(&f.component, &["add", ".gitmodules"]).unwrap();
    git::run(&f.component, &["commit", "-m", "stale"]).unwrap();
    Store::open(&f.state).unwrap().init("example").unwrap();
    success(command(&f, 32, false).output().unwrap());
    assert_eq!(
        Store::open(&f.state)
            .unwrap()
            .get("example")
            .unwrap()
            .lock()
            .unwrap()
            .record
            .map
            .mounts
            .len(),
        1
    );
    fs::write(f.component.join(".gitmodules"), "[broken").unwrap();
    git::run(&f.component, &["add", ".gitmodules"]).unwrap();
    let tree = git::text(&f.component, &["write-tree"]).unwrap();
    let pin = git::text(
        &f.component,
        &["commit-tree", &tree, "-p", "HEAD", "-m", "malformed config"],
    )
    .unwrap();
    git::run(&f.component, &["update-ref", "HEAD", &pin]).unwrap();
    Store::open(&f.state)
        .unwrap()
        .remove_mount("example", None, ".", true)
        .unwrap();
    let out = command(&f, 32, false)
        .arg("--skip-unavailable")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(Store::open(&f.state)
        .unwrap()
        .get("example")
        .unwrap()
        .lock()
        .unwrap()
        .record
        .map
        .mounts
        .is_empty());
}

#[test]
fn fatal_error_cancels_slow_sibling_without_masking_error() {
    let f = Fixture::new();
    forest(
        &f.component,
        &[
            (
                "a".into(),
                "ssh://example.invalid/slow".into(),
                "1".repeat(40),
            ),
            (
                "b".into(),
                "ssh://example.invalid/fast".into(),
                "2".repeat(40),
            ),
        ],
    );
    Store::open(&f.state).unwrap().init("example").unwrap();
    let (helper,_) = ssh(&f,"case \"$*\" in *slow*) sleep 30;; *) sleep 0.3;; esac\necho 'Permission denied (publickey).' >&2\nexit 255");
    let start = Instant::now();
    let out = command(&f, 32, false)
        .env("GIT_SSH_COMMAND", helper)
        .env("GIT_SSH_VARIANT", "ssh")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Permission denied"));
    assert!(start.elapsed() < Duration::from_secs(6));
    assert!(Store::open(&f.state)
        .unwrap()
        .get("example")
        .unwrap()
        .lock()
        .unwrap()
        .record
        .map
        .mounts
        .is_empty());
}

#[test]
fn children_are_scheduled_before_slow_sibling_finishes() {
    let f = Fixture::new();
    let leaf_pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let leaf = f.component.parent().unwrap().join("leaf");
    git::run(
        f.component.parent().unwrap(),
        &[
            "clone",
            "--bare",
            f.component.to_str().unwrap(),
            leaf.to_str().unwrap(),
        ],
    )
    .unwrap();
    forest(
        &f.component,
        &[(
            "nested".into(),
            "ssh://example.invalid/grandchild".into(),
            leaf_pin.clone(),
        )],
    );
    let intermediate_pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
    let intermediate = f.component.parent().unwrap().join("intermediate");
    git::run(
        f.component.parent().unwrap(),
        &[
            "clone",
            "--bare",
            f.component.to_str().unwrap(),
            intermediate.to_str().unwrap(),
        ],
    )
    .unwrap();
    git::run(&f.component, &["reset", "--hard", &leaf_pin]).unwrap();
    forest(
        &f.component,
        &[
            ("slow".into(), "ssh://example.invalid/slow".into(), leaf_pin),
            (
                "fast".into(),
                "ssh://example.invalid/fast".into(),
                intermediate_pin,
            ),
        ],
    );
    Store::open(&f.state).unwrap().init("example").unwrap();
    let events = f.component.parent().unwrap().join("events");
    let (helper,_) = ssh(&f,&format!("case \"$*\" in\n*slow*) sleep 2; echo slow-done >> '{}'; exec git-upload-pack '{}' ;;\n*fast*) exec git-upload-pack '{}' ;;\n*) echo grandchild >> '{}'; exec git-upload-pack '{}' ;;\nesac",events.display(),leaf.display(),intermediate.display(),events.display(),leaf.display()));
    success(
        command(&f, 32, false)
            .env("GIT_SSH_COMMAND", helper)
            .env("GIT_SSH_VARIANT", "ssh")
            .output()
            .unwrap(),
    );
    let events = fs::read_to_string(events).unwrap();
    assert!(
        events.find("grandchild").unwrap() < events.find("slow-done").unwrap(),
        "{events}"
    );
}

#[test]
fn cancelling_parallel_import_preserves_active_layout() {
    use std::process::Stdio;
    let f = Fixture::new();
    forest(
        &f.component,
        &(0..16)
            .map(|i| {
                (
                    format!("m{i}"),
                    format!("ssh://example.invalid/slow{i}"),
                    "1".repeat(40),
                )
            })
            .collect::<Vec<_>>(),
    );
    Store::open(&f.state).unwrap().init("example").unwrap();
    let (helper, report) = ssh(&f, "sleep 30\nexit 1");
    let mut child = command(&f, 32, false)
        .env("GIT_SSH_COMMAND", helper)
        .env("GIT_SSH_VARIANT", "ssh")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while fs::read_to_string(&report)
        .unwrap_or_default()
        .lines()
        .count()
        < 4
    {
        assert!(child.try_wait().unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(6));
        std::thread::sleep(Duration::from_millis(20));
    }
    let start = Instant::now();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(Store::open(&f.state)
        .unwrap()
        .get("example")
        .unwrap()
        .lock()
        .unwrap()
        .record
        .map
        .mounts
        .is_empty());
}

#[test]
fn mount_and_depth_limits_abort_before_activation() {
    for deep in [false, true] {
        let f = Fixture::new();
        let mut pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
        let url = f.component.to_str().unwrap().to_string();
        if deep {
            for _ in 0..34 {
                forest(&f.component, &[("next".into(), url.clone(), pin)]);
                pin = git::text(&f.component, &["rev-parse", "HEAD"]).unwrap();
            }
        } else {
            forest(
                &f.component,
                &(0..4096)
                    .map(|i| (format!("m{i:04}"), url.clone(), pin.clone()))
                    .collect::<Vec<_>>(),
            );
        }
        Store::open(&f.state).unwrap().init("example").unwrap();
        let out = command(&f, 32, false).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("depth/mount limit"));
        assert!(Store::open(&f.state)
            .unwrap()
            .get("example")
            .unwrap()
            .lock()
            .unwrap()
            .record
            .map
            .mounts
            .is_empty());
    }
}
