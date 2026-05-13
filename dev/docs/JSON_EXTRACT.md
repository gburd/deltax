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

| Q | Pre-walker | After walker fixes (3 cores) | +16 workers per gather | Cumulative |
|---|---|---|---|---|
| Q0 | 26s | 5.7s | **1.1s** | 24× |
| Q1 | 354s | 90s | **43s** | 8.2× |
| Q2 | 51s | 28.3s | **17.0s** | 3.0× |
| Q3 | 33s | 7.4s | **3.2s** | 10× |
| Q4 | 34s | 7.9s | **3.6s** | 9.4× |

Total warm time across the 5 queries dropped from ~498s baseline to ~68s — about 7.3× overall. The benchmark setup script (`jsonbench/benchmark.sh`) now bumps `max_parallel_workers_per_gather=16`, `max_parallel_workers=32`, `max_worker_processes=64` to use the 32-vCPU box; PG defaults of 2 workers per gather + 1 leader were leaving ~28 cores idle on every scan.

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

### 4. ~~Inject synthetics through intermediate-plan tlist elision~~ — DONE

Two changes in `json_extract.rs`:

1. **`propagate_synthetics_through_ancestors`**: when the walker descends into a cscan, it walks back up the parent stack and injects resjunk forwarder `TargetEntry` nodes into every ancestor's tlist for each cscan synthetic. Each forwarder is `Var(OUTER_VAR, k)` pointing at the next-level-down's just-added forwarder. `find_outer_var_forwarder` de-dups when the same position is propagated by multiple sibling cscans of an Append.
2. **`compute_my_subplan_tlist` rebases `forwarder_resno`**: when propagating a `Synthetic` SubplanColumn up via a `Var(OUTER_VAR, k)` ref, the cloned entry's `forwarder_resno` is reset to the position in MY tlist (`i + 1`). Without this, the matcher's returned `Var(OUTER_VAR, fr)` would carry the cscan-level position and wrongly index into the immediate child's tlist.
3. **`substitute_in_expr_node` descends into `T_TargetEntry`**: `Aggref.args` is a list of TargetEntries (not raw Exprs). Without this, chain Exprs nested inside aggregates like `COUNT(DISTINCT data->>'did')` and `MIN((data->>'time_us')::bigint)` never reached the matcher — the walker stopped at the TargetEntry boundary and the entire aggregate stayed on the slow chain-eval path.

JSONBench results post-fix (warm, 100M rows, m6i.8xlarge):

| Q | Pre-fix | Post-fix |
|---|---|---|
| Q0 | 5.8s | 8.7s (regression — propagation overhead) |
| Q1 | 354s | **94s** (3.8×) — was the canonical case |
| Q2 | 26.8s | 32.3s (regression) |
| Q3 | 25.6s | **7.8s** (3.3×) — `MIN((data->>'time_us')::bigint)` |
| Q4 | 26.3s | **8.3s** (3.2×) — `MAX-MIN((data->>'time_us')::bigint)` |

Net: 437s total warm → 151s. About 3× overall.

**Narrow-propagation pre-pass (`collect_chain_signatures_in_plan`)**: phase 0 walks the plan tree and collects every chain Expr's `(path, leaf_kind)` signature. The propagation step then only injects forwarders for cscan synthetics whose signature appears in that set. Without this, simple queries like Q0 (`SELECT data->>'kind', count(*) GROUP BY 1`) paid 5 forwarders × 4 plan levels of per-row slot copies even though only one synthetic mattered. With it, propagation cost scales with what the query actually needs — Q0 went 8.7s → 5.7s (down to its pre-propagation level), Q2 32.3s → 28.3s.

SubqueryScan / CTE pass-through is a related but separable issue: subqueries opacify the chain at the boundary. Walker would descend into `SubqueryScan.subplan` and map its tlist 1:1.

### 4b. ~~Walker rewrite gate against `chain_signatures`~~ — DONE

`subplan_tlist_from_deltax_decompress` (in `json_extract.rs`) now early-returns when no `(path, leaf_kind)` synthetic for this cscan's parent rel is present in the plan-wide signature set. Both the `custom_scan_tlist` rebuild and the per-synthetic forwarder extension are gated on this, so queries that don't touch any chain Expr but run over a parent table whose deltatable has `json_extract` configured stay at the cscan shape `plan_custom_path` set — no synthetic widens the scan tuple descriptor.

Two distinct bugs fall out of this:

- **Append width mismatch** (`psycopg.DatabaseError: unexpected field count in "D" message`). Without the gate, the cscan emits one extra column (the synthetic forwarder); a `SeqScan` over an uncompressed sibling partition emits the original count; PG's `Append`/`MergeAppend` requires children to agree, so the protocol-level decoder errored on the wider child's `D` message. Regression test: `test_jsonb_extract.py::TestWalkerForwarderGate::test_chain_unreferenced_query_over_mixed_partitions`.
- **Direct-feed JOIN returns 0 rows**. When the cscan's output feeds a Hash or NestLoop directly (no Materialize / no CTE between), the join produced 0 rows even though `SELECT order_id FROM oe WHERE …`, `SELECT count(*) …`, and `WITH oe_filt AS MATERIALIZED (…) JOIN` all returned the correct values from the same data. Root cause: PG's `set_customscan_references` resolves `Var(OUTER_VAR, k)` against the cscan's `custom_scan_tlist`-derived slot descriptor; widening cstlist with synthetics shifts physical column slot positions when scan.plan.targetlist isn't extended in lockstep, and the join's hash/probe side reads the wrong physical slot — every comparison fails. Fixed by skipping cstlist rebuild entirely when no chain signature in the plan references any synthetic. Regression test added under the same class: query that ran on RTABench's `order_events` shape directly into a Hash Join.

Discovered while enabling `json_extract` on RTABench's `event_payload->>'terminal'` chain (the only chain RTABench queries touch — Q0, Q1, Q3, Q4, Q8, Q23). The integration test (`tests/test_rtabench_correctness.py`), the local bench (`tests/rtabench_data.py`), and the EC2 bench (`rtabench/benchmark.sh`) all now configure `json_extract` so the chain-Expr eligibility infrastructure on DeltaXAgg actually fires for those six queries.

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

### P4. Push GROUP BY / count(\*) into DeltaXAppend (and make it synthetic-aware)

The bulk of the ClickHouse gap sits in PG's per-row HashAgg over decompressed rows. Pushing simple aggregations into the custom scan (return per-segment partial aggregates, let PG's HashAgg combine) is the multi-hundred-percent lever — but big surgery. Helps ClickBench too.

json_extract interaction worth calling out: the existing pushdown (`DeltaXCount`, future `DeltaXAgg`) keys off bare `Var` references at the scan level. After the walker rewrites a chain Expr in an upper plan to `Var(OUTER_VAR, k_synth)`, that ref lives at the upper plan, not the scan — so pushdown won't see it as an aggregable column. Closing this requires teaching the pushdown planner to look through forwarders (or, equivalently, doing the json_extract rewrite in `set_rel_pathlist_hook` early enough that the synthetic Var is visible to the existing pushdown machinery as if it were a physical column). Without that, aggregates over chain Exprs lose both the json_extract speedup AND the aggregate-pushdown speedup — Q3/Q4's `MIN(time_us)` patterns are the obvious cases.

Cross-reference: `dev/docs/VECTORIZE.md` may already sketch this for the non-JSON path.

### P5. COUNT(DISTINCT) approximation

Q1's `COUNT(DISTINCT data->>'did')` is the dominant per-row cost in that query. Exposing HLL via a planner hint or session GUC would cut it ~3× on this workload at the cost of approximation. ClickHouse's `uniq()` defaults to HLL.

## Test infrastructure note

Create `tests/test_jsonb_extract.py` as the home for the integration tests above. Pattern to follow (from `test_compression.py` / `test_rtabench_correctness.py`):

- Each test creates a fresh table with `pg_deltax.mock_now`, configures `json_extract` via `deltax_enable_compression`, COPYs synthetic data, and asserts.
- Correctness tests A/B between `json_extract_mode = 'fields'` and `'none'` — the result sets must match exactly.
- Plan-shape tests use `EXPLAIN (FORMAT JSON, COSTS OFF)` and walk the JSON for the structural assertion (don't grep the deparsed text — `Var(OUTER_VAR, k)` deparses identically to the original chain).
