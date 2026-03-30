# Columnar Blob Storage Architecture

## Overview

DeltaX splits each compressed partition into three tables:

1. **Meta table** — scalar per-segment metadata (no BYTEA, no TOAST)
2. **Blob table** — compressed column data, inserted in column-major order for sequential I/O
3. **Blooms table** — per-segment packed bloom filters for equality predicate pushdown

This layout replaces the original single companion table design where all
compressed column blobs were stored as BYTEA columns in one row per segment.
The motivation and measurements behind this change are in the appendix.

## How Data is Organized into Segments

A DeltaX compressed table partitions data in two dimensions: **time-based
partitions** (PostgreSQL declarative partitioning) and **segments** within
each partition (groups of rows compressed together).

```
Original table: hits (100M rows, 105 columns)
│
├── Partition 1 (2013-07-01 to 2013-07-08) ── ~20M rows
│   ├── Segment 1 ── 30,000 rows, ordered by EventTime
│   ├── Segment 2 ── 30,000 rows
│   ├── ... (~667 segments per partition)
│   └── Segment 667 ── remaining rows
│
├── Partition 2 (2013-07-08 to 2013-07-15)
│   └── ... (~667 segments)
│
├── ... (5 partitions total)
│
└── Partition 5
    └── ...
```

Each segment's compressed blobs are stored in the blob table (one row per
column per segment). The segment size defaults to 30,000 rows (configurable
via `segment_size` parameter).

## Table Layout

### 1. Segment Metadata Table (`<partition>_meta`)

Stores all scalar per-segment metadata. No BYTEA columns, so scanning it
involves zero TOAST I/O.

```sql
CREATE TABLE "_deltax_compressed"."<partition>_meta" (
    _segment_id   SERIAL PRIMARY KEY,

    -- Segment-by columns (original types)
    "<seg_by_col>" <type>,

    -- Per-segment row count
    _row_count    INT,

    -- Time bounds
    _min_<time_col> TIMESTAMPTZ,
    _max_<time_col> TIMESTAMPTZ,

    -- Per-column min/max (orderable types only)
    _min_<col>    <type>,
    _max_<col>    <type>,

    -- Per-column sum + nonnull count (numeric types only)
    _sum_<col>    DOUBLE PRECISION,
    _nonnull_count_<col> INT,

    -- Per-column cardinality estimate
    _ndistinct_<col> INT
);
```

For ClickBench (105 columns): each row is ~2 KB (all small scalars). With
~667 segments per partition, the entire metadata table fits in ~1.3 MB. A full
sequential scan takes <1 ms even on cold storage.

### 2. Column Blob Table (`<partition>_blobs`)

Stores compressed column data. One row per (column, segment) pair.

```sql
CREATE TABLE "_deltax_compressed"."<partition>_blobs" (
    _col_idx     SMALLINT NOT NULL,
    _segment_id  INT NOT NULL,
    _data        BYTEA,
    PRIMARY KEY (_col_idx, _segment_id)
);
```

The key design decision is **column-major insertion order**: blobs are
inserted sorted by `(_col_idx, _segment_id)`. Because PostgreSQL writes TOAST
chunks in insertion order, this naturally produces a columnar physical layout
with no post-processing:

```
Metadata table (no TOAST, ~1.3 MB):
┌──────────────────────────────────────────────────────┐
│ seg 1: seg_by | _row_count | _min/max_time | min/max │
│ seg 2: ...                                           │
│ ...                                                  │
│ seg 667: ...                                         │
└──────────────────────────────────────────────────────┘

Blob table (column-major insertion → columnar TOAST):
┌──────────────────────────────────────────────────────┐
│ (col=0, seg=1, blob) | (col=0, seg=2, blob) | ...   │  ← col 0 blobs
│ (col=0, seg=667, blob) |                             │    contiguous
│ (col=1, seg=1, blob) | (col=1, seg=2, blob) | ...   │  ← col 1 blobs
│ ...                                                  │    contiguous
│ (col=104, seg=1, blob) | ... | (col=104, seg=667)   │
└──────────────────────────────────────────────────────┘

TOAST heap (follows insertion order):
┌──────────────────────────────────────────────────────┐
│ col0_seg1 | col0_seg2 | ... | col0_seg667 |         │  ← sequential
│ col1_seg1 | col1_seg2 | ... | col1_seg667 |         │    for each
│ ...                                                  │    column
│ col104_seg1 | ... | col104_seg667 |                  │
└──────────────────────────────────────────────────────┘
  ↑ Reading AdvEngineID = first 1/105th of TOAST = sequential I/O
```

Reading one column = sequential I/O on a contiguous ~1/105th slice of the
TOAST table. The kernel's readahead (128 KB default on Linux) prefetches
upcoming chunks automatically.

### 3. Bloom Filter Table (`<partition>_blooms`)

Stores per-segment packed bloom filters for equality predicate pushdown.
Kept in a separate table (rather than inline in meta) to avoid adding TOAST
overhead to the meta table, which must stay fast for all queries.

```sql
CREATE TABLE "_deltax_compressed"."<partition>_blooms" (
    _segment_id  INT PRIMARY KEY,
    _data        BYTEA NOT NULL
);
```

The `_data` column contains a packed binary format with variable-size bloom
filters for each numeric/date/timestamp column in the segment:

```
Wire format (repeated per column):
┌─────────────┬────────────┬───────────┬──────────────────────┐
│ col_idx: u16 │ num_hashes: u8 │ size: u16 │ bloom_bits: [u8; size] │
└─────────────┴────────────┴───────────┴──────────────────────┘
```

**Dynamic sizing**: Bloom filter size scales with `ndistinct` for each
column: `bloom_size = ndistinct × 10 bits / 8 bytes`, clamped to 64 B – 8 KB.
The optimal number of hash functions is `k = (m/n) × ln(2)`, clamped to
[1, 10]. This gives a false positive rate of ~0.8% at ~2-3% storage overhead.

Bloom filters are built during compression for numeric, date, and timestamp
columns. Building can be disabled via the `pg_deltax.bloom_filters` GUC
(default: on). The read path gracefully handles missing bloom data — if the
blooms table doesn't exist, the bloom phase is skipped.

## Read Path

The read path is a three-phase process in `load_segments_heap`:

### Phase 1: Metadata Scan (zero TOAST I/O)

Scan the meta table with `heap_getnext()`. Apply pruning:

1. **Segment-by filters**: skip segments with non-matching segment_by values
2. **Time range filters**: skip segments outside query time range
3. **MinMax filters**: skip segments where min/max metadata proves no rows
   can match

Collect surviving `_segment_id` values into an array. This phase involves
zero TOAST I/O — the metadata table has no BYTEA columns.

### Bloom Phase: Equality Predicate Pushdown

Only runs when the query has equality (`=`) or `IN` predicates on numeric,
date, or timestamp columns. Scans the blooms table for surviving segments:

1. Open the blooms table and scan via `scan_getnextslot`
2. For each row, check if the segment survived Phase 1 (HashSet lookup)
3. Detoast the packed bloom data, look up the target column's bloom filter
4. If the bloom filter says the value is definitely not present, mark the
   segment for pruning

Pruned segments are removed from the surviving set before Phase 2, avoiding
unnecessary blob I/O.

**Performance**: On ClickBench Q19 (`WHERE UserID = <value>`), bloom filters
prune ~97.5% of segments, reducing query time from ~8s to ~0.2s.

### Phase 2: Column Blob Reads (sequential TOAST I/O)

For each needed column, read blobs from the blob table via PK index scan:

```
For each needed col_idx:
    Index scan: _col_idx = X (scans all segment_ids for this column)
    For each row: check if segment_id is in surviving set
    If yes: detoast blob → sequential TOAST I/O (contiguous region)
    Store in SegmentData.compressed_blobs[blob_idx]
```

Because blobs were inserted in column-major order, the TOAST chunks for one
column are contiguous on disk.

### Parallel Workers

The current parallel dispatch pattern is preserved:
1. Main thread: Phase 1 + Bloom + Phase 2 → `Vec<SegmentData>`
2. Dispatch segments to parallel workers for decompression + aggregation

Phase 2 runs on the main thread because `pg_detoast_datum` requires a valid
PostgreSQL backend context. However, the I/O is now sequential per column,
so the main thread can saturate the storage bandwidth.

## Write Path

### During Compression

Compression processes one segment at a time (all columns for segment 1, then
all columns for segment 2, etc.). To achieve column-major insertion into the
blob table, compressed blobs are buffered in memory and flushed after all
segments are processed.

```
Phase 1: Compress all segments, buffer blobs and blooms

for each batch of 30,000 rows:
    compress all columns → 105 blobs
    compute bloom filters for numeric/date/timestamp columns
    INSERT metadata into meta table immediately (returns _segment_id)
    buffer compressed blobs in memory (keyed by col_idx, segment_id)
    buffer packed bloom data in memory (keyed by segment_id)

Phase 2: Flush blobs in column-major order

sort blob_buffer by (col_idx, segment_id)
for each (col_idx, segment_id, blob) in blob_buffer:
    INSERT INTO blobs (_col_idx, _segment_id, _data)

Phase 3: Flush bloom filters

for each (segment_id, bloom_data) in bloom_buffer:
    INSERT INTO blooms (_segment_id, _data)

ANALYZE meta, blobs, blooms
```

### Memory Impact

Buffering requires holding all compressed blobs for one partition in memory.
For ClickBench (105 columns, ~667 segments per partition), the total
compressed data is ~2.8 GB per partition. This is the worst case — a typical
time-series table with 10-20 narrower columns and smaller partitions would
buffer tens of MB.

This is acceptable because:
- Compression is a batch operation (not latency-sensitive)
- The server already needs substantial memory for the uncompressed data
  during compression
- Peak memory can be reduced by flushing one column at a time: after all
  segments are compressed, iterate columns and flush each column's blobs,
  freeing them immediately after insertion

### During Decompression

`deltax_decompress_partition` reads from all three tables (meta for segment
metadata, blobs for column data) and drops all three tables after restoring
data to the original partition.

## Alternatives Considered

### CLUSTER after segment-major insertion
Instead of buffering blobs for column-major insertion, insert in segment
order (natural compression order) and then run `CLUSTER blobs USING pkey`
to rewrite the heap + TOAST in column-major order. Rejected because CLUSTER
writes the data twice (2× write amplification) and takes
`AccessExclusiveLock`. Column-major insertion achieves the same physical
layout with no extra I/O.

### One table per column
Perfect I/O locality but creates N tables per partition (105 × 7 = 735
tables for ClickBench). Management overhead is high, catalog bloat affects
planning time, and DDL operations (DROP PARTITION) become expensive.

### Column-chunk concatenation
Store all segments' data for one column in a single large BYTEA with an
offsets array. Perfect locality (one TOAST detoast per column) but:
- Cannot skip individual segments (must read entire column blob)
- Any modification requires rewriting the entire blob
- Maximum BYTEA size is 1 GB (could be hit for large columns)

### posix_fadvise prefetching
Tested and found ineffective on gp2 EBS. The prefetch reads 100× more data
than needed for selective queries. On high-IOPS storage (NVMe), the random
reads aren't a bottleneck in the first place.

### Separate files (outside PostgreSQL)
Would give full control over I/O layout but breaks PostgreSQL replication,
pg_dump, and crash recovery. Incompatible with the requirement that standard
PostgreSQL replication must work.

### Bloom filters inline in meta table
The initial implementation stored packed bloom data as a `_blooms BYTEA`
column in the meta table. This caused 5-15% regression on all queries because
the meta table rows became TOAST-heavy — even queries with no equality
predicates paid the cost of larger heap pages. Moving blooms to a separate
table keeps the meta table TOAST-free.

## Open Questions

1. **TOAST chunk size**: PostgreSQL's default TOAST chunk size is ~2000 bytes.
   For blobs that are 50-100 KB, this means 25-50 chunks per blob. We could
   investigate `toast_tuple_target` to reduce chunk overhead, but this is a
   minor optimization.

2. **Blob table without TOAST**: If we ensure blob sizes stay under ~2 KB
   (by chunking at our level), we could use `ALTER COLUMN _data SET STORAGE
   MAIN` to prevent TOAST entirely. This eliminates the TOAST index lookup
   overhead but requires managing our own chunking. Worth investigating if
   TOAST lookup overhead is significant.

3. **Insertion order durability**: TOAST physical layout depends on insertion
   order, which PostgreSQL does not formally guarantee across restarts or
   `VACUUM FULL`. In practice, since compressed data is write-once (immutable
   after compression) and never updated/deleted, the layout should be stable.
   `VACUUM` won't reorder existing pages. However, if we ever need to
   re-guarantee ordering (e.g., after a pg_dump/restore), we could add a
   `CLUSTER` as a repair step.

## Appendix: Motivation — TOAST I/O Analysis

The columnar layout was motivated by measuring TOAST I/O overhead with the
original single companion table design. In that design, all compressed column
blobs (~105 BYTEA columns) were stored in one row per segment. PostgreSQL
TOASTed these blobs into a shared TOAST heap where chunks from different
columns were interleaved in insertion order.

```
Original single-table layout:
┌──────────────────────────────────────────────────────────────────────┐
│ Row 1: seg_by | _row_count | _min/max_time | _min/max_col0..104 |  │
│        _col0_compressed (BYTEA→TOAST) | ... | _col104_compressed   │
├──────────────────────────────────────────────────────────────────────┤
│ Row 2: ... same 105 BYTEA blobs, all TOASTed ...                   │
├──────────────────────────────────────────────────────────────────────┤
│ ...                                                                 │
└──────────────────────────────────────────────────────────────────────┘

TOAST heap (physical disk order = insertion order):
┌─────────────────────────────────────────────────────────┐
│ seg1_col0 | seg1_col1 | ... | seg1_col104 |            │  ← all cols
│ seg2_col0 | seg2_col1 | ... | seg2_col104 |            │    interleaved
│ ...                                                     │
│ seg667_col0 | seg667_col1 | ... | seg667_col104 |      │
└─────────────────────────────────────────────────────────┘
  ↑ Reading AdvEngineID = every 105th blob = random I/O
```

When a query needed only one column, PostgreSQL detoasted that column's blob
from each segment independently. Because the chunks for one column were
scattered across the entire TOAST table, the I/O pattern was random.

**Measured on gp2 EBS (ClickBench 100M rows, r7i.4xlarge, cold cache):**

We instrumented `load_segments_heap` to separately measure `heap_getnext`
(reading companion table heap pages), `heap_deform_tuple` (extracting
datums), and `pg_detoast_datum` (TOAST I/O). Results across all 43 queries:

- **`heap_getnext` + `heap_deform_tuple`**: ~1-2ms per partition — negligible
- **`pg_detoast_datum`**: 99-100% of `heap_scan` time for every DeltaXAgg query
- **All blobs were TOASTed** (0 inline blobs observed)

Example queries:

| Query | Columns | Cold Total | detoast | detoast % of total |
|-------|---------|------------|---------|-------------------|
| Q7    | 1       | 3.3s       | 3131ms  | 96%               |
| Q21   | 3       | 30.1s      | 28679ms | 95%               |
| Q22   | 5       | 53.4s      | 50308ms | 94%               |
| Q32   | 4       | 36.8s      | 27905ms | 76%               |

The entire cold-run bottleneck was TOAST random I/O. The median detoast % of
total execution time across DeltaXAgg queries was **86%**.

### Full Cold Run Measurements (Original Layout)

Measured on r7i.4xlarge, gp2 500GB EBS, ClickBench 100M rows, PostgreSQL 18.
Each query run after `systemctl restart postgresql && echo 3 > /proc/sys/vm/drop_caches`.

Sorted by detoast % of total execution time (descending).

#### DeltaXAgg Path (32 queries)

| Query | Total (s) | heap_scan (ms) | detoast (ms) | detoast % of heap_scan | detoast % of total | Description |
|-------|-----------|----------------|--------------|------------------------|--------------------|-------------|
| Q10 | 12.5 | 12266 | 12239 | 100% | 98% | MobilePhoneModel, COUNT(DISTINCT UserID) WHERE MobilePhoneModel <> '' |
| Q11 | 14.7 | 14401 | 14371 | 100% | 98% | MobilePhone+Model, COUNT(DISTINCT UserID) |
| Q7 | 3.3 | 3155 | 3131 | 99% | 96% | AdvEngineID, COUNT(*) WHERE <> 0 |
| Q21 | 30.1 | 28709 | 28679 | 100% | 95% | SearchPhrase, MIN(URL), COUNT(*) WHERE URL LIKE google |
| Q9 | 21.7 | 20482 | 20452 | 100% | 94% | 5 aggs GROUP BY RegionID, COUNT(DISTINCT UserID) |
| Q22 | 53.4 | 50340 | 50308 | 100% | 94% | SearchPhrase+URL+Title, 5 aggs, 3 LIKE filters |
| Q14 | 16.9 | 15664 | 15637 | 100% | 93% | SearchEngineID+SearchPhrase, COUNT(*) |
| Q8 | 17.8 | 16396 | 16367 | 100% | 92% | RegionID, COUNT(DISTINCT UserID) |
| Q17 | 21.1 | 19394 | 19365 | 100% | 92% | UserID+SearchPhrase, COUNT(*) LIMIT |
| Q12 | 12.4 | 11350 | 11323 | 100% | 91% | SearchPhrase, COUNT(*) WHERE <> '' |
| Q16 | 21.6 | 19342 | 19314 | 100% | 90% | UserID+SearchPhrase, COUNT(*) ORDER BY |
| Q15 | 11.5 | 10248 | 10220 | 100% | 89% | UserID, COUNT(*) |
| Q18 | 35.3 | 30932 | 30901 | 100% | 88% | UserID+minute+SearchPhrase, COUNT(*) |
| Q41 | 0.7 | 598 | 590 | 99% | 88% | Filtered: CounterID=62, date range, URLHash match |
| Q33 | 25.1 | 21701 | 21673 | 100% | 86% | URL, COUNT(*) (full table) |
| Q34 | 25.1 | 21714 | 21686 | 100% | 86% | 1+URL, COUNT(*) (full table) |
| Q1 | 3.7 | 3142 | 3119 | 99% | 84% | COUNT(*) WHERE AdvEngineID <> 0 |
| Q38 | 0.6 | 510 | 502 | 98% | 84% | Filtered: CounterID=62, date+flags, URL GROUP BY |
| Q20 | 26.3 | 21725 | 21697 | 100% | 83% | COUNT(*) WHERE URL LIKE google |
| Q37 | 0.5 | 407 | 399 | 98% | 80% | Filtered: CounterID=62, date range, Title |
| Q36 | 0.6 | 502 | 494 | 98% | 79% | Filtered: CounterID=62, date range, URL |
| Q5 | 14.5 | 11347 | 11319 | 100% | 78% | COUNT(DISTINCT SearchPhrase) |
| Q30 | 35.7 | 27646 | 27615 | 100% | 77% | SearchEngineID+ClientIP, 3 aggs, WHERE SearchPhrase <> '' |
| Q31 | 46.8 | 35414 | 35382 | 100% | 76% | WatchID+ClientIP, 3 aggs, WHERE SearchPhrase <> '' |
| Q32 | 36.8 | 27937 | 27905 | 100% | 76% | WatchID+ClientIP, 3 aggs (full table) |
| Q13 | 26.2 | 19355 | 19326 | 100% | 74% | SearchPhrase, COUNT(DISTINCT UserID) |
| Q40 | 0.8 | 571 | 564 | 99% | 74% | Filtered: CounterID=62, TraficSourceID IN, RefererHash= |
| Q42 | 0.5 | 388 | 380 | 98% | 74% | Filtered: CounterID=62, narrow date, DATE_TRUNC agg |
| Q27 | 33.4 | 24003 | 23976 | 100% | 72% | CounterID, AVG(length(URL)), HAVING >100K |
| Q4 | 15.6 | 10187 | 10160 | 100% | 65% | COUNT(DISTINCT UserID) |
| Q28 | 33.1 | 20788 | 20761 | 100% | 63% | REGEXP_REPLACE(Referer), AVG(length), HAVING >100K |
| Q35 | 32.6 | 9310 | 9284 | 100% | 29% | ClientIP expressions, COUNT(*) (agg-heavy) |

#### DeltaXAgg — Sum/Count Pushdown (3 queries — no TOAST, metadata only)

| Query | Total (s) | heap_scan (ms) | Description |
|-------|-----------|----------------|-------------|
| Q2 | 0.1 | 101 | SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) |
| Q3 | 0.1 | 99 | AVG(UserID) |
| Q29 | 0.2 | 101 | 90 × SUM(ResolutionWidth + N) |

#### DeltaXCount / DeltaXMinMax (2 queries — metadata only)

| Query | Total (s) | heap_scan (ms) | Description |
|-------|-----------|----------------|-------------|
| Q0 | 0.1 | 82 | COUNT(*) |
| Q6 | 0.1 | 100 | MIN(EventDate), MAX(EventDate) |

#### DeltaXDecompress Path (6 queries)

| Query | Total (s) | heap_scan (ms) | Description |
|-------|-----------|----------------|-------------|
| Q19 | 8.6 | 8054 | WHERE UserID = specific value (point lookup) |
| Q23 | 0.9 | 604 | WHERE URL LIKE google ORDER BY EventTime LIMIT 10 |
| Q24 | 0.4 | 241 | WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10 |
| Q25 | 16.9 | 11335 | WHERE SearchPhrase <> '' ORDER BY SearchPhrase LIMIT 10 |
| Q26 | 0.3 | 239 | WHERE SearchPhrase <> '' ORDER BY EventTime, SearchPhrase LIMIT 10 |
| Q39 | 1.8 | 922 | Filtered: CounterID=62, CASE expression, multi-GROUP BY |
