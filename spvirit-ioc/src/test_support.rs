//! Shared test-only helpers. Not part of the public API.

use crate::build::build_records;
use crate::lockset::RecordDb;
use spvirit_server::db::parse_db_records;
use std::collections::HashMap;

/// Parse and build a `.db` fragment into a [`RecordDb`], panicking on any
/// parse or build error — the tests that use this only care about the
/// success path.
pub(crate) fn db(text: &str) -> RecordDb {
    let raw = parse_db_records(text, "t.db", &HashMap::new()).expect("parse");
    RecordDb::build(build_records(&raw).expect("build"))
}
