use std::ffi::CString;

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;

mod catalog;
mod compress;
mod compression;
mod functions;
mod partition;
mod scan;
mod timeparse;
mod worker;

pg_module_magic!();

pub(crate) static MOCK_NOW: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);

/// Maximum distinct values tracked per column during streaming compression.
/// Columns exceeding this limit fall back to SQL COUNT(DISTINCT).
/// Set to 0 to always use SQL. Default: 1,000,000.
pub(crate) static NDISTINCT_MAX_TRACK: GucSetting<i32> =
    GucSetting::<i32>::new(1_000_000);

/// Threshold for choosing array_agg vs streaming compression path.
/// If row_count * num_columns < this value AND segment_by is empty, use array_agg.
/// Set to 0 to always use streaming. Default: 200,000,000.
pub(crate) static ARRAY_AGG_THRESHOLD: GucSetting<i32> =
    GucSetting::<i32>::new(200_000_000);

extension_sql!(
    r#"
CREATE SCHEMA IF NOT EXISTS _deltax_compressed;

CREATE TABLE IF NOT EXISTS deltax_deltatable (
    id              SERIAL PRIMARY KEY,
    schema_name     TEXT NOT NULL,
    table_name      TEXT NOT NULL,
    time_column     TEXT NOT NULL,
    partition_interval INTERVAL NOT NULL,
    compress_after  INTERVAL,
    drop_after      INTERVAL,
    segment_by      TEXT[],
    order_by        TEXT[],
    segment_size    INT DEFAULT 30000,
    created_at      TIMESTAMPTZ DEFAULT now(),
    UNIQUE(schema_name, table_name)
);

CREATE TABLE IF NOT EXISTS deltax_partition (
    id              SERIAL PRIMARY KEY,
    deltatable_id   INT REFERENCES deltax_deltatable(id) ON DELETE CASCADE,
    schema_name     TEXT NOT NULL,
    table_name      TEXT NOT NULL,
    range_start     TIMESTAMPTZ NOT NULL,
    range_end       TIMESTAMPTZ NOT NULL,
    is_compressed   BOOLEAN DEFAULT false,
    compressed_size BIGINT,
    raw_size        BIGINT,
    row_count       BIGINT,
    compressed_at   TIMESTAMPTZ,
    column_ndistinct JSONB,
    UNIQUE(schema_name, table_name)
);
"#,
    name = "create_catalog_tables",
);

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    GucRegistry::define_string_guc(
        c"pg_deltax.mock_now",
        c"Override current time for testing (timestamptz literal, empty = use real time)",
        c"Override current time for testing (timestamptz literal, empty = use real time)",
        &MOCK_NOW,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_deltax.ndistinct_max_track",
        c"Max distinct values tracked per column during streaming compression (0 = always use SQL)",
        c"Max distinct values tracked per column during streaming compression (0 = always use SQL)",
        &NDISTINCT_MAX_TRACK,
        0,
        100_000_000,
        GucContext::Suset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_deltax.array_agg_threshold",
        c"row_count * num_columns threshold for array_agg path (0 = always streaming)",
        c"row_count * num_columns threshold for array_agg path (0 = always streaming)",
        &ARRAY_AGG_THRESHOLD,
        0,
        2_000_000_000,
        GucContext::Suset,
        GucFlags::default(),
    );
    worker::register_bgworker();
    unsafe { scan::register_hook(); }
    unsafe { scan::register_executor_start_hook(); }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_extension_loads() {
        // Extension is loaded if this test runs at all
        let result = Spi::get_one::<i32>("SELECT 1").expect("query failed");
        assert_eq!(result, Some(1));
    }
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_deltax'"]
    }
}
