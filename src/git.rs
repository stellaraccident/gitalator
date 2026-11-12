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
    execute(path, args, input, false)
}
pub fn local(path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    execute(path, args, input, true)
}
fn execute(path: &Path, args: &[&str], input: Option<&[u8]>, local_only: bool) -> Result<Vec<u8>> {
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
    .env("GIT_NO_LAZY_FETCH", if local_only { "1" } else { "0" })
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

/// Probe without network access, including older Git's fatal missing-promisor
/// behavior. A missing object is not corruption or permission failure.
pub fn blob_sizes_local(repo: &Path, oids: &[String]) -> Result<Vec<Option<u64>>> {
    if oids.iter().any(|id| !crate::model::oid(id)) {
        return Err(error("invalid object ID"));
    }
    if oids.is_empty() {
        return Ok(vec![]);
    }
    let input = oids.join("\n") + "\n";
    let out = match local(repo, &["cat-file", "--batch-check"], Some(input.as_bytes())) {
        Ok(out) => out,
        Err(e)
            if e.to_string().contains("lazy fetching disabled")
                && e.to_string().contains("from promisor remote") =>
        {
            let present = local(
                repo,
                &[
                    "cat-file",
                    "--batch-check",
                    "--batch-all-objects",
                    "--unordered",
                ],
                None,
            )?;
            let text = std::str::from_utf8(&present)?;
            let wanted: std::collections::HashSet<_> = oids.iter().map(String::as_str).collect();
            let found: std::collections::HashMap<_, _> = text
                .lines()
                .filter_map(|line| {
                    let (id, _) = line.split_once(' ')?;
                    wanted.contains(id).then_some((id, line))
                })
                .collect();
            oids.iter()
                .map(|id| {
                    found
                        .get(id.as_str())
                        .map(|line| format!("{line}\n"))
                        .unwrap_or_else(|| format!("{id} missing\n"))
                })
                .collect::<String>()
                .into_bytes()
        }
        Err(e) => return Err(e),
    };
    let text = std::str::from_utf8(&out)?;
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != oids.len() {
        return Err(error("incomplete blob-size response"));
    }
    oids.iter()
        .zip(lines)
        .map(|(id, line)| {
            let fields: Vec<_> = line.split_whitespace().collect();
            match fields.as_slice() {
                [oid, "missing"] if *oid == id => Ok(None),
                [oid, "blob", size] if *oid == id => Ok(Some(size.parse()?)),
                _ => Err(error("invalid blob-size response")),
            }
        })
        .collect()
}
/// Call under the shared repository lock. Fetch exact missing objects in one
/// request; never hydrate a whole commit as a side effect of a local probe.
pub fn ensure_blobs(repo: &Path, oids: &[String]) -> Result<Vec<u64>> {
    let mut sizes = blob_sizes_local(repo, oids)?;
    for attempt in 0..2 {
        let missing: Vec<_> = oids
            .iter()
            .zip(&sizes)
            .filter(|(_, size)| size.is_none())
            .map(|(id, _)| id.as_str())
            .collect();
        if missing.is_empty() {
            return Ok(sizes.into_iter().map(Option::unwrap).collect());
        }
        let input = missing.join("\n") + "\n";
        let fetch = input_fetch(repo, input.as_bytes());
        sizes = blob_sizes_local(repo, oids)?;
        if sizes.iter().all(Option::is_some) {
            return Ok(sizes.into_iter().map(Option::unwrap).collect());
        }
        let e = fetch
            .err()
            .unwrap_or_else(|| error("fetch succeeded but requested blobs remain missing"));
        let message = e.to_string().to_ascii_lowercase();
        if attempt == 1
            || ![
                "timed out",
                "connection reset",
                "connection refused",
                "could not resolve host",
                "early eof",
                "http 503",
                "http 502",
                "temporary failure",
            ]
            .iter()
            .any(|p| message.contains(p))
        {
            return Err(e);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    unreachable!()
}
fn input_fetch(repo: &Path, bytes: &[u8]) -> Result<Vec<u8>> {
    input(
        repo,
        &[
            "-c",
            "fetch.negotiationAlgorithm=noop",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--recurse-submodules=no",
            "--no-auto-maintenance",
            "--filter=blob:none",
            "--stdin",
            "origin",
        ],
        Some(bytes),
    )
}
