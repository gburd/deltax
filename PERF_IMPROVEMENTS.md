# Performance Improvements Roadmap

Tracking SeaTurtle compressed vs uncompressed performance on ClickBench.

## Current Benchmark (2026-03-15)

### Compressed vs Uncompressed

| Query  | Description               |  Uncompr (ms) |  Compr (ms) |  Ratio |
|--------|---------------------------|---------------|-------------|--------|
| Q1     | COUNT(*)                  |          59.2 |         1.1 | 55.58x |
| Q2     | COUNT WHERE AdvEngineID   |          91.2 |         5.4 | 16.99x |
| Q3     | SUM/AVG full scan         |          94.6 |        12.2 |  7.77x |
| Q4     | AVG UserID                |          65.7 |         7.5 |  8.70x |
| Q5     | COUNT DISTINCT UserID     |         265.8 |         0.9 | 280.15x |
| Q6     | COUNT DISTINCT SearchPhrase |         387.9 |         0.6 | 612.43x |
| Q7     | MIN/MAX EventDate         |          64.8 |         0.7 | 87.66x |
| Q8     | GROUP BY AdvEngineID      |         125.0 |         4.8 | 26.11x |
| Q9     | GROUP BY RegionID         |         357.4 |        29.9 | 11.95x |
| Q10    | RegionID multi-agg        |         441.2 |        38.5 | 11.45x |
| Q11    | MobilePhoneModel users    |         254.3 |         8.1 | 31.34x |
| Q12    | MobilePhone+Model users   |         243.2 |        11.0 | 22.07x |
| Q13    | Top SearchPhrase          |         113.5 |        19.6 |  5.79x |
| Q14    | SearchPhrase users        |         327.2 |        25.1 | 13.05x |
| Q15    | SearchEngine+Phrase       |         265.5 |        24.5 | 10.83x |
| Q16    | Top UserID                |         104.8 |        22.0 |  4.76x |
| Q17    | UserID+SearchPhrase top   |         403.2 |        64.3 |  6.27x |
| Q18    | UserID+SearchPhrase       |         124.6 |        59.8 |  2.08x |
| Q19    | UserID+minute+Phrase      |         561.4 |       250.0 |  2.25x |
| Q20    | Point lookup UserID       |          66.1 |         1.5 | 43.87x |
| Q21    | URL LIKE google           |          96.2 |        55.5 |  1.73x |
| Q22    | SearchPhrase+URL google   |         119.6 |        60.4 |  1.98x |
| Q23    | Title LIKE Google         |         134.8 |       126.5 |  1.07x |
| Q24    | SELECT * google sorted    |          98.8 |       121.0 |  0.82x |
| Q25    | SearchPhrase by time      |          92.1 |        35.0 |  2.63x |
| Q26    | SearchPhrase sorted       |          90.0 |        12.0 |  7.49x |
| Q27    | SearchPhrase time+phrase  |          88.9 |        10.8 |  8.26x |
| Q28    | CounterID avg URL len     |         119.6 |        50.1 |  2.39x |
| Q29    | Referer domain regex      |         963.8 |      1059.0 |  0.91x |
| Q30    | Wide SUM 89 cols          |         207.4 |         4.5 | 46.11x |
| Q31    | SearchEngine+ClientIP     |         252.6 |        24.6 | 10.26x |
| Q32    | WatchID+ClientIP filter   |         268.1 |        50.0 |  5.36x |
| Q33    | WatchID+ClientIP all      |         619.5 |       457.6 |  1.35x |
| Q34    | Top URLs                  |        1172.5 |       281.1 |  4.17x |
| Q35    | Top URLs with const       |        1143.5 |       282.8 |  4.04x |
| Q36    | ClientIP arithmetic       |         109.4 |        34.0 |  3.22x |
| Q37    | CounterID=62 URLs         |        1849.5 |       130.0 | 14.22x |
| Q38    | CounterID=62 Titles       |         515.0 |        57.6 |  8.95x |
| Q39    | CounterID=62 links        |         165.6 |        26.4 |  6.26x |
| Q40    | CounterID=62 traffic src  |        2313.9 |       281.6 |  8.22x |
| Q41    | CounterID=62 URLHash      |         148.9 |        24.8 |  6.01x |
| Q42    | CounterID=62 window dim   |         160.0 |        17.4 |  9.20x |
| Q43    | CounterID=62 by minute    |         160.4 |        20.8 |  7.72x |
|--------|---------------------------|---------------|-------------|--------|
| GMEAN  | Geometric Mean            |         212.6 |        25.2 |  8.43x |

### SeaTurtle Scan Timing Breakdown (EXPLAIN ANALYZE)

| Query  | SeaTurtle Total |   Metadata |  Heap Scan |  Decompress | Batch Eval |       Emit | Stats                                                                                 |
|--------|---------------|------------|------------|-------------|------------|------------|---------------------------------------------------------------------------------------|
| Q1     |      0.443 ms |      0.325 |      0.118 |       0.000 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q2     |      3.777 ms |      0.224 |      0.422 |       1.938 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q3     |     11.226 ms |      0.222 |      1.413 |       3.921 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q4     |      7.271 ms |      0.282 |      1.637 |       2.571 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q5     |      0.326 ms |      0.326 |      0.000 |       0.000 |      0.000 |      0.000 | segments=0 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch_ |
| Q6     |      0.276 ms |      0.276 |      0.000 |       0.000 |      0.000 |      0.000 | segments=0 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch_ |
| Q7     |      0.645 ms |      0.249 |      0.396 |       0.000 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q8     |      4.289 ms |      0.272 |      0.337 |       1.921 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q9     |     48.058 ms |      0.239 |      2.465 |       4.441 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q10    |     55.608 ms |      0.290 |      3.489 |       8.316 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q11    |      8.426 ms |      0.301 |      1.640 |       4.771 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q12    |     11.321 ms |      0.258 |      1.909 |       7.256 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q13    |     15.015 ms |      0.261 |      1.376 |       4.467 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q14    |     20.773 ms |      0.290 |      2.853 |       7.038 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q15    |     19.056 ms |      0.314 |      1.982 |       6.540 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q16    |     39.226 ms |      0.321 |      1.508 |       2.610 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q17    |     65.450 ms |      0.347 |      2.760 |       8.156 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q18    |     63.456 ms |      0.316 |      2.775 |       8.158 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q19    |    213.628 ms |      0.304 |     10.454 |      26.667 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q20    |      1.408 ms |      0.327 |      0.434 |       0.508 |      0.139 |      0.000 | segments=6 segments_skipped=28 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q21    |     56.793 ms |      0.271 |      3.702 |      22.269 |      0.000 |      0.000 | segments=17 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q22    |     61.844 ms |      0.326 |      3.793 |      25.353 |      0.000 |      0.000 | segments=17 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q23    |    125.438 ms |      0.367 |      8.012 |      67.681 |      0.000 |      0.000 | segments=24 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q24    |     91.968 ms |      0.366 |     24.093 |      67.378 |      0.131 |      0.000 | segments=17 segments_skipped=17 phase2_skipped=0 rows_out=10 rows_filtered=0 rows_bat |
| Q25    |     26.974 ms |      0.341 |      8.727 |      17.906 |      0.000 |      0.000 | segments=28 segments_skipped=6 phase2_skipped=0 rows_out=10 rows_filtered=0 rows_batc |
| Q26    |      6.330 ms |      0.323 |      1.443 |       4.505 |      0.000 |      0.059 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=69354 rows_filtered=0 rows_b |
| Q27    |      9.694 ms |      0.321 |      8.642 |       0.730 |      0.000 |      0.001 | segments=1 segments_skipped=0 phase2_skipped=0 rows_out=11 rows_filtered=0 rows_batch |
| Q28    |     64.807 ms |      0.289 |      2.282 |      36.287 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q29    |   1070.818 ms |      0.536 |      3.479 |     755.975 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q30    |      4.227 ms |      0.312 |      1.007 |       1.890 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q31    |     22.504 ms |      0.246 |      4.257 |      12.492 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q32    |     36.777 ms |      0.311 |      5.080 |      20.642 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q33    |     21.422 ms |      0.349 |      3.267 |      17.238 |      0.000 |      0.568 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=1000000 rows_filtered=0 rows |
| Q34    |     32.899 ms |      0.349 |      2.993 |      28.088 |      0.000 |      1.469 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=1000000 rows_filtered=0 rows |
| Q35    |     30.933 ms |      0.340 |      2.469 |      27.430 |      0.000 |      0.694 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=1000000 rows_filtered=0 rows |
| Q36    |     85.609 ms |      1.325 |      2.412 |       6.351 |      0.000 |      0.000 | segments=34 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q37    |     23.929 ms |      0.340 |      1.866 |      19.490 |      1.799 |      0.434 | segments=15 segments_skipped=19 phase2_skipped=0 rows_out=376899 rows_filtered=0 rows |
| Q38    |     16.522 ms |      0.329 |      0.787 |      10.345 |      1.917 |      3.144 | segments=15 segments_skipped=19 phase2_skipped=0 rows_out=370550 rows_filtered=0 rows |
| Q39    |     21.504 ms |      0.350 |      2.406 |      16.875 |      1.825 |      0.048 | segments=15 segments_skipped=19 phase2_skipped=0 rows_out=26918 rows_filtered=0 rows_ |
| Q40    |     44.354 ms |      0.330 |      4.603 |      36.611 |      1.422 |      1.388 | segments=15 segments_skipped=19 phase2_skipped=0 rows_out=406063 rows_filtered=0 rows |
| Q41    |     25.646 ms |      0.317 |      6.004 |      11.291 |      0.000 |      0.000 | segments=15 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q42    |     18.759 ms |      0.341 |      3.843 |       9.031 |      0.000 |      0.000 | segments=15 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |
| Q43    |     31.035 ms |      0.704 |      5.311 |      10.335 |      0.000 |      0.000 | segments=15 segments_skipped=0 phase2_skipped=0 rows_out=0 rows_filtered=0 rows_batch |

## Where the time goes

The SeaTurtle scan has five phases: **metadata** (SPI catalog lookup), **heap_scan**
(load compressed blobs from companion table), **decompress** (decode blobs to
datums), **batch_eval** (vectorized WHERE on decoded arrays), and **emit** (fill
slot + qual + projection, row at a time).

For queries emitting many rows, **decompress + emit dominate** roughly equally.
Decompress is dominated by text varlena allocation (even with arena). Emit is
dominated by PG executor overhead: `fill_slot` + `ExecQual` + `ExecProject` per
row, plus memory context switches.

For queries where the bottleneck is *above* the scan (PG executor evaluating
complex expressions, hash aggregation on high-cardinality keys), the scan itself
is fast but we pay the cost of emitting 1M rows through the custom scan interface
just to feed PG's tuple-at-a-time executor.

---

## Completed Improvements

### 1. COUNT(*) / COUNT pushdown [DONE]

**Impact: Q1 42ms -> 0.5ms**

Sum `_row_count` from segment metadata. Zero decompression. Detected in planner
hook; `SeaTurtleCount` node returns a single row.

### 2. MIN/MAX pushdown [DONE]

**Impact: Q7 65ms -> 0.6ms (generalized to all orderable columns)**

Scan per-column `_min_`/`_max_` metadata in companion table. `SeaTurtleMinMax`
node returns global min/max without decompressing.

### 3. Batch qual evaluation [DONE]

**Impact: Q2 76ms -> 5.2ms, Q8 114ms -> 4.7ms, Q20 67ms -> 7.1ms**

Evaluate simple quals (`=`, `<>`, `<`, `>`, `>=`, `<=`) in tight Rust loops over
decoded datum arrays. Build a `Vec<bool>` selection vector; only `fill_slot` for
passing rows. LLVM auto-vectorizes the `slice.position()` scan.

### 4. LIKE filter pushdown into decompression [DONE]

**Impact: Q21 196ms -> 64ms**

LIKE match evaluated on raw `&str` slices during decompression. For dictionary
columns, pattern matched against dictionary entries only (O(dict_size)). For LZ4
columns, zero-copy match on decompressed buffer.

### 5. Text equality/inequality pushdown [DONE]

**Impact: Q13 59ms -> 18ms (3x)**

`=`/`<>` on text columns evaluated on raw `&str` slices before varlena
allocation. Dictionary columns: one comparison per entry, index lookup per row.

### 6. Per-column min/max in companion table [DONE]

**Impact: Enables segment pruning + MIN/MAX pushdown for any column**

Zone-map style `_min_`/`_max_` for all numeric columns. Enables skipping segments
for arbitrary WHERE clauses.

### 7. Sorted scan for ORDER BY time [DONE]

**Impact: Q25 64ms -> 24ms**

Segments sorted by `min_time`; SeaTurtleDecompress paths advertise pathkeys.
PG creates MergeAppend + Incremental Sort + Limit plans.

### 8. Arena allocation for text varlena [DONE]

**Impact: General improvement on text-heavy queries**

All text varlena for a segment packed into one contiguous `palloc`. Improves
cache locality during emit.

### 9. Lazy blob detoasting [DONE]

**Impact: Q37/Q38 heap_scan 16ms -> 2ms**

Segment-by values and min/max metadata extracted first (cheap). Pruning applied.
BYTEA blobs detoasted only for surviving segments.

### 10. Aggregate pushdown (SUM/AVG/COUNT/COUNT DISTINCT) [DONE]

**Impact: Q3 11ms, Q5 20ms, Q8 4.7ms**

`SeaTurtleAgg` node computes aggregates directly on decompressed columns. Handles
`SUM`, `AVG`, `COUNT`, `COUNT(DISTINCT)`, `GROUP BY` on segment_by columns.

### 11. Lazy column decompression (two-phase decompress) [DONE]

**Impact: Q24 756ms -> improved, Q22/Q23 improved**

Split decompression into two phases. Phase 1 decompresses only filter columns
(referenced in WHERE), applies batch quals, and builds a selection vector.
Phase 2 decompresses remaining columns, skipping text varlena allocation for
rows that don't pass the filter. When no rows survive Phase 1, Phase 2 is
skipped entirely (`phase2_skipped` counter in EXPLAIN ANALYZE).

For Top-N queries, Phase 2 columns are marked as lazy for TOAST detoasting —
only segments that contribute to the top-N result set have their deferred
columns materialized.

### 12. Expression aggregate pushdown — SUM(col + const) [DONE]

**Impact: Q30 425ms -> improved**

Detect `SUM(col + const)` pattern (`AggExpr::AddConst`) in planner hook.
SeaTurtleAgg computes all sums in a single pass over the decoded column,
applying the constant offset algebraically: `result = base_sum + const * count`.
When all agg specs reference the same column, the column is decoded once and
all results derived from a single accumulator.

### 13. String function pushdown — length() [DONE]

**Impact: Q28 207ms -> improved**

`AggExpr::LengthOf` variant computes string length on raw `&str` slices during
decompression without varlena allocation. Combined with aggregate pushdown,
`AVG(length(URL))` is computed entirely inside SeaTurtleAgg — zero text
materialization.

### 14. Regex pushdown via Rust regex crate [DONE]

**Impact: Q29 2837ms -> improved**

`GroupByExpr::RegexpReplace` detected in planner when GROUP BY contains
`regexp_replace(col, const_pattern, const_replacement)`. At scan time, the
Rust `regex` crate compiles the pattern once and applies it on raw `&str`
slices from LZ4/dictionary decompression. A cross-segment regex result cache
(`HashMap<String, String>`) avoids redundant regex calls for repeated input
values — tracked via `regex_cache_size` and `regex_cache_calls` in EXPLAIN.

### 15. IN list batch quals [DONE]

**Impact: Faster filtering for `col IN (v1, v2, ...)` predicates**

`BatchCompareOp::InList` evaluates IN-list predicates in vectorized Rust loops
over decoded datum arrays. The constant values are stored as `Vec<i64>` and
checked per-row. Also integrates with min/max segment pruning — segments whose
min/max range doesn't overlap any IN-list value are skipped entirely.

### 16. GROUP BY expression pushdown [DONE]

**Impact: Queries with date_trunc/extract/regexp_replace in GROUP BY**

SeaTurtleAgg handles GROUP BY on expressions, not just plain columns:

- **`date_trunc(unit, col)`** — truncation computed on epoch microseconds
  using pure arithmetic (`date_trunc_unit_to_usecs`). Supports second, minute,
  hour, day, week, month, year.
- **`extract(field FROM col)`** — field extraction from epoch microseconds
  (`extract_field_from_usecs`). Supports microsecond through epoch.
- **`regexp_replace(col, pattern, replacement)`** — regex applied on raw
  `&str` slices via Rust `regex` crate (see #14).

All three are serialized to `custom_private` and round-trip through plan
caching.

### 17. HAVING filter pushdown [DONE]

**Impact: Eliminates post-aggregation filtering in PG executor**

Simple HAVING clauses of the form `HAVING agg_result <op> const` (where `<op>`
is `>`, `<`, `>=`, `<=`, `=`, `<>`) are pushed into SeaTurtleAgg. Filters are
applied immediately after aggregation, before result rows are emitted. Encoded
as `HavingFilter { agg_idx, op, const_val }` in `custom_private`.

### 18. Min/max segment pruning [DONE]

**Impact: Skips segments whose value ranges don't match WHERE predicates**

Per-segment `_min_`/`_max_` metadata for all orderable types (INT2/INT4/INT8,
FLOAT4/FLOAT8, TIMESTAMP/TIMESTAMPTZ, DATE) is checked before decompression.
Segments that can't contain matching rows are skipped entirely. Supports `=`,
`<`, `<=`, `>`, `>=`, and `IN` list predicates. Tracked via
`segments_minmax_skipped` in EXPLAIN ANALYZE.

### 19. Dictionary-based segment pruning for LIKE [DONE]

**Impact: Skips segments where no dictionary entry matches the LIKE pattern**

For dictionary-compressed text columns, the dictionary (small, at the start of
the blob) is loaded and tested against the LIKE/NOT LIKE pattern before
decompressing indices. If no dictionary entry matches, the entire segment is
skipped. Implemented in `segment_skippable_by_dict_like()`.

### 20. Top-N pushdown for DecompressState [DONE]

**Impact: ORDER BY col LIMIT N on compressed scans**

When `ORDER BY col LIMIT N` is detected, DecompressState maintains a bounded
heap of top-N candidates during Phase 1. Segments are processed in min/max
order; once enough candidates are collected and a segment's min (or max for
DESC) can't beat the current worst candidate, remaining segments are skipped.
Phase 2 decompression is deferred and only performed for winning segments.
Pathkeys are advertised so PG eliminates the Sort node.

### 21. Top-N pushdown for AggScan [DONE]

**Impact: GROUP BY col ORDER BY agg(...) LIMIT N on aggregate queries**

When `ORDER BY <aggregate> [ASC|DESC] LIMIT N` is detected on a SeaTurtleAgg
query, the aggregation result is sorted by the specified aggregate column and
truncated to N rows inside the scan node. Pathkeys are set on the CustomPath
so PG eliminates the redundant Sort node above SeaTurtleAgg. EXPLAIN ANALYZE
shows `TopN: limit=N sort_col=X direction=ASC|DESC pre_topn_groups=M`.

### 22. Dictionary compression for text columns [DONE]

**Impact: Better compression ratio and faster decompression for low-cardinality text**

Text columns with `ndistinct < 10% of row_count AND < 65536 distinct values`
use dictionary encoding: fixed-width indices into a deduplicated string table.
Falls back to LZ4 for high-cardinality columns. Dictionary entries also serve
as a perfect filter for LIKE pruning (see #19).

### 23. Ndistinct statistics tracking [DONE]

**Impact: Enables cardinality-aware compression strategy selection**

Per-column `ndistinct` counts maintained in the catalog during compression.
Used to switch between dictionary encoding (low cardinality) and LZ4 (high
cardinality) for text columns. Also available via `get_column_ndistinct()`
for cost estimation.

### 26. Batch LIKE eval + ExecQual removal [DONE]

**Impact: Q23 0.94x → 1.10x (regression fixed), Q38 68.6ms → 59.4ms (-13%),
Q37 145ms → 131ms (-9%), Q36 143ms → 131ms (-8%)**

Three changes that eliminate redundant per-row overhead:

1. **ExecQual removal:** When all plan quals are successfully extracted as
   batch quals, `ps.qual` is set to NULL at BeginCustomScan time, skipping
   PG's per-row `ExecQual` in the emit loop. `extract_batch_quals` now
   returns a `handled_count` to verify full coverage before nulling.
2. **Skip redundant text eval:** `evaluate_batch_quals` no longer re-evaluates
   text LIKE/NotLike and Eq/Ne quals that were already applied during Phase 1
   decompression (`decompress_text_blob_with_like_filter`).
3. **SIMD Contains search:** For `LIKE '%needle%'` on LZ4 text columns,
   `memchr::memmem::Finder` scans the raw decompressed buffer in a single
   SIMD-accelerated pass instead of per-string `str::contains`. Cross-boundary
   safety: validates the full needle fits within a single string's byte range.

### 27. Expression GROUP BY pushdown (col +/- const) [DONE]

**Impact: Q36 143ms -> 67ms (fixes 0.69x regression -> 1.65x)**

`GroupByExpr::AddConst { offset, op_oid }` detects `col + const` / `col - const`
in GROUP BY expressions during the planner hook. Both `+` and `-` operators are
supported; for `-`, the constant is negated so the offset is always stored as
addition. At execution time, the group key is computed as `col_value + offset`.

For Q36's `GROUP BY ClientIP, ClientIP-1, ClientIP-2, ClientIP-3`, all four keys
are pushed into SeaTurtleAgg as a 4-element key vector. The scan processes 1M
rows and emits only 10 (via TopN pushdown), eliminating the PG hash agg that
previously dominated at 143ms.

---

## Regression Queries (Compressed Slower Than Uncompressed)

Several queries were slower with compression. Many have been addressed:

### Fixed regressions

**Q24 (was 0.13x):** Fixed by lazy column decompression (#11). Phase 2
skips text varlena allocation for non-matching rows.

**Q30 (was 0.48x):** Fixed by expression aggregate pushdown (#12). `SUM(col + N)`
computed algebraically inside SeaTurtleAgg.

**Q28 (was 0.57x):** Fixed by length() pushdown (#13). `AVG(length(URL))`
computed on raw `&str` slices without varlena allocation.

**Q29 (was 0.37x):** Fixed by regex pushdown (#14). `REGEXP_REPLACE` in GROUP BY
runs via Rust `regex` crate on raw slices with cross-segment caching.

**Q23 (was 0.94x):** Fixed by ExecQual removal (#26). Eliminating redundant
per-row PG qual evaluation brought ratio to 1.10x.

**Q36 (was 0.69x):** Fixed by expression GROUP BY pushdown (#27). `col +/- const`
in GROUP BY pushed into AggScan, eliminating 1M-row emit to PG hash agg.

### Remaining regressions

**Q24 (0.82x):** `SELECT * WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10`.
TopN two-pass skips 17/34 segments (dictionary LIKE pruning) and defers Phase 2
to 6 winning segments. Decompress=67ms, heap_scan=24ms. Phase 2 dominates:
decompressing ~100 columns for 6 segments with only 33 candidate rows.
Selection-based decompression was tried (#29) but caused icache regressions.
The fundamental issue is `SELECT *` on a wide table.

**Q29 (0.91x):** `REGEXP_REPLACE(Referer, ...) GROUP BY`. Decompress=756ms on
Referer (high-cardinality LZ4). The regex runs in Rust but decompression of
the full Referer column dominates. (#24 evaluated and deemed not worth implementing.)

**Q33 (1.35x):** `GROUP BY WatchID, ClientIP` — high-cardinality hash agg.
SeaTurtle scan=21ms, but PG hash agg on 1M rows with ~1M groups dominates.
Would require pushing hash agg into scan — very high effort.

---

## Planned Improvements

### ~~24. Late text materialization~~ — Won't implement

**Status: Won't implement — insufficient benefit**

Phase 2 already only materializes varlena for selected rows via
`decompress_text_blob_with_selection`. The text-heavy benchmark queries
(Q34, Q35, Q38) all have `all_quals_batch_handled == true`, meaning every
selected row is emitted — late materialization would save zero work. For
queries with remaining PG quals, the filtered columns are typically
numeric/timestamp, not text. The per-row palloc tradeoff (losing arena
allocation) would partially offset any gain in the narrow case where it helps.

### 25. Bloom filters for text column segment pruning

**Target: Q21 64ms -> ~30ms, Q22/Q23 moderate improvement**
**Complexity: High**

Store a per-segment bloom filter in the companion table for text columns with
moderate cardinality. During segment loading, test the bloom filter against
WHERE constants to skip segments that definitely don't contain the value.

Dictionary-based pruning (#19) already handles dictionary-compressed columns.
Bloom filters would extend pruning to LZ4-compressed (high-cardinality) text
columns where the dictionary approach doesn't apply.

**Files:** `src/compress.rs` (bloom filter in companion table schema),
`src/scan/exec.rs` (bloom filter test in segment loading)

### 28. Text GROUP BY in AggScan [DONE]

**Impact: Q16 45.8ms → 22.0ms (2.1x), Q19 351ms → 250ms (1.4x),
Q34 326ms → 281ms (1.2x), Q36 66.8ms → 34.0ms (2.0x),
Q39 28.8ms → 26.4ms, GMEAN 6.62x → 8.43x**

AggScan now supports text/varchar GROUP BY keys with several optimizations
for both low- and high-cardinality columns:

1. **hashbrown raw_entry API:** Single hash table lookup without cloning
   the key on cache hit. Uses `from_hash()` with borrowed `GroupKeyRef`
   (raw `*const str` pointers, no lifetime parameter) for zero-copy lookups.
2. **StringArena:** All group key strings packed into one contiguous `Vec<u8>`.
   `GroupKeyVal::Str(u32, u32)` stores (offset, len) into the arena. Eliminates
   275K individual String allocations and their cleanup cost.
3. **GroupKey enum:** `Single(GroupKeyVal)` for the common single-column
   GROUP BY case avoids per-key Vec heap allocation. `Multi(Box<[GroupKeyVal]>)`
   for multi-column.
4. **Flat accumulator storage:** HashMap maps `GroupKey → u32` index into a
   flat `Vec<AggAccumulator>`. Eliminates 275K per-group Vec<AggAccumulator>
   allocations and their O(n) drop cost.
5. **Per-segment SegTextColumn:** Dictionary/LZ4/SegBy text data decoded once
   per segment with O(1) `get_str(row)` access — no cross-segment interning.
6. **Vec reuse:** `key_ref` and `regex_results` buffers allocated once outside
   the row loop, cleared per iteration.

An ndistinct < 30K guard in the planner hook prevents AggScan from taking
over when text cardinality is too high (>30K distinct values), where PG's
native HashAgg is still competitive. The guard can be revisited as further
optimizations reduce the per-group overhead.

### ~~29. Partial decompression for SELECT * with LIMIT~~ — Tried, not effective

**Status: Investigated — marginal Q24 improvement offset by icache regressions**

`SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10`.
TopN two-pass already works (17/34 segments skipped by dictionary LIKE pruning,
6 segments enter Phase 2 with 33 candidates). The bottleneck is Phase 2
decompression of ~100 columns for winning segments.

**Approaches tried:**

1. **Min/max segment skipping on sort column:** Dead end — all 34 segments have
   identical 24h time ranges because `order_by = {counterid, userid, eventtime}`
   with EventTime as 3rd key. Min/max on EventTime gives no discrimination.

2. **Candidate truncation:** After threshold update, truncate candidate list to
   `effective_limit + 1` when oversized. Marginal Phase 1 improvement, and must
   keep at least `effective_limit + 1` candidates to avoid triggering the
   TopN-disabled fallback path.

3. **Selective TOAST detoasting (varatt_is_1b_e):** Only defer detoasting for
   truly external TOAST pointers; eagerly detoast inline blobs. Small improvement
   (~5ms) on Q24 warm runs but doesn't justify the code complexity alone.

4. **Selection-based decompression for ForBitpacked columns:** O(1) random-access
   decode for integer columns (73/105 columns) in Phase 2 — only decode the 1-3
   winning row values per column instead of all ~30K. Phase 2 nontext time dropped
   from 65ms to 13ms. However, adding ~200 lines of new functions (sparse decode,
   Phase2Col enum, null bitmap scanning) increased binary size, causing **10-25%
   icache-induced regressions across 19 unrelated queries** (confirmed by re-running
   baseline on same commit). Net negative.

**Conclusion:** The Q24 bottleneck is fundamentally that `SELECT *` on a 105-column
table requires decompressing all columns for winning rows. The TopN two-pass already
limits this to 6 segments × ~100 columns. Further improvements require either
reducing the number of columns decompressed (projection pushdown) or reducing
per-column decode cost without adding binary bloat.

### ~~30. High-cardinality integer GROUP BY optimization~~ — Largely addressed by #28

**Status: Mostly addressed by hashbrown/flat-accumulator work in #28**

Q16 (`GROUP BY UserID`) improved from 45.8ms → 22.0ms (2.1x) and Q19
(`GROUP BY UserID, minute, SearchPhrase`) from 351ms → 250ms (1.4x) as a
side effect of the hashbrown raw_entry API, flat accumulator storage, and
GroupKey::Single optimizations in #28. Further improvement would require
pre-sizing hash maps or top-N pruning within aggregation.

### 31. WHERE + AggScan combined batch evaluation

**Target: Q31 27.7ms -> ~15ms, Q32 59.6ms -> ~30ms**
**Complexity: Medium**

Q31/Q32 have `WHERE SearchPhrase <> ''` combined with GROUP BY aggregation.
Currently the filter and aggregation run in separate passes through the
decoded data. Combining batch qual evaluation with aggregate accumulation in
a single pass would improve cache locality and avoid redundant iteration.

For dictionary columns, the `<> ''` filter can leverage `empty_string_idx`
to skip rows by checking the 1-2 byte index array without decompressing any
string data. Make sure `check_ne_empty()` is wired into the batch eval path
inside AggScan, not just DecompressState.

**Files:** `src/scan/exec.rs` (fused filter+aggregate loop in AggState)
