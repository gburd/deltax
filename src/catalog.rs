use pgrx::prelude::*;
use pgrx::spi::SpiClient;

/// Metadata for a cocoon-managed hypertable.
#[derive(Debug, Clone)]
pub struct HypertableInfo {
    pub id: i32,
    pub schema_name: String,
    pub table_name: String,
    pub time_column: String,
    pub partition_interval: pgrx::datum::Interval,
}

/// Metadata for a single partition.
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub id: i32,
    pub hypertable_id: i32,
    pub schema_name: String,
    pub table_name: String,
    pub range_start: TimestampWithTimeZone,
    pub range_end: TimestampWithTimeZone,
    pub is_compressed: bool,
}

/// Register a new hypertable in the catalog. Returns the new hypertable id.
pub fn register_hypertable(
    client: &mut SpiClient,
    schema_name: &str,
    table_name: &str,
    time_column: &str,
    partition_interval: &pgrx::datum::Interval,
) -> spi::SpiResult<i32> {
    let result = client.update(
        "INSERT INTO cocoon_hypertable (schema_name, table_name, time_column, partition_interval)
         VALUES ($1, $2, $3, $4)
         RETURNING id",
        None,
        &[
            schema_name.into(),
            table_name.into(),
            time_column.into(),
            partition_interval.clone().into(),
        ],
    )?;
    Ok(result.first().get_one::<i32>()?.unwrap())
}

/// Register a partition in the catalog.
pub fn register_partition(
    client: &mut SpiClient,
    hypertable_id: i32,
    schema_name: &str,
    table_name: &str,
    range_start: TimestampWithTimeZone,
    range_end: TimestampWithTimeZone,
) -> spi::SpiResult<()> {
    client.update(
        "INSERT INTO cocoon_partition (hypertable_id, schema_name, table_name, range_start, range_end)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (schema_name, table_name) DO NOTHING",
        None,
        &[
            hypertable_id.into(),
            schema_name.into(),
            table_name.into(),
            range_start.into(),
            range_end.into(),
        ],
    )?;
    Ok(())
}

/// Look up a hypertable by schema + table name.
pub fn get_hypertable(
    client: &SpiClient,
    schema_name: &str,
    table_name: &str,
) -> spi::SpiResult<Option<HypertableInfo>> {
    let result = client.select(
        "SELECT id, schema_name, table_name, time_column, partition_interval
         FROM cocoon_hypertable
         WHERE schema_name = $1 AND table_name = $2",
        None,
        &[schema_name.into(), table_name.into()],
    )?;

    if result.is_empty() {
        return Ok(None);
    }

    let id: Option<i32> = result.first().get_one::<i32>()?;
    let id = match id {
        Some(id) => id,
        None => return Ok(None),
    };

    // Re-query to get all fields since first() consumed the table
    let result2 = client.select(
        "SELECT id, schema_name, table_name, time_column, partition_interval
         FROM cocoon_hypertable
         WHERE id = $1",
        None,
        &[id.into()],
    )?;

    for row in result2 {
        let ht_id: i32 = row.get_datum_by_ordinal(1)?.value::<i32>()?.unwrap();
        let s: String = row.get_datum_by_ordinal(2)?.value::<String>()?.unwrap();
        let t: String = row.get_datum_by_ordinal(3)?.value::<String>()?.unwrap();
        let tc: String = row.get_datum_by_ordinal(4)?.value::<String>()?.unwrap();
        let pi: pgrx::datum::Interval = row.get_datum_by_ordinal(5)?.value::<pgrx::datum::Interval>()?.unwrap();
        return Ok(Some(HypertableInfo {
            id: ht_id,
            schema_name: s,
            table_name: t,
            time_column: tc,
            partition_interval: pi,
        }));
    }

    Ok(None)
}

/// Get all hypertables.
pub fn get_all_hypertables(
    client: &SpiClient,
) -> spi::SpiResult<Vec<HypertableInfo>> {
    let result = client.select(
        "SELECT id, schema_name, table_name, time_column, partition_interval
         FROM cocoon_hypertable",
        None,
        &[],
    )?;

    let mut hypertables = Vec::new();
    for row in result {
        hypertables.push(HypertableInfo {
            id: row.get_datum_by_ordinal(1)?.value::<i32>()?.unwrap(),
            schema_name: row.get_datum_by_ordinal(2)?.value::<String>()?.unwrap(),
            table_name: row.get_datum_by_ordinal(3)?.value::<String>()?.unwrap(),
            time_column: row.get_datum_by_ordinal(4)?.value::<String>()?.unwrap(),
            partition_interval: row.get_datum_by_ordinal(5)?.value::<pgrx::datum::Interval>()?.unwrap(),
        });
    }
    Ok(hypertables)
}

/// Get partitions for a hypertable, ordered by range_start.
pub fn get_partitions(
    client: &SpiClient,
    hypertable_id: i32,
) -> spi::SpiResult<Vec<PartitionInfo>> {
    let result = client.select(
        "SELECT id, hypertable_id, schema_name, table_name, range_start, range_end, is_compressed
         FROM cocoon_partition
         WHERE hypertable_id = $1
         ORDER BY range_start",
        None,
        &[hypertable_id.into()],
    )?;

    let mut partitions = Vec::new();
    for row in result {
        partitions.push(PartitionInfo {
            id: row.get_datum_by_ordinal(1)?.value::<i32>()?.unwrap(),
            hypertable_id: row.get_datum_by_ordinal(2)?.value::<i32>()?.unwrap(),
            schema_name: row.get_datum_by_ordinal(3)?.value::<String>()?.unwrap(),
            table_name: row.get_datum_by_ordinal(4)?.value::<String>()?.unwrap(),
            range_start: row.get_datum_by_ordinal(5)?.value::<TimestampWithTimeZone>()?.unwrap(),
            range_end: row.get_datum_by_ordinal(6)?.value::<TimestampWithTimeZone>()?.unwrap(),
            is_compressed: row.get_datum_by_ordinal(7)?.value::<bool>()?.unwrap_or(false),
        });
    }
    Ok(partitions)
}
