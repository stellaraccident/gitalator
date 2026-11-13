mod common;
use common::Fixture;
use gitalator::store::Store;
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn ssh_failure_preserves_custom_command_and_has_interactive_retry() {
    check_ssh(false);
}

#[test]
#[ignore = "requires a usable devpts instance"]
fn ssh_cannot_prompt_on_callers_controlling_terminal() {
    check_ssh(true);
}

fn check_ssh(tty: bool) {
    for from_config in [false, true] {
        let mut f = Fixture::new();
        f.state = f.state.with_file_name("state with ' quote");
        Store::open(&f.state).unwrap().init("example").unwrap();
        let parent = f.component.parent().unwrap();
        let helper = parent.join("ssh-helper");
        let report = parent.join("ssh-report");
        fs::write(&helper, r#"#!/bin/sh
if ( : </dev/tty ) 2>/dev/null; then
  echo unexpected-terminal > "$GTL_SSH_REPORT"
else
  echo no-terminal > "$GTL_SSH_REPORT"
fi
printf '%s\n' "$GIT_TERMINAL_PROMPT" "$GIT_ASKPASS" "$SSH_ASKPASS" "$SSH_ASKPASS_REQUIRE" >> "$GTL_SSH_REPORT"
echo 'Host key verification failed. Permission denied (publickey).' >&2
exit 255
"#).unwrap();
        let custom = format!("sh '{}'", helper.display());
        let global = parent.join("gitconfig");
        fs::write(
            &global,
            if from_config {
                format!("[core]\nsshCommand = {custom}\n")
            } else {
                String::new()
            },
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_gtl"));
        command
            .arg("--state-dir")
            .arg(&f.state)
            .args([
                "--offline",
                "mount",
                "add",
                "ssh://example.invalid/repo",
                "--at",
                ".",
                "--workspace",
                "example",
            ])
            .env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_SSH")
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_SSH_VARIANT", "ssh")
            .env("GTL_SSH_REPORT", &report)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSH_ASKPASS", "/should/not/run")
            .env("GIT_ASKPASS", "/should/not/run");
        if !from_config {
            command.env("GIT_SSH_COMMAND", &custom);
        }
        let _master = if tty {
            use std::os::{fd::FromRawFd, unix::process::CommandExt};
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
            let slave = unsafe { fs::File::from_raw_fd(slave) };
            command.stdin(slave);
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Some(unsafe { fs::File::from_raw_fd(master) })
        } else {
            None
        };
        let start = Instant::now();
        let out = command.output().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!out.status.success());
        assert_eq!(
            fs::read_to_string(report).unwrap(),
            "no-terminal\n0\n/bin/false\n/bin/false\nnever\n"
        );
        let error = String::from_utf8(out.stderr).unwrap();
        assert!(error.contains("Host key verification failed"), "{error}");
        let retry = error
            .split("Retry interactively from a terminal:\n  ")
            .nth(1)
            .expect("retry command")
            .trim();
        // Exercise quoting using a real successful fetch into this same managed bare repo.
        // Substitute only the remote: preserve the generated shell-quoted directory.
        let retry = retry.replace("'origin'", &format!("'{}'", f.component.display()));
        let out = Command::new("sh").args(["-c", &retry]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
