# ClickBench Query-by-Query Analysis

Investigation of all 43 ClickBench queries on the full 100M row dataset
(c6a.4xlarge EC2, PostgreSQL 18, pg_deltax). Each section records the
EXPLAIN ANALYZE output, what dominates execution, and improvement
ideas.

> Environment: 18 partitions, ~3338 compressed segments total,
> `pg_deltax.parallel_workers=0` (auto, capped at 16), `max_parallel_workers=8`.
> Meta table split into narrow meta + wide stats table. Local meta table
> cache. SPI planning cache. Length-sidecar blob for text columns.
> Partitioned parallel merge for both compact (int) and mixed (text) paths.

## What changed since the last analysis

Major optimizations landed in the interval:

| Change | Commits | Queries helped | Effect |
|--------|---------|----------------|--------|
| Partitioned parallel merge for the **mixed (text GROUP BY) path** | 83b5ad0 | Q13 | 4.96 s → 2.14 s (2.3×) |
| Parallel merge + HAVING path | 81ca6f1, f4e005b | Q28 | 9.59 s → 6.75 s (1.4×); now **faster than CH** |
| Length-sidecar blob for `length(text_col)` | e0f09a9 | Q27 | 1.82 s → 0.55 s (3.3×) |
| Knock-on partitioned-merge gains (compact path) | — | Q8, Q30, Q31, Q39 | 2–2.3× each |

Bench best-of-3 (seconds) — before/after snapshot:

| Query | Before | After | Speedup |
|-------|--------|-------|---------|
| Q8 GROUP BY RegionID COUNT(DISTINCT) | 2.074 | **1.012** | 2.05× |
| Q13 SearchPhrase COUNT(DISTINCT) | 4.960 | **2.143** | 2.31× |
| Q27 AVG(length(URL)) GROUP BY CounterID | 1.815 | **0.550** | 3.30× |
| Q28 Referer regex GROUP BY HAVING | 9.585 | **6.748** | 1.42× |
| Q30 SearchEngine+ClientIP multi-agg | 1.548 | **1.055** | 1.47× |
| Q31 WatchID+ClientIP multi-agg | 2.253 | **1.751** | 1.29× |
| Q39 CounterID=62 traffic src | 0.380 | **0.214** | 1.78× |

**Now faster than ClickHouse: Q3, Q24, Q26, Q28, Q33, Q34** (was 5 queries,
now 6 — Q28 moved into this bucket).

## Top-level findings (actionable)

Ranked by remaining cumulative wallclock across the benchmark.

### F3. Detoast still dominates most DeltaXAgg queries (unchanged)

**Impact: 20+ queries, ~40 s cumulative wallclock.** Pipelined detoast is
active and helps a little, but serial TOAST I/O sets a floor. Cold-run
`detoast` numbers from EXPLAIN ANALYZE on queries still well above CH:

| Query | detoast (ms) | Total DeltaX (ms) |
|-------|--------------|-------------------|
| Q20 | 14,376 | 18,955 |
| Q28 | 7,214 | 11,522 |
| Q22 | 7,626 | 8,963 |
| Q32 | 6,615 | 13,271 |
| Q31 | 4,700 | 5,589 |
| Q18 | 3,576 | 5,859 |
| Q30 | 3,342 | 3,894 |
| Q13 | 2,457 | 3,956 |
| Q5 | 1,722 | 2,918 |

Previously investigated and ruled out (no change): inline `STORAGE MAIN`
(LZ4-on-LZ4 compression is a net I/O win), `STORAGE EXTERNAL` (same
reason), tightening `needed_cols` (already correct).

### F4. Merge phase on essentially-unique GROUP BY keys (narrowed)

With #41 done, the high-cardinality merge bottleneck is largely solved for
`GROUP BY text_col` shapes (Q13, Q28). What **remains** is GROUP BY on
near-unique integer keys, where every 100 M row produces a distinct group.
Partitioned merge still iterates every worker's full map with a
hash-modulo filter, so work is O(total entries × n_partitions/n_partitions) =
O(total entries).

| Query | merge (ms) | pre_topn_groups | Total DeltaX (ms) |
|-------|-----------|-----------------|-------------------|
| Q32 WatchID+ClientIP | 5,750 | 99,997,494 | 9,440 |
| Q15 Top UserID | 1,040 | 21,981,595 | 1,957 |
| Q35 ClientIP arithmetic | 841 | 20,960,937 | 1,710 |

This is the shape where **#36 two-level hash aggregation**
(already spec'd in PERF_IMPROVEMENTS.md, not yet implemented) would
help: each worker writes directly into 256 partition-local hashmaps
during phase-1, eliminating the re-routing scan at merge time.

### F5. Q20/Q21/Q22 URL LIKE on LZ4 columns (unchanged)

Still the worst outlier in the benchmark. Q20 remains ~22× CH. Options
covered previously: #40 dict-accelerated LIKE helps only where the column
is dict-compressed (Title yes, URL no). #33 trigram bloom has been tried
and doesn't prune meaningfully on common patterns.

### F6. `COUNT(DISTINCT)` still detoasts every blob (new)

Q4 (`COUNT DISTINCT UserID`) and Q5 (`COUNT DISTINCT SearchPhrase`) still
cost 3.0 s and 1.85 s. The dict-only fast path for text
`COUNT(DISTINCT)` is **already implemented** (agg.rs
`count_distinct_only_str`), but it still has to load the full compressed
blob via `pg_detoast_datum` to reach the dict header. Result: Q5 is
detoast-bound, not agg-bound.

Proposed fix: store the dict (and/or its hashes) as a separate sidecar
TOAST column, analogous to the length sidecar — see "Dict sidecar blob"
in the improvement list below. This is not covered by any existing item
in PERF_IMPROVEMENTS.md.

---

## Query-by-query details

Format per query:
- **Query** (from queries.sql)
- **CH / deltax / ratio** (CH = ClickHouse c6a.4xlarge reference, deltax = bench best-of-3)
- **EXPLAIN ANALYZE timing breakdown** (warm run unless noted)
- **Analysis + potential improvements**

### Q0 — COUNT(*)

```
SELECT COUNT(*) FROM hits;
```

- CH 0.001 s / deltax **0.003 s** / **3.0×**
- `DeltaXCount`, metadata=0.001 ms, heap_scan=0.000 ms.
- **Near-optimal.** Nothing actionable.

### Q1 — COUNT(*) WHERE AdvEngineID <> 0

```
SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0;
```

- CH 0.006 s / deltax **0.023 s** / **3.8×**
- Metadata-resolvable via nonzero count stats. Remaining gap is framework
  overhead.

### Q2 — SUM/AVG full-scan

```
SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits;
```

- CH 0.021 s / deltax **0.028 s** / **1.3×**
- Fully metadata-resolved. **Near parity.**

### Q3 — AVG(UserID)

```
SELECT AVG(UserID) FROM hits;
```

- CH 0.027 s / deltax **0.025 s** / **0.93× (faster)**
- Metadata-resolved.

### Q4 — COUNT(DISTINCT UserID)

```
SELECT COUNT(DISTINCT UserID) FROM hits;
```

- CH 0.353 s / deltax **3.022 s** / **8.6×**
- `DeltaXAgg`, wall=3063. detoast=417, agg=450 (per-worker phase times —
  parallel wall dominates).
- 100 M int8s hashed into a single distinct set.
- **Improvement:** Per-segment HLL sketches at compress time
  (`_hll_<col>`), merge across segments in O(segments × 16 KB).
  Expected: 3.0 s → ~100 ms.

### Q5 — COUNT(DISTINCT SearchPhrase)

```
SELECT COUNT(DISTINCT SearchPhrase) FROM hits;
```

- CH 0.623 s / deltax **1.848 s** / **3.0×**
- detoast=1722, agg=1747. Dict-only COUNT(DISTINCT) fast path is active
  (hashes only dict entries, ~500 per segment) — but the full blob must
  still be **detoasted** to reach the dict header.
- **Improvement: dict sidecar blob** (new idea — not in
  PERF_IMPROVEMENTS.md). Store each segment's dict as a separate short
  column, skip detoast of the main blob entirely.
  Expected: 1.85 s → ~200 ms.

### Q6 — MIN/MAX EventDate

```
SELECT MIN(EventDate), MAX(EventDate) FROM hits;
```

- CH 0.010 s / deltax **0.014 s** / **1.4×**
- `DeltaXMinMax`, metadata-resolved. **Near parity.**

### Q7 — GROUP BY AdvEngineID

```
SELECT AdvEngineID, COUNT(*) FROM hits WHERE AdvEngineID <> 0 GROUP BY AdvEngineID ORDER BY COUNT(*) DESC;
```

- CH 0.009 s / deltax **0.091 s** / **10.1×**
- detoast=46, agg=29. 2262 segments, 630 K rows, 18 groups.
- CH resolves this from metadata (18 fixed values). Could pre-compute
  per-(AdvEngineID, segment) COUNT counters in stats table — ambitious.

### Q8 — GROUP BY RegionID COUNT(DISTINCT UserID)

```
SELECT RegionID, COUNT(DISTINCT UserID) AS u FROM hits GROUP BY RegionID ORDER BY u DESC LIMIT 10;
```

- CH 0.452 s / deltax **1.012 s** / **2.2×** ← 2.1× faster than before
- detoast=673, decompress=7, agg=36, **merge=308**, finalize=0.2.
- Partitioned parallel merge halved the merge cost vs the previous run.
- **Remaining improvement:** HLL per-group sketches. Expected: 1.0 s → ~300 ms.

### Q9 — RegionID multi-agg

```
SELECT RegionID, SUM(AdvEngineID), COUNT(*) AS c, AVG(ResolutionWidth), COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10;
```

- CH 0.522 s / deltax **1.499 s** / **2.9×**
- detoast=942, decompress=18, agg=39, finalize=320, topn_select=108.
- finalize=320 ms is COUNT(DISTINCT UserID) finalization — HLL-style would help.

### Q10 — MobilePhoneModel users

```
SELECT MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhoneModel ORDER BY u DESC LIMIT 10;
```

- CH 0.147 s / deltax **0.462 s** / **3.1×**
- detoast=611, decompress=51, agg=46, merge=28.
- Dominant: detoast (F3).

### Q11 — MobilePhone + Model users

```
SELECT MobilePhone, MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhone, MobilePhoneModel ORDER BY u DESC LIMIT 10;
```

- CH 0.143 s / deltax **0.481 s** / **3.4×**
- detoast=581, decompress=97, agg=55, merge=27.
- Dominant: detoast (F3).

### Q12 — Top 10 SearchPhrase

```
SELECT SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.599 s / deltax **1.117 s** / **1.9×**
- detoast=399, decompress=144, agg=559, merge=0, topn_select=10.
- agg=559 ms on 55 M rows with 4.8 M groups — near the limit for dict
  text hashing. Minor remaining gains from F3.

### Q13 — SearchPhrase users (COUNT DISTINCT)

```
SELECT SearchPhrase, COUNT(DISTINCT UserID) AS u FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY u DESC LIMIT 10;
```

- CH 0.804 s / deltax **2.143 s** / **2.7×** ← 2.3× faster than before
- detoast=539, decompress=164, agg=769, **merge=574**, finalize=0.1.
- Partitioned parallel merge cut merge from 2956 ms → 574 ms.
- agg=769 ms is the hash-distinct on UserIDs per group — HLL sketches per
  group would help further.

### Q14 — SearchEngine + SearchPhrase

```
SELECT SearchEngineID, SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.597 s / deltax **1.227 s** / **2.1×**
- detoast=418, decompress=150, agg=612. Similar shape to Q12.

### Q15 — Top 10 UserID

```
SELECT UserID, COUNT(*) FROM hits GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10;
```

- CH 0.384 s / deltax **2.022 s** / **5.3×**
- detoast=576, decompress=5, agg=309, **merge=1040**.
- 22 M distinct UserIDs. Current partitioned merge still visits every
  worker's entries — see F4.
- **Improvement:** #36 two-level hash agg. Expected: 2.0 s → ~1.2 s.

### Q16 — UserID + SearchPhrase top

```
SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10;
```

- CH 1.709 s / deltax **2.045 s** / **1.2×**
- detoast=515, decompress=186, agg=1072. Competitive.

### Q17 — UserID + SearchPhrase (no order)

```
SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase LIMIT 10;
```

- CH 0.999 s / deltax **1.685 s** / **1.7×**
- detoast=517, decompress=178, agg=797, merge=0.

### Q18 — UserID + extract(minute) + SearchPhrase

```
SELECT UserID, extract(minute FROM EventTime) AS m, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, m, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10;
```

- CH 3.041 s / deltax **3.670 s** / **1.2×**
- detoast=3576, decompress=331, agg=2103. Competitive. Three-column
  detoast is inherent.

### Q19 — Point lookup UserID = const

```
SELECT UserID FROM hits WHERE UserID = 435090932899640449;
```

- CH 0.003 s / deltax **0.042 s** / **14×**
- `DeltaXAppend`. segments=50, segments_skipped=3288
  (1870 minmax + 1418 bloom). Warm: heap_scan=23, decompress=13.
- Cost split roughly: 20 ms bloom checking (122 meta + 5926 bloom buffer
  hits), 13 ms decompress, rest framework.
- **Improvement:** Partition-level bloom filter to prune 18 partitions
  first. Expected: 42 ms → ~10 ms.

### Q20 — COUNT(*) WHERE URL LIKE '%google%'

```
SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%';
```

- CH 0.312 s / deltax **6.798 s** / **21.8×**
- `DeltaXAgg`, segments=1703 (after dict pruning). rows_processed=15,911.
- Warm: detoast=2182, decompress=2699, agg=1837.
- **Open problem.** URL is LZ4, not dict. #40 dict-accelerated LIKE
  doesn't help. #33 trigram bloom ineffective on common patterns.
- Only lever is #39 pipelined detoast (already active, limited help) or
  heavier inverted-index style approach.

### Q21 — SearchPhrase MIN(URL) WHERE URL LIKE '%google%'

```
SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits WHERE URL LIKE '%google%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.098 s / deltax **1.953 s** / **19.9×**
- detoast=1351, decompress=315, agg=340.
- segments=2417. Dominant: detoast URL blobs. Same fundamental issue as
  Q20.

### Q22 — Title LIKE Google + URL NOT LIKE

```
SELECT SearchPhrase, MIN(URL), MIN(Title), COUNT(*) AS c, COUNT(DISTINCT UserID) FROM hits WHERE Title LIKE '%Google%' AND URL NOT LIKE '%.google.%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.717 s / deltax **3.719 s** / **5.2×**
- detoast=7626, decompress=703, agg=1149. segments=2915.
- Title is dict-encoded so #40 dict-accelerated LIKE would skip per-row
  Title hashing. URL is LZ4 — only #39 helps.

### Q23 — SELECT * WHERE URL LIKE ... ORDER BY EventTime LIMIT 10

```
SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10;
```

- CH 0.393 s / deltax **0.477 s** / **1.2×**
- `DeltaXAppend` TopN, 12 surviving segments. **Competitive.**

### Q24 — SearchPhrase ORDER BY EventTime LIMIT 10

```
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10;
```

- CH 0.147 s / deltax **0.110 s** / **0.75× (faster)**

### Q25 — ORDER BY SearchPhrase LIMIT 10

```
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY SearchPhrase LIMIT 10;
```

- CH 0.192 s / deltax **1.925 s** / **10×**
- `DeltaXAppend`, 3332 segments, decompress=2280 ms loading SearchPhrase
  column across all segments for lexicographic sort.
- **Improvement: dict-only ORDER BY text LIMIT.** For dict-encoded
  columns, read only the dict portion of each blob, merge candidates via
  min-heap. 3332 segments × ~500 dict entries = 1.66 M items.
  Even better combined with the dict sidecar blob idea — no full-blob
  detoast. Expected: 1.9 s → ~150 ms.

### Q26 — ORDER BY EventTime, SearchPhrase LIMIT 10

```
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime, SearchPhrase LIMIT 10;
```

- CH 0.149 s / deltax **0.109 s** / **0.73× (faster)**

### Q27 — CounterID AVG(length(URL)) HAVING c > 100K

```
SELECT CounterID, AVG(length(URL)) AS l, COUNT(*) AS c FROM hits WHERE URL <> '' GROUP BY CounterID HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25;
```

- CH 0.083 s / deltax **0.550 s** / **6.6×** ← was 21.9×, now 3.3× faster
- detoast=260, decompress=27, agg=241, merge=1.
- Length-sidecar blob (#42) avoids decompressing the URL payload —
  only the length sidecar + CounterID column are loaded.
- Remaining 550 ms is split ~half/half between sidecar+CounterID I/O
  and the aggregation loop over 100 M rows.
- No obvious metadata-only fast path here: the per-segment `SUM(length)`
  that #42 already tracks can't collapse `GROUP BY CounterID` without
  per-(CounterID, segment) stats, which is a much heavier change.

### Q28 — Referer REGEXP_REPLACE GROUP BY HAVING

```
SELECT REGEXP_REPLACE(Referer, ...) AS k, AVG(length(Referer)) AS l, COUNT(*) AS c, MIN(Referer) FROM hits WHERE Referer <> '' GROUP BY k HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25;
```

- CH 9.582 s / deltax **6.748 s** / **0.70× (faster)** ← was 1.0×
- detoast=1159, decompress=3184, agg=2036, merge=276.
- Parallel merge with HAVING pushdown now parallelizes the final merge.
  Regex evaluation (decompress=3184 ms includes the MIN(Referer) and
  REGEXP_REPLACE work) is the remaining cost.
- **Now outperforms ClickHouse.**

### Q29 — Wide SUM 89 cols

```
SELECT SUM(ResolutionWidth), SUM(ResolutionWidth + 1), ... SUM(ResolutionWidth + 89) FROM hits;
```

- CH 0.029 s / deltax **0.045 s** / **1.6×**
- Fully metadata-resolved. **Near parity.**

### Q30 — SearchEngine + ClientIP multi-agg

```
SELECT SearchEngineID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, ClientIP ORDER BY c DESC LIMIT 10;
```

- CH 0.342 s / deltax **1.055 s** / **3.1×** ← improved from 4.5×
- detoast=437, decompress=110, agg=469, merge=0, topn_select=13.
- Dominant: detoast + agg. Partitioned merge made merge=0.

### Q31 — WatchID + ClientIP with SearchPhrase filter

```
SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10;
```

- CH 0.562 s / deltax **1.751 s** / **3.1×** ← improved from 4.0×
- detoast=765, decompress=189, agg=707, merge=0.
- Further gains from #36 two-level hash agg (8.68 M pre_topn groups).

### Q32 — WatchID + ClientIP all

```
SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10;
```

- CH 3.793 s / deltax **9.530 s** / **2.5×**
- detoast=2045, decompress=20, agg=1636, **merge=5750**.
- 99,997,494 essentially-unique groups. Partitioned merge threads still
  iterate every worker's full map.
- **Primary remaining bottleneck in the benchmark.** #36 two-level
  hash agg expected: 9.5 s → ~4 s.

### Q33 — GROUP BY URL ORDER BY c DESC LIMIT 10

```
SELECT URL, COUNT(*) AS c FROM hits GROUP BY URL ORDER BY c DESC LIMIT 10;
```

- CH 2.782 s / deltax **2.680 s** / **0.96× (tied/faster)**
- detoast=1058, decompress=219, agg=1236. **At parity.**

### Q34 — GROUP BY 1, URL

```
SELECT 1, URL, COUNT(*) AS c FROM hits GROUP BY 1, URL ORDER BY c DESC LIMIT 10;
```

- CH 2.851 s / deltax **2.689 s** / **0.94× (faster)**
- Same shape as Q33.

### Q35 — GROUP BY ClientIP, IP−1, IP−2, IP−3

```
SELECT ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3, COUNT(*) AS c FROM hits GROUP BY ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3 ORDER BY c DESC LIMIT 10;
```

- CH 0.297 s / deltax **1.722 s** / **5.8×**
- detoast=507, decompress=3, agg=329, **merge=841**.
- 21 M distinct ClientIPs. Partitioned merge applies but still O(total
  entries). #36 two-level hash agg: expected 1.7 s → ~900 ms.

### Q36 — Top URLs for CounterID=62

```
SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 AND IsRefresh = 0 AND URL <> '' GROUP BY URL ORDER BY PageViews DESC LIMIT 10;
```

- CH 0.043 s / deltax **0.101 s** / **2.3×**
- segments=26 (min/max pruning), rows_processed=671 K.
- Reasonable. Remaining is agg on 314 K URL groups.

### Q37 — Top Titles for CounterID=62

- CH 0.021 s / deltax **0.041 s** / **2.0×**
- segments=26, rows_processed=660 K.

### Q38 — CounterID=62 links OFFSET 1000

- CH 0.017 s / deltax **0.079 s** / **4.6×**
- segments=26, rows_processed=47,740, 13 K groups.

### Q39 — CounterID=62 traffic src

```
SELECT TraficSourceID, SearchEngineID, AdvEngineID, CASE ... THEN Referer ELSE '' END AS Src, URL AS Dst, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND ... GROUP BY ... ORDER BY PageViews DESC LIMIT 10 OFFSET 1000;
```

- CH 0.077 s / deltax **0.214 s** / **2.8×** ← improved from 4.9×
- detoast=60, decompress=31, agg=132, merge=48, topn_select=0.
- Partitioned merge + HAVING path reduced merge cost.

### Q40 — CounterID=62 URLHash

- CH 0.013 s / deltax **0.136 s** / **10×**
- segments=26, rows_processed=89,914, 41 K groups.
- detoast=145, decompress=72, agg=22. Still the "too many columns
  decompressed for 6 batch quals" smell — worth a column-pruning audit.

### Q41 — CounterID=62 window dim

- CH 0.009 s / deltax **0.052 s** / **5.8×**
- Falls back to PG Sort because result_rows > OFFSET 10000.

### Q42 — CounterID=62 by minute

- CH 0.008 s / deltax **0.041 s** / **5.1×**
- 671 K rows pass through PG's tuple interface — framework overhead.

---

## Summary table

| Q | CH (s) | deltax (s) | Ratio | Dominant cost | Key improvement |
|---|--------|-----------|-------|---------------|-----------------|
| 0 | 0.001 | 0.003 | 3.0× | framework | — |
| 1 | 0.006 | 0.023 | 3.8× | framework | — |
| 2 | 0.021 | 0.028 | 1.3× | metadata I/O | — |
| 3 | 0.027 | 0.025 | **0.93×** | — | — |
| 4 | 0.353 | 3.022 | 8.6× | hash-distinct | HLL sketches |
| 5 | 0.623 | 1.848 | 3.0× | detoast | **dict sidecar** |
| 6 | 0.010 | 0.014 | 1.4× | metadata I/O | — |
| 7 | 0.009 | 0.091 | 10.1× | detoast | — |
| 8 | 0.452 | 1.012 | 2.2× | detoast + agg | HLL |
| 9 | 0.522 | 1.499 | 2.9× | detoast + finalize | HLL |
| 10 | 0.147 | 0.462 | 3.1× | detoast | #39 |
| 11 | 0.143 | 0.481 | 3.4× | detoast | #39 |
| 12 | 0.599 | 1.117 | 1.9× | agg | — |
| 13 | 0.804 | 2.143 | 2.7× | detoast + agg | HLL |
| 14 | 0.597 | 1.227 | 2.1× | detoast + agg | #39 |
| 15 | 0.384 | 2.022 | 5.3× | merge | **#36 two-level hash** |
| 16 | 1.709 | 2.045 | 1.2× | agg | — |
| 17 | 0.999 | 1.685 | 1.7× | agg | — |
| 18 | 3.041 | 3.670 | 1.2× | detoast + agg | — |
| 19 | 0.003 | 0.042 | 14× | bloom scan | partition bloom |
| 20 | 0.312 | 6.798 | **21.8×** | detoast URL (LZ4) | open problem |
| 21 | 0.098 | 1.953 | 19.9× | detoast URL | — |
| 22 | 0.717 | 3.719 | 5.2× | detoast URL+Title | #40 for Title |
| 23 | 0.393 | 0.477 | 1.2× | decompress | — |
| 24 | 0.147 | 0.110 | **0.75×** | — | — |
| 25 | 0.192 | 1.925 | 10× | decompress text | **dict sidecar** |
| 26 | 0.149 | 0.109 | **0.73×** | — | — |
| 27 | 0.083 | 0.550 | 6.6× | detoast (sidecar) | — |
| 28 | 9.582 | 6.748 | **0.70×** | regex | — |
| 29 | 0.029 | 0.045 | 1.6× | metadata I/O | — |
| 30 | 0.342 | 1.055 | 3.1× | detoast + agg | — |
| 31 | 0.562 | 1.751 | 3.1× | detoast + agg | **#36 two-level hash** |
| 32 | 3.793 | 9.530 | 2.5× | **merge** | **#36 two-level hash** |
| 33 | 2.782 | 2.680 | **0.96×** | agg | — |
| 34 | 2.851 | 2.689 | **0.94×** | agg | — |
| 35 | 0.297 | 1.722 | 5.8× | merge | **#36 two-level hash** |
| 36 | 0.043 | 0.101 | 2.3× | agg | — |
| 37 | 0.021 | 0.041 | 2.0× | agg | — |
| 38 | 0.017 | 0.079 | 4.6× | heap_scan + agg | — |
| 39 | 0.077 | 0.214 | 2.8× | agg + merge | — |
| 40 | 0.013 | 0.136 | 10× | detoast + decompress | column-pruning audit |
| 41 | 0.009 | 0.052 | 5.8× | agg + detoast | — |
| 42 | 0.008 | 0.041 | 5.1× | framework | — |

**Queries faster than CH:** Q3, Q24, Q26, Q28, Q33, Q34 (6 queries, +Q28)
**Within 2× of CH:** Q0, Q1, Q2, Q6, Q12, Q14 (near), Q16, Q17, Q18, Q23, Q29 (11)
**2–5× of CH:** Q5, Q8, Q9, Q10, Q11, Q13, Q14, Q30, Q31, Q32, Q36, Q37 (12)
**5–10× of CH:** Q4, Q15, Q22, Q25, Q35, Q38, Q39, Q41, Q42 (9)
**>10× of CH:** Q7, Q19, Q20, Q21, Q40 (5)

---

## Prioritized improvement list

Ranked by estimated combined wallclock benefit across the benchmark.
All entries cross-referenced against PERF_IMPROVEMENTS.md.

| # | Improvement | Queries helped | Est. benefit | Complexity | In PERF_IMPROVEMENTS.md? |
|---|-------------|----------------|--------------|------------|--------------------------|
| **1** | **#36 Two-level hash aggregation** | Q15, Q31, Q32, Q35 | ~7 s | Med-High | Yes — planned, not done |
| **2** | **Dict sidecar blob** | Q5, Q25 (+Q22 partial) | ~3.5 s | Medium | **No — new idea** |
| **3** | **HLL sketches for COUNT(DISTINCT)** | Q4, Q8, Q9, Q13 | ~5–6 s | Medium | No — proposed here previously |
| **4** | **#40 Dict-accelerated LIKE** | Q22 (Title) | ~1.5 s | Medium | Yes — planned, not done |
| **5** | **Partition-level bloom for point lookups** | Q19 | ~30 ms | Low-Med | No — proposed here previously |
| **6** | **Q40 column-pruning audit** | Q40 | ~60 ms | Investigation | — |

### Recommended order of attack

1. **Dict sidecar blob (new).** Biggest per-query gains on Q5 (1.85 s →
   ~200 ms) and Q25 (1.93 s → ~150 ms); also the cheapest to prototype
   (reuse the length-sidecar plumbing from #42 in compress.rs +
   segments.rs). Compress-time cost: a second LZ4 blob per dict column
   carrying just the dict bytes.

2. **#36 two-level hash aggregation.** Largest single improvement (~5 s
   on Q32 alone). Already spec'd in PERF_IMPROVEMENTS.md. The current
   #41 is *receive-side* partitioning (threads filter every worker's
   full map by hash modulo at merge time). #36 changes phase-1 so each
   worker writes directly into 256 hash-byte-indexed sub-tables,
   eliminating the O(entries × n_partitions) rescan.

3. **HLL sketches (new).** Adds a compress-time sidecar (~16 KB per
   segment per HLL column). Best wins on Q4 (no GROUP BY) where
   per-segment sketches merge trivially. Per-group sketches (Q8, Q13)
   are more invasive.

4. **#40 dict-accelerated LIKE** for Title in Q22. Already spec'd in
   PERF_IMPROVEMENTS.md.

### Ideas genuinely new vs PERF_IMPROVEMENTS.md

- **Dict sidecar blob** for dict-encoded text columns: store each
  segment's dict as a separate short TOAST column, letting dict-only
  fast paths (COUNT DISTINCT, ORDER BY LIMIT, dict-accelerated LIKE
  pre-check) skip detoasting the main blob entirely. Reuses the
  length-sidecar infrastructure (#42).
- **HLL sketches for COUNT(DISTINCT)** — proposed in previous
  QUERY_ANALYSIS iteration but not carried into PERF_IMPROVEMENTS.md.
- **Partition-level bloom filter** for point lookups (Q19) — same.

### Previously-new ideas that turn out to duplicate PERF_IMPROVEMENTS.md

- The "send-side partitioned hash aggregation" I sketched above is
  **already #36 two-level hash aggregation**. Both describe per-worker
  multiple hashmaps during accumulation with parallel bucket-wise
  merge. Treating it as the same item.
- The "Q27 metadata-only path" I sketched doesn't work: Q27 has
  `GROUP BY CounterID`, so per-segment `SUM(length)` + `nonnull_count`
  can't collapse without per-(CounterID, segment) stats. Removed.
