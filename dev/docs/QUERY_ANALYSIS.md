# ClickBench Query-by-Query Analysis

Investigation of all 43 ClickBench queries on the full 100M row dataset
(c6a.4xlarge EC2, PostgreSQL 18, pg_deltax). Each section records the
EXPLAIN ANALYZE output, what dominates execution, and improvement
ideas.

> Environment: 18 partitions, ~3338 compressed segments total,
> `pg_deltax.parallel_workers=0` (auto, capped at 16), `max_parallel_workers=8`.
> Meta table split into narrow meta + wide stats table (commits 7d2643b, 9bff354).
> Local meta table cache (fd8ce56). SPI planning cache (172cf6e, 8810ce6).

## What changed since the last analysis

F1 (planner SPI cache) and F2 (meta table split + local cache) have
been implemented:

| Change | Before | After | Effect |
|--------|--------|-------|--------|
| **F1: Planning cache** | ~30 ms on every DeltaXAgg query | 3–5 ms | ~25 ms saved per query |
| **F2: Meta table split + cache** | heap_scan 17–68 ms on metadata queries | 0–15 ms (hot) | Metadata-only queries 3–5× faster |

Impact on key metadata queries (bench best-of-3, seconds):

| Query | Before | After | Speedup |
|-------|--------|-------|---------|
| Q0 COUNT(*) | 0.022 | 0.003 | 7.3× |
| Q1 COUNT WHERE | 0.088 | 0.019 | 4.6× |
| Q2 SUM/AVG | 0.079 | 0.023 | 3.4× |
| Q3 AVG(UserID) | 0.077 | 0.020 | 3.9× |
| Q6 MIN/MAX | 0.061 | 0.013 | 4.7× |
| Q29 Wide SUM 89 | 0.151 | 0.041 | 3.7× |

Planning time improvements on filtered aggregate queries:

| Query | Planning before | Planning after |
|-------|-----------------|----------------|
| Q7 | 33 ms | 8.8 ms |
| Q36 | 33 ms | 5.4 ms |
| Q37 | 33 ms | 4.9 ms |
| Q41 | 33 ms | 5.0 ms |

## Top-level findings (actionable)

With F1 and F2 done, three cross-cutting bottlenecks remain:

### F3. Detoast is the dominant cost on most DeltaXAgg queries

**Impact: 25+ queries, ~50 s cumulative wallclock across the benchmark.**

Detoast time (loading compressed column blobs from TOAST storage) is
now the single biggest cost since planning and metadata overhead have
been addressed. Cold-run numbers from EXPLAIN ANALYZE:

| Query | detoast (ms) | % of DeltaX time | Total DeltaX (ms) |
|-------|-------------|-------------------|-------------------|
| Q20 | 11,053 | 66% | 16,714 |
| Q22 | 7,888 | 86% | 9,218 |
| Q28 | 7,061 | 49% | 14,405 |
| Q32 | 6,048 | 48% | 12,713 |
| Q31 | 4,882 | 81% | 6,012 |
| Q18 | 3,683 | 62% | 5,909 |
| Q9 | 1,734 | 79% | 2,204 |
| Q8 | 1,561 | 53% | 2,939 |
| Q21 | 1,338 | 69% | 1,934 |
| Q30 | 1,258 | 61% | 2,066 |
| Q33 | 1,063 | 42% | 2,509 |
| Q34 | 1,048 | 42% | 2,482 |
| Q27 | 1,039 | 57% | 1,810 |
| Q14 | 832 | 51% | 1,618 |
| Q10 | 660 | 85% | 776 |
| Q11 | 634 | 79% | 799 |
| Q15 | 582 | 29% | 1,982 |
| Q13 | 557 | 12% | 4,745 |
| Q16 | 527 | 29% | 1,805 |
| Q17 | 519 | 35% | 1,490 |
| Q35 | 502 | 30% | 1,691 |
| Q12 | 393 | 37% | 1,073 |

This is **#39 Pipelined detoast + parallel aggregation** in
PERF_IMPROVEMENTS.md — still the single biggest win.

**Additional angle:** Some queries detoast columns they don't need.
For instance Q32 (WatchID + ClientIP) detoasts for 6 s despite
WatchID being bitpacked int8 (decompress=18 ms). The detoast is
loading IsRefresh, ResolutionWidth, and other agg columns. Verify
`needed_cols` is tight.

### F4. Merge phase dominates high-cardinality GROUP BY / COUNT DISTINCT

**Impact: Q8, Q13, Q15, Q28, Q32, Q35, Q39 — ~14 s cumulative.**

| Query | merge (ms) | pre_topn_groups | Total DeltaX (ms) |
|-------|-----------|-----------------|-------------------|
| Q32 | 5,739 | 99,997,494 | 12,713 |
| Q13 | 2,956 | 3,890,822 | 4,745 |
| Q28 | 1,538 | — | 14,405 |
| Q8 | 1,336 | 9,040 | 2,939 |
| Q15 | 1,057 | 21,981,283 | 1,982 |
| Q35 | 817 | 20,961,910 | 1,691 |
| Q39 | 174 | 426,328 | 517 |

This is **#36 Two-level hash aggregation** in PERF_IMPROVEMENTS.md.

### F5. Q20 (URL LIKE '%google%') is the worst query: 25× slower than CH

**Impact: Q20 alone accounts for ~8 s of the benchmark gap.**

Q20 is uniquely bad because URL is LZ4-compressed (not dictionary):
- 1709 segments survive dict pruning (51% of total)
- Each surviving segment's URL blob must be fully detoasted and
  decompressed just to check LIKE
- Only 15,911 rows match (0.016% of scanned data)
- detoast=11,053 ms + decompress=3,775 ms = 14.8 s of the 16.7 s

**#40 dict-accelerated LIKE** doesn't help here (URL is LZ4).
**#33 trigram bloom filters** was investigated but doesn't work —
common search terms like 'google' produce trigrams that are present
in virtually every segment, so the bloom filter prunes nothing.

This leaves Q20 as an open problem. Possible approaches:
- **#39 pipelined detoast** to at least overlap the I/O with work
- **Parallel LIKE scan** — split surviving segments across workers
  for the LIKE check itself, rather than detoasting serially
- **Inverted index** — full-text or ngram index on URL, but this
  is a much heavier solution

Q21, Q22 remain bottlenecked by URL/Title LIKE filtering on LZ4 columns.

---

## Query-by-query details

Format per query:
- **Query** (from queries.sql)
- **CH / deltax / ratio** (CH = ClickHouse c6a.4xlarge reference, deltax = bench best-of-3 hot run)
- **EXPLAIN ANALYZE timing breakdown** (cold single run)
- **Analysis + potential improvements**

### Q0 — COUNT(*)

```
SELECT COUNT(*) FROM hits;
```

- CH 0.001 s / deltax **0.003 s** / **3.0×**
- `DeltaXCount`, metadata=0.001 ms, heap_scan=0.000 ms, planning=2.9 ms.
- **Near-optimal.** Meta cache returns instantly. The 3 ms is
  PostgreSQL framework overhead (planning + custom scan init).
  Nothing actionable.

### Q1 — COUNT(*) WHERE AdvEngineID <> 0

```
SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0;
```

- CH 0.006 s / deltax **0.019 s** / **3.2×**
- `DeltaXAgg`, metadata=1.7, heap_scan=39.0 (cold), rows_processed=0
  (metadata-resolved via nonzero count stats).
- **Cold EXPLAIN shows 39 ms heap_scan, but hot bench is 19 ms.**
  The stat table still requires I/O on first access. With warmed
  cache this is fine. Remaining gap to CH is framework overhead.

### Q2 — SUM/AVG full-scan

```
SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits;
```

- CH 0.021 s / deltax **0.023 s** / **1.1×**
- `DeltaXAgg`, metadata=2.3, heap_scan=43.6 (cold),
  segments_metadata_resolved=3338, segments_decompressed=0.
- **At parity with ClickHouse.** Fully metadata-resolved. No action needed.

### Q3 — AVG(UserID)

```
SELECT AVG(UserID) FROM hits;
```

- CH 0.027 s / deltax **0.020 s** / **0.74× (faster)**
- Same profile as Q2. **Already faster than ClickHouse.**

### Q4 — COUNT(DISTINCT UserID)

```
SELECT COUNT(DISTINCT UserID) FROM hits;
```

- CH 0.353 s / deltax **3.359 s** / **9.5×**
- heap_scan=1669 ms (loading UserID blobs), agg=555 ms.
- Dominant: heap_scan loading all 3338 segments' UserID column blobs
  to build a global distinct set. 100 M UserIDs.
- **Improvements:**
  - **Per-segment HLL sketches.** Store a HyperLogLog sketch (16 KB)
    per segment at compression time. `COUNT(DISTINCT)` without GROUP BY
    merges sketches in O(segments) instead of hashing 100 M values.
    3338 segments × 16 KB = 52 MB metadata, but this is an opt-in
    feature for high-value columns.
  - Alternative: exact distinct via sorted run merge, avoiding the
    hash table entirely.

### Q5 — COUNT(DISTINCT SearchPhrase)

```
SELECT COUNT(DISTINCT SearchPhrase) FROM hits;
```

- CH 0.623 s / deltax **2.014 s** / **3.2×**
- heap_scan=2719 ms, agg=396 ms.
- SearchPhrase is dictionary-encoded. Each segment's dictionary
  contains its distinct values already.
- **Improvement: dict-only COUNT(DISTINCT).** Load only the dict
  portion of each blob (tiny), union dict entries across segments.
  3338 segments × ~500 dict entries = ~1.7 M strings to dedupe
  vs 100 M row values. Expected: 2.0 s → ~200 ms.

### Q6 — MIN/MAX EventDate

```
SELECT MIN(EventDate), MAX(EventDate) FROM hits;
```

- CH 0.010 s / deltax **0.013 s** / **1.3×**
- `DeltaXMinMax`, metadata=21.1, heap_scan=51.8, planning=42.4 ms.
- **Cold EXPLAIN shows 42 ms planning — anomalously high.** Hot bench
  is 13 ms. The DeltaXMinMax planner path may be doing extra catalog
  lookups not covered by the SPI cache, or this is cold-catalog.
  No action needed — hot performance is near CH.

### Q7 — GROUP BY AdvEngineID

```
SELECT AdvEngineID, COUNT(*) FROM hits WHERE AdvEngineID <> 0 GROUP BY AdvEngineID ORDER BY COUNT(*) DESC;
```

- CH 0.009 s / deltax **0.091 s** / **10.1×**
- `DeltaXAgg`, heap_scan=138 [detoast=161], decompress=3.5, agg=28.5,
  planning=8.8 ms. segments=2262, rows_processed=630,500.
- Dominant: detoast loading column blobs (F3).
- **Hot bench is 91 ms.** With pipelined detoast (#39) this could
  drop to ~30 ms. The agg itself (28 ms for 630 K rows, 18 groups)
  is efficient.
- Still 10× CH because CH resolves this from metadata (AdvEngineID
  has only 18 values).

### Q8 — GROUP BY RegionID COUNT(DISTINCT UserID)

```
SELECT RegionID, COUNT(DISTINCT UserID) AS u FROM hits GROUP BY RegionID ORDER BY u DESC LIMIT 10;
```

- CH 0.452 s / deltax **2.074 s** / **4.6×**
- detoast=1561, decompress=8, agg=61, **merge=1336**, finalize=1.9.
- Dominant: merge phase for per-group UserID distinct sets
  (9040 groups × ~11 K distinct UserIDs each).
- **Improvements:** F4 (#36 two-level hash agg). Also HLL sketches
  per group would reduce merge to O(groups × sketch_size).

### Q9 — RegionID multi-agg

```
SELECT RegionID, SUM(AdvEngineID), COUNT(*) AS c, AVG(ResolutionWidth), COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID ORDER BY c DESC LIMIT 10;
```

- CH 0.522 s / deltax **1.488 s** / **2.9×**
- detoast=1734, decompress=18, agg=68, finalize=312, topn_select=109.
- Dominant: detoast (79%). F3.
- finalize=312 ms is COUNT(DISTINCT UserID) finalization — F4 applies.

### Q10 — MobilePhoneModel users

```
SELECT MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhoneModel ORDER BY u DESC LIMIT 10;
```

- CH 0.147 s / deltax **0.458 s** / **3.1×**
- detoast=660, decompress=42, agg=63, merge=26.
- Dominant: detoast (85%). F3.

### Q11 — MobilePhone + Model users

```
SELECT MobilePhone, MobilePhoneModel, COUNT(DISTINCT UserID) AS u FROM hits WHERE MobilePhoneModel <> '' GROUP BY MobilePhone, MobilePhoneModel ORDER BY u DESC LIMIT 10;
```

- CH 0.143 s / deltax **0.489 s** / **3.4×**
- detoast=634, decompress=86, agg=68, merge=27.
- Dominant: detoast (79%). F3.

### Q12 — Top 10 SearchPhrase

```
SELECT SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.599 s / deltax **1.143 s** / **1.9×**
- detoast=393, decompress=132, agg=550.
- Dominant: agg (51%) on 55 M rows with 4.8 M groups.
- agg at 10 ns/row — near the limit for dictionary text hashing.
  Modest improvement with F3 (detoast) and #36 (two-level hash).

### Q13 — SearchPhrase users (COUNT DISTINCT)

```
SELECT SearchPhrase, COUNT(DISTINCT UserID) AS u FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY u DESC LIMIT 10;
```

- CH 0.804 s / deltax **4.960 s** / **6.2×**
- detoast=557, decompress=176, agg=777, **merge=2956**, finalize=301,
  topn_select=301.
- Dominant: merge phase (62%). 3.89 M groups × distinct UserIDs.
- **Improvements:** F4 (#36). HLL sketches per group.

### Q14 — SearchEngine + SearchPhrase

```
SELECT SearchEngineID, SearchPhrase, COUNT(*) AS c FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.597 s / deltax **1.231 s** / **2.1×**
- detoast=832, decompress=163, agg=652.
- Similar to Q12. F3 for detoast.

### Q15 — Top 10 UserID

```
SELECT UserID, COUNT(*) FROM hits GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10;
```

- CH 0.384 s / deltax **2.040 s** / **5.3×**
- detoast=582, decompress=4, agg=317, **merge=1057**.
- Dominant: merge (53%). 22 M groups merged across workers.
- **Improvements:** F4 / #36 two-level hash.

### Q16 — UserID + SearchPhrase top

```
SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10;
```

- CH 1.709 s / deltax **2.000 s** / **1.2×**
- detoast=527, decompress=188, agg=1097.
- Competitive. Modest improvement with F3.

### Q17 — UserID + SearchPhrase (no order)

```
SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase LIMIT 10;
```

- CH 0.999 s / deltax **1.684 s** / **1.7×**
- detoast=519, decompress=181, agg=812.
- Competitive. F3.

### Q18 — UserID + extract(minute) + SearchPhrase

```
SELECT UserID, extract(minute FROM EventTime) AS m, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, m, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10;
```

- CH 3.041 s / deltax **3.699 s** / **1.2×**
- detoast=3683, decompress=323, agg=2081.
- Competitive. detoast=3.7 s is high — three columns × all segments.
  F3 would help. agg=2.1 s for 34 M groups needs #36.

### Q19 — Point lookup UserID = const

```
SELECT UserID FROM hits WHERE UserID = 435090932899640449;
```

- CH 0.003 s / deltax **0.042 s** / **14×**
- `DeltaXAppend`. segments=48, segments_skipped=3290
  (1851 minmax + 1439 bloom).
- Cold EXPLAIN: heap_scan=834 ms (loading all metadata for bloom
  filter checking). Hot bench: 42 ms.
- **With the meta cache, this is reasonable (42 ms).** The remaining
  gap to CH (3 ms) is from:
  1. Checking 3338 bloom filters even cached (~20 ms)
  2. Decompressing 48 surviving segments (~12 ms)
  3. Framework overhead
- **Improvement:** Partition-level bloom filter. Check 18 partition
  blooms first, then only load segment blooms for partitions that
  pass. For point lookups with time locality, this eliminates most
  bloom checks. Expected: 42 ms → ~10 ms.

### Q20 — COUNT(*) WHERE URL LIKE '%google%'

```
SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%';
```

- CH 0.312 s / deltax **7.822 s** / **25×**
- `DeltaXAgg`, segments=1709 (after dict pruning).
  rows_processed=15,911 (only 0.016% match!).
- Cold: heap_scan=11096 [detoast=11053], decompress=3775, agg=1840.
- **The worst query in the benchmark.** URL is LZ4-compressed (not
  dictionary), so dict-accelerated LIKE (#40) doesn't apply.
- **#33 trigram bloom filters were tried but don't work** — common
  trigrams like 'goo','oog','ogl','gle' are present in virtually
  every segment, so bloom filters prune nothing.
- **#39 pipelined detoast** would overlap the serial TOAST I/O with
  parallel work, reducing wall time even without pruning.
- **Open problem** — no good segment-pruning approach exists for
  common LIKE patterns on LZ4 columns.

### Q21 — SearchPhrase MIN(URL) WHERE URL LIKE '%google%'

```
SELECT SearchPhrase, MIN(URL), COUNT(*) AS c FROM hits WHERE URL LIKE '%google%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.098 s / deltax **1.966 s** / **20×**
- detoast=1338, decompress=337, agg=333.
- segments=2419 — more than Q20 because SearchPhrase <> '' filter
  is less selective than URL LIKE filter for pruning.
- Dominant: detoast URL blobs.
- #33 trigram bloom was tried but doesn't work (common trigrams).
  #39 pipelined detoast is the main lever here.

### Q22 — Title LIKE Google + URL NOT LIKE

```
SELECT SearchPhrase, MIN(URL), MIN(Title), COUNT(*) AS c, COUNT(DISTINCT UserID) FROM hits WHERE Title LIKE '%Google%' AND URL NOT LIKE '%.google.%' AND SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY c DESC LIMIT 10;
```

- CH 0.717 s / deltax **3.766 s** / **5.3×**
- detoast=7888, decompress=655, agg=1221.
- segments=2931. Detoast dominates (86%).
- #33 trigram bloom doesn't work (common trigrams). Title is
  dict-encoded so #40 helps for the Title LIKE filter. URL is LZ4 —
  only #39 pipelined detoast helps.

### Q23 — SELECT * WHERE URL LIKE ... ORDER BY EventTime LIMIT 10

```
SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10;
```

- CH 0.393 s / deltax **0.468 s** / **1.2×**
- `DeltaXAppend` TopN, 12 surviving segments, 36 candidates.
- heap_scan=1301 (cold), decompress=240.
- **Competitive.** TopN + time-ordered scan works well here.
  Cold heap_scan is from loading metadata; hot bench is 468 ms.

### Q24 — SearchPhrase ORDER BY EventTime LIMIT 10

```
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime LIMIT 10;
```

- CH 0.147 s / deltax **0.103 s** / **0.70× (faster)**
- `DeltaXAppend` TopN, 21 segments, 97 K candidates.
- **Already faster than ClickHouse.**

### Q25 — ORDER BY SearchPhrase LIMIT 10

```
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY SearchPhrase LIMIT 10;
```

- CH 0.192 s / deltax **1.933 s** / **10×**
- `DeltaXAppend`, 3332 segments, decompress=2336 ms.
- Dominant: decompressing SearchPhrase across all segments for
  lexicographic sort. 160 K topn_candidates.
- **Improvement: dict-only scan for ORDER BY text LIMIT N.**
  SearchPhrase is dictionary-encoded. For each segment, load only
  the dict header (~1 KB), find the lexicographically smallest
  entries. Merge across segments. Skip the main LZ4 block entirely.
  3332 segments × ~500 dict entries = 1.66 M items to min-heap.
  Expected: 1.9 s → ~100 ms.

### Q26 — ORDER BY EventTime, SearchPhrase LIMIT 10

```
SELECT SearchPhrase FROM hits WHERE SearchPhrase <> '' ORDER BY EventTime, SearchPhrase LIMIT 10;
```

- CH 0.149 s / deltax **0.103 s** / **0.69× (faster)**
- Already faster. Time-ordered pathkey + TopN.

### Q27 — CounterID AVG(length(URL)) HAVING c > 100K

```
SELECT CounterID, AVG(length(URL)) AS l, COUNT(*) AS c FROM hits WHERE URL <> '' GROUP BY CounterID HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25;
```

- CH 0.083 s / deltax **1.815 s** / **21.9×**
- detoast=1039, decompress=275, agg=548. 99.97 M rows, 60 result groups.
- Dominant: detoast loading URL blobs to compute `length()`.
- **Improvement: per-segment `SUM(length(text_col))` metadata.**
  Store `_sum_length_<col>` and `_nonnull_count_<col>` at compression
  time. Then `AVG(length(col))` with GROUP BY on a low-cardinality
  column (CounterID has 898 groups, 60 after HAVING) becomes
  metadata-resolvable. Q27: 1.8 s → ~50 ms.
  **New idea not in PERF_IMPROVEMENTS.md.**

### Q28 — Referer REGEXP_REPLACE GROUP BY

```
SELECT REGEXP_REPLACE(Referer, ...) AS k, AVG(length(Referer)) AS l, COUNT(*) AS c, MIN(Referer) FROM hits WHERE Referer <> '' GROUP BY k HAVING COUNT(*) > 100000 ORDER BY l DESC LIMIT 25;
```

- CH 9.582 s / deltax **9.585 s** / **1.0×**
- detoast=7061, decompress=3548, agg=1323, merge=1538, finalize=1426.
- **Tied with ClickHouse.** Not a priority. The regex evaluation is
  inherently expensive.

### Q29 — Wide SUM 89 cols

```
SELECT SUM(ResolutionWidth), SUM(ResolutionWidth + 1), ... SUM(ResolutionWidth + 89) FROM hits;
```

- CH 0.029 s / deltax **0.041 s** / **1.4×**
- heap_scan=15.5 ms (cold), metadata-resolved, 0 segments decompressed.
- **Near parity.** The remaining gap is framework overhead reading
  the stats table. No action needed.

### Q30 — SearchEngine + ClientIP multi-agg

```
SELECT SearchEngineID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY SearchEngineID, ClientIP ORDER BY c DESC LIMIT 10;
```

- CH 0.342 s / deltax **1.548 s** / **4.5×**
- detoast=1258, decompress=222, agg=627, topn_select=14.
- Dominant: detoast (61%). F3.

### Q31 — WatchID + ClientIP with SearchPhrase filter

```
SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits WHERE SearchPhrase <> '' GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10;
```

- CH 0.562 s / deltax **2.253 s** / **4.0×**
- detoast=4882, decompress=287, agg=1074.
- Dominant: detoast (81%). F3.
- **Note:** detoast=4882 ms for 55 M rows is much higher than Q30's
  1258 ms for the same row count. Difference is WatchID (int8, larger
  blobs) vs SearchEngineID (small int). F3 pipelined detoast would
  help both.

### Q32 — WatchID + ClientIP all

```
SELECT WatchID, ClientIP, COUNT(*) AS c, SUM(IsRefresh), AVG(ResolutionWidth) FROM hits GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10;
```

- CH 3.793 s / deltax **9.527 s** / **2.5×**
- detoast=6048, decompress=18, agg=920, **merge=5739**.
- Dominant: merge (45%). 100 M essentially-unique groups.
- **Improvement:** #36 two-level hash agg. Partition merges into
  256 buckets for parallel lock-free merging. Expected: 9.5 s → ~3 s.

### Q33 — GROUP BY URL ORDER BY c DESC LIMIT 10

```
SELECT URL, COUNT(*) AS c FROM hits GROUP BY URL ORDER BY c DESC LIMIT 10;
```

- CH 2.782 s / deltax **2.669 s** / **0.96× (tied/faster)**
- detoast=1063, decompress=228, agg=1260.
- **At parity.** Nothing to do.

### Q34 — GROUP BY 1, URL

```
SELECT 1, URL, COUNT(*) AS c FROM hits GROUP BY 1, URL ORDER BY c DESC LIMIT 10;
```

- CH 2.851 s / deltax **2.650 s** / **0.93× (faster)**
- Same as Q33. **Already faster.**

### Q35 — GROUP BY ClientIP, IP−1, IP−2, IP−3

```
SELECT ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3, COUNT(*) AS c FROM hits GROUP BY ClientIP, ClientIP - 1, ClientIP - 2, ClientIP - 3 ORDER BY c DESC LIMIT 10;
```

- CH 0.297 s / deltax **1.722 s** / **5.8×**
- detoast=502, decompress=3, agg=344, **merge=817**.
- merge dominates (48%). 21 M distinct ClientIPs.
- **Improvement:** #36 two-level hash. Expected: 1.7 s → ~500 ms.

### Q36 — Top URLs for CounterID=62

```
SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 AND IsRefresh = 0 AND URL <> '' GROUP BY URL ORDER BY PageViews DESC LIMIT 10;
```

- CH 0.043 s / deltax **0.102 s** / **2.4×**
- segments=26 (min/max pruning), rows_processed=671 K.
- detoast=40, decompress=13, agg=69, planning=5.4.
- heap_scan=80 ms (cold), with bloom checks. Hot bench = 102 ms.
- **Improved from 2.8× to 2.4× thanks to F1.** Remaining gap is
  agg=69 ms for 672 K rows with 314 K groups (URL) — inherent.

### Q37 — Top Titles for CounterID=62

```
SELECT Title, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND DontCountHits = 0 AND IsRefresh = 0 AND Title <> '' GROUP BY Title ORDER BY PageViews DESC LIMIT 10;
```

- CH 0.021 s / deltax **0.044 s** / **2.1×**
- segments=26, rows_processed=660 K.
- detoast=35, decompress=6, agg=58, planning=4.9.
- **Improved from 3.1× to 2.1×.** Reasonable. agg=58 ms for 54 K
  groups is the remaining cost.

### Q38 — CounterID=62 links OFFSET 1000

```
SELECT URL, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND ... AND IsLink <> 0 AND IsDownload = 0 GROUP BY URL ORDER BY PageViews DESC LIMIT 10 OFFSET 1000;
```

- CH 0.017 s / deltax **0.086 s** / **5.1×**
- segments=26, rows_processed=47,740, 13,299 groups.
- detoast=31, decompress=26, agg=33, planning=4.9.
- heap_scan=59 ms (cold) with bloom reads. Hot bench = 86 ms.
- **Improvement:** The heap_scan/bloom overhead (~59 ms cold) is a
  significant fraction. With warmer caches it's better. The OFFSET
  1000 forces materializing 1010 rows, adding overhead.

### Q39 — CounterID=62 traffic src

```
SELECT TraficSourceID, SearchEngineID, AdvEngineID, CASE WHEN ... THEN Referer ELSE '' END AS Src, URL AS Dst, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND ... GROUP BY ... ORDER BY PageViews DESC LIMIT 10 OFFSET 1000;
```

- CH 0.077 s / deltax **0.380 s** / **4.9×**
- segments=26, rows_processed=722,688, 426,328 groups.
- detoast=198, decompress=22, agg=276, merge=174, finalize=28.
- merge=174 ms and agg=276 ms are the main costs. CASE expression
  pushdown is active.
- **Improvement:** #36 two-level hash for the merge. Also, 426 K
  groups for 722 K rows means very high cardinality — the
  URL column (Dst) creates near-unique groups.

### Q40 — CounterID=62 URLHash

```
SELECT URLHash, EventDate, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND ... AND TraficSourceID IN (-1, 6) AND RefererHash = 3594120000172545465 GROUP BY URLHash, EventDate ORDER BY PageViews DESC LIMIT 10 OFFSET 100;
```

- CH 0.013 s / deltax **0.130 s** / **10×**
- segments=26, rows_processed=89,914, 41,194 groups.
- detoast=137, decompress=71, agg=23, planning=6.3.
- heap_scan=96 ms with meta hit=639 read=120, bloom hit=170 read=86.
- **decompress=71 ms for 26 segments / 90 K rows is anomalously
  high.** 6 batch_quals means 6+ columns are decompressed for
  filtering (CounterID, EventDate, IsRefresh, TraficSourceID,
  RefererHash, URLHash). Worth investigating whether column pruning
  is tight — some filter columns may be decompressed unnecessarily
  when metadata could prune them.
- **Also:** 86 bloom reads is high for 26 segments. RefererHash
  bloom filter checking is reading from disk for most segments.
  With warm cache this drops.

### Q41 — CounterID=62 window dim

```
SELECT WindowClientWidth, WindowClientHeight, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND ... AND URLHash = 2868770270353813622 GROUP BY WindowClientWidth, WindowClientHeight ORDER BY PageViews DESC LIMIT 10 OFFSET 10000;
```

- CH 0.009 s / deltax **0.051 s** / **5.7×**
- segments=26, rows_processed=102,676, 10,948 result rows.
- detoast=54, decompress=12, agg=56, merge=1, planning=5.0.
- Falls back to PG Sort because result_rows=10,948 (> OFFSET 10000).
- **Reasonable.** The 51 ms is mostly agg + detoast overhead.
  F1 already helped (was 68 ms → 51 ms).

### Q42 — CounterID=62 by minute

```
SELECT DATE_TRUNC('minute', EventTime) AS M, COUNT(*) AS PageViews FROM hits WHERE CounterID = 62 AND EventDate >= '2013-07-14' AND EventDate <= '2013-07-15' AND IsRefresh = 0 AND DontCountHits = 0 GROUP BY DATE_TRUNC('minute', EventTime) ORDER BY DATE_TRUNC('minute', EventTime) LIMIT 10 OFFSET 1000;
```

- CH 0.008 s / deltax **0.041 s** / **5.1×**
- segments=26, rows_processed=671,519, 1,440 result rows.
- detoast=8.5, decompress=9.3, agg=11.7, planning=3.8.
- All phases small. PG Sort adds 0.4 ms.
- **Remaining gap is framework overhead.** Each of the 671 K rows
  passes through the custom scan → PG executor interface.

---

## Summary table

| Q | CH (s) | deltax (s) | Ratio | Dominant cost | Key improvement |
|---|--------|-----------|-------|---------------|-----------------|
| 0 | 0.001 | 0.003 | 3.0× | framework | — |
| 1 | 0.006 | 0.019 | 3.2× | heap_scan (cold) | — |
| 2 | 0.021 | 0.023 | 1.1× | metadata I/O | — |
| 3 | 0.027 | 0.020 | **0.74×** | — | — |
| 4 | 0.353 | 3.359 | 9.5× | heap_scan + agg | HLL sketches |
| 5 | 0.623 | 2.014 | 3.2× | heap_scan | dict-only distinct |
| 6 | 0.010 | 0.013 | 1.3× | metadata I/O | — |
| 7 | 0.009 | 0.091 | 10.1× | detoast | #39 pipelined detoast |
| 8 | 0.452 | 2.074 | 4.6× | merge | #36 two-level hash |
| 9 | 0.522 | 1.488 | 2.9× | detoast | #39 |
| 10 | 0.147 | 0.458 | 3.1× | detoast | #39 |
| 11 | 0.143 | 0.489 | 3.4× | detoast | #39 |
| 12 | 0.599 | 1.143 | 1.9× | agg | — |
| 13 | 0.804 | 4.960 | 6.2× | merge | #36 |
| 14 | 0.597 | 1.231 | 2.1× | detoast + agg | #39 |
| 15 | 0.384 | 2.040 | 5.3× | merge | #36 |
| 16 | 1.709 | 2.000 | 1.2× | agg | — |
| 17 | 0.999 | 1.684 | 1.7× | agg | — |
| 18 | 3.041 | 3.699 | 1.2× | detoast + agg | #39 + #36 |
| 19 | 0.003 | 0.042 | 14× | bloom scan | partition bloom |
| 20 | 0.312 | 7.822 | **25×** | detoast URL (LZ4) | #39 (open problem) |
| 21 | 0.098 | 1.966 | 20× | detoast URL | #39 |
| 22 | 0.717 | 3.766 | 5.3× | detoast URL+Title | #39 + #40 |
| 23 | 0.393 | 0.468 | 1.2× | decompress | — |
| 24 | 0.147 | 0.103 | **0.70×** | — | — |
| 25 | 0.192 | 1.933 | 10× | decompress text | dict-only ORDER BY |
| 26 | 0.149 | 0.103 | **0.69×** | — | — |
| 27 | 0.083 | 1.815 | 21.9× | detoast URL | **SUM(length) metadata** |
| 28 | 9.582 | 9.585 | 1.0× | all phases | — |
| 29 | 0.029 | 0.041 | 1.4× | metadata I/O | — |
| 30 | 0.342 | 1.548 | 4.5× | detoast | #39 |
| 31 | 0.562 | 2.253 | 4.0× | detoast | #39 |
| 32 | 3.793 | 9.527 | 2.5× | merge | #36 |
| 33 | 2.782 | 2.669 | **0.96×** | agg | — |
| 34 | 2.851 | 2.650 | **0.93×** | agg | — |
| 35 | 0.297 | 1.722 | 5.8× | merge | #36 |
| 36 | 0.043 | 0.102 | 2.4× | agg | — |
| 37 | 0.021 | 0.044 | 2.1× | agg | — |
| 38 | 0.017 | 0.086 | 5.1× | heap_scan + agg | — |
| 39 | 0.077 | 0.380 | 4.9× | agg + merge | #36 |
| 40 | 0.013 | 0.130 | 10× | detoast + decompress | investigate |
| 41 | 0.009 | 0.051 | 5.7× | agg + detoast | — |
| 42 | 0.008 | 0.041 | 5.1× | framework | — |

**Queries faster than CH:** Q3, Q24, Q26, Q33, Q34 (5 queries)
**Queries within 2× of CH:** Q0, Q1, Q2, Q6, Q12, Q16, Q17, Q18, Q23, Q28, Q29 (11 queries)
**Queries 2–5× of CH:** Q5, Q8, Q9, Q10, Q11, Q14, Q30, Q31, Q32, Q36, Q37 (11 queries)
**Queries 5–10× of CH:** Q4, Q7, Q13, Q15, Q22, Q25, Q35, Q38, Q39, Q40, Q41, Q42 (12 queries)
**Queries >10× of CH:** Q19, Q20, Q21, Q27 (4 queries)

---

## Prioritized improvement list

Sorted by estimated combined wallclock benefit across the benchmark:

| # | Improvement | Queries helped | Est. benefit | Complexity | In PERF_IMPROVEMENTS.md? |
|---|-------------|----------------|-------------|------------|--------------------------|
| **1** | **#39 Pipelined detoast** | Q7–Q18, Q20–Q22, Q27, Q30–Q35 | ~15–20 s | Medium | Yes |
| ~~2~~ | ~~#33 Trigram bloom filters~~ | ~~Q20, Q21, Q22~~ | — | — | **Tried — doesn't work** |
| **3** | **#36 Two-level hash aggregation** | Q8, Q13, Q15, Q32, Q35, Q39 | ~12 s | Med-High | Yes |
| **4** | **Per-segment SUM(length(text_col)) metadata** | Q27 | ~1.7 s | Low | **No — new idea** |
| **5** | **Dict-only scan for ORDER BY text LIMIT** | Q25 | ~1.8 s | Low-Med | **No — new idea** |
| **6** | **Dict-only COUNT(DISTINCT) for dict columns** | Q5 | ~1.8 s | Low-Med | **No — new idea** |
| **7** | **HLL sketches for COUNT(DISTINCT)** | Q4, Q8, Q13 | ~8 s | Medium | **No — new idea** |
| **8** | **#40 Dict-accelerated LIKE** | Q22 (Title is dict) | ~1 s | Medium | Yes |
| **9** | **Partition-level bloom filter** | Q19 | ~30 ms | Low-Med | **No — new idea** |
| **10** | **Q40 decompress investigation** | Q40 | ~60 ms | Investigation | — |

### Recommended order of attack

1. **#4 per-segment `SUM(length(text))` metadata.** Q27 goes from
   21.9× to ~1× CH. Small self-contained change in compress + stats
   table. Low risk, high ratio improvement.

2. **#5 dict-only ORDER BY text LIMIT.** Q25 drops from 10× to ~1× CH.
   Only needs dict header parsing, no full decompression.

3. **#6 dict-only COUNT(DISTINCT).** Q5 drops from 3.2× to ~0.5× CH.
   Union dict entries across segments instead of hashing 100 M values.

4. **#1 #39 pipelined detoast.** Biggest absolute win. Overlaps TOAST
   I/O with decompression and aggregation. Affects 20+ queries.

5. ~~#33 trigram bloom filters~~ — **tried, doesn't work.** Common
   trigrams saturate the bloom; no pruning on realistic patterns.
   Q20/Q21/Q22 remain open problems, helped only by #39.

6. **#3 #36 two-level hash aggregation.** Solves the high-cardinality
   merge bottleneck (Q13, Q15, Q32, Q35).

7. **#7 HLL sketches.** Best-bang-for-buck on Q4/Q8/Q13 triad.
   Can be opt-in per column.

### New ideas proposed (not in PERF_IMPROVEMENTS.md)

- **#4** Per-segment `SUM(length(text_col))` + `_nonnull_count` for metadata fast path on `AVG(length(col))`
- **#5** Dict-only scan for `ORDER BY text_col LIMIT N`
- **#6** Dict-only scan for `COUNT(DISTINCT dict_text_col)` — union dict entries
- **#7** Per-segment HLL sketches for `COUNT(DISTINCT)` metadata fast path
- **#9** Partition-level bloom filter for point lookup acceleration
- **#10** Q40: investigate decompress=71 ms for 26 segments / 90 K rows — check column pruning
