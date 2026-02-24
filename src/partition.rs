use pgrx::prelude::*;
use pgrx::spi::SpiClient;

use crate::catalog;

/// Convert an Interval to microseconds. Errors if months are present.
fn interval_to_usec(interval: &pgrx::datum::Interval) -> i64 {
    let months: i32 = interval
        .extract_part(DateTimeParts::Month)
        .and_then(|v| v.try_into().ok())
        .unwrap_or(0);

    if months != 0 {
        pgrx::error!("pg_cocoon: monthly partition intervals are not supported; use days instead");
    }

    let days: i64 = interval
        .extract_part(DateTimeParts::Day)
        .and_then(|v| v.try_into().ok())
        .unwrap_or(0);
    let hours: i64 = interval
        .extract_part(DateTimeParts::Hour)
        .and_then(|v| v.try_into().ok())
        .unwrap_or(0);
    let minutes: i64 = interval
        .extract_part(DateTimeParts::Minute)
        .and_then(|v| v.try_into().ok())
        .unwrap_or(0);
    let secs: i64 = interval
        .extract_part(DateTimeParts::Second)
        .and_then(|v| v.try_into().ok())
        .unwrap_or(0);

    days * 86_400_000_000 + hours * 3_600_000_000 + minutes * 60_000_000 + secs * 1_000_000
}

/// Format a microsecond epoch timestamp as a PostgreSQL-compatible TIMESTAMPTZ literal.
fn format_ts(usec: i64) -> String {
    let epoch_sec = usec as f64 / 1_000_000.0;
    Spi::get_one_with_args::<String>(
        "SELECT to_char(to_timestamp($1), 'YYYY-MM-DD HH24:MI:SS')",
        &[epoch_sec.into()],
    )
    .expect("failed to format timestamp")
    .unwrap()
}

/// Generate the partition table name from the parent table name and range start.
fn partition_name(table_name: &str, range_start_usec: i64, interval_usec: i64) -> String {
    let epoch_sec = range_start_usec as f64 / 1_000_000.0;
    let query = if interval_usec >= 86_400_000_000 {
        "SELECT to_char(to_timestamp($1), 'YYYYMMDD')"
    } else {
        "SELECT to_char(to_timestamp($1), 'YYYYMMDD_HH24MI')"
    };

    let date_part = Spi::get_one_with_args::<String>(query, &[epoch_sec.into()])
        .expect("failed to format partition name")
        .unwrap();

    format!("{}_p{}", table_name, date_part)
}

/// Align a timestamp (in microseconds since Unix epoch) down to the nearest
/// interval boundary.
fn align_to_interval(ts_usec: i64, interval_usec: i64) -> i64 {
    let d = ts_usec / interval_usec;
    let r = ts_usec % interval_usec;
    if r < 0 {
        (d - 1) * interval_usec
    } else {
        d * interval_usec
    }
}

/// Get current time as microseconds since Unix epoch via SPI.
fn now_usec() -> i64 {
    Spi::get_one::<i64>("SELECT (EXTRACT(EPOCH FROM now()) * 1000000)::int8")
        .expect("failed to get current time")
        .unwrap()
}

/// Convert unix-epoch microseconds to a TimestampWithTimeZone via SPI.
fn usec_to_tstz(usec: i64) -> TimestampWithTimeZone {
    let epoch_sec = usec as f64 / 1_000_000.0;
    Spi::get_one_with_args::<TimestampWithTimeZone>(
        "SELECT to_timestamp($1)",
        &[epoch_sec.into()],
    )
    .expect("failed to convert to timestamptz")
    .unwrap()
}

/// Format a fully-qualified table name.
fn fqn(schema: &str, table: &str) -> String {
    if schema == "public" {
        format!("\"{}\"", table)
    } else {
        format!("\"{}\".\"{}\"", schema, table)
    }
}

/// Create a single partition via SPI.
pub fn create_partition(
    client: &mut SpiClient,
    schema_name: &str,
    table_name: &str,
    part_name: &str,
    range_start: &str,
    range_end: &str,
) -> spi::SpiResult<()> {
    let parent = fqn(schema_name, table_name);
    let child = fqn(schema_name, part_name);
    client.update(
        &format!(
            "CREATE TABLE IF NOT EXISTS {} PARTITION OF {} FOR VALUES FROM ('{}') TO ('{}')",
            child, parent, range_start, range_end
        ),
        None,
        &[],
    )?;
    Ok(())
}

/// Core logic: create initial partitions for a hypertable.
pub fn create_initial_partitions(
    client: &mut SpiClient,
    schema_name: &str,
    table_name: &str,
    hypertable_id: i32,
    interval: &pgrx::datum::Interval,
    premake: i32,
) -> spi::SpiResult<i32> {
    let interval_usec = interval_to_usec(interval);
    let current_usec = now_usec();
    let current_aligned = align_to_interval(current_usec, interval_usec);

    let mut count = 0;

    // Create partitions from 1 in the past to `premake` in the future
    for i in -1..=premake {
        let start_usec = current_aligned + (i as i64 * interval_usec);
        let end_usec = start_usec + interval_usec;
        let start_str = format_ts(start_usec);
        let end_str = format_ts(end_usec);
        let part_name = partition_name(table_name, start_usec, interval_usec);

        create_partition(client, schema_name, table_name, &part_name, &start_str, &end_str)?;

        let start_tstz = usec_to_tstz(start_usec);
        let end_tstz = usec_to_tstz(end_usec);

        catalog::register_partition(client, hypertable_id, schema_name, &part_name, start_tstz, end_tstz)?;
        count += 1;
    }

    // Create default partition
    let default_name = format!("{}_default", table_name);
    let parent = fqn(schema_name, table_name);
    let default_fqn = fqn(schema_name, &default_name);
    client.update(
        &format!("CREATE TABLE IF NOT EXISTS {} PARTITION OF {} DEFAULT", default_fqn, parent),
        None,
        &[],
    )?;

    Ok(count)
}

/// Ensure future partitions exist for a hypertable. Called by the background worker.
pub fn ensure_future_partitions(
    client: &mut SpiClient,
    ht: &catalog::HypertableInfo,
    premake: i32,
) -> spi::SpiResult<i32> {
    let interval_usec = interval_to_usec(&ht.partition_interval);
    let current_usec = now_usec();
    let current_aligned = align_to_interval(current_usec, interval_usec);
    let mut created = 0;

    for i in 0..=premake {
        let start_usec = current_aligned + (i as i64 * interval_usec);
        let end_usec = start_usec + interval_usec;
        let part_name = partition_name(&ht.table_name, start_usec, interval_usec);

        // Check if partition already registered
        let exists = client.select(
            "SELECT 1 FROM cocoon_partition WHERE schema_name = $1 AND table_name = $2",
            None,
            &[ht.schema_name.as_str().into(), part_name.as_str().into()],
        )?;

        if exists.is_empty() {
            let start_str = format_ts(start_usec);
            let end_str = format_ts(end_usec);
            create_partition(client, &ht.schema_name, &ht.table_name, &part_name, &start_str, &end_str)?;

            let start_tstz = usec_to_tstz(start_usec);
            let end_tstz = usec_to_tstz(end_usec);

            catalog::register_partition(client, ht.id, &ht.schema_name, &part_name, start_tstz, end_tstz)?;
            created += 1;
        }
    }

    Ok(created)
}

// ============================================================================
// User-facing SQL functions
// ============================================================================

#[pg_extern]
fn cocoon_create_table(
    relation: &str,
    time_column: &str,
    partition_interval: default!(pgrx::datum::Interval, "'1 day'"),
    premake: default!(i32, 3),
) -> String {
    Spi::connect_mut(|client| {
        // 1. Resolve schema and table name
        let (schema, table) = resolve_relation(client, relation);

        // 2. Check if already registered as a cocoon table
        if catalog::get_hypertable(client, &schema, &table)
            .unwrap_or(None)
            .is_some()
        {
            return format!("Table {}.{} is already a cocoon table", schema, table);
        }

        // 3. Validate the time column exists and is a timestamp type
        validate_time_column(client, &schema, &table, time_column);

        // 4. Check if table is already partitioned
        let is_partitioned = check_partitioned(client, &schema, &table);

        if !is_partitioned {
            // 5. Reject non-empty tables
            let has_rows = client
                .select(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM \"{}\".\"{}\" LIMIT 1)",
                        schema, table
                    ),
                    None,
                    &[],
                )
                .expect("failed to check table emptiness")
                .first()
                .get_one::<bool>()
                .unwrap_or(Some(false))
                .unwrap_or(false);

            if has_rows {
                pgrx::error!(
                    "pg_cocoon: table {}.{} is not empty. Only empty tables are supported.",
                    schema,
                    table
                );
            }

            // 6. Convert to partitioned table
            convert_to_partitioned(client, &schema, &table, time_column);
        }

        // 7. Register in catalog
        let ht_id = catalog::register_hypertable(
            client,
            &schema,
            &table,
            time_column,
            &partition_interval,
        )
        .expect("failed to register hypertable");

        // 8. Create initial partitions
        let count = create_initial_partitions(
            client,
            &schema,
            &table,
            ht_id,
            &partition_interval,
            premake,
        )
        .expect("failed to create initial partitions");

        format!(
            "Created cocoon table {}.{} with {} partitions",
            schema, table, count
        )
    })
}

/// Resolve a relation name to (schema, table).
fn resolve_relation(_client: &SpiClient, relation: &str) -> (String, String) {
    let parts: Vec<&str> = relation.split('.').collect();
    match parts.len() {
        1 => {
            let schema = Spi::get_one_with_args::<String>(
                "SELECT schemaname::text FROM pg_tables WHERE tablename = $1::name LIMIT 1",
                &[parts[0].into()],
            )
            .expect("failed to look up table schema")
            .unwrap_or_else(|| {
                pgrx::error!("pg_cocoon: table '{}' not found", relation);
            });
            (schema, parts[0].to_string())
        }
        2 => (parts[0].to_string(), parts[1].to_string()),
        _ => {
            pgrx::error!("pg_cocoon: invalid relation name '{}'", relation);
        }
    }
}

/// Validate that the time column exists and is a timestamp type.
fn validate_time_column(_client: &SpiClient, schema: &str, table: &str, time_column: &str) {
    let data_type = Spi::get_one_with_args::<String>(
        "SELECT data_type::text FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
        &[schema.into(), table.into(), time_column.into()],
    )
    .unwrap_or(None);

    match data_type {
        None => {
            pgrx::error!(
                "pg_cocoon: column '{}' not found in table {}.{}",
                time_column,
                schema,
                table
            );
        }
        Some(ref dt) if dt.contains("timestamp") => {
            // OK
        }
        Some(ref dt) => {
            pgrx::error!(
                "pg_cocoon: column '{}' has type '{}', expected a timestamp type",
                time_column,
                dt
            );
        }
    }
}

/// Check if a table is already partitioned.
fn check_partitioned(client: &SpiClient, schema: &str, table: &str) -> bool {
    Spi::get_one_with_args::<bool>(
        "SELECT c.relkind = 'p'
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = $1::name AND c.relname = $2::name",
        &[schema.into(), table.into()],
    )
    .unwrap_or(Some(false))
    .unwrap_or(false)
}

/// Convert a regular (empty) table to a partitioned table.
fn convert_to_partitioned(
    client: &mut SpiClient,
    schema: &str,
    table: &str,
    time_column: &str,
) {
    let table_fqn = fqn(schema, table);
    let tmp_name = format!("_cocoon_tmp_{}", table);
    let tmp_fqn = fqn(schema, &tmp_name);

    // Rename original table
    client
        .update(
            &format!("ALTER TABLE {} RENAME TO \"{}\"", table_fqn, tmp_name),
            None,
            &[],
        )
        .expect("failed to rename table");

    // Create new partitioned table with same structure
    client
        .update(
            &format!(
                "CREATE TABLE {} (LIKE {} INCLUDING ALL) PARTITION BY RANGE (\"{}\")",
                table_fqn, tmp_fqn, time_column
            ),
            None,
            &[],
        )
        .expect("failed to create partitioned table");

    // Drop the temp table
    client
        .update(&format!("DROP TABLE {}", tmp_fqn), None, &[])
        .expect("failed to drop temp table");
}

// ============================================================================
// Info functions
// ============================================================================

#[pg_extern]
fn cocoon_partition_info(
    relation: &str,
) -> TableIterator<
    'static,
    (
        name!(partition_name, String),
        name!(range_start, TimestampWithTimeZone),
        name!(range_end, TimestampWithTimeZone),
        name!(is_compressed, bool),
    ),
> {
    let rows = Spi::connect(|client| {
        let (schema, table) = resolve_relation(client, relation);
        let ht = catalog::get_hypertable(client, &schema, &table)
            .expect("failed to query hypertable")
            .unwrap_or_else(|| {
                pgrx::error!("pg_cocoon: table {}.{} is not a cocoon table", schema, table)
            });

        let partitions = catalog::get_partitions(client, ht.id).expect("failed to query partitions");
        partitions
            .into_iter()
            .map(|p| (p.table_name, p.range_start, p.range_end, p.is_compressed))
            .collect::<Vec<_>>()
    });

    TableIterator::new(rows)
}

#[pg_extern]
fn cocoon_hypertable_info(
    relation: &str,
) -> TableIterator<
    'static,
    (
        name!(schema_name, String),
        name!(table_name, String),
        name!(time_column, String),
        name!(partition_interval, pgrx::datum::Interval),
        name!(num_partitions, i64),
    ),
> {
    let rows = Spi::connect(|client| {
        let (schema, table) = resolve_relation(client, relation);
        let ht = catalog::get_hypertable(client, &schema, &table)
            .expect("failed to query hypertable")
            .unwrap_or_else(|| {
                pgrx::error!("pg_cocoon: table {}.{} is not a cocoon table", schema, table)
            });

        let partitions = catalog::get_partitions(client, ht.id).expect("failed to query partitions");
        let num_partitions = partitions.len() as i64;

        vec![(
            ht.schema_name,
            ht.table_name,
            ht.time_column,
            ht.partition_interval,
            num_partitions,
        )]
    });

    TableIterator::new(rows)
}
