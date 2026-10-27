//! Per-repo record store (`.git/devctl/store.redb`).
//!
//! Every operation opens the database, runs one transaction and closes it
//! again, retrying with exponential backoff while another handle holds the
//! file (redb allows one open handle at a time, across processes and across
//! handles in one process).

mod record;
mod redb_store;
mod schema;

pub use record::{
    CoverageSummary, LineCounts, PruneStats, RawReports, Record, RecordId, RetentionPolicy, Status,
    TestSummary,
};
pub use redb_store::{RedbStore, Store, StoreError};
pub use schema::SCHEMA_VERSION;
