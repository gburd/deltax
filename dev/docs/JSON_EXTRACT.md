# JSON Extract — Status & Improvements Plan

## What's there today

Tier 1 (`pg_deltax.json_extract_mode = 'fields'`) is end-to-end functional:

- Catalog: `json_extract JSONB` column on `deltax_deltatable`. GUC for the rewrite mode (`none` | `fields`; `all` reserved).
- API: `deltax_enable_compression(json_extract => '[{"src":"data","path":[...],"name":"x_kind","type":"text"}, ...]'::jsonb)`.
- COPY-time extraction (`copyparse.rs` Jsonb arm): `serde_json::Value::from_str` per row, descend the path, push into a typed companion column. Missing paths → NULL; never aborts.
- Companion-table layout: extracted columns get the next `_col_idx` slots and reuse the existing compress / minmax / bloom / valbitmap pipeline.
- Plan rewrite: `planner_hook` (`hook::deltax_planner`) wraps `standard_planner`, then runs `json_extract::rewrite_plan_tree` over the final plan to substitute `data->>'kind'`-style chain Exprs with `Var(OUTER_VAR, forwarder_resno)` referencing the synthetic columns. Both `DeltaXDecompress` (per-partition) and `DeltaXAppend` (parallel parent-baserel) are matched.
- Executor: synthetic slot positions are populated from companion blobs alongside physical columns. EXPLAIN annotation lists the configured paths.

JSONBench results (m6i.8xlarge, 100M rows, warm):

| Q | Pre-walker | After ref-count + qual rewrite | Notes |
|---|---|---|---|
| Q0 | 26s | **5.8s** (4.5×) | filter-free GROUP BY; cleanest case |
| Q1 | 354s | 353s (≈ baseline) | hits the COUNT(DISTINCT) elision limit (Functional #4) |
| Q2 | 51s | 26.8s (1.9×) | GROUP BY collection + EXTRACT(time_us) |
| Q3 | 33s | 25.6s (1.3×) | GROUP BY did + MIN(time_us); LIMIT 3 |
| Q4 | 34s | 26.3s (1.3×) | GROUP BY did + MAX(time_us)-MIN(time_us); LIMIT 3 |

Earlier numbers in this doc cited a much steeper speedup (Q1 4.1s, Q2-Q4 ~4.1s). Those came from an interim "unconditional Section::Cols prune" walker that silently dropped raw `data` from needed-cols. For queries with chain-Expr filters at the scan level, that prune broke correctness — the chain evaluated to NULL and all rows were filtered out, producing empty result sets very quickly. The bench harness only captured timings, not row counts, so the regression went undetected. The current ref-count walker returns correct results; the speedups above are real.

## Functional improvements

Listed roughly in priority order. Each item names the target test file. There's no `tests/test_jsonb_extract.py` yet — first items should create it; later items extend it.

### 1. ~~Ref-count walker + scan-qual rewrite~~ — DONE

The walker is now two phases (`json_extract.rs::rewrite_plan_tree`):

- Phase 1 (`rewrite_plan_subtree`) recursively rewrites chain Exprs in upper plans to `Var(OUTER_VAR, k)` refs at the matched synthetic positions. It also calls `rewrite_scan_qual_chains` on the cscan itself to rewrite chain Exprs in the scan-level filter to `Var(INDEX_VAR, k_synth)` — without this, queries like `WHERE data ->> 'kind' = 'commit'` evaluated the chain per-row against raw `data`, which kept `data` in needed-cols and erased the speedup.
- Phase 2 (`prune_cscans_by_ref_count` → `descend_for_refs` → `rebuild_cscan_custom_private`) walks the final plan once more, counts `Var(OUTER_VAR, k)` refs that resolve into our scan's tlist plus `Var(INDEX_VAR, k)` / relation-Var refs in the scan-level qual, and rebuilds `custom_private`'s Section::Cols + Section::Synth from that set.

Tests in `tests/test_jsonb_extract.py`: `test_groupby_kind`, `test_filter_and_group`, `test_cast_to_bigint`, `test_raw_data_and_chain_together` (regression for the prior unconditional-prune bug — that approach silently dropped `data` and broke any query reading both raw `data` and a chain expr), `test_select_star_with_chain`, `test_missing_path_returns_null`. All pass.

**Known limitation surfaced by Q1**: when an upper-level Aggref still contains a chain Expr because intermediate plans (Sort, GatherMerge) elided the synthetic from their tlists, raw `data` flows up through the plan unchanged, the walker correctly sees position 1 of cscan as referenced, and `data` stays in Section::Cols. JSONBench Q1's `COUNT(DISTINCT data->>'did')` is the canonical case — Sort and GatherMerge pass `data` upward but not the `did` synthetic, so the GroupAgg above can't be rewritten. Functional #4 below is the structural fix; for now Q1 stays slow.

### 2. ~~Mixed-partition gate~~ — DONE

`deltax_deltatable.json_extract_added_at TIMESTAMPTZ` is now stamped by `update_deltatable_compression` whenever `json_extract` is (re)set. The walker (`scan::path::is_json_extract_safe_for_rel`) consults `MIN(compressed_at)` over relevant partitions: if any compressed partition predates `json_extract_added_at`, the rewrite is skipped for that cscan and the query falls through to the slow chain-Expr path on every partition. Conservative — a mixed-partition table loses the speedup on its newer partitions too — but correct, and the user can `deltax_compress_partition` over the older ones to lift the gate.

Tests: `TestMixedPartitionGate::test_old_partition_still_returns_correct_results` in `tests/test_jsonb_extract.py`. Setup: enable_compression without json_extract, load+compress partition A, then re-enable_compression with json_extract added, load+compress partition B. Asserts mode='fields' result equals mode='none' AND every row contributed (raw `data->>'kind'` resolves correctly even on partition A).

Follow-up (perf): per-partition gate inside DeltaXAppend, so newer partitions still get the rewrite while only older ones fall back. Requires the executor to track per-partition synthetic availability.

### 3. Walker node-type coverage — partially DONE

The phase-2 ref-counter (`collect_outer_var_attnos`, `collect_index_and_rel_var_attnos_in_list`) now delegates the tree walk to PG's `pull_var_clause` with `PVC_RECURSE_AGGREGATES | PVC_RECURSE_WINDOWFUNCS | PVC_RECURSE_PLACEHOLDERS`. That covers every node type PG itself knows about, including `JsonValueExpr`, `CoalesceExpr`, `MinMaxExpr`, `RowExpr`, `BooleanTest`, `XmlExpr`, etc. — node types our hand-rolled walker would have missed and silently produced wrong refs for.

Tests in `tests/test_jsonb_extract.py`: `test_coalesce_with_chain`, `test_chain_in_case_when`, `test_chain_in_in_clause` (`ScalarArrayOpExpr`).

Still hand-rolled and incomplete: `substitute_in_expr_node` and `substitute_scan_chains_in_node` (the rewrite-side walkers). They mutate node trees in place so can't trivially be replaced with `pull_var_clause`. Coverage today: `OpExpr`, `BoolExpr`, `FuncExpr`, `CoerceViaIO`, `RelabelType`, `NullTest`, `CaseExpr`/`CaseWhen`, `Aggref`, `WindowFunc`, `ScalarArrayOpExpr`. Missing the JSON-related ones (`JsonValueExpr`, `JsonExpr`) plus `CoalesceExpr`/`MinMaxExpr`/`RowExpr`/`NullIfExpr`. The miss is a perf gap (chain Exprs inside those nodes don't get rewritten — fall through to slow path) but not correctness, since the ref-counter now keeps `data` for those quals automatically. Migrate to `expression_tree_mutator` when convenient.

### 4. Inject synthetics through intermediate-plan tlist elision

Triggered by JSONBench Q1's `COUNT(DISTINCT data->>'did')`. The PG planner builds intermediate plan tlists (Sort, GatherMerge) to pass through only what the *next* level needs. When an upper-level Aggref contains a chain Expr but the immediate child's tlist doesn't carry the corresponding synthetic, the walker can't match — the chain stays, raw `data` flows up the tree, and the perf win evaporates.

Two implementation directions:
- **Tlist injection at plan time**: hook into the planner earlier (`set_rel_pathlist_hook` or `create_partial_grouping_paths`) to ensure that any synthetic needed by an upper-level chain Expr is added to all intermediate plan tlists between the cscan and the level where the Aggref lives. This is the most robust but touches more of PG's planning machinery.
- **Aggref-args pre-walk**: scan top-level Aggref/WindowFunc args for chain Exprs first, then push the matching synthetic into the cscan tlist + every intermediate tlist before phase-1 rewrite runs. Less invasive but only handles aggregates (not e.g. lateral subqueries).

SubqueryScan / CTE pass-through is a related but separable issue: subqueries opacify the chain at the boundary. Walker would descend into `SubqueryScan.subplan` and map its tlist 1:1.

Tests:
- Q1-style: `SELECT collection, COUNT(DISTINCT data->>'did') FROM bluesky GROUP BY 1` — assert the cscan's `custom_private` has `did` synthetic in Section::Synth, not raw `data` in Section::Cols. Pair with a timing assertion that the warm run is sub-second on a small dataset.
- CTE: `WITH t AS (SELECT data FROM bluesky) SELECT data->>'kind', count(*) FROM t GROUP BY 1` — assert the rewrite still fires.

### 5. Type coverage

Currently: `text, bigint, integer, double precision, boolean, timestamptz`. Add: `numeric`, `date`, `time`, `jsonb` (extract sub-object so chains can extract from it). `jsonb` in particular unlocks compositional extraction without re-parsing the original row.

Each new type: extend `parse_extract_specs`, `kind_to_type_oid`/`type_oid_to_kind`, COPY-time coercion in `apply_extract_specs`. Test: round-trip enable_compression + COPY + SELECT for each new type, plus NULL/missing-path/coercion-failure cases.

### 6. Array-index paths

Extension to `ExtractSpec.path` to allow integer indices (`["arr", 0, "key"]`). Today's `match_extract_chain` only walks `->'key'`/`->>'key'`; needs to also recognize `->0`/`->>0` (`OpExpr` with the int-index variant of the JSON operators).

Test: data with array structures, extract from `path[N]`, assert correctness across NULL/missing-index/out-of-bounds.

### 7. `deltax_add_json_extract` retrofit

Add paths to a deltatable that's already compressed without re-running COPY. Backfill: for each existing partition, walk segments, decompress raw JSONB, extract path, write new companion blob columns, update minmax/bloom.

Test: compress without json_extract, query (returns NULL or falls through), call `deltax_add_json_extract`, query again, assert correct values.

### 8. `json_extract_mode = 'all'`

Tier 2 from the original plan. Auto-discover scalar leaves per partition during compression; populate a path-map catalog the planner consults at chain-match time. Larger surface area.

## Performance improvements

Some of these get partially solved by the functional work above; others are independent levers.

### P1. Confirm dictionary encoding fires on low-cardinality synthetics

`kind` has 3 distinct values across 94M rows, `commit.operation` has ~3, `commit.collection` ~20-50. pg_deltax already has dictionary compression for low-cardinality text (`PERF_IMPROVEMENTS.md` items 19, 23) — it should be kicking in for synthetics, but worth verifying with `deltax_compression_stats`. If it isn't (e.g., the synthetic-column path takes a different code branch in compression), that's a quick win.

Test: assertion in `test_jsonb_extract.py` reading `deltax_compression_stats` after a load and checking that low-cardinality columns are dictionary-encoded.

### P2. Selective synthetic loading

Falls out of Functional #1. Listed here too because it's where the 4.1s warm floor on Q1–Q4 lives.

### P3. Top-N path verification for `LIMIT`-bounded queries

Q3/Q4 are `... ORDER BY ... LIMIT 3`. The existing Top-N early-exit path should engage, but with synthetic columns in the picture it hasn't been audited. Verify via EXPLAIN that Top-N skips segments past the limit threshold.

Test: small LIMIT query asserting `Phase 2 skipped` segments > 0 in the EXPLAIN annotation.

### P4. Push GROUP BY / count(\*) into DeltaXAppend

The bulk of the ClickHouse gap sits in PG's per-row HashAgg over decompressed rows. Pushing simple aggregations into the custom scan (return per-segment partial aggregates, let PG's HashAgg combine) is the multi-hundred-percent lever — but big surgery. Helps ClickBench too.

Cross-reference: `dev/docs/VECTORIZE.md` may already sketch this for the non-JSON path.

### P5. COUNT(DISTINCT) approximation

Q1's `COUNT(DISTINCT data->>'did')` is the dominant per-row cost in that query. Exposing HLL via a planner hint or session GUC would cut it ~3× on this workload at the cost of approximation. ClickHouse's `uniq()` defaults to HLL.

## Test infrastructure note

Create `tests/test_jsonb_extract.py` as the home for the integration tests above. Pattern to follow (from `test_compression.py` / `test_rtabench_correctness.py`):

- Each test creates a fresh table with `pg_deltax.mock_now`, configures `json_extract` via `deltax_enable_compression`, COPYs synthetic data, and asserts.
- Correctness tests A/B between `json_extract_mode = 'fields'` and `'none'` — the result sets must match exactly.
- Plan-shape tests use `EXPLAIN (FORMAT JSON, COSTS OFF)` and walk the JSON for the structural assertion (don't grep the deparsed text — `Var(OUTER_VAR, k)` deparses identically to the original chain).
