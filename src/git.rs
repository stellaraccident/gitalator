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

/// Check local object storage before asking cat-file about sizes. Git 2.43
/// may walk the entire promisor history when asked about a missing object,
/// even with GIT_NO_LAZY_FETCH. Never pass known-absent IDs to that path.
pub fn blob_sizes_local(repo: &Path, oids: &[String]) -> Result<Vec<Option<u64>>> {
    if oids.iter().any(|id| !crate::model::oid(id)) {
        return Err(error("invalid object ID"));
    }
    if oids.is_empty() {
        return Ok(vec![]);
    }
    let objects = text(
        repo,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
    )?;
    let mut present = vec![false; oids.len()];
    object_presence(
        Path::new(&objects),
        oids,
        &mut present,
        &mut std::collections::HashSet::new(),
    )?;
    let wanted: Vec<_> = oids
        .iter()
        .zip(&present)
        .filter(|(_, p)| **p)
        .map(|(id, _)| id.as_str())
        .collect();
    let mut sizes = vec![None; oids.len()];
    if wanted.is_empty() {
        return Ok(sizes);
    }
    let input = wanted.join("\n") + "\n";
    let out = local(repo, &["cat-file", "--batch-check"], Some(input.as_bytes()))?;
    let output = std::str::from_utf8(&out)?;
    let mut lines = output.lines();
    for ((id, found), size) in oids.iter().zip(present).zip(&mut sizes) {
        if !found {
            continue;
        }
        let fields: Vec<_> = lines
            .next()
            .ok_or("incomplete blob-size response")?
            .split_whitespace()
            .collect();
        match fields.as_slice() {
            [oid, "blob", value] if oid.eq_ignore_ascii_case(id) => *size = Some(value.parse()?),
            _ => return Err(error("invalid blob-size response")),
        }
    }
    if lines.next().is_some() {
        return Err(error("extra blob-size response"));
    }
    Ok(sizes)
}

fn object_presence(
    objects: &Path,
    oids: &[String],
    present: &mut [bool],
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
) -> Result<()> {
    let objects = std::fs::canonicalize(objects)?;
    if !seen.insert(objects.clone()) {
        return Ok(());
    }
    for (id, found) in oids.iter().zip(present.iter_mut()) {
        let id = id.to_ascii_lowercase();
        match std::fs::metadata(objects.join(&id[..2]).join(&id[2..])) {
            Ok(m) => *found |= m.is_file(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    let packs = objects.join("pack");
    if packs.exists() {
        for entry in std::fs::read_dir(packs)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "idx") {
                continue;
            }
            let file = std::fs::File::open(&path)?;
            for (id, found) in oids.iter().zip(present.iter_mut()) {
                if !*found {
                    *found = pack_contains(&file, id)?;
                }
            }
        }
    }
    match std::fs::read_to_string(objects.join("info/alternates")) {
        Ok(alternates) => {
            for line in alternates.lines().filter(|line| !line.is_empty()) {
                object_presence(&objects.join(line), oids, present, seen)?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Binary-search the fanout and sorted IDs, without scanning/decompressing packs.
/// Layout: https://git-scm.com/docs/pack-format (v1 and v2 indexes).
fn pack_contains(file: &std::fs::File, id: &str) -> Result<bool> {
    use std::os::unix::fs::FileExt;
    let bytes: Vec<u8> = (0..id.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&id[i..i + 2], 16))
        .collect::<std::result::Result<_, _>>()?;
    let mut header = [0; 8];
    file.read_exact_at(&mut header, 0)?;
    let v2 = header[..4] == [255, b't', b'O', b'c'];
    if v2 && u32::from_be_bytes(header[4..8].try_into().unwrap()) != 2 {
        return Err(error("unsupported pack index version"));
    }
    let start = if v2 { 8 } else { 0 };
    let mut fanout = [0; 1024];
    file.read_exact_at(&mut fanout, start)?;
    let count = |i: usize| u32::from_be_bytes(fanout[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
    let first = bytes[0] as usize;
    let mut lo = if first == 0 { 0 } else { count(first - 1) };
    let mut hi = count(first);
    let width = bytes.len() as u64;
    let stride = width + if v2 { 0 } else { 4 };
    if lo > hi
        || hi > count(255)
        || file.metadata()?.len() < start + 1024 + count(255) * stride + 2 * width
    {
        return Err(error("invalid pack index"));
    }
    let mut candidate = vec![0; bytes.len()];
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        file.read_exact_at(
            &mut candidate,
            start + 1024 + mid * stride + if v2 { 0 } else { 4 },
        )?;
        match candidate.cmp(&bytes) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(true),
        }
    }
    Ok(false)
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
