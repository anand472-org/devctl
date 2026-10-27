//! Per-repo record store (`.git/devctl/store.redb`).
//!
//! Every operation opens the database, runs one transaction and closes it
//! again. Writers hold redb's exclusive file lock; readers open read-only
//! under a shared lock (and never create the file). While another handle
//! holds a conflicting lock (across processes or handles in one process) the
//! open is retried with short jittered polling (2-8 ms) bounded by the busy
//! budget; long backoffs would let a handle that just released the lock win
//! it again and starve the waiters.

mod record;
mod redb_store;
mod schema;

pub use record::{
    CoverageSummary, LineCounts, PruneStats, RawReports, Record, RecordId, RetentionPolicy, Status,
    TestSummary,
};
pub use redb_store::{RedbStore, Store, StoreError};
pub use schema::SCHEMA_VERSION;
