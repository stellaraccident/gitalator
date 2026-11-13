mod common;
use common::Fixture;
use gitalator::{activity, git};
use std::{
    io::Read,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

#[test]
fn cancelled_process_group_and_inherited_pipes_are_reaped() {
    for background in [false, true] {
        let task = Arc::new(activity::Task::new("test backend"));
        let worker = task.clone();
        let thread = std::thread::spawn(move || {
            let _context = worker.enter();
            let mut cmd = Command::new("sh");
            cmd.args(["-c", if background { "sleep 60 &" } else { "sleep 60" }]);
            git::execute_command(&mut cmd, None, Duration::from_secs(60))
        });
        std::thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        task.cancel();
        let err = thread.join().unwrap().unwrap_err();
        assert!(activity::interrupted(&*err));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn file_logs_when_redirected() {
    log_output(false);
}

#[test]
#[ignore = "requires a usable /dev/ptmx"]
fn interactive_console() {
    log_output(true);
}

fn log_output(console: bool) {
    {
        let f = Fixture::new();
        let mut command = Command::new(env!("CARGO_BIN_EXE_gtl"));
        command
            .arg("--state-dir")
            .arg(&f.state)
            .args(["--offline", "workspace", "create", "dev", "--from"])
            .arg(&f.depot);
        let reader = if console {
            use std::os::fd::FromRawFd;
            let (mut master, mut slave) = (-1, -1);
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0
            );
            let slave = unsafe { std::fs::File::from_raw_fd(slave) };
            let mut master = unsafe { std::fs::File::from_raw_fd(master) };
            command.stderr(slave);
            Some(std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    match master.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                        Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                        Err(e) => panic!("{e}"),
                    }
                }
                String::from_utf8(bytes).unwrap()
            }))
        } else {
            None
        };
        let output = command.output().unwrap();
        // Drop parent command's PTY fd so the reader observes EOF.
        drop(command);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let logs = std::fs::read_to_string(f.state.join("logs/activity.jsonl")).unwrap();
        let entries: Vec<serde_json::Value> = logs
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(entries
            .iter()
            .any(|e| e["message"].as_str().unwrap().contains("Fetch completed")));
        assert!(entries
            .iter()
            .any(|e| e["message"].as_str().unwrap().contains("Index mount")));
        assert!(entries.iter().all(|e| e["operation_id"].is_number()));
        if let Some(reader) = reader {
            let text = reader.join().unwrap();
            assert!(
                text.contains("Fetch") && text.contains("Index mount"),
                "{text}"
            );
        } else {
            assert!(
                output.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
    }
}
