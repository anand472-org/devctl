use redb::{ReadableTable, TableDefinition};

pub const SCHEMA_VERSION: u32 = 1;

pub(crate) const META: TableDefinition<&str, u32> = TableDefinition::new("meta");
pub(crate) const RECORDS: TableDefinition<u64, &[u8]> = TableDefinition::new("records");
pub(crate) const BY_COMMIT: TableDefinition<&[u8], u64> = TableDefinition::new("by_commit");
pub(crate) const BY_TREE: TableDefinition<&[u8], u64> = TableDefinition::new("by_tree");
pub(crate) const BY_BRANCH: TableDefinition<(&str, i64), u64> = TableDefinition::new("by_branch");
pub(crate) const RAW: TableDefinition<u64, &[u8]> = TableDefinition::new("raw");

const SCHEMA_KEY: &str = "schema";

/// Outcome of looking at the stored schema version.
pub(crate) enum SchemaCheck {
    Current,
    Newer(u32),
}

/// Migrate forward from schema `from` to [`SCHEMA_VERSION`]. No steps exist
/// yet; each future schema bump adds an arm that upgrades by one version.
#[allow(clippy::match_single_binding)]
fn migrate(from: u32) {
    match from {
        // Identity: nothing to do until schema 2 exists.
        _ => {}
    }
}

/// Check the schema inside a write transaction, initialising or migrating it.
pub(crate) fn ensure_write(tx: &redb::WriteTransaction) -> Result<SchemaCheck, redb::Error> {
    let mut meta = tx.open_table(META)?;
    let found = meta.get(SCHEMA_KEY)?.map(|v| v.value());
    match found {
        None => {
            meta.insert(SCHEMA_KEY, SCHEMA_VERSION)?;
            Ok(SchemaCheck::Current)
        }
        Some(v) if v > SCHEMA_VERSION => Ok(SchemaCheck::Newer(v)),
        Some(v) => {
            if v < SCHEMA_VERSION {
                migrate(v);
                meta.insert(SCHEMA_KEY, SCHEMA_VERSION)?;
            }
            Ok(SchemaCheck::Current)
        }
    }
}

/// Check the schema inside a read transaction. A missing `meta` table or key
/// means a store nothing has written to yet.
pub(crate) fn check_read(tx: &redb::ReadTransaction) -> Result<SchemaCheck, redb::Error> {
    let meta = match tx.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(SchemaCheck::Current),
        Err(e) => return Err(e.into()),
    };
    match meta.get(SCHEMA_KEY)?.map(|v| v.value()) {
        Some(v) if v > SCHEMA_VERSION => Ok(SchemaCheck::Newer(v)),
        _ => Ok(SchemaCheck::Current),
    }
}
