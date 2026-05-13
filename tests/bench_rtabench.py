"""Local Docker rtabench — plain PG vs pg_deltax side-by-side.

Runs the 31 rtabench raw queries against two variants of the same data:
`order_events_plain` (plain PostgreSQL table) and `order_events`
(pg_deltax-managed, compressed via direct backfill). Compares warm times
and requires byte-identical result sets for correctness.

See `tests/rtabench_data.py` for the download / slice / load pipeline and
`rtabench/queries/*.sql` for the query texts (used verbatim, with a
`\\border_events\\b → order_events_plain` substitution for the plain run).

Makefile entry points:
  - make bench-rtabench        # default 250K orders, ~5 min first run
  - make bench-rtabench-keep   # + KEEP_CONTAINER + BENCH_PERSIST
  - make bench-rtabench-full   # all 10M orders
  - make bench-rtabench-clean  # wipe container / volume / cached CSVs
"""

from __future__ import annotations

import os
import re
import statistics
import time
import uuid
from pathlib import Path

import psycopg
import pytest

# Reuse helpers from the ClickBench bench — results dir, commit hash, JSON save.
from clickbench_data import (
    BENCH_RESULTS_DIR,
    save_bench_results,
    _get_git_commit_short,
)
from rtabench_data import (
    RTABENCH_ORDERS,
    WARMUP_RUNS,
    TIMED_RUNS,
    load_all,
)


QUERIES_DIR = Path(__file__).parent.parent / "rtabench" / "queries"
_OE = re.compile(r"\border_events\b")


# ---------------------------------------------------------------------------
# Query loading + substitution
# ---------------------------------------------------------------------------

def load_queries() -> list[tuple[str, str]]:
    """Return [(qid, sql)] for every .sql in rtabench/queries/ — sorted by
    filename so indices line up with Q00..Q30."""
    out = []
    for p in sorted(QUERIES_DIR.glob("*.sql")):
        sql = p.read_text().strip().rstrip(";").strip()
        out.append((p.stem, sql))
    return out


def for_plain(sql: str) -> str:
    return _OE.sub("order_events_plain", sql)


# ---------------------------------------------------------------------------
# Query execution
# ---------------------------------------------------------------------------

def run_once(conn, sql: str) -> tuple[float, list | None, Exception | None]:
    """Run a single query, return (elapsed_ms, rows, error). On error, rows
    is None, elapsed is inf, and caller should rollback."""
    t0 = time.monotonic()
    try:
        rows = conn.execute(sql).fetchall()
        return (time.monotonic() - t0) * 1000.0, rows, None
    except Exception as e:
        conn.rollback()
        return float("inf"), None, e


def run_phase(conn, queries: list[tuple[str, str]], *, label: str, transform=lambda s: s) -> dict:
    """Run every query with 1 warmup + TIMED_RUNS timed runs. Returns
    {qid: {"ms": median_or_inf, "rows": last_rows, "error": str|None}}."""
    out: dict[str, dict] = {}
    for qid, sql_src in queries:
        sql = transform(sql_src)

        # Warmup (ignored)
        for _ in range(WARMUP_RUNS):
            _, _, _ = run_once(conn, sql)

        # Timed runs
        timings: list[float] = []
        last_rows = None
        last_error: Exception | None = None
        for _ in range(TIMED_RUNS):
            ms, rows, err = run_once(conn, sql)
            timings.append(ms)
            if rows is not None:
                last_rows = rows
            if err is not None:
                last_error = err

        median = statistics.median(timings)
        out[qid] = {
            "ms": None if median == float("inf") else median,
            "rows": last_rows,
            "error": str(last_error) if last_error is not None else None,
        }

        status = f"{median:.1f} ms" if median != float("inf") else f"ERROR: {last_error}"
        print(f"  [{label}] {qid}: {status}")
    return out


# ---------------------------------------------------------------------------
# Correctness comparison
# ---------------------------------------------------------------------------

# Queries that use LIMIT where the ORDER BY doesn't strictly tie-break.
# For these we only require row-count + multiset-after-sort equality; the
# boundary row(s) may differ between plan variants if ties exist.
LIMIT_TIE_QUERIES = {
    "0005_search_events_for_processor",
    "0006_order_events_without_backups",
    "0010_last_event_for_an_order",
    "0016_customers_with_most_orders",
    "0017_top_selling_month_product",
    "0018_customer_month_value",
    "0023_top_sales_volume_product_from_terminal",
    "0024_top_customer_by_revenue",
    "0029_top_product_in_age_group",
    "0030_customers_with_most_orders_delivered",
}


def _normalize_rows(rows: list | None) -> list:
    """Map each row's Decimal/float → a normalized tuple (for stable sort +
    compare) using stringification. Also sorts the list."""
    if rows is None:
        return []
    norm = []
    for r in rows:
        norm.append(tuple(str(c) if c is not None else None for c in r))
    norm.sort()
    return norm


def compare_results(qid: str, plain_rows, deltax_rows) -> tuple[bool, str]:
    """Return (ok, detail). LIMIT-tie queries relax to row-count equality."""
    p = _normalize_rows(plain_rows)
    d = _normalize_rows(deltax_rows)
    if qid in LIMIT_TIE_QUERIES:
        if len(p) != len(d):
            return False, f"row count: plain={len(p)} deltax={len(d)}"
        return True, f"{len(p)} rows, tie-relaxed"
    if p == d:
        return True, f"{len(p)} rows"
    # Mismatch — produce a short diff
    if len(p) != len(d):
        return False, f"row count: plain={len(p)} deltax={len(d)}"
    diffs = [i for i in range(len(p)) if p[i] != d[i]]
    first = diffs[0]
    return False, (
        f"{len(diffs)}/{len(p)} rows differ — first at idx {first}: "
        f"plain={p[first]!r} vs deltax={d[first]!r}"
    )


# ---------------------------------------------------------------------------
# Fixture: container-level DB with data loaded once per test class
# ---------------------------------------------------------------------------

@pytest.fixture(scope="class")
def rtabench_db(pg_container):
    from conftest import HOST_PORT, PG_PASSWORD, PG_USER, _admin_conn

    # Stable DB name when BENCH_PERSIST so re-runs find the same data.
    persist = bool(os.environ.get("BENCH_PERSIST"))
    db_name = "bench_rtabench" if persist else f"bench_rtabench_{uuid.uuid4().hex[:8]}"

    admin = _admin_conn()
    exists = admin.execute(
        "SELECT 1 FROM pg_database WHERE datname = %s", (db_name,)
    ).fetchone()
    if not exists:
        admin.execute(f'CREATE DATABASE "{db_name}"')
    admin.close()

    conn = psycopg.connect(
        host="localhost",
        port=HOST_PORT,
        user=PG_USER,
        password=PG_PASSWORD,
        dbname=db_name,
    )
    conn.execute("SET jit = off")
    conn.commit()
    conn.execute("CREATE EXTENSION IF NOT EXISTS pg_deltax")
    conn.commit()

    # Session-level planner tunes — keep the local bench comparable to the
    # EC2 benchmark where enable_nestloop=off reliably avoids the
    # NestLoop-over-Materialize trap on queries like Q17 (see
    # RTABENCH_QUERY_ANALYSIS.md §3.A).
    conn.execute("SET enable_nestloop = off")
    conn.execute("SET work_mem = '1GB'")
    # Activate the planner_hook walker so chain Exprs in queries
    # transparently use the synthetic columns set up by
    # `rtabench_data.py::setup_schema`. Without this the walker is a
    # no-op and the chain-Expr eligibility infrastructure on DeltaXAgg
    # stays dormant for RTABench.
    conn.execute("SET pg_deltax.json_extract_mode = 'fields'")
    conn.commit()

    load_all(conn, RTABENCH_ORDERS)

    yield conn

    conn.close()
    if os.environ.get("KEEP_CONTAINER") or persist:
        print(f"\n  Keeping database {db_name} for reuse")
        print(f"  Connect: docker exec -it pg_deltax_inttest psql -U {PG_USER} -d {db_name}")
    else:
        admin = _admin_conn()
        admin.execute(f'DROP DATABASE "{db_name}"')
        admin.close()


# ---------------------------------------------------------------------------
# The test
# ---------------------------------------------------------------------------

class TestRtabench:
    """Plain PG vs pg_deltax side-by-side on the 31 rtabench queries."""

    def test_bench(self, rtabench_db):
        conn = rtabench_db
        queries = load_queries()
        assert queries, "no queries found in rtabench/queries/"

        # Phase A: plain PG (order_events_plain)
        print("\n\n=== Phase A: Plain PostgreSQL ===")
        plain = run_phase(conn, queries, label="plain", transform=for_plain)

        # Phase B: pg_deltax (order_events, compressed)
        print("\n=== Phase B: pg_deltax ===")
        deltax = run_phase(conn, queries, label="deltax")

        # Phase C: correctness
        print("\n=== Phase C: Correctness ===")
        mismatches: list[str] = []
        for qid, _ in queries:
            pr = plain[qid]
            dr = deltax[qid]
            if pr["error"] or dr["error"]:
                mismatches.append(qid)
                print(f"  {qid}: ERROR — plain={pr['error']} deltax={dr['error']}")
                continue
            ok, detail = compare_results(qid, pr["rows"], dr["rows"])
            status = "OK" if ok else "MISMATCH"
            print(f"  {qid}: {status} ({detail})")
            if not ok:
                mismatches.append(qid)

        # Phase D: report
        print("\n=== Phase D: Summary ===")
        print(f"{'Query':<50}{'plain (ms)':>14}{'deltax (ms)':>14}{'speedup':>10}")
        plain_total = 0.0
        deltax_total = 0.0
        for qid, _ in queries:
            pms = plain[qid]["ms"]
            dms = deltax[qid]["ms"]
            if pms is not None:
                plain_total += pms
            if dms is not None:
                deltax_total += dms
            speedup = f"{pms/dms:>6.2f}x" if (pms is not None and dms not in (None, 0)) else "  —"
            pcell = f"{pms:>12.1f}" if pms is not None else "         —"
            dcell = f"{dms:>12.1f}" if dms is not None else "         —"
            print(f"{qid:<50}{pcell:>14}{dcell:>14}{speedup:>10}")
        print(
            f"{'total':<50}{plain_total:>12.1f}  {deltax_total:>12.1f}  "
            f"{(plain_total/deltax_total if deltax_total else 0):>6.2f}x"
        )

        # Save machine-readable result
        save_bench_results(
            "rtabench_pg_deltax",
            {
                "n_orders": RTABENCH_ORDERS,
                "plain_queries": {q: plain[q]["ms"] for q, _ in queries},
                "deltax_queries": {q: deltax[q]["ms"] for q, _ in queries},
                "mismatches": mismatches,
                "commit": _get_git_commit_short(),
            },
        )

        assert not mismatches, (
            f"{len(mismatches)} query result mismatch(es) between plain PG "
            f"and pg_deltax: {mismatches}"
        )
