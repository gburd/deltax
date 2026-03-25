# Direct Backfill: Compressed Bulk Loading

## Problem

Loading data into pg_deltax currently follows a two-phase approach:

1. **COPY** data into uncompressed PostgreSQL partitions (WAL, indexes, MVCC overhead)
2. **Compress** each partition via `deltax_compress_partition()` (reads back from heap via cursor, compresses, writes to companion table, truncates heap)

This means every row is written to the heap and then read back — doubling I/O. The heap write also incurs WAL, index maintenance, and partition routing overhead that is ultimately thrown away.

## Proposal

Intercept `COPY ... FROM` with a custom format and compress data in-flight, writing directly to companion tables without ever touching the heap.

```sql
COPY hits FROM '/data/file.csv' WITH (FORMAT deltax_compress);

-- also works with stdin, program, etc.
COPY hits FROM STDIN WITH (FORMAT deltax_compress);
```

Rows are parsed, buffered in memory up to `segment_size` (default 30,000) per partition, sorted by the configured `order_by` columns, compressed, and inserted directly into the companion table in `_deltax_compressed`. The original heap partitions remain empty.

## Mechanism

PostgreSQL has no native pluggable COPY format API (a patch for PG19 exists but is not committed). The standard approach used by extensions like pg_parquet is to intercept COPY statements via the `ProcessUtility_hook`.

### Hook Flow

1. `_PG_init()` registers a `ProcessUtility_hook`
2. Hook inspects incoming utility statements for `CopyStmt` with `FORMAT 'deltax_compress'`
3. Non-matching statements chain to the previous hook / `standard_ProcessUtility`
4. Matching statements are handled entirely by our code

### Data Flow

```
COPY FROM (csv/text/binary)
  → parse rows (via PG's COPY parser: BeginCopyFrom / NextCopyFrom)
  → extract time column value
  → route to partition buffer (binary search on partition ranges)
  → buffer reaches segment_size
    → sort by order_by columns in Rust
    → compress each column
    → compute segment metadata (min/max, sum, nonnull_count, ndistinct)
    → INSERT compressed segment into companion table
  → flush remaining partial segments at end
  → update catalog (is_compressed, row_count, compressed_size, etc.)
```

### Partition Routing

Each row's time column value determines which partition it belongs to. Partition ranges are loaded from the `deltax_partition` catalog at the start and searched via binary search. Rows that fall outside any existing partition range either go to the default partition (uncompressed, as today) or trigger on-demand partition creation.

### Memory

Each partition buffer holds up to `segment_size` rows in typed column format (`TypedColumn` vecs). For ClickBench (107 columns, 30k rows), this is roughly 50 MB per active partition. With data spanning 6 partitions, peak memory is ~300 MB — well within reason for a bulk load operation.

If memory is a concern for very wide time ranges (many partitions active simultaneously), we can flush partitions that hit `segment_size` eagerly and keep a bounded number of partition buffers in an LRU.

## When to Use

**Use `FORMAT deltax_compress` when:**

- **Backfilling historical data.** The ideal case — loading large volumes of data that will be compressed anyway. Skips the entire write-read-compress-truncate cycle.
- **Initial bulk loading.** Any scenario where you're populating a table for the first time (migrations, data imports, benchmarks).
- **Large batch ingestion.** Bulk loads where the data volume per partition is significantly larger than `segment_size` (default 30,000 rows).

**Use normal COPY/INSERT when:**

- **Small or incremental inserts.** Direct backfill flushes all buffered rows as segments at the end of each COPY, even if the buffer hasn't reached `segment_size`. Repeated small COPYs (e.g. 5,000 rows each) would create many undersized segments with poor compression ratios and more segments to scan at query time. For small batches, use normal COPY/INSERT and let the background worker or `deltax_compress_partition()` handle compression when enough data has accumulated.
- **Unique constraint enforcement is required.** Direct backfill bypasses the heap and its indexes, so unique constraints are not checked.
- **Data needs to be immediately queryable row-by-row during ingestion.** With direct backfill, data becomes visible only when the transaction commits (same as normal COPY, but worth noting).

### Streaming Behavior

COPY FROM is streaming — PostgreSQL parses rows one at a time via `NextCopyFrom()`, regardless of file size. The file is never loaded into memory as a whole. Direct backfill uses the same loop: each row is parsed, routed to the correct partition buffer, and when a buffer reaches `segment_size`, it is sorted, compressed, and flushed to the companion table. Memory is bounded by `segment_size × num_active_partitions × row_width`, not by the input file size.

For example, a 1M-row CSV produces 33 full segments of 30,000 rows + 1 partial segment of 10,000 rows, all flushed incrementally during the COPY.

This works the same with `\copy` in psql, which is a client-side wrapper that streams the file to the server via the COPY protocol — server-side it's identical to `COPY FROM STDIN`.

### Crash Safety

Crash safety is the same as normal COPY. Each completed segment is written to the companion table via a standard SQL INSERT, which is fully WAL-logged. The in-flight buffer (rows parsed but not yet flushed as a segment) lives in memory — but this is no different from normal COPY, which is also transactional: if PostgreSQL crashes mid-COPY, the entire COPY is rolled back regardless of method. Once the COPY transaction commits, all data is durable.

### Segment Sizing

Direct backfill creates one segment per `segment_size` rows per partition. At the end of each COPY, any remaining rows in a partition buffer are flushed as a partial segment. This means the last segment per partition may be undersized, which is acceptable for large bulk loads but problematic if used for many small loads. As a rule of thumb, each COPY should load at least `segment_size` rows per partition to get good segment utilization.

A future enhancement could send the leftover tail to the heap instead of creating a partial segment, letting the background worker merge it into a full segment later.

## What We Reuse

The existing compression pipeline in `compress.rs` is already factored into reusable pieces:

- `classify_column` / `TypedColumn` / `init_typed_columns` — column type classification and storage
- `sort_typed_columns` — in-memory sorting by order_by columns
- `flush_segment_data` / `flush_with_splitting` — compression + companion table INSERT
- `compute_segment_ndistinct` — HyperLogLog cardinality estimation
- Companion table DDL generation from `compress_partition_impl`
- `compress_typed_column` — per-column compression dispatch

The new code is primarily: ProcessUtility hook, COPY row parsing into TypedColumn buffers, and partition routing.

## What Changes for the Scan Hook

Nothing. The scan hook already detects compressed partitions by checking for a companion table in `_deltax_compressed`. Since direct backfill writes to the same companion tables with the same schema, queries work transparently.

## Limitations (Initial Version)

- **Bulk load only.** This is for initial data loading, not for ongoing inserts into compressed partitions. The DML blocking on compressed partitions remains.
- **Compression must be enabled first.** The table must have `deltax_enable_compression()` called before using `FORMAT deltax_compress`, so we know the order_by, segment_by, and segment_size settings.
- **Partitions must exist.** The target partitions should already be created (via `deltax_create_table`). Rows that don't fit any partition go to the default partition uncompressed.
- **No unique constraint enforcement.** Since we bypass the heap, unique indexes on the original table are not checked.

## Future: Accepting Writes to Compressed Partitions

Direct backfill is a stepping stone toward a hybrid storage model where compressed partitions accept ongoing writes:

1. Allow INSERTs to land in the heap even when a companion table exists (partially compressed partition)
2. Scan hook merges data from both companion (compressed) and heap (uncompressed)
3. Background worker periodically folds heap rows into new compressed segments

This is the same architecture TimescaleDB uses for inserts into compressed chunks. Direct backfill establishes the infrastructure (partition routing, in-memory compression, companion writes) that the hybrid model builds on.
