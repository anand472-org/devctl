use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use redb::{Database, DatabaseError, ReadOnlyDatabase, ReadableDatabase, ReadableTable};

use crate::record::{PruneStats, RawReports, Record, RecordId, RetentionPolicy, Status};
use crate::schema::{
    self, BY_BRANCH, BY_COMMIT, BY_TREE, RAW, RECORDS, SCHEMA_VERSION, SchemaCheck,
};

const DEFAULT_BUSY_BUDGET: Duration = Duration::from_secs(5);
const FIRST_BACKOFF: Duration = Duration::from_millis(2);
const MAX_BACKOFF: Duration = Duration::from_millis(8);
/// Pause after a write closes the file so a waiter polling at the backoff
/// rate gets a turn before a writer looping on the store (e.g. a worker) can
/// take the exclusive lock again; without it the releasing handle starves the
/// others. Readers share the lock and need no pause.
const RELEASE_PAUSE: Duration = Duration::from_millis(3);
const NEXT_ID_KEY: &str = "next_id";
const DAY_MS: i64 = 86_400_000;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store at {path} is busy (another devctl process held it for 5 s); try again")]
    Busy { path: PathBuf },
    #[error("store at {path} is corrupt ({reason}); delete it and run `devctl record`")]
    Corrupt { path: PathBuf, reason: String },
    #[error("store at {path} has schema {found}, this devctl supports {supported}; upgrade devctl")]
    NewerSchema {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("store at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub trait Store {
    /// Insert `record` (and optionally its raw reports) and return its ID.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn put_record(&self, record: &Record, raw: Option<&RawReports>)
    -> Result<RecordId, StoreError>;
    /// The latest record for a commit.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn record_for(&self, commit: &[u8]) -> Result<Option<(RecordId, Record)>, StoreError>;
    /// The latest `Ok` or `TestsFailed` record for a tree.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn record_for_tree(&self, tree: &[u8]) -> Result<Option<(RecordId, Record)>, StoreError>;
    /// Up to `limit` records on a branch, newest first.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn records_on(&self, branch: &str, limit: usize)
    -> Result<Vec<(RecordId, Record)>, StoreError>;
    /// The newest `Ok` record on a branch.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn latest_ok(&self, branch: &str) -> Result<Option<(RecordId, Record)>, StoreError>;
    /// Every branch that has at least one record.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn branches(&self) -> Result<Vec<String>, StoreError>;
    /// The raw reports stored for a record, if any remain.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn raw(&self, id: RecordId) -> Result<Option<RawReports>, StoreError>;
    /// Apply the retention policy.
    ///
    /// # Errors
    /// See [`StoreError`].
    fn prune(&self, policy: &RetentionPolicy, now_ms: i64) -> Result<PruneStats, StoreError>;
}

/// redb-backed [`Store`]. Holds only the path: each call opens the file, runs
/// one transaction and closes it.
///
/// `by_branch` collisions: the key is `(branch, finished_at_ms)`, so two
/// records on one branch finishing in the same millisecond would share a key.
/// On insert the timestamp part of the key is bumped by 1 ms until it is free
/// (the record itself keeps its real `finished_at_ms`). Ordering is preserved
/// (later insert sorts later) and the index entry for a record is found by
/// matching the stored record ID, never by recomputing the key.
pub struct RedbStore {
    path: PathBuf,
    busy_budget: Duration,
}

impl RedbStore {
    #[must_use]
    pub fn new(path: PathBuf) -> RedbStore {
        RedbStore {
            path,
            busy_budget: DEFAULT_BUSY_BUDGET,
        }
    }

    #[must_use]
    pub fn with_busy_budget(mut self, d: Duration) -> RedbStore {
        self.busy_budget = d;
        self
    }

    fn convert(&self, e: impl Into<redb::Error>) -> StoreError {
        convert_error(&self.path, e.into())
    }

    fn corrupt(&self, reason: impl std::fmt::Display) -> StoreError {
        StoreError::Corrupt {
            path: self.path.clone(),
            reason: reason.to_string(),
        }
    }

    /// Sleep for one polling step of the busy loop, or fail with `Busy` once
    /// the budget is spent. Polling is short and jittered (2-8 ms, bounded by
    /// the busy budget): a long exponential backoff lets a handle that just
    /// released the lock win it again before escalated waiters wake, which
    /// starves them.
    fn wait_busy(&self, start: Instant, delay: &mut Duration) -> Result<(), StoreError> {
        let remaining = self.busy_budget.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(StoreError::Busy {
                path: self.path.clone(),
            });
        }
        tracing::debug!(monotonic_counter.devctl.store.busy_retries = 1_u64);
        let half = u64::try_from(delay.as_nanos() / 2).unwrap_or(1).max(1);
        let jitter = Duration::from_nanos(fastrand::u64(0..half));
        std::thread::sleep((*delay + jitter).min(remaining));
        *delay = (*delay * 2).min(MAX_BACKOFF);
        Ok(())
    }

    /// Open the database for writing, polling while another handle holds it.
    /// Only `DatabaseError::DatabaseAlreadyOpen` (redb's failed file lock)
    /// counts as busy.
    fn open(&self) -> Result<Database, StoreError> {
        let start = Instant::now();
        let mut delay = FIRST_BACKOFF;
        loop {
            match Database::create(&self.path) {
                Ok(db) => return Ok(db),
                Err(DatabaseError::DatabaseAlreadyOpen) => self.wait_busy(start, &mut delay)?,
                Err(e) => return Err(self.convert(e)),
            }
        }
    }

    /// Run `f` in a read transaction. Opens read-only (shared file lock, so
    /// readers do not block each other and never create the file). A missing
    /// or empty file yields `empty`. If redb says the file needs repair
    /// (unclean shutdown) the read falls back to a writable open, which
    /// repairs it.
    fn read<T>(
        &self,
        op: &'static str,
        empty: T,
        f: impl FnOnce(&redb::ReadTransaction) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let _span = tracing::info_span!("store.txn", op).entered();
        let start = Instant::now();
        let mut delay = FIRST_BACKOFF;
        loop {
            match std::fs::metadata(&self.path) {
                Ok(m) if m.len() == 0 => return Ok(empty),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(empty),
                Err(source) => {
                    return Err(StoreError::Io {
                        path: self.path.clone(),
                        source,
                    });
                }
            }
            match ReadOnlyDatabase::open(&self.path) {
                Ok(db) => return self.read_tx(&db, f),
                Err(DatabaseError::DatabaseAlreadyOpen) => self.wait_busy(start, &mut delay)?,
                Err(DatabaseError::RepairAborted) => {
                    let db = self.open()?;
                    let out = self.read_tx(&db, f);
                    drop(db);
                    return out;
                }
                Err(e) => return Err(self.convert(e)),
            }
        }
    }

    fn read_tx<T>(
        &self,
        db: &impl ReadableDatabase,
        f: impl FnOnce(&redb::ReadTransaction) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let tx = db.begin_read().map_err(|e| self.convert(e))?;
        match schema::check_read(&tx).map_err(|e| self.convert(e))? {
            SchemaCheck::Current => {}
            SchemaCheck::Newer(found) => return Err(self.newer(found)),
        }
        f(&tx)
    }

    fn write<T>(
        &self,
        op: &'static str,
        f: impl FnOnce(&redb::WriteTransaction) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let _span = tracing::info_span!("store.txn", op).entered();
        let db = self.open()?;
        let out = (|| {
            let tx = db.begin_write().map_err(|e| self.convert(e))?;
            match schema::ensure_write(&tx).map_err(|e| self.convert(e))? {
                SchemaCheck::Current => {}
                SchemaCheck::Newer(found) => return Err(self.newer(found)),
            }
            let out = f(&tx)?;
            tx.commit().map_err(|e| self.convert(e))?;
            Ok(out)
        })();
        drop(db);
        std::thread::sleep(RELEASE_PAUSE);
        out
    }

    fn id_overflow(&self) -> StoreError {
        StoreError::Io {
            path: self.path.clone(),
            source: std::io::Error::other("record id counter exhausted"),
        }
    }

    fn encode_error(&self, e: &postcard::Error) -> StoreError {
        StoreError::Io {
            path: self.path.clone(),
            source: std::io::Error::other(format!("cannot encode value: {e}")),
        }
    }

    fn newer(&self, found: u32) -> StoreError {
        StoreError::NewerSchema {
            path: self.path.clone(),
            found,
            supported: SCHEMA_VERSION,
        }
    }

    fn decode_record(&self, bytes: &[u8]) -> Result<Record, StoreError> {
        postcard::from_bytes(bytes).map_err(|e| self.corrupt(format!("undecodable record: {e}")))
    }

    fn decode_raw(&self, bytes: &[u8]) -> Result<RawReports, StoreError> {
        let mut dec = ruzstd::decoding::StreamingDecoder::new(bytes)
            .map_err(|e| self.corrupt(format!("undecodable raw reports: {e}")))?;
        let mut plain = Vec::new();
        dec.read_to_end(&mut plain)
            .map_err(|e| self.corrupt(format!("undecodable raw reports: {e}")))?;
        postcard::from_bytes(&plain)
            .map_err(|e| self.corrupt(format!("undecodable raw reports: {e}")))
    }

    /// Fetch a record by ID from any readable `records` table.
    fn fetch(
        &self,
        records: &impl ReadableTable<u64, &'static [u8]>,
        id: RecordId,
    ) -> Result<Option<Record>, StoreError> {
        match records.get(id).map_err(|e| self.convert(e))? {
            Some(v) => Ok(Some(self.decode_record(v.value())?)),
            None => Ok(None),
        }
    }

    /// Resolve an index (`by_commit` / `by_tree`) entry to its record.
    fn lookup(
        &self,
        table: redb::TableDefinition<'static, &'static [u8], u64>,
        key: &[u8],
    ) -> Result<Option<(RecordId, Record)>, StoreError> {
        self.read("lookup", None, |tx| {
            let index = match tx.open_table(table) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(e) => return Err(self.convert(e)),
            };
            let Some(id) = index
                .get(key)
                .map_err(|e| self.convert(e))?
                .map(|v| v.value())
            else {
                return Ok(None);
            };
            let records = tx.open_table(RECORDS).map_err(|e| self.convert(e))?;
            Ok(self.fetch(&records, id)?.map(|r| (id, r)))
        })
    }

    /// Branch index entries newest first as `(record id)`, lazily bounded by
    /// `limit` through the caller.
    fn branch_ids_newest_first(
        &self,
        tx: &redb::ReadTransaction,
        branch: &str,
    ) -> Result<Option<Vec<RecordId>>, StoreError> {
        let table = match tx.open_table(BY_BRANCH) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(self.convert(e)),
        };
        let mut ids = Vec::new();
        for entry in table
            .range((branch, i64::MIN)..=(branch, i64::MAX))
            .map_err(|e| self.convert(e))?
            .rev()
        {
            let (_, v) = entry.map_err(|e| self.convert(e))?;
            ids.push(v.value());
        }
        Ok(Some(ids))
    }
}

fn convert_error(path: &Path, e: redb::Error) -> StoreError {
    let path = path.to_path_buf();
    match e {
        redb::Error::DatabaseAlreadyOpen => StoreError::Busy { path },
        redb::Error::Corrupted(reason) => StoreError::Corrupt { path, reason },
        redb::Error::UpgradeRequired(v) => StoreError::Corrupt {
            path,
            reason: format!("old redb file format {v}"),
        },
        e @ (redb::Error::TableTypeMismatch { .. }
        | redb::Error::TypeDefinitionChanged { .. }
        | redb::Error::TableIsMultimap(_)
        | redb::Error::TableIsNotMultimap(_)) => StoreError::Corrupt {
            path,
            reason: e.to_string(),
        },
        // redb reports a non-redb file (bad magic number, truncated header) as
        // an I/O error of these kinds; permission denied, disk full etc. stay Io.
        redb::Error::Io(source)
            if matches!(
                source.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            StoreError::Corrupt {
                path,
                reason: source.to_string(),
            }
        }
        redb::Error::Io(source) => StoreError::Io { path, source },
        other => StoreError::Io {
            path,
            source: std::io::Error::other(other.to_string()),
        },
    }
}

impl Store for RedbStore {
    fn put_record(
        &self,
        record: &Record,
        raw: Option<&RawReports>,
    ) -> Result<RecordId, StoreError> {
        // Encode before touching the file so the exclusive lock is held briefly.
        let bytes = postcard::to_stdvec(record).map_err(|e| self.encode_error(&e))?;
        let raw_bytes = match raw {
            Some(r) => {
                let plain = postcard::to_stdvec(r).map_err(|e| self.encode_error(&e))?;
                Some(ruzstd::encoding::compress_to_vec(
                    plain.as_slice(),
                    ruzstd::encoding::CompressionLevel::Fastest,
                ))
            }
            None => None,
        };
        self.write("put_record", |tx| {
            let mut records = tx.open_table(RECORDS).map_err(|e| self.convert(e))?;
            // IDs only ever increase, even after prune removes the newest
            // record: `meta.next_id` is the counter (seeded from the highest
            // existing ID for stores that predate it).
            let mut meta = tx.open_table(schema::META).map_err(|e| self.convert(e))?;
            let stored = meta
                .get(NEXT_ID_KEY)
                .map_err(|e| self.convert(e))?
                .map(|v| v.value());
            // `meta` values are u32 (spec 7.2), so the counter is a u32.
            let id = match stored {
                Some(n) => n,
                None => records
                    .last()
                    .map_err(|e| self.convert(e))?
                    .map_or(Ok(1), |(k, _)| u32::try_from(k.value() + 1))
                    .map_err(|_| self.id_overflow())?,
            };
            let next = id.checked_add(1).ok_or_else(|| self.id_overflow())?;
            meta.insert(NEXT_ID_KEY, next)
                .map_err(|e| self.convert(e))?;
            let id = RecordId::from(id);
            records
                .insert(id, bytes.as_slice())
                .map_err(|e| self.convert(e))?;

            let mut by_commit = tx.open_table(BY_COMMIT).map_err(|e| self.convert(e))?;
            by_commit
                .insert(record.commit.as_slice(), id)
                .map_err(|e| self.convert(e))?;

            if matches!(record.status, Status::Ok | Status::TestsFailed) {
                let mut by_tree = tx.open_table(BY_TREE).map_err(|e| self.convert(e))?;
                by_tree
                    .insert(record.tree.as_slice(), id)
                    .map_err(|e| self.convert(e))?;
            }

            let mut by_branch = tx.open_table(BY_BRANCH).map_err(|e| self.convert(e))?;
            let mut ts = record.finished_at_ms;
            while by_branch
                .get((record.branch.as_str(), ts))
                .map_err(|e| self.convert(e))?
                .is_some()
            {
                ts = ts.checked_add(1).ok_or_else(|| StoreError::Io {
                    path: self.path.clone(),
                    source: std::io::Error::other("finished_at_ms overflow in by_branch index"),
                })?;
            }
            by_branch
                .insert((record.branch.as_str(), ts), id)
                .map_err(|e| self.convert(e))?;

            if let Some(raw_bytes) = &raw_bytes {
                let mut raw_table = tx.open_table(RAW).map_err(|e| self.convert(e))?;
                raw_table
                    .insert(id, raw_bytes.as_slice())
                    .map_err(|e| self.convert(e))?;
            }
            Ok(id)
        })
    }

    fn record_for(&self, commit: &[u8]) -> Result<Option<(RecordId, Record)>, StoreError> {
        self.lookup(BY_COMMIT, commit)
    }

    fn record_for_tree(&self, tree: &[u8]) -> Result<Option<(RecordId, Record)>, StoreError> {
        self.lookup(BY_TREE, tree)
    }

    fn records_on(
        &self,
        branch: &str,
        limit: usize,
    ) -> Result<Vec<(RecordId, Record)>, StoreError> {
        self.read("records_on", Vec::new(), |tx| {
            let Some(ids) = self.branch_ids_newest_first(tx, branch)? else {
                return Ok(Vec::new());
            };
            let records = tx.open_table(RECORDS).map_err(|e| self.convert(e))?;
            let mut out = Vec::new();
            for id in ids.into_iter().take(limit) {
                if let Some(r) = self.fetch(&records, id)? {
                    out.push((id, r));
                }
            }
            Ok(out)
        })
    }

    fn latest_ok(&self, branch: &str) -> Result<Option<(RecordId, Record)>, StoreError> {
        self.read("latest_ok", None, |tx| {
            let Some(ids) = self.branch_ids_newest_first(tx, branch)? else {
                return Ok(None);
            };
            let records = tx.open_table(RECORDS).map_err(|e| self.convert(e))?;
            for id in ids {
                if let Some(r) = self.fetch(&records, id)?
                    && r.status == Status::Ok
                {
                    return Ok(Some((id, r)));
                }
            }
            Ok(None)
        })
    }

    fn branches(&self) -> Result<Vec<String>, StoreError> {
        self.read("branches", Vec::new(), |tx| {
            let table = match tx.open_table(BY_BRANCH) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(e) => return Err(self.convert(e)),
            };
            let mut out: Vec<String> = Vec::new();
            for entry in table.iter().map_err(|e| self.convert(e))? {
                let (k, _) = entry.map_err(|e| self.convert(e))?;
                let (branch, _) = k.value();
                if out.last().is_none_or(|last| last != branch) {
                    out.push(branch.to_string());
                }
            }
            Ok(out)
        })
    }

    fn raw(&self, id: RecordId) -> Result<Option<RawReports>, StoreError> {
        self.read("raw", None, |tx| {
            let table = match tx.open_table(RAW) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(e) => return Err(self.convert(e)),
            };
            match table.get(id).map_err(|e| self.convert(e))? {
                Some(v) => Ok(Some(self.decode_raw(v.value())?)),
                None => Ok(None),
            }
        })
    }

    fn prune(&self, policy: &RetentionPolicy, now_ms: i64) -> Result<PruneStats, StoreError> {
        self.write("prune", |tx| {
            let mut stats = PruneStats::default();
            let mut records = tx.open_table(RECORDS).map_err(|e| self.convert(e))?;
            let mut by_commit = tx.open_table(BY_COMMIT).map_err(|e| self.convert(e))?;
            let mut by_tree = tx.open_table(BY_TREE).map_err(|e| self.convert(e))?;
            let mut by_branch = tx.open_table(BY_BRANCH).map_err(|e| self.convert(e))?;
            let mut raw = tx.open_table(RAW).map_err(|e| self.convert(e))?;

            // Oldest-first (key, id) entries per branch.
            let mut per_branch: Vec<(String, Vec<(i64, RecordId)>)> = Vec::new();
            for entry in by_branch.iter().map_err(|e| self.convert(e))? {
                let (k, v) = entry.map_err(|e| self.convert(e))?;
                let (branch, ts) = k.value();
                match per_branch.last_mut() {
                    Some((b, list)) if b == branch => list.push((ts, v.value())),
                    _ => per_branch.push((branch.to_string(), vec![(ts, v.value())])),
                }
            }

            // Raw reports older than the cutoff go; the summaries stay. The
            // index key's ts is >= the record's finished_at_ms (collision
            // bumps only add), so `ts < cutoff` implies the record is stale
            // and no record has to be decoded for the check.
            let cutoff = now_ms.saturating_sub(i64::from(policy.raw_reports_days) * DAY_MS);
            for (branch, entries) in per_branch {
                let excess = entries.len().saturating_sub(policy.records_per_branch);
                for &(ts, id) in &entries[excess..] {
                    if ts < cutoff && raw.remove(id).map_err(|e| self.convert(e))?.is_some() {
                        stats.raw_removed += 1;
                    }
                }
                for &(ts, id) in &entries[..excess] {
                    let record = match records.remove(id).map_err(|e| self.convert(e))? {
                        Some(v) => Some(self.decode_record(v.value())?),
                        None => None,
                    };
                    by_branch
                        .remove((branch.as_str(), ts))
                        .map_err(|e| self.convert(e))?;
                    if raw.remove(id).map_err(|e| self.convert(e))?.is_some() {
                        stats.raw_removed += 1;
                    }
                    if let Some(r) = record {
                        let commit_points_here = by_commit
                            .get(r.commit.as_slice())
                            .map_err(|e| self.convert(e))?
                            .is_some_and(|v| v.value() == id);
                        if commit_points_here {
                            by_commit
                                .remove(r.commit.as_slice())
                                .map_err(|e| self.convert(e))?;
                        }
                        let tree_points_here = by_tree
                            .get(r.tree.as_slice())
                            .map_err(|e| self.convert(e))?
                            .is_some_and(|v| v.value() == id);
                        if tree_points_here {
                            by_tree
                                .remove(r.tree.as_slice())
                                .map_err(|e| self.convert(e))?;
                        }
                    }
                    stats.records_removed += 1;
                }
            }

            Ok(stats)
        })
    }
}
