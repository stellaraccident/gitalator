mod common;
use common::*;
use gitalator::{cache::Lock, cli};
use std::{
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[test]
fn daemon_streams_progress_before_completion_and_keeps_json_stdout() {
    let f = Fixture::new();
    let draft = format!(
        "{}\n[[mount]]\nid='copy'\nrepo='origin'\npath='vendor/copy'\nrevision='main'\n",
        f.store.draft("a").unwrap()
    );
    f.store.set_draft("a", &draft).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server = cli::server(f.store.clone(), stop.clone()).unwrap();
    let held = Lock::take(&f.store.root.join("locks/repo-origin")).unwrap();
    let log_path = f.temp.path().join("progress.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .args(["--json", "activate", "a", "--plan"])
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&log_path).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let streamed = loop {
        let log = std::fs::read_to_string(&log_path).unwrap();
        if log.contains("waiting for repository lock") {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let still_running = child.try_wait().unwrap().is_none();
    drop(held);
    let output = child.wait_with_output().unwrap();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(streamed && still_running);
    assert!(
        output.status.success(),
        "{}",
        std::fs::read_to_string(&log_path).unwrap()
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["proposal"]["mount"].is_array());
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("origin [/, vendor/copy · origin]: pending"));
    assert!(log.contains("origin [/, vendor/copy · origin]: fetch complete"));
    assert!(log.contains("origin [/, vendor/copy · origin]: ready"));
}

#[test]
fn offline_cli_reports_repository_fetch_failure() {
    let f = Fixture::new();
    std::fs::rename(&f.repo, f.temp.path().join("offline-origin")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gtl"))
        .arg("--state-dir")
        .arg(&f.store.root)
        .args(["--json", "activate", "a", "--plan"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let log = String::from_utf8(output.stderr).unwrap();
    assert!(log.contains("origin [/ · origin]: fetch failed"));
    assert!(log.contains("repository origin"));
}
