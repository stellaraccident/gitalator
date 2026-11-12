use crate::{error, Result};
use std::{
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};
pub type Progress = std::sync::Arc<dyn Fn(&str) + Send + Sync>;
pub fn execute_command(
    cmd: &mut Command,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    execute_command_progress(cmd, input, timeout, None)
}
fn execute_command_progress(
    cmd: &mut Command,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
    progress: Option<Progress>,
) -> Result<std::process::Output> {
    cmd.process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut b = vec![];
        stdout.read_to_end(&mut b).map(|_| b)
    });
    let err = std::thread::spawn(move || {
        let mut b = vec![];
        let mut chunk = [0u8; 4096];
        let mut line = Vec::new();
        loop {
            let n = stderr.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            b.extend_from_slice(&chunk[..n]);
            if let Some(ref progress) = progress {
                for &byte in &chunk[..n] {
                    if byte == b'\r' || byte == b'\n' {
                        if !line.is_empty() {
                            progress(&redact(&String::from_utf8_lossy(&line)));
                            line.clear();
                        }
                    } else {
                        line.push(byte);
                    }
                }
            }
        }
        if let Some(progress) = progress {
            if !line.is_empty() {
                progress(&redact(&String::from_utf8_lossy(&line)));
            }
        }
        Ok::<_, std::io::Error>(b)
    });
    let writer = if let Some(b) = input {
        let b = b.to_vec();
        let mut stdin = child.stdin.take().unwrap();
        Some(std::thread::spawn(move || stdin.write_all(&b)))
    } else {
        None
    };
    let start = std::time::Instant::now();
    let (status, timedout) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if start.elapsed() >= timeout {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            break (child.wait()?, true);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let stdout = out.join().map_err(|_| error("stdout worker failed"))??;
    let stderr = err.join().map_err(|_| error("stderr worker failed"))??;
    if let Some(w) = writer {
        let r = w.join().map_err(|_| error("stdin worker failed"))?;
        if status.success() {
            r?;
        }
    }
    if timedout {
        return Err(error(format!(
            "backend command timed out after {} seconds",
            timeout.as_secs_f64()
        )));
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}
pub fn redact(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            if word.contains("://") || word.contains("token=") || word.contains("password=") {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    input(path, args, None)
}
pub fn input(path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "protocol.file.allow=always",
        "-C",
    ])
    .arg(path)
    .args(args)
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_LFS_SKIP_SMUDGE", "1")
    .env("LC_ALL", "C")
    .stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let out = execute_command(&mut cmd, input, std::time::Duration::from_secs(300))?;
    if !out.status.success() {
        return Err(error(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            redact(&String::from_utf8_lossy(&out.stderr))
        )));
    }
    Ok(out.stdout)
}
pub fn text(path: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(run(path, args)?)?.trim_end().to_owned())
}
