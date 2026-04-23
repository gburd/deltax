# RTABench query analysis

Per-query analysis of pg_deltax on the 31 raw RTABench queries, based on
`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` of every query on a warm
cache. Raw plans live in `rtabench_explain_raw.txt`.

## Setup

- **Hardware**: `c6a.4xlarge` (16 vCPU, 32 GB RAM, 500 GB gp2 EBS).
- **PostgreSQL**: 18 with `shared_buffers=8GB`, `effective_cache_size=24GB`,
  `max_parallel_workers_per_gather=8`, `max_worker_processes=16`,
  `work_mem=8GB`, `jit=off`.
- **Session**: `SET enable_nestloop = off` (forced; see §3.A).
- **Data**: 181,737,692 events in `order_events` (123 compressed partitions,
  avg 1.48M rows/partition). `order_items` = 105M rows / 6.6 GB heap.
  `orders` = 10M rows. `customers` = 1,102, `products` = 9,255.
- **Warm run**: each query executed once before the `EXPLAIN ANALYZE` capture.

## Results summary

31 queries grouped by warm execution time and what's actually happening
in the plan. Competitor numbers pulled from rtabench.com at the same
hardware tier.

| #   | Query | pg_deltax (warm) | Plan shape | Category |
|----:|-------|-----------------:|------------|----------|
| Q14 | sum_prod_stock_price_per_category | **0.7 ms** | products⋈order_items only | G — no order_events |
| Q11 | events_for_an_order | 5.0 ms | DeltaXAppend(order_id=N) | F — point lookup ok |
| Q06 | order_events_without_backups | 20 ms | DeltaXAppend + aggregation | **✓ good** |
| Q09 | departed_orders_count | 8.7 ms | DeltaXAgg | **✓ good** |
| Q10 | last_event_for_an_order | 9.0 ms | DeltaXAppend(order_id=N) | F |
| Q07 | last_order_event_for_order | 12 ms | DeltaXAppend(order_id=N) | F |
| Q13 | satisfaction_with_without_backup | 13 ms | DeltaXAppend | ✓ |
| Q12 | max_satisfaction_for_order_per_day | 15 ms | DeltaXAgg | ✓ |
| Q05 | search_events_for_processor | 90 ms | DeltaXAppend | ✓ |
| Q03 | exists_order_delivered_from_terminal | 240 ms | DeltaXAppend + HashJoin | ✓ |
| Q16 | customers_with_most_orders | 254 ms | *no* DeltaXAppend, **Parallel Hash Join ×8** | G |
| Q15 | exists_order_delivered_for_customer | 435 ms | DeltaXAppend + HashJoin | E — indexes win |
| Q02 | global_agg (max(counter)) | 772 ms | DeltaXAgg, rows_processed=15M | D — no metadata path |
| Q08 | most_week_delayed_order | 1.12 s | DeltaXAppend + aggregation | ✓ |
| Q01 | count_orders_from_terminal | 1.55 s | DeltaXAppend + aggregation | ✓ |
| Q24 | top_customer_by_revenue | 1.70 s | no order_events, **Parallel HJ ×8** | G |
| Q29 | top_product_in_age_group | 1.69 s | no order_events, **Parallel HJ ×8** | G |
| Q28 | sales_volume_by_age_group | 1.91 s | no order_events, **Parallel HJ ×8** | G |
| Q26 | average_order_value | 2.06 s | no order_events, **Parallel HJ ×8** | G |
| Q27 | country_category_performance | 2.34 s | DeltaXAppend + **Parallel HJ** above | C — hybrid |
| Q18 | customer_month_value | 2.43 s | no order_events, **Parallel HJ ×8** | G |
| Q21 | sales_volume_by_country | 2.43 s | no order_events, **Parallel HJ ×8** | G |
| Q22 | sales_volume_by_country_state | 2.51 s | no order_events, **Parallel HJ ×8** | G |
| Q19 | out_of_stock_products | 3.10 s | DeltaXAppend + **Parallel HJ** above | C |
| Q00 | terminal_hourly_stats | 3.58 s | DeltaXAppend + window agg | ✓ |
| Q20 | customers_outstanding | 3.77 s | DeltaXAppend + **Gather** | C |
| Q04 | count_delayed_orders_per_day | 4.68 s | DeltaXAppend, jsonb @> filter | ✓ |
| Q30 | customers_with_most_orders_delivered | **10.77 s** | DeltaXAppend + 2× Hash Join serial | **A — parallel_safe wall** |
| Q25 | product_category_performance | **13.13 s** | DeltaXAppend + 2× Hash Join serial | **A** |
| Q23 | top_sales_volume_product_from_terminal | **23.71 s** | DeltaXAppend + Hash Join + re-agg | **A** |
| Q17 | top_selling_month_product | **25.41 s** | DeltaXAppend + HJ(105M) serial | **A** |

Total warm: **128.6 s** across 31 queries. **Four queries (Q17, Q23, Q25,
Q30) account for 73 s (57%) of that total.** They share one plan shape:
`DeltaXAppend on order_events ⋈ Seq Scan on order_items(105M)` with no
parallelism above the join.

## 1 · What's actually happening, by category

### Category A — DeltaXAppend in the plan, no parallelism above it (Q17, Q23, Q25, Q30)

The single biggest drag on the suite. Every one of these queries joins
`order_events` (selective filter → a few hundred K to a few M rows) with
the full `order_items` table (105M rows, 6.6 GB). Because `DeltaXAppend`
has `parallel_safe = false` (set unconditionally at every `CustomPath`
construction site in `src/scan/path.rs`), PostgreSQL cannot place a
`Gather` above any join that contains it — the entire subtree runs on
one core.

**Q17 plan, real timings:**

```
Limit                                    actual=24958 ms
  Sort                                   actual=24958 ms
    GroupAggregate                       actual=23423..24957 ms
      Sort (443 MB, 6.3M rows)           actual=23423..24442 ms
        Hash Join  p ⋈                   actual=19970..22300 ms
          Hash Join  oe ⋈ oi             actual=19968..21341 ms
            DeltaXAppend on order_events rows=602,731   time=581 ms
                                         (15M scanned, filter hits 602K)
            Hash(order_items 105M rows)  actual=19747 ms    ← 78% of total
              Seq Scan on order_items    actual=6867 ms     (I/O)
```

Single-threaded hash build of 105M rows takes **19.7 s**. Parallel Hash
Join with 8 workers would be ~2.5 s. That alone would drop Q17 from 25 s
to ~7-8 s.

**Cost-estimate bug visible here.** `DeltaXAppend` reports
`rows=14,808,054` but actual output is `602,731` — a 25× overestimate
because the `event_type='Delivered'` filter selectivity isn't in the
path's row estimate (only the time-range selectivity is). With an
accurate estimate PG would prefer hashing the filtered DeltaXAppend side
(~600K rows) instead of the 105M `order_items` side, a further ~10×
speedup for these queries.

Q23, Q25, Q30 are structurally identical: 6.6 GB heap hash build
serialized on one core.

### Category B (unused label — skipped)

### Category C — DeltaXAppend + parallel plan above it (Q19, Q20, Q27)

These plans *do* have a `Gather` node, but the `DeltaXAppend` below is
still serial. What's parallelized is the plain-PG part of the plan
(`order_items`, `orders`, `customers`, `products` as Parallel Seq Scans,
then Parallel Hash Join). pg_deltax runs a full scan of `order_events`
on one core, hands its output to a main-thread step, then everything
above goes parallel.

Q19 (3.10 s) is the most interesting here: `DeltaXAppend` emits 5.3M
rows in 683 ms (8 cores would do it in ~0.5 s if it were parallel), then
a parallel hash join with `orders ⋈ order_items ⋈ products` takes ~2.4 s
using 8 workers. If `DeltaXAppend` were parallel-safe, both halves could
overlap and Q19 would drop under 1 s.

Q20 (3.77 s) has no Hash Join in the plan — it's an EXISTS/NOT EXISTS
pair with a Gather over a serial `DeltaXAppend`. PostgreSQL's NestLoop
was force-disabled, so the outer side is a Parallel Hash. But the inner
has to walk DeltaXAppend serially for each correlated subquery
evaluation.

### Category D — DeltaXAgg full decompress when metadata would suffice (Q02)

```sql
-- Q02
SELECT max(counter) FROM order_events
WHERE event_created >= '2024-04-20' AND event_created < '2024-05-20';
```

```
Custom Scan (DeltaXAgg)                     actual=0.003 ms
  DeltaX Timing: 771.625 ms
    metadata=2.5  heap_scan=1.5  detoast=78
    decompress=545  agg=145  merge=0  topn=0
  DeltaX Stats: segments=510 rows_processed=15,111,288
```

We decompressed 15M rows of the `counter` column and ran a max() over
them — taking 545 ms of decompress + 145 ms of aggregation. But every
compressed segment already has `_max_counter` stored in its metadata
table; the right answer is `SELECT max(_max_counter) FROM
_deltax_compressed.order_events_pXXXX_meta` over ~510 segments, which
should run in < 10 ms.

**DeltaXAgg lacks a metadata-only fast path for trivial aggregates**
(`max`/`min`/`sum`/`count` with no GROUP BY, no WHERE except on the
segment-key column). DuckDB does this in 7 ms; ClickHouse in 47 ms.

Q12 (`max(satisfaction)` grouped by `order_id`, 15 ms) also goes through
DeltaXAgg but is fast because the grouping column is in `order_by` and
the aggregation runs per-segment. The missing fast path specifically
bites global aggregates.

### Category E — EXISTS where a B-tree index wins (Q15)

```sql
-- Q15
SELECT EXISTS (
  SELECT FROM order_events JOIN orders USING (order_id)
   WHERE customer_id = 124 AND event_type='Delivered'
);
```

```
Hash Join                              actual=9 ms
  DeltaXAppend  filter event_type='Delivered'  (1 segment, 9 skipped)
    segments=1 rows_out=159 heap_scan=420 ms    ← despite finding 1 segment
  Hash
    Bitmap Heap Scan on orders(customer_id_idx)  actual=8 ms, 9101 rows
```

Plan is structurally sensible — we skip 9 of 10 segments — but the **one
segment that survives still takes 420 ms** of `heap_scan` to find 159
Delivered events. Plain PostgreSQL answers this in 4 ms via two index
lookups (orders → order_events by order_id, stop at first hit).

Two gaps:
1. **No early termination for EXISTS.** We don't know the executor only
   wants one row, so we scan all 159 matches. PG's ScanState flag
   (`ss_ps_resultslot`) + returning after first match could cut this.
2. **`heap_scan` is the bulk of the 420 ms** — reading compressed-blob
   TOAST pages from shared_buffers for 1.48M-row segment. For this
   selectivity, a B-tree index on `orders(customer_id)` + the existing
   segment-level bloom on `order_id` should skip even the one remaining
   segment. But the filter flows the wrong direction: we scan all
   Delivered events, then join with customer-124's orders, rather than
   starting from the 9,101 customer-124 order_ids and probing.

### Category F — Point lookups by `order_id` (Q07, Q10, Q11)

**These work well** (5–12 ms warm). Since `order_by = ['order_id',
event_created]`, segments are sorted by order_id within each partition;
the segment-level `_min_order_id`/`_max_order_id` effectively skip most
segments on an `order_id = N` predicate. Q11 hits 5 ms, roughly on par
with TimescaleDB (4.7 ms) and DuckDB (1 ms).

The one gap: TimescaleDB's `enable_chunk_skipping('order_events',
'order_id')` prunes whole 3-day *chunks* before any per-row work. We
prune at segment granularity (30K rows), which is fine here but would
matter at larger chunk sizes.

### Category G — Queries that don't touch `order_events` (Q14, Q16, Q18,
Q21, Q22, Q24, Q26, Q28, Q29)

Wholly parallel plans, no pg_deltax code involved. These take 0.7 ms to
2.5 s depending on what gets scanned. They mostly filter `orders.created_at`
directly and join with `order_items`; PG has a parallel hash join with
up to 8 workers here.

### Category H — Queries with heavy scan-side load (Q00, Q01, Q04, Q08)

All between 1.1–4.7 s. Each scans 15M events in a 1-month window with a
jsonb predicate (`event_payload->>'terminal'` or `event_payload @>
'["Delayed","Priority"]'`), aggregates by some bucket, and optionally
joins. `DeltaXAppend` does its job well (time dominated by decompress,
not heap I/O), but with only one core the decompress itself is the
bottleneck.

Decompress time breakdown for Q04 (4.68 s):
```
DeltaX Timing: 4610 ms
  metadata=3  heap_scan=45  decompress=4451  batch_eval=67  emit=42
DeltaX Stats: segments=504 rows_processed=14,955,000 ...
```

4.45 s of decompress is CPU on one core processing 15M rows. 8 cores
would do it in ~0.6 s.

## 2 · Root causes, ranked by impact

1. **DeltaXAppend is not parallel-safe** (12+ queries; ~60% of total
   warm time). Single fix with the widest impact. Ongoing as task #18.

2. **No row-count estimate for filter predicates in DeltaXAppend**
   (Q17, Q23, Q25, Q30 × ~10× each when combined with #1).
   `cost_deltaxappend` in `src/scan/cost.rs` reports post-partition-prune
   row count but ignores pushed-down filter selectivity (event_type,
   jsonb containment, etc.). Bloom-filter selectivity and the
   segment-level `_ndistinct`/`_min`/`_max` metadata have the data; the
   estimator just doesn't consume them.

3. **No metadata-only fast path for simple aggregates** (Q02, ~760 ms
   savings). `DeltaXAgg` should detect `max/min/sum/count` with no GROUP
   BY and no predicate beyond time-column range, and answer from the
   segment-metadata table alone. ~100× speedup on Q02.

4. **DeltaXAgg decompress is single-threaded** (affects every query in
   category H). Related to #1 — fixing parallel-safe unblocks multi-core
   decompress through the standard PG parallel-agg path.

5. **`heap_scan` cost on EXISTS/selective scans** (Q15). The per-segment
   heap read is surprisingly slow even when the segment is in
   shared_buffers. Profile worth doing — may be TOAST detoasting for
   jsonb columns that we don't need (Q15 only reads order_id +
   event_type), suggesting we aren't dropping unnecessary column blobs
   from the decompress set.

6. **No EXISTS short-circuit in DeltaXAppend** (Q15 partial). Low
   priority — only a handful of queries.

7. **ClickBench-style partition-level min/max index for non-time columns**
   (Q07, Q09–Q13 parity with TimescaleDB). 5–7× gap on point lookups by
   `order_id`. Segment-level metadata already carries `_min/_max`, but
   PG's partition pruner doesn't see it — only the time-column
   declarative partition bounds are known at plan time. A post-plan hook
   that prunes at the partition level using `order_id` min/max would
   close the gap.

## 3 · Notes on the benchmark setup

### A · `SET enable_nestloop = off` is currently load-bearing

Without it, Q17 picked a `NestLoop → Materialize → HashJoin(order_items
in inner)` plan with a 9,255 × 22M = ~2×10¹¹-op loop that ran for > 30
minutes. The NestLoop-over-materialized-HJ plan was selected because
the Hash Join row estimate was wildly off (2,408 estimated vs 6.3M
actual). Root cause is #2 above (filter-selectivity not in the
estimator). Once #2 is fixed, this hack should be removable.

### B · `work_mem = 8 GB` is load-bearing for Q17/Q23/Q25/Q30

The 105M-row hash build needs ~5 GB peak. At the default 256 MB it
spills to ~25 batches → 10-15× slowdown. Once #1 is fixed,
`work_mem=2GB` with 8 parallel workers splits the build across workers
and the per-worker memory pressure drops.

### C · Cold-run I/O dominates Q17–Q30

Cold runs are 1.5–2× the warm time. Reading 5.8 GB compressed
`order_events` + 6.6 GB `order_items` from gp2 EBS (250 MB/s) is ~50 s
of I/O. `shared_buffers=8GB` covers about half the data; with the
current plans a single query alone exceeds cache. Not really a pg_deltax
issue — TimescaleDB shows similar cold/warm ratios.

## 4 · Recommendations, priority-ordered

| Priority | Fix | Est. suite impact |
|---|---|---|
| P0 | Make `DeltaXAppend`/`DeltaXAgg` parallel-safe | **-60 s** on Q17/Q23/Q25/Q30 + broad gains |
| P0 | Filter-selectivity in DeltaXAppend cost estimator | **-15 s** (join-side-choice on Q17 class) |
| P1 | Metadata-only fast path for global min/max/sum/count (DeltaXAgg) | **-700 ms** (Q02, also Q06 pattern) |
| P2 | Trim column-blob decompress set to referenced columns only | helps Q15 (EXISTS) and any narrow-projection query |
| P2 | Partition-level `order_id` min/max pruning hook | 5–7× on Q07, Q09–Q13 |
| P3 | EXISTS short-circuit in `DeltaXAppend` executor | Q15 specifically |

Fixing the top two alone should halve the total suite time and bring us
to within spitting distance of TimescaleDB on the join-heavy queries;
all four top fixes together would put pg_deltax clearly ahead on the 31
raw queries.
