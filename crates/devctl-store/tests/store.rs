use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use devctl_store::{
    CoverageSummary, LineCounts, RawReports, Record, RedbStore, RetentionPolicy, Status, Store,
    StoreError, TestSummary,
};
use redb::{Database, TableDefinition};

const DAY_MS: i64 = 86_400_000;

fn store() -> (tempfile::TempDir, RedbStore) {
    let dir = tempfile::tempdir().unwrap();
    let s = RedbStore::new(dir.path().join("store.redb"));
    (dir, s)
}

fn rec(commit: u8, tree: u8, branch: &str, finished: i64, status: Status) -> Record {
    let mut files = BTreeMap::new();
    files.insert(
        "src/a.rs".to_string(),
        LineCounts {
            coverable: 10,
            covered: 9,
        },
    );
    Record {
        commit: vec![commit; 20],
        tree: vec![tree; 20],
        branch: branch.to_string(),
        started_at_ms: finished - 100,
        finished_at_ms: finished,
        status,
        coverage: CoverageSummary {
            total: LineCounts {
                coverable: 10,
                covered: 9,
            },
            files,
        },
        tests: TestSummary {
            passed: 3,
            failed: 0,
            errored: 0,
            skipped: 1,
            duration_ms: 42,
        },
        devctl_version: "0.1.0".into(),
        just_version: "1.59.0".into(),
    }
}

#[test]
fn put_then_lookup_by_commit_tree_branch() {
    let (_d, s) = store();
    let r = rec(1, 2, "dev", 1000, Status::Ok);
    let id = s.put_record(&r, None).unwrap();
    assert_eq!(s.record_for(&r.commit).unwrap(), Some((id, r.clone())));
    assert_eq!(s.record_for_tree(&r.tree).unwrap(), Some((id, r.clone())));
    assert_eq!(s.records_on("dev", 10).unwrap(), vec![(id, r.clone())]);
    assert_eq!(s.latest_ok("dev").unwrap(), Some((id, r)));
    assert_eq!(s.branches().unwrap(), vec!["dev".to_string()]);
    assert_eq!(s.record_for(&[9; 20]).unwrap(), None);
}

#[test]
fn by_tree_ignores_error_records() {
    let (_d, s) = store();
    let r = rec(1, 2, "dev", 1000, Status::error("boom"));
    let id = s.put_record(&r, None).unwrap();
    assert_eq!(s.record_for_tree(&r.tree).unwrap(), None);
    assert_eq!(s.record_for(&r.commit).unwrap().unwrap().0, id);
    assert_eq!(s.latest_ok("dev").unwrap(), None);
}

#[test]
fn record_for_returns_latest() {
    let (_d, s) = store();
    let a = s
        .put_record(&rec(1, 2, "dev", 1000, Status::TestsFailed), None)
        .unwrap();
    let b = s
        .put_record(&rec(1, 2, "dev", 2000, Status::Ok), None)
        .unwrap();
    assert!(b > a);
    assert_eq!(s.record_for(&[1; 20]).unwrap().unwrap().0, b);
    assert_eq!(s.record_for_tree(&[2; 20]).unwrap().unwrap().0, b);
}

#[test]
fn records_on_is_newest_first_and_limited() {
    let (_d, s) = store();
    let mut ids = Vec::new();
    for i in 0..5u8 {
        ids.push(
            s.put_record(&rec(i, i, "dev", 1000 + i64::from(i), Status::Ok), None)
                .unwrap(),
        );
    }
    s.put_record(&rec(50, 50, "other", 9999, Status::Ok), None)
        .unwrap();
    let got: Vec<_> = s
        .records_on("dev", 3)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(got, vec![ids[4], ids[3], ids[2]]);
    let mut b = s.branches().unwrap();
    b.sort();
    assert_eq!(b, vec!["dev".to_string(), "other".to_string()]);
}

#[test]
fn same_branch_same_timestamp_does_not_collide() {
    let (_d, s) = store();
    let a = s
        .put_record(&rec(1, 1, "dev", 5000, Status::Ok), None)
        .unwrap();
    let b = s
        .put_record(&rec(2, 2, "dev", 5000, Status::Ok), None)
        .unwrap();
    let c = s
        .put_record(&rec(3, 3, "dev", 5000, Status::Ok), None)
        .unwrap();
    let got: Vec<_> = s
        .records_on("dev", 10)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(got, vec![c, b, a]);
}

#[test]
fn raw_round_trips_compressed() {
    let (d, s) = store();
    let mut lcov = String::new();
    for i in 0..40_000 {
        writeln!(lcov, "DA:{},{}", i % 500, i % 3).unwrap();
    }
    // ~3 MB of LCOV. redb never shrinks below a ~1 MiB initial file, so the
    // check is that 3 MB of text did not grow the file past 1.5 MiB.
    let lcov = lcov.repeat(9);
    assert!(lcov.len() > 3_000_000);
    let raw = RawReports {
        lcov,
        junit: vec!["<testsuite/>".repeat(1000), "<x/>".into()],
    };
    let id = s
        .put_record(&rec(1, 1, "dev", 1000, Status::Ok), Some(&raw))
        .unwrap();
    assert_eq!(s.raw(id).unwrap(), Some(raw));
    let size = std::fs::metadata(d.path().join("store.redb"))
        .unwrap()
        .len();
    assert!(size < 1536 * 1024, "store is {size} bytes");
    let id2 = s
        .put_record(&rec(2, 2, "dev", 2000, Status::Ok), None)
        .unwrap();
    assert_eq!(s.raw(id2).unwrap(), None);
}

#[test]
fn prune_keeps_n_per_branch_and_drops_old_raw() {
    let (_d, s) = store();
    let now = 1000 * DAY_MS;
    let raw = RawReports {
        lcov: "TN:\n".into(),
        junit: vec![],
    };
    let mut ids = Vec::new();
    for i in 0..205u32 {
        let mut r = rec(0, 0, "dev", now - 1000 + i64::from(i), Status::Ok);
        r.commit = i.to_be_bytes().to_vec();
        r.tree = (i + 10_000).to_be_bytes().to_vec();
        ids.push(s.put_record(&r, Some(&raw)).unwrap());
    }
    // Old record on another branch.
    let old = rec(7, 7, "old", now - 91 * DAY_MS, Status::Ok);
    let old_id = s.put_record(&old, Some(&raw)).unwrap();
    // A later record reusing the oldest dev commit/tree: those index entries
    // must keep pointing at it after the oldest dev record is pruned.
    let mut reuse = rec(0, 0, "feature", now - 500, Status::Ok);
    reuse.commit = 0u32.to_be_bytes().to_vec();
    reuse.tree = 10_000u32.to_be_bytes().to_vec();
    let reuse_id = s.put_record(&reuse, Some(&raw)).unwrap();

    let stats = s
        .prune(
            &RetentionPolicy {
                records_per_branch: 200,
                raw_reports_days: 90,
            },
            now,
        )
        .unwrap();
    assert_eq!(stats.records_removed, 5);
    assert_eq!(stats.raw_removed, 5 + 1);

    assert_eq!(s.records_on("dev", 1000).unwrap().len(), 200);
    for id in &ids[..5] {
        assert_eq!(s.raw(*id).unwrap(), None);
    }
    assert!(s.raw(ids[5]).unwrap().is_some());
    assert_eq!(s.record_for(&1u32.to_be_bytes()).unwrap(), None);
    assert_eq!(s.record_for_tree(&10_001u32.to_be_bytes()).unwrap(), None);
    assert_eq!(
        s.record_for(&0u32.to_be_bytes()).unwrap().unwrap().0,
        reuse_id
    );
    assert_eq!(
        s.record_for_tree(&10_000u32.to_be_bytes())
            .unwrap()
            .unwrap()
            .0,
        reuse_id
    );
    assert_eq!(s.raw(old_id).unwrap(), None);
    assert_eq!(s.record_for(&[7; 20]).unwrap().unwrap().0, old_id);
}

#[test]
fn newer_schema_is_refused() {
    let (d, s) = store();
    s.put_record(&rec(1, 1, "dev", 1, Status::Ok), None)
        .unwrap();
    let path = d.path().join("store.redb");
    {
        let db = Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx
                .open_table(TableDefinition::<&str, u32>::new("meta"))
                .unwrap();
            t.insert("schema", 2).unwrap();
        }
        tx.commit().unwrap();
    }
    match s.branches() {
        Err(StoreError::NewerSchema {
            found: 2,
            supported: 1,
            path: p,
        }) => assert_eq!(p, path),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn garbage_file_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.redb");
    std::fs::write(&path, b"not redb").unwrap();
    let s = RedbStore::new(path.clone());
    match s.branches() {
        Err(StoreError::Corrupt { path: p, .. }) => assert_eq!(p, path),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn missing_parent_dir_is_io_not_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("nope").join("store.redb");
    let s = RedbStore::new(path.clone());
    assert!(matches!(
        s.put_record(&rec(1, 1, "dev", 1, Status::Ok), None),
        Err(StoreError::Io { .. })
    ));
    // Reads of a missing store are empty and do not create it.
    assert_eq!(s.branches().unwrap(), Vec::<String>::new());
    assert_eq!(s.record_for(&[1; 20]).unwrap(), None);
    assert!(!path.exists());
}

#[test]
fn reads_of_missing_store_do_not_create_it() {
    let (d, s) = store();
    assert_eq!(s.records_on("dev", 5).unwrap(), vec![]);
    assert_eq!(s.latest_ok("dev").unwrap(), None);
    assert_eq!(s.raw(1).unwrap(), None);
    assert!(!d.path().join("store.redb").exists());
}

#[test]
fn readers_share_the_store() {
    let (d, s) = store();
    let id = s
        .put_record(&rec(1, 1, "dev", 1, Status::Ok), None)
        .unwrap();
    let held = redb::ReadOnlyDatabase::open(d.path().join("store.redb")).unwrap();
    let s2 =
        RedbStore::new(d.path().join("store.redb")).with_busy_budget(Duration::from_millis(200));
    assert_eq!(s2.record_for(&[1; 20]).unwrap().unwrap().0, id);
    drop(held);
}

#[test]
fn ids_never_reused_after_prune() {
    let (_d, s) = store();
    let _a = s
        .put_record(&rec(1, 1, "dev", 1000, Status::Ok), None)
        .unwrap();
    // Newest id, but older finished_at: prune(1) removes it.
    let b = s
        .put_record(&rec(2, 2, "dev", 500, Status::Ok), None)
        .unwrap();
    let stats = s
        .prune(
            &RetentionPolicy {
                records_per_branch: 1,
                raw_reports_days: 90,
            },
            2000,
        )
        .unwrap();
    assert_eq!(stats.records_removed, 1);
    assert_eq!(s.record_for(&[2; 20]).unwrap(), None);
    let c = s
        .put_record(&rec(3, 3, "dev", 3000, Status::Ok), None)
        .unwrap();
    assert!(c > b, "{c} <= {b}");
}

#[test]
fn concurrent_processes_all_succeed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.redb");
    let handles: Vec<_> = (0..8u8)
        .map(|t| {
            let path = path.clone();
            std::thread::spawn(move || {
                // Not latency-sensitive: only checks that no write is lost.
                let s = RedbStore::new(path).with_busy_budget(Duration::from_secs(60));
                for i in 0..20u8 {
                    let mut r = rec(t, t, "dev", i64::from(t) * 100 + i64::from(i), Status::Ok);
                    r.commit = vec![t, i];
                    s.put_record(&r, None).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let s = RedbStore::new(path);
    assert_eq!(s.records_on("dev", 1000).unwrap().len(), 160);
}

#[test]
fn realistic_mix_with_default_budget_has_no_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.redb");
    let raw = RawReports {
        lcov: "TN:\n".repeat(100),
        junit: vec![],
    };
    let seed = RedbStore::new(path.clone());
    let mut r0 = rec(0, 0, "dev", 1, Status::Ok);
    r0.commit = vec![0, 0];
    seed.put_record(&r0, Some(&raw)).unwrap();

    let writer = {
        let path = path.clone();
        let raw = raw.clone();
        std::thread::spawn(move || {
            let s = RedbStore::new(path);
            for i in 1..=10u8 {
                let mut r = rec(i, i, "dev", i64::from(i), Status::Ok);
                r.commit = vec![i, i];
                s.put_record(&r, Some(&raw)).unwrap();
                s.prune(
                    &RetentionPolicy {
                        records_per_branch: 200,
                        raw_reports_days: 90,
                    },
                    1_000,
                )
                .unwrap();
            }
        })
    };
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let s = RedbStore::new(path);
                for _ in 0..10 {
                    let (id, _) = s.record_for(&[0, 0]).unwrap().unwrap();
                    assert!(s.raw(id).unwrap().is_some());
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for h in readers {
        h.join().unwrap();
    }
}

#[test]
fn busy_budget_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.redb");
    let _held = Database::create(&path).unwrap();
    let s = RedbStore::new(path.clone()).with_busy_budget(Duration::from_millis(200));
    let start = std::time::Instant::now();
    match s.branches() {
        Err(StoreError::Busy { path: p }) => assert_eq!(p, path),
        other => panic!("unexpected: {other:?}"),
    }
    let el = start.elapsed();
    assert!(
        el >= Duration::from_millis(150) && el < Duration::from_secs(3),
        "{el:?}"
    );
}

#[test]
fn status_error_keeps_last_4096_bytes_on_char_boundary() {
    let msg = format!("{}{}", "é".repeat(3000), "tail");
    let Status::Error { stderr_tail } = Status::error(&msg) else {
        panic!()
    };
    assert!(stderr_tail.len() <= 4096 && stderr_tail.len() >= 4094);
    assert!(stderr_tail.ends_with("tail"));
    assert_eq!(
        Status::error("short"),
        Status::Error {
            stderr_tail: "short".into()
        }
    );
}
