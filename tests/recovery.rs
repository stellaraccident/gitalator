mod common;
use common::*;
use gitalator::store::Store;
#[test]
fn recovery_child() {
    let Ok(root) = std::env::var("GTL_TEST_STATE") else {
        return;
    };
    let mut s = Store::open(std::path::Path::new(&root)).unwrap();
    if std::env::var("GTL_TEST_OP").unwrap() == "workspace" {
        s.create(
            "recovered",
            &std::env::var("GTL_TEST_DEPOT").unwrap(),
            "main",
        )
        .unwrap();
        return;
    }
    let w = s.get("dev").unwrap();
    let w = w.lock().unwrap();
    match std::env::var("GTL_TEST_OP").unwrap().as_str() {
        "rename" => w.rename("src/hello", "src/dest").unwrap(),
        "create" => {
            w.create_file("src/hello", 0o644).unwrap();
        }
        "delete" => w.unlink("src/hello", false).unwrap(),
        "copyup" => {
            w.copy_up("src/hello", false).unwrap();
        }
        _ => panic!(),
    }
}
#[test]
fn namespace_journal_redo_is_idempotent() {
    for point in ["journal-published", "journal-applied"] {
        for op in ["rename", "create", "delete", "copyup"] {
            let f = Fixture::new();
            let s = f.store();
            {
                let w = s.get("dev").unwrap();
                let w = w.lock().unwrap();
                if op == "create" {
                    w.unlink("src/hello", false).unwrap();
                }
                if op == "rename" {
                    w.copy_up("src/hello", false).unwrap();
                    std::io::Write::write_all(
                        &mut w.create_file("src/dest", 0o644).unwrap(),
                        b"old destination",
                    )
                    .unwrap();
                }
            }
            drop(s);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "recovery_child", "--nocapture"])
                .env("GTL_TEST_STATE", &f.state)
                .env("GTL_TEST_OP", op)
                .env("GTL_FAILPOINT", point)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(86));
            for _ in 0..2 {
                let s = Store::open(&f.state).unwrap();
                let w = s.get("dev").unwrap();
                let w = w.lock().unwrap();
                w.validate().unwrap();
                match op {
                    "rename" => {
                        assert!(w.entry("src/hello").is_err());
                        assert_eq!(
                            std::fs::read(w.entry("src/dest").unwrap().physical.unwrap()).unwrap(),
                            b"hello\n"
                        );
                    }
                    "create" => assert_eq!(w.entry("src/hello").unwrap().size, 0),
                    "copyup" => {
                        assert_eq!(
                            std::fs::read(w.upper().join("src/hello")).unwrap(),
                            b"hello\n"
                        );
                        assert_eq!(w.entry("src/hello").unwrap().mode, 0o644);
                    }
                    "delete" => assert!(w.entry("src/hello").is_err()),
                    _ => unreachable!(),
                }
            }
        }
    }
}

#[test]
fn interrupted_workspace_creation_recovers_without_touching_source() {
    for point in ["workspace-record", "workspace-materialized"] {
        let f = Fixture::new();
        let s = f.store();
        let original = s
            .get("dev")
            .unwrap()
            .lock()
            .unwrap()
            .record
            .active_commit
            .clone();
        drop(s);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "recovery_child", "--nocapture"])
            .env("GTL_TEST_STATE", &f.state)
            .env("GTL_TEST_OP", "workspace")
            .env("GTL_TEST_DEPOT", &f.depot)
            .env("GTL_FAILPOINT", point)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86));
        let s = Store::open(&f.state).unwrap();
        let w = s.get("recovered").unwrap();
        let w = w.lock().unwrap();
        assert!(!w.record.editing);
        assert!(w.depot().join(".git").is_file());
        assert_eq!(
            s.get("dev").unwrap().lock().unwrap().record.active_commit,
            original
        );
    }
}
