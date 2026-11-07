//! Explicit, opt-in remote operations. Merely exporting never pushes anything.
use crate::{
    backend::{execute_command, git, redact},
    cache::{atomic, Lock},
    db::Store,
    error,
    model::name_ok,
    Result,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{path::Path, process::Command, time::Duration};
fn gh(path: &Path, args: &[&str]) -> Result<Value> {
    let mut cmd = Command::new("gh");
    cmd.current_dir(path)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1");
    let out = execute_command(&mut cmd, None, Duration::from_secs(120))?;
    if !out.status.success() {
        return Err(error(format!(
            "gh failed: {}",
            redact(&String::from_utf8_lossy(&out.stderr))
        )));
    }
    if out.stdout.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| json!(String::from_utf8_lossy(&out.stdout).trim())))
    }
}
fn manifest(path: &Path) -> Result<Value> {
    let v: Value = serde_json::from_slice(&std::fs::read(path.join("gitalator-export.json"))?)?;
    if v["format"] != 1 || v["repositories"].as_array().is_none() {
        return Err(error("invalid export manifest"));
    }
    Ok(v)
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .ok_or_else(|| error(format!("missing manifest field {k}")))
}
pub fn review(s: &Store, path: &Path, publish: bool) -> Result<Value> {
    let path = std::fs::canonicalize(path)?;
    let _lock = Lock::take(&s.root.join("locks").join(format!(
        "review-{}",
        crate::cache::hash(path.to_string_lossy().as_bytes())
    )))?;
    let v = manifest(&path)?;
    let workspace = text(&v, "workspace")?;
    let revision = v["revision"]
        .as_i64()
        .ok_or_else(|| error("missing revision"))?;
    let mut result = vec![];
    for repo in v["repositories"].as_array().unwrap() {
        let mount = text(repo, "mount")?;
        if !name_ok(workspace) || !name_ok(mount) {
            return Err(error("invalid export identity"));
        }
        let root = Path::new(text(repo, "path")?);
        let head = text(repo, "head")?;
        let branch = format!("gtl/{workspace}/{revision}/{mount}");
        if !publish {
            result.push(json!({"mount":mount,"branch":branch,"head":head,"action":"push and create/reconcile PR"}));
            continue;
        }
        if !git(root, &["status", "--porcelain"])?.is_empty() {
            return Err(error(
                "export worktree is dirty; import and export a new checkpoint before review",
            ));
        }
        let actual = String::from_utf8(git(root, &["rev-parse", "HEAD"])?)?;
        if actual.trim() != head {
            return Err(error("export HEAD differs from recorded checkpoint"));
        }
        s.conn()?.execute("INSERT INTO reviews(export_path,mount,branch,head,state) VALUES(?1,?2,?3,?4,'PREPARING') ON CONFLICT(export_path,mount) DO NOTHING",params![path.to_string_lossy(),mount,branch,head])?;
        git(
            root,
            &["push", "origin", &format!("{head}:refs/heads/{branch}")],
        )?;
        let existing = gh(
            root,
            &[
                "pr",
                "list",
                "--head",
                &branch,
                "--state",
                "all",
                "--json",
                "url,headRefOid,state",
            ],
        )?;
        let url = if let Some(pr) = existing.as_array().and_then(|a| a.first()) {
            if pr["headRefOid"].as_str() != Some(head) {
                return Err(error("existing PR head differs from checkpoint"));
            }
            text(pr, "url")?.to_string()
        } else {
            let body = path.join(format!("review-{mount}.md"));
            atomic(&body,format!("Gitalator workspace `{workspace}`, checkpoint `{revision}`.\n\nThis review contains only the `{mount}` silo changes.\n").as_bytes())?;
            let response = gh(
                root,
                &[
                    "pr",
                    "create",
                    "--head",
                    &branch,
                    "--title",
                    &format!("{workspace}: {mount}"),
                    "--body-file",
                    body.to_str().ok_or_else(|| error("non-UTF8 path"))?,
                ],
            )?;
            response
                .as_str()
                .ok_or_else(|| error("unexpected gh response"))?
                .to_string()
        };
        s.conn()?.execute(
            "UPDATE reviews SET url=?3,state='EXPORTED' WHERE export_path=?1 AND mount=?2",
            params![path.to_string_lossy(), mount, url],
        )?;
        result.push(json!({"mount":mount,"url":url}));
    }
    Ok(json!({"publish":publish,"reviews":result}))
}
pub fn land(s: &Store, path: &Path, verify_only: bool) -> Result<Value> {
    let path = std::fs::canonicalize(path)?;
    let _lock = Lock::take(&s.root.join("locks").join(format!(
        "review-{}",
        crate::cache::hash(path.to_string_lossy().as_bytes())
    )))?;
    let v = manifest(&path)?;
    let mut landed = vec![];
    for repo in v["repositories"].as_array().unwrap() {
        let mount = text(repo, "mount")?;
        let root = Path::new(text(repo, "path")?);
        let head = text(repo, "head")?;
        let url: Option<String> = s
            .conn()?
            .query_row(
                "SELECT url FROM reviews WHERE export_path=?1 AND mount=?2",
                params![path.to_string_lossy(), mount],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let url = url.ok_or_else(|| error("export has no recorded review"))?;
        let status = gh(
            root,
            &["pr", "view", &url, "--json", "state,headRefOid,mergeCommit"],
        )?;
        if status["headRefOid"].as_str() != Some(head) {
            return Err(error(
                "review head moved; export and test the new checkpoint",
            ));
        }
        if status["state"] != "MERGED" {
            if verify_only {
                return Err(error(format!("review for {mount} is not merged")));
            }
            return Err(error("automatic multi-repo landing requires integration-test evidence and policy wiring; merge in your silo, then use gtl land --verify-only (see BACKLOG.md)"));
        }
        let commit = status["mergeCommit"]["oid"]
            .as_str()
            .ok_or_else(|| error("missing landed commit"))?;
        git(root, &["fetch", "origin", commit])?;
        // Merge commits can include unrelated base movement: prove the checkpoint diff is present through tree equality only when exact.
        let expected = git(root, &["rev-parse", &format!("{head}^{{tree}}")])?;
        let actual = git(root, &["rev-parse", &format!("{commit}^{{tree}}")])?;
        if expected != actual {
            return Err(error(
                "landed tree differs from tested export; revalidation required",
            ));
        }
        s.conn()?.execute(
            "UPDATE reviews SET state='LANDED' WHERE export_path=?1 AND mount=?2",
            params![path.to_string_lossy(), mount],
        )?;
        landed.push(json!({"mount":mount,"commit":commit}));
    }
    let receipt = json!({"workspace":v["workspace"],"revision":v["revision"],"landed":landed});
    atomic(
        &path.join("landed.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(receipt)
}
pub fn bundle(export: &Path, destination: &Path) -> Result<Value> {
    let mut v = manifest(export)?;
    if destination.exists() {
        return Err(error("bundle destination exists"));
    }
    std::fs::create_dir_all(destination)?;
    let dest = std::fs::canonicalize(destination)?;
    for repo in v["repositories"].as_array_mut().unwrap() {
        let root = Path::new(text(repo, "path")?).to_path_buf();
        let name = text(repo, "mount")?.to_string();
        if !name_ok(&name) {
            return Err(error("invalid mount ID"));
        }
        git(
            &root,
            &[
                "bundle",
                "create",
                dest.join(format!("{name}.bundle")).to_str().unwrap(),
                "HEAD",
            ],
        )?;
        git(
            &root,
            &[
                "bundle",
                "verify",
                dest.join(format!("{name}.bundle")).to_str().unwrap(),
            ],
        )?;
        let lfs = git(&root, &["lfs", "ls-files", "--all", "--long"])?;
        let mut payloads = vec![];
        for line in std::str::from_utf8(&lfs)?.lines() {
            let oid = line
                .split_whitespace()
                .next()
                .ok_or_else(|| error("invalid LFS listing"))?;
            if oid.len() != 64 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(error("invalid LFS object ID"));
            }
            let file = root
                .join(".git/lfs/objects")
                .join(&oid[..2])
                .join(&oid[2..4])
                .join(oid);
            if !file.exists() {
                return Err(error("bundle incomplete: fetch required LFS history in export worktree with git lfs fetch --all, then retry at a new destination"));
            }
            let bytes = std::fs::read(&file)?;
            if crate::cache::hash(&bytes) != oid {
                return Err(error("LFS bundle integrity failure"));
            }
            let relative = format!("{name}.lfs/{}/{}/{oid}", &oid[..2], &oid[2..4]);
            atomic(&dest.join(&relative), &bytes)?;
            payloads.push(json!({"sha256":oid,"path":relative,"size":bytes.len()}));
        }
        repo["bundle"] = json!(format!("{name}.bundle"));
        repo["lfs_payloads"] = json!(payloads);
        repo.as_object_mut().unwrap().remove("path");
    }
    atomic(
        &dest.join("gitalator-export.json"),
        &serde_json::to_vec_pretty(&v)?,
    )?;
    Ok(json!({"bundle_directory":dest}))
}
