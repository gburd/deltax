use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::pg_guard;

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::time::Instant;

use crate::compression;
use super::super::SyncStatic;
use super::batch_qual::{BatchCompareOp,
    extract_batch_quals, evaluate_batch_quals};
use super::datum_utils::{
    decompress_blob_to_datums, decompress_text_blob_to_raw_strings,
    decompress_text_blob_to_lengths, decompress_text_blob_with_like_filter,
    decompress_text_blob_with_eq_filter, string_to_datum, pg_type_name,
    count_non_null, collation_strcmp,
};
use super::segments::{
    SegmentData, load_metadata, load_segments_heap,
    segment_skippable_by_dict_like, extract_segment_filters,
};

// ============================================================================
// DeltaXAgg: aggregate pushdown (SUM, AVG, COUNT, COUNT(DISTINCT), GROUP BY)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AggType { Sum, Count, CountStar, Avg, CountDistinct, Min, Max }

/// Expression kind for aggregate arguments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AggExpr {
    /// Plain column reference: AGG(col)
    Column,
    /// length(col): AGG(length(col)) — compute string lengths without varlena allocation
    LengthOf,
    /// col + const: AGG(col + N) — add integer constant before aggregation
    AddConst,
}

enum AggAccumulator {
    SumInt { sum: i128, count: i64 },
    SumFloat { sum: f64, count: i64 },
    Count { count: i64 },
    CountDistinctInt { seen: std::collections::HashSet<i64> },
    CountDistinctStr { seen: std::collections::HashSet<String> },
    MinInt { val: Option<i64> },
    MaxInt { val: Option<i64> },
    MinFloat { val: Option<f64> },
    MaxFloat { val: Option<f64> },
    MinStr { val: Option<String> },
    MaxStr { val: Option<String> },
}

impl AggAccumulator {
    fn new_for(agg_type: AggType, col_type: pg_sys::Oid) -> Self {
        match agg_type {
            AggType::Sum | AggType::Avg => {
                if col_type == pg_sys::FLOAT4OID || col_type == pg_sys::FLOAT8OID {
                    AggAccumulator::SumFloat { sum: 0.0, count: 0 }
                } else {
                    AggAccumulator::SumInt { sum: 0, count: 0 }
                }
            }
            AggType::Count | AggType::CountStar => AggAccumulator::Count { count: 0 },
            AggType::CountDistinct => {
                if col_type == pg_sys::TEXTOID || col_type == pg_sys::VARCHAROID || col_type == pg_sys::BPCHAROID {
                    AggAccumulator::CountDistinctStr { seen: std::collections::HashSet::new() }
                } else {
                    AggAccumulator::CountDistinctInt { seen: std::collections::HashSet::new() }
                }
            }
            AggType::Min => {
                if col_type == pg_sys::TEXTOID || col_type == pg_sys::VARCHAROID || col_type == pg_sys::BPCHAROID {
                    AggAccumulator::MinStr { val: None }
                } else if col_type == pg_sys::FLOAT4OID || col_type == pg_sys::FLOAT8OID {
                    AggAccumulator::MinFloat { val: None }
                } else {
                    AggAccumulator::MinInt { val: None }
                }
            }
            AggType::Max => {
                if col_type == pg_sys::TEXTOID || col_type == pg_sys::VARCHAROID || col_type == pg_sys::BPCHAROID {
                    AggAccumulator::MaxStr { val: None }
                } else if col_type == pg_sys::FLOAT4OID || col_type == pg_sys::FLOAT8OID {
                    AggAccumulator::MaxFloat { val: None }
                } else {
                    AggAccumulator::MaxInt { val: None }
                }
            }
        }
    }

    fn clone_fresh(&self) -> Self {
        match self {
            AggAccumulator::SumInt { .. } => AggAccumulator::SumInt { sum: 0, count: 0 },
            AggAccumulator::SumFloat { .. } => AggAccumulator::SumFloat { sum: 0.0, count: 0 },
            AggAccumulator::Count { .. } => AggAccumulator::Count { count: 0 },
            AggAccumulator::CountDistinctInt { .. } => AggAccumulator::CountDistinctInt { seen: std::collections::HashSet::new() },
            AggAccumulator::CountDistinctStr { .. } => AggAccumulator::CountDistinctStr { seen: std::collections::HashSet::new() },
            AggAccumulator::MinInt { .. } => AggAccumulator::MinInt { val: None },
            AggAccumulator::MaxInt { .. } => AggAccumulator::MaxInt { val: None },
            AggAccumulator::MinFloat { .. } => AggAccumulator::MinFloat { val: None },
            AggAccumulator::MaxFloat { .. } => AggAccumulator::MaxFloat { val: None },
            AggAccumulator::MinStr { .. } => AggAccumulator::MinStr { val: None },
            AggAccumulator::MaxStr { .. } => AggAccumulator::MaxStr { val: None },
        }
    }
}

pub(crate) struct AggExecSpec {
    pub(crate) agg_type: AggType,
    pub(crate) col_idx: i32,               // -1 for COUNT(*)
    pub(crate) col_type_oid: pg_sys::Oid,  // source column type
    pub(crate) expr_kind: AggExpr,         // Column, LengthOf, or AddConst
    pub(crate) const_offset: i64,          // Only used when expr_kind == AddConst
}

/// Expression kind for GROUP BY columns.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GroupByExpr {
    /// Plain column reference: GROUP BY col
    Column,
    /// regexp_replace(col, pattern, replacement): GROUP BY regexp_replace(col, ...)
    RegexpReplace { pattern: String, replacement: String, func_oid: u32, collation: u32 },
    /// date_trunc(unit, timestamp_col): GROUP BY date_trunc('minute', ts)
    DateTrunc { unit: String, unit_usecs: i64, func_oid: u32 },
    /// extract(field FROM timestamp_col): GROUP BY extract(minute FROM ts)
    Extract { unit: String, func_oid: u32 },
    /// col +/- const: GROUP BY col - 1  (offset is always stored as addition, so col-1 → offset=-1)
    AddConst { offset: i64, op_oid: u32 },
}

/// Convert a date_trunc unit string to microseconds.
/// Only sub-day units are supported (integer arithmetic is exact).
pub(crate) fn date_trunc_unit_to_usecs(unit: &str) -> i64 {
    match unit {
        "microsecond" | "microseconds" | "us" => 1,
        "millisecond" | "milliseconds" | "ms" => 1_000,
        "second" | "seconds" => 1_000_000,
        "minute" | "minutes" => 60_000_000,
        "hour" | "hours" => 3_600_000_000,
        "day" | "days" => 86_400_000_000,
        _ => 1, // fallback — should not happen (validated in hook)
    }
}

/// Extract a time field from PG epoch microseconds using pure arithmetic.
/// Only supports sub-day fields + dow + epoch (validated in hook).
fn extract_field_from_usecs(pg_usec: i64, unit: &str) -> i64 {
    match unit {
        "microsecond" | "microseconds" => {
            // PG returns second * 1_000_000 (including whole seconds within the minute)
            let usec_in_day = pg_usec.rem_euclid(86_400_000_000);
            let sec_of_min = (usec_in_day / 1_000_000) % 60;
            let frac_usec = usec_in_day.rem_euclid(1_000_000);
            sec_of_min * 1_000_000 + frac_usec
        }
        "millisecond" | "milliseconds" => {
            // PG returns second * 1000 (including whole seconds within the minute)
            let usec_in_day = pg_usec.rem_euclid(86_400_000_000);
            let sec_of_min = (usec_in_day / 1_000_000) % 60;
            let frac_ms = usec_in_day.rem_euclid(1_000_000) / 1_000;
            sec_of_min * 1_000 + frac_ms
        }
        "second" | "seconds" => {
            (pg_usec.rem_euclid(86_400_000_000) / 1_000_000) % 60
        }
        "minute" | "minutes" => {
            (pg_usec.rem_euclid(86_400_000_000) / 60_000_000) % 60
        }
        "hour" | "hours" => {
            pg_usec.rem_euclid(86_400_000_000) / 3_600_000_000
        }
        "dow" => {
            // Day of week (0=Sunday..6=Saturday)
            // PG epoch 2000-01-01 is a Saturday (dow=6)
            let days = pg_usec.div_euclid(86_400_000_000);
            (days + 6).rem_euclid(7)
        }
        "epoch" => {
            // PG epoch is 2000-01-01, Unix epoch offset = 946684800 seconds
            (pg_usec / 1_000_000) + 946_684_800
        }
        _ => 0, // Should not happen (validated in hook)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GroupByColSpec {
    pub(crate) col_idx: i32,  // 0-based column index
    pub(crate) type_oid: pg_sys::Oid,
    pub(crate) expr: GroupByExpr,
}

/// A HAVING filter: compare an aggregate result against a constant.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HavingOp { Gt, Lt, Ge, Le, Eq, Ne }

#[derive(Debug, Clone)]
pub(crate) struct HavingFilter {
    pub(crate) agg_idx: usize,    // index into agg_specs
    pub(crate) op: HavingOp,
    pub(crate) const_val: i64,    // constant value (int8)
}

/// State for DeltaXAgg (aggregate pushdown).
pub(crate) struct AggScanState {
    pub(crate) _agg_specs: Vec<AggExecSpec>,
    pub(crate) _group_specs: Vec<GroupByColSpec>,
    pub(crate) result_rows: Vec<Vec<(pg_sys::Datum, bool)>>,
    pub(crate) result_idx: usize,
    pub(crate) _num_result_cols: usize,
    pub(crate) metadata_us: u64,
    pub(crate) heap_scan_us: u64,
    pub(crate) decompress_us: u64,
    pub(crate) agg_us: u64,
    pub(crate) total_segments: u64,
    pub(crate) total_rows_processed: u64,
    pub(crate) batch_quals_count: usize,
    pub(crate) where_quals_null: bool,
    pub(crate) regex_cache_size: u64,
    pub(crate) regex_cache_calls: u64,
    pub(crate) topn_limit: u64,
    pub(crate) topn_sort_col: i64,
    pub(crate) topn_ascending: bool,
    pub(crate) pre_topn_groups: u64,
}


/// Static CustomExecMethods struct for DeltaXAgg.
pub(crate) static DELTAX_AGG_EXEC_METHODS: SyncStatic<pg_sys::CustomExecMethods> =
    SyncStatic(pg_sys::CustomExecMethods {
        CustomName: super::super::DELTAX_AGG_NAME.as_ptr(),
        BeginCustomScan: Some(begin_agg_scan),
        ExecCustomScan: Some(exec_agg_scan),
        EndCustomScan: Some(end_agg_scan),
        ReScanCustomScan: Some(rescan_agg_scan),
        MarkPosCustomScan: None,
        RestrPosCustomScan: None,
        EstimateDSMCustomScan: None,
        InitializeDSMCustomScan: None,
        ReInitializeDSMCustomScan: None,
        InitializeWorkerCustomScan: None,
        ShutdownCustomScan: None,
        ExplainCustomScan: Some(super::super::explain::explain_agg_scan),
    });


// ============================================================================
// DeltaXAgg execution callbacks
// ============================================================================

/// CreateCustomScanState callback for DeltaXAgg.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn create_agg_scan_state(
    cscan: *mut pg_sys::CustomScan,
) -> *mut pg_sys::Node {
    unsafe {
        let css = pg_sys::palloc0(std::mem::size_of::<pg_sys::CustomScanState>())
            as *mut pg_sys::CustomScanState;

        (*css).ss.ps.type_ = pg_sys::NodeTag::T_CustomScanState;
        (*css).methods = &DELTAX_AGG_EXEC_METHODS.0;
        (*css).custom_ps = (*cscan).custom_private;

        css as *mut pg_sys::Node
    }
}

/// Output mapping entry: which internal data to put at this slot position.
#[derive(Debug, Clone, Copy)]
enum OutputEntry {
    Agg(usize),    // index into agg_specs
    Group(usize),  // index into group_specs
}

/// BeginCustomScan callback for DeltaXAgg: decompress and aggregate.
#[pg_guard]
pub(super) unsafe extern "C-unwind" fn begin_agg_scan(
    node: *mut pg_sys::CustomScanState,
    _estate: *mut pg_sys::EState,
    _eflags: i32,
) {
    unsafe {
        let custom_private = (*node).custom_ps;
        if custom_private.is_null() {
            pgrx::error!("pg_deltax: missing custom_private in DeltaXAgg state");
        }

        let list_len = (*custom_private).length;

        // Parse custom_private:
        // [oid1, ..., -1, num_aggs, agg_spec_fields...,
        //  num_groups, group_spec_fields...,
        //  num_output, output_type_0, output_ref_0, ...]
        let mut companion_oids: Vec<pg_sys::Oid> = Vec::new();
        let mut agg_specs: Vec<AggExecSpec> = Vec::new();
        let mut group_specs: Vec<GroupByColSpec> = Vec::new();
        let mut output_map: Vec<OutputEntry> = Vec::new();

        let mut idx = 0;
        // Parse OIDs until sentinel
        while idx < list_len {
            let val = pg_sys::list_nth_int(custom_private, idx);
            idx += 1;
            if val == -1 { break; }
            companion_oids.push(pg_sys::Oid::from(val as u32));
        }
        // Parse agg specs
        if idx < list_len {
            let num_aggs = pg_sys::list_nth_int(custom_private, idx) as usize;
            idx += 1;
            for _ in 0..num_aggs {
                let agg_type_val = pg_sys::list_nth_int(custom_private, idx);
                let col_idx = pg_sys::list_nth_int(custom_private, idx + 1);
                let result_oid = pg_sys::list_nth_int(custom_private, idx + 2) as u32;
                let col_type_oid = pg_sys::list_nth_int(custom_private, idx + 3) as u32;
                let expr_kind_val = pg_sys::list_nth_int(custom_private, idx + 4);
                idx += 5;
                let agg_type = match agg_type_val {
                    0 => AggType::Sum,
                    1 => AggType::Count,
                    2 => AggType::CountStar,
                    3 => AggType::Avg,
                    4 => AggType::CountDistinct,
                    5 => AggType::Min,
                    6 => AggType::Max,
                    _ => AggType::Count,
                };
                let (expr_kind, const_offset) = match expr_kind_val {
                    1 => (AggExpr::LengthOf, 0i64),
                    2 => {
                        let offset = pg_sys::list_nth_int(custom_private, idx) as i64;
                        idx += 1;
                        (AggExpr::AddConst, offset)
                    }
                    _ => (AggExpr::Column, 0i64),
                };
                let _ = result_oid; // parsed for offset, not stored
                agg_specs.push(AggExecSpec {
                    agg_type,
                    col_idx,
                    col_type_oid: pg_sys::Oid::from(col_type_oid),
                    expr_kind,
                    const_offset,
                });
            }
        }
        // Parse group specs
        if idx < list_len {
            let num_groups = pg_sys::list_nth_int(custom_private, idx) as usize;
            idx += 1;
            for _ in 0..num_groups {
                let col_idx = pg_sys::list_nth_int(custom_private, idx);
                let type_oid = pg_sys::list_nth_int(custom_private, idx + 1) as u32;
                let expr_tag = pg_sys::list_nth_int(custom_private, idx + 2);
                idx += 3;
                let expr = if expr_tag == 1 {
                    // RegexpReplace: func_oid, collation, pattern_len, pattern_bytes..., replacement_len, replacement_bytes...
                    let func_oid = pg_sys::list_nth_int(custom_private, idx) as u32;
                    let collation = pg_sys::list_nth_int(custom_private, idx + 1) as u32;
                    idx += 2;
                    let pattern_len = pg_sys::list_nth_int(custom_private, idx) as usize;
                    idx += 1;
                    let mut pattern_bytes = Vec::with_capacity(pattern_len);
                    for _ in 0..pattern_len {
                        pattern_bytes.push(pg_sys::list_nth_int(custom_private, idx) as u8);
                        idx += 1;
                    }
                    let pattern = String::from_utf8_lossy(&pattern_bytes).into_owned();
                    let replacement_len = pg_sys::list_nth_int(custom_private, idx) as usize;
                    idx += 1;
                    let mut replacement_bytes = Vec::with_capacity(replacement_len);
                    for _ in 0..replacement_len {
                        replacement_bytes.push(pg_sys::list_nth_int(custom_private, idx) as u8);
                        idx += 1;
                    }
                    let replacement = String::from_utf8_lossy(&replacement_bytes).into_owned();
                    GroupByExpr::RegexpReplace { pattern, replacement, func_oid, collation }
                } else if expr_tag == 2 {
                    // DateTrunc: func_oid, unit_len, unit_bytes...
                    let func_oid = pg_sys::list_nth_int(custom_private, idx) as u32;
                    idx += 1;
                    let unit_len = pg_sys::list_nth_int(custom_private, idx) as usize;
                    idx += 1;
                    let mut unit_bytes = Vec::with_capacity(unit_len);
                    for _ in 0..unit_len {
                        unit_bytes.push(pg_sys::list_nth_int(custom_private, idx) as u8);
                        idx += 1;
                    }
                    let unit = String::from_utf8_lossy(&unit_bytes).into_owned();
                    let unit_usecs = date_trunc_unit_to_usecs(&unit);
                    GroupByExpr::DateTrunc { unit, unit_usecs, func_oid }
                } else if expr_tag == 3 {
                    // Extract: func_oid, unit_len, unit_bytes...
                    let func_oid = pg_sys::list_nth_int(custom_private, idx) as u32;
                    idx += 1;
                    let unit_len = pg_sys::list_nth_int(custom_private, idx) as usize;
                    idx += 1;
                    let mut unit_bytes = Vec::with_capacity(unit_len);
                    for _ in 0..unit_len {
                        unit_bytes.push(pg_sys::list_nth_int(custom_private, idx) as u8);
                        idx += 1;
                    }
                    let unit = String::from_utf8_lossy(&unit_bytes).into_owned();
                    GroupByExpr::Extract { unit, func_oid }
                } else if expr_tag == 4 {
                    // AddConst: offset_i32, op_oid
                    let offset = pg_sys::list_nth_int(custom_private, idx) as i64;
                    let op_oid = pg_sys::list_nth_int(custom_private, idx + 1) as u32;
                    idx += 2;
                    GroupByExpr::AddConst { offset, op_oid }
                } else {
                    GroupByExpr::Column
                };
                group_specs.push(GroupByColSpec {
                    col_idx,
                    type_oid: pg_sys::Oid::from(type_oid),
                    expr,
                });
            }
        }
        // Parse output mapping
        if idx < list_len {
            let num_output = pg_sys::list_nth_int(custom_private, idx) as usize;
            idx += 1;
            for _ in 0..num_output {
                let otype = pg_sys::list_nth_int(custom_private, idx);
                let oref = pg_sys::list_nth_int(custom_private, idx + 1) as usize;
                idx += 2;
                output_map.push(if otype == 0 {
                    OutputEntry::Agg(oref)
                } else {
                    OutputEntry::Group(oref)
                });
            }
        }
        // If no output mapping (backward compat), default to aggs then groups
        if output_map.is_empty() {
            for i in 0..agg_specs.len() {
                output_map.push(OutputEntry::Agg(i));
            }
            for i in 0..group_specs.len() {
                output_map.push(OutputEntry::Group(i));
            }
        }

        // Parse HAVING filters
        let mut having_filters: Vec<HavingFilter> = Vec::new();
        if idx < list_len {
            let num_having = pg_sys::list_nth_int(custom_private, idx) as usize;
            idx += 1;
            for _ in 0..num_having {
                let agg_idx = pg_sys::list_nth_int(custom_private, idx) as usize;
                let op_val = pg_sys::list_nth_int(custom_private, idx + 1);
                let const_val = pg_sys::list_nth_int(custom_private, idx + 2) as i64;
                idx += 3;
                let op = match op_val {
                    0 => HavingOp::Gt,
                    1 => HavingOp::Lt,
                    2 => HavingOp::Ge,
                    3 => HavingOp::Le,
                    4 => HavingOp::Eq,
                    5 => HavingOp::Ne,
                    _ => HavingOp::Gt,
                };
                having_filters.push(HavingFilter { agg_idx, op, const_val });
            }
        }
        // Read WHERE quals from custom_private (serialized as string by plan_agg_path).
        // Format: [str_len, char0, char1, ...] where str_len=0 means no quals.
        let where_quals: *mut pg_sys::List = if idx < list_len {
            let str_len = pg_sys::list_nth_int(custom_private, idx) as usize;
            idx += 1;
            if str_len > 0 {
                let mut chars: Vec<u8> = Vec::with_capacity(str_len + 1);
                for _ in 0..str_len {
                    chars.push(pg_sys::list_nth_int(custom_private, idx) as u8);
                    idx += 1;
                }
                chars.push(0); // null terminator
                pg_sys::stringToNode(chars.as_ptr() as *const std::ffi::c_char) as *mut pg_sys::List
            } else {
                std::ptr::null_mut()
            }
        } else {
            std::ptr::null_mut()
        };
        // Parse top-N info
        let mut topn_limit: i64 = 0;
        let mut topn_sort_col: usize = 0;
        let mut topn_ascending: bool = true;
        if idx < list_len {
            let limit_val = pg_sys::list_nth_int(custom_private, idx);
            idx += 1;
            if limit_val > 0 {
                topn_limit = limit_val as i64;
                topn_sort_col = pg_sys::list_nth_int(custom_private, idx) as usize;
                idx += 1;
                topn_ascending = pg_sys::list_nth_int(custom_private, idx) != 0;
                idx += 1;
            }
        }
        let _ = idx;

        if companion_oids.is_empty() {
            pgrx::error!("pg_deltax: DeltaXAgg has no companion tables");
        }

        // Get first companion table name for metadata
        let first_name = {
            let name_ptr = pg_sys::get_rel_name(companion_oids[0]);
            if name_ptr.is_null() {
                pgrx::error!(
                    "pg_deltax: companion table not found for OID {}",
                    u32::from(companion_oids[0])
                );
            }
            std::ffi::CStr::from_ptr(name_ptr)
                .to_string_lossy()
                .into_owned()
        };

        // Load metadata via SPI
        let t0 = Instant::now();
        let meta = Spi::connect(|client| load_metadata(client, &first_name));
        let metadata_us = t0.elapsed().as_micros() as u64;

        // Short-circuit: answer scalar COUNT(*)/COUNT(DISTINCT) from catalog
        // without scanning any segments.
        if group_specs.is_empty()
            && where_quals.is_null()
            && having_filters.is_empty()
        {
            let catalog_answers: Option<Vec<(pg_sys::Datum, bool)>> = (|| {
                let mut agg_results: Vec<(pg_sys::Datum, bool)> = Vec::new();
                for spec in &agg_specs {
                    match spec.agg_type {
                        AggType::CountStar => {
                            let mut total: i64 = 0;
                            for &oid in &companion_oids {
                                total += super::super::cost::get_row_count(oid)?;
                            }
                            agg_results.push((pg_sys::Datum::from(total as usize), false));
                        }
                        AggType::CountDistinct if spec.expr_kind == AggExpr::Column => {
                            // Can't merge distinct counts across partitions
                            if companion_oids.len() != 1 {
                                return None;
                            }
                            let nd_map = super::super::cost::get_column_ndistinct(companion_oids[0]);
                            if nd_map.is_empty() {
                                return None;
                            }
                            let col_name = meta.col_names.get(spec.col_idx as usize)?;
                            let nd = nd_map.get(col_name)?;
                            agg_results.push((pg_sys::Datum::from(*nd as usize), false));
                        }
                        _ => return None, // Non-catalog-answerable agg
                    }
                }
                Some(agg_results)
            })();

            if let Some(agg_results) = catalog_answers {
                let num_result_cols = output_map.len();
                let mut row: Vec<(pg_sys::Datum, bool)> = Vec::with_capacity(num_result_cols);
                for entry in &output_map {
                    match entry {
                        OutputEntry::Agg(ai) => row.push(agg_results[*ai]),
                        OutputEntry::Group(_) => row.push((pg_sys::Datum::from(0usize), true)),
                    }
                }
                let state = AggScanState {
                    _agg_specs: agg_specs,
                    _group_specs: group_specs,
                    result_rows: vec![row],
                    result_idx: 0,
                    _num_result_cols: num_result_cols,
                    metadata_us,
                    heap_scan_us: 0,
                    decompress_us: 0,
                    agg_us: 0,
                    total_segments: 0,
                    total_rows_processed: 0,
                    batch_quals_count: 0,
                    where_quals_null: true,
                    regex_cache_size: 0,
                    regex_cache_calls: 0,
                    topn_limit: 0,
                    topn_sort_col: 0,
                    topn_ascending: true,
                    pre_topn_groups: 0,
                };
                let state_ptr = Box::into_raw(Box::new(state));
                (*node).custom_ps = state_ptr as *mut pg_sys::List;
                return;
            }
        }

        // Check if we can use the sum/metadata fast path:
        // All agg specs are metadata-resolvable AND no GROUP BY AND no WHERE clause.
        // SUM(col + const) is resolvable: SUM(col + C) = SUM(col) + C * COUNT(*)
        let metadata_fast_path = group_specs.is_empty()
            && where_quals.is_null()
            && having_filters.is_empty()
            && agg_specs.iter().all(|spec| {
                match spec.agg_type {
                    AggType::CountStar => true,
                    AggType::Sum => {
                        (spec.expr_kind == AggExpr::Column || spec.expr_kind == AggExpr::AddConst)
                            && spec.col_idx >= 0 && {
                            let t = spec.col_type_oid;
                            t == pg_sys::INT2OID || t == pg_sys::INT4OID || t == pg_sys::INT8OID
                                || t == pg_sys::FLOAT4OID || t == pg_sys::FLOAT8OID
                        }
                    }
                    AggType::Avg | AggType::Count => {
                        spec.expr_kind == AggExpr::Column && spec.col_idx >= 0 && {
                            let t = spec.col_type_oid;
                            t == pg_sys::INT2OID || t == pg_sys::INT4OID || t == pg_sys::INT8OID
                                || t == pg_sys::FLOAT4OID || t == pg_sys::FLOAT8OID
                        }
                    }
                    AggType::Min | AggType::Max => {
                        spec.expr_kind == AggExpr::Column && spec.col_idx >= 0
                    }
                    _ => false,
                }
            });

        if metadata_fast_path {
            // Load segments with metadata only (no column decompression)
            let needed_cols = vec![false; meta.col_names.len()];
            let needs_sums = agg_specs.iter().any(|s| matches!(s.agg_type, AggType::Sum | AggType::Avg));
            let needs_counts = agg_specs.iter().any(|s| matches!(s.agg_type, AggType::Count));
            let needs_minmax = agg_specs.iter().any(|s| matches!(s.agg_type, AggType::Min | AggType::Max));

            let t1 = Instant::now();
            let mut all_segments: Vec<SegmentData> = Vec::new();
            for &oid in &companion_oids {
                let (segs, _, _) = load_segments_heap(
                    oid, &meta.col_names, &meta.segment_by, &needed_cols,
                    &meta.time_column, needs_minmax, &[], None, None, None,
                    &[], needs_sums || needs_counts,
                );
                all_segments.extend(segs);
            }
            let heap_scan_us = t1.elapsed().as_micros() as u64;

            // Check that sum metadata actually exists for all needed columns
            let sums_available = agg_specs.iter().all(|spec| {
                match spec.agg_type {
                    AggType::Sum | AggType::Avg | AggType::Count => {
                        let col_name = &meta.col_names[spec.col_idx as usize];
                        all_segments.is_empty()
                            || all_segments.iter().all(|seg| {
                                match spec.agg_type {
                                    AggType::Sum | AggType::Avg => seg.col_sums.contains_key(col_name),
                                    AggType::Count => seg.col_sums.contains_key(col_name),
                                    _ => true,
                                }
                            })
                    }
                    _ => true,
                }
            });
            let minmax_available = agg_specs.iter().all(|spec| {
                match spec.agg_type {
                    AggType::Min | AggType::Max => {
                        let col_name = &meta.col_names[spec.col_idx as usize];
                        all_segments.is_empty()
                            || all_segments.iter().all(|seg| seg.col_minmax.contains_key(col_name))
                    }
                    _ => true,
                }
            });

            if sums_available && minmax_available {
                // Accumulate from metadata
                let mut accumulators: Vec<AggAccumulator> = agg_specs
                    .iter()
                    .map(|spec| AggAccumulator::new_for(spec.agg_type, spec.col_type_oid))
                    .collect();

                for seg in &all_segments {
                    if seg.row_count == 0 { continue; }
                    for (i, spec) in agg_specs.iter().enumerate() {
                        match spec.agg_type {
                            AggType::CountStar => {
                                if let AggAccumulator::Count { count } = &mut accumulators[i] {
                                    *count += seg.row_count as i64;
                                }
                            }
                            AggType::Count => {
                                let col_name = &meta.col_names[spec.col_idx as usize];
                                if let Some(cs) = seg.col_sums.get(col_name)
                                    && let AggAccumulator::Count { count } = &mut accumulators[i]
                                {
                                    *count += cs.nonnull_count;
                                }
                            }
                            AggType::Sum | AggType::Avg => {
                                let col_name = &meta.col_names[spec.col_idx as usize];
                                if let Some(cs) = seg.col_sums.get(col_name) {
                                    if cs.sum_null { continue; }
                                    // For SUM(col + C): SUM(col) + C * nonnull_count
                                    let add_const = if spec.expr_kind == AggExpr::AddConst {
                                        spec.const_offset
                                    } else {
                                        0
                                    };
                                    match &mut accumulators[i] {
                                        AggAccumulator::SumInt { sum, count } => {
                                            // Sum datum is NUMERIC — extract via numeric_out, parse as i128
                                            let cstr = pg_sys::OidOutputFunctionCall(
                                                pg_sys::Oid::from(1702u32), // numeric_out
                                                cs.sum_datum,
                                            );
                                            let s = std::ffi::CStr::from_ptr(cstr)
                                                .to_string_lossy();
                                            if let Ok(v) = s.parse::<i128>() {
                                                *sum += v + add_const as i128 * cs.nonnull_count as i128;
                                                *count += cs.nonnull_count;
                                            }
                                            pg_sys::pfree(cstr as *mut _);
                                        }
                                        AggAccumulator::SumFloat { sum, count } => {
                                            // Sum datum is FLOAT8
                                            let f = f64::from_bits(cs.sum_datum.value() as u64);
                                            *sum += f + add_const as f64 * cs.nonnull_count as f64;
                                            *count += cs.nonnull_count;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            AggType::Min => {
                                let col_name = &meta.col_names[spec.col_idx as usize];
                                if let Some(cm) = seg.col_minmax.get(col_name) {
                                    if cm.min_null { continue; }
                                    match &mut accumulators[i] {
                                        AggAccumulator::MinInt { val } => {
                                            let v = cm.min_datum.value() as i64;
                                            *val = Some(val.map_or(v, |cur| cur.min(v)));
                                        }
                                        AggAccumulator::MinFloat { val } => {
                                            let v = f64::from_bits(cm.min_datum.value() as u64);
                                            *val = Some(val.map_or(v, |cur| if v < cur { v } else { cur }));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            AggType::Max => {
                                let col_name = &meta.col_names[spec.col_idx as usize];
                                if let Some(cm) = seg.col_minmax.get(col_name) {
                                    if cm.max_null { continue; }
                                    match &mut accumulators[i] {
                                        AggAccumulator::MaxInt { val } => {
                                            let v = cm.max_datum.value() as i64;
                                            *val = Some(val.map_or(v, |cur| cur.max(v)));
                                        }
                                        AggAccumulator::MaxFloat { val } => {
                                            let v = f64::from_bits(cm.max_datum.value() as u64);
                                            *val = Some(val.map_or(v, |cur| if v > cur { v } else { cur }));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Finalize accumulators
                let num_result_cols = output_map.len();
                let mut agg_results: Vec<(pg_sys::Datum, bool)> = Vec::new();
                for (i, acc) in accumulators.iter().enumerate() {
                    agg_results.push(finalize_accumulator(acc, &agg_specs[i]));
                }
                let mut row: Vec<(pg_sys::Datum, bool)> = Vec::with_capacity(num_result_cols);
                for entry in &output_map {
                    match entry {
                        OutputEntry::Agg(ai) => row.push(agg_results[*ai]),
                        OutputEntry::Group(_) => row.push((pg_sys::Datum::from(0usize), true)),
                    }
                }

                let total_segments = all_segments.len() as u64;
                let state = AggScanState {
                    _agg_specs: agg_specs,
                    _group_specs: group_specs,
                    result_rows: vec![row],
                    result_idx: 0,
                    _num_result_cols: num_result_cols,
                    metadata_us,
                    heap_scan_us,
                    decompress_us: 0,
                    agg_us: 0,
                    total_segments,
                    total_rows_processed: 0,
                    batch_quals_count: 0,
                    where_quals_null: true,
                    regex_cache_size: 0,
                    regex_cache_calls: 0,
                    topn_limit: 0,
                    topn_sort_col: 0,
                    topn_ascending: true,
                    pre_topn_groups: 0,
                };
                let state_ptr = Box::into_raw(Box::new(state));
                (*node).custom_ps = state_ptr as *mut pg_sys::List;
                return;
            }
            // If metadata not available, fall through to normal path
        }

        // Build needed_cols: only columns referenced by aggregates and group-by
        let num_cols = meta.col_names.len();
        let mut needed_cols = vec![false; num_cols];
        for spec in &agg_specs {
            if spec.col_idx >= 0 && (spec.col_idx as usize) < num_cols {
                needed_cols[spec.col_idx as usize] = true;
            }
        }
        for gs in &group_specs {
            if gs.col_idx >= 0 && (gs.col_idx as usize) < num_cols {
                needed_cols[gs.col_idx as usize] = true;
            }
        }

        // Build length_cols: columns where ALL referencing agg specs use LengthOf.
        // These columns will be decompressed as int4 lengths instead of text datums.
        let length_cols: Vec<bool> = (0..num_cols)
            .map(|col_idx| {
                let refs: Vec<&AggExecSpec> = agg_specs
                    .iter()
                    .filter(|s| s.col_idx >= 0 && s.col_idx as usize == col_idx)
                    .collect();
                !refs.is_empty() && refs.iter().all(|s| s.expr_kind == AggExpr::LengthOf)
            })
            .collect();

        // Extract batch quals and segment filters from WHERE clause (quals from custom_private)
        let (batch_quals, _handled_count) = extract_batch_quals(where_quals, &meta.col_names, &meta.col_types);

        for bq in &batch_quals {
            if bq.col_idx < num_cols {
                needed_cols[bq.col_idx] = true;
            }
        }
        let (seg_filters, time_min, time_max) = extract_segment_filters(
            where_quals,
            &meta.col_names,
            &meta.segment_by,
            &meta.time_column,
        );
        // Load segments from all companion tables (with lazy pruning)
        let t1 = Instant::now();
        let mut all_segments: Vec<SegmentData> = Vec::new();
        for &oid in &companion_oids {
            let (segs, _, _) = load_segments_heap(
                oid, &meta.col_names, &meta.segment_by, &needed_cols,
                &meta.time_column, false, &seg_filters, time_min, time_max, None,
                &batch_quals, false,
            );
            all_segments.extend(segs);
        }
        let heap_scan_us = t1.elapsed().as_micros() as u64;

        // Create per-segment memory context
        let query_ctx = (*(*node).ss.ps.state).es_query_cxt;
        let segment_mcxt = pg_sys::AllocSetContextCreateInternal(
            query_ctx,
            c"DeltaXAggSegment".as_ptr(),
            pg_sys::ALLOCSET_DEFAULT_MINSIZE as usize,
            pg_sys::ALLOCSET_DEFAULT_INITSIZE as usize,
            pg_sys::ALLOCSET_DEFAULT_MAXSIZE as usize,
        );

        // Initialize accumulators
        let has_group_by = !group_specs.is_empty();
        let num_result_cols = output_map.len();

        let prototype_accumulators: Vec<AggAccumulator> = agg_specs
            .iter()
            .map(|spec| AggAccumulator::new_for(spec.agg_type, spec.col_type_oid))
            .collect();

        let mut global_accumulators = if !has_group_by {
            Some(prototype_accumulators.iter().map(|a| a.clone_fresh()).collect::<Vec<_>>())
        } else {
            None
        };
        let mut group_map: GroupMap = GroupMap::with_hasher(BuildHasherDefault::default());
        let mut string_arena = StringArena::new();
        // Flat accumulator storage: group i's accumulators are at
        // flat_accs[i * n_agg_specs .. (i+1) * n_agg_specs]
        let n_agg_specs = agg_specs.len();
        let mut flat_accs: Vec<AggAccumulator> = Vec::new();
        let is_single_group_key = group_specs.len() == 1;

        // Check if any GROUP BY uses RegexpReplace — set up cross-segment caches
        let has_regexp_group = group_specs.iter().any(|gs| matches!(gs.expr, GroupByExpr::RegexpReplace { .. }));

        // Cross-segment regex dedup cache: input_string → regexp_replace result
        let mut regex_cache: HashMap<String, String> = HashMap::new();
        let mut regex_cache_calls: u64 = 0;

        // Build PG datums for regexp pattern/replacement (once, not per-segment)
        // and identify which columns need raw string decompression
        struct RegexpGroupInfo {
            group_idx: usize,
            func_oid: pg_sys::Oid,
            collation: pg_sys::Oid,
            pattern_datum: pg_sys::Datum,
            replacement_datum: pg_sys::Datum,
        }
        let mut regexp_group_infos: Vec<RegexpGroupInfo> = Vec::new();
        // Columns that need raw string decompression instead of PG datum decompression
        let mut raw_string_cols: Vec<bool> = vec![false; meta.col_names.len()];

        if has_regexp_group {
            for (gi, gs) in group_specs.iter().enumerate() {
                if let GroupByExpr::RegexpReplace { ref pattern, ref replacement, func_oid, collation } = gs.expr {
                    raw_string_cols[gs.col_idx as usize] = true;
                    let pattern_datum = {
                        let text = pg_sys::cstring_to_text_with_len(pattern.as_ptr() as *const _, pattern.len() as i32);
                        pg_sys::Datum::from(text as usize)
                    };
                    let replacement_datum = {
                        let text = pg_sys::cstring_to_text_with_len(replacement.as_ptr() as *const _, replacement.len() as i32);
                        pg_sys::Datum::from(text as usize)
                    };
                    regexp_group_infos.push(RegexpGroupInfo {
                        group_idx: gi,
                        func_oid: pg_sys::Oid::from(func_oid),
                        collation: pg_sys::Oid::from(collation),
                        pattern_datum,
                        replacement_datum,
                    });
                }
            }
        }

        // Also check if any agg references a raw_string_col for Min/Max on text
        // (e.g. MIN(Referer) where Referer is also the regexp GROUP BY column)

        // Identify text GROUP BY columns (dictionary or LZ4)
        let mut text_group_cols: Vec<bool> = vec![false; meta.col_names.len()];
        for gs in &group_specs {
            if matches!(gs.expr, GroupByExpr::Column)
                && (gs.type_oid == pg_sys::TEXTOID
                    || gs.type_oid == pg_sys::VARCHAROID
                    || gs.type_oid == pg_sys::BPCHAROID
                    || gs.type_oid == pg_sys::NAMEOID)
            {
                text_group_cols[gs.col_idx as usize] = true;
            }
        }

        // Per-segment decoded text data for GROUP BY columns.
        // Keeps decompressed string data alive during the row loop,
        // providing O(1) &str access per row without interning.
        let mut seg_text_columns: Vec<Option<SegTextColumn>> = Vec::new();

        let t2 = Instant::now();
        let mut total_segments: u64 = 0;
        let mut total_rows_processed: u64 = 0;
        let mut decompress_us: u64 = 0;

        for seg in &all_segments {
            if seg.row_count == 0 {
                continue;
            }

            // Segment-by pruning
            if !seg_filters.is_empty() {
                let mut skip = false;
                for &(seg_val_idx, ref filter_val) in &seg_filters {
                    match &seg.segment_values[seg_val_idx] {
                        Some(val) if val == filter_val => {}
                        _ => { skip = true; break; }
                    }
                }
                if skip { continue; }
            }

            // Time-range pruning
            if let (Some(seg_min), Some(seg_max)) = (seg.min_time, seg.max_time) {
                if time_min.is_some_and(|query_min| seg_max < query_min) { continue; }
                if time_max.is_some_and(|query_max| seg_min > query_max) { continue; }
            }

            // Dictionary-based LIKE pruning: skip segment if no dict entry matches
            if segment_skippable_by_dict_like(
                &batch_quals, &meta.col_names, &meta.segment_by, &seg.compressed_blobs,
            ) {
                continue;
            }

            total_segments += 1;

            // Decompress needed columns
            let t_dec = Instant::now();
            pg_sys::MemoryContextReset(segment_mcxt);
            let old_ctx = pg_sys::MemoryContextSwitchTo(segment_mcxt);

            let mut decompressed: Vec<Vec<(pg_sys::Datum, bool)>> = Vec::new();
            // Raw strings for columns that need regexp_replace (parallel to decompressed)
            let mut raw_strings: Vec<Option<Vec<Option<String>>>> = Vec::new();
            let mut blob_idx = 0;
            let mut seg_val_idx = 0;
            let mut pre_selection: Vec<bool> = Vec::new();

            for (col_idx, col_name) in meta.col_names.iter().enumerate() {
                let type_oid = meta.col_types[col_idx];

                if !needed_cols[col_idx] {
                    if meta.segment_by.contains(col_name) {
                        seg_val_idx += 1;
                    } else {
                        blob_idx += 1;
                    }
                    decompressed.push(Vec::new());
                    raw_strings.push(None);
                    continue;
                }

                if meta.segment_by.contains(col_name) {
                    let val = &seg.segment_values[seg_val_idx];
                    let (datum, is_null) = match val {
                        Some(s) => (string_to_datum(s, type_oid), false),
                        None => (pg_sys::Datum::from(0), true),
                    };
                    let repeated: Vec<(pg_sys::Datum, bool)> =
                        (0..seg.row_count).map(|_| (datum, is_null)).collect();
                    decompressed.push(repeated);
                    raw_strings.push(None);
                    seg_val_idx += 1;
                } else {
                    let blob = &seg.compressed_blobs[blob_idx];
                    let typmod = meta.col_typmods[col_idx];

                    if raw_string_cols[col_idx] {
                        // Dictionary-optimized path: pre-warm regex cache from dict entries only
                        let cc_ref = compression::CompressedColumnRef::from_bytes(blob);
                        if cc_ref.type_tag == compression::CompressionType::Dictionary
                            || cc_ref.type_tag == compression::CompressionType::DictionaryLz4
                        {
                            let total_count = cc_ref.row_count as usize;
                            let non_null_count = count_non_null(cc_ref.null_bitmap, total_count);
                            let norm_buf;
                            let dict_data = if cc_ref.type_tag == compression::CompressionType::DictionaryLz4 {
                                norm_buf = compression::dictionary::normalize_lz4(cc_ref.data);
                                &norm_buf[..]
                            } else {
                                cc_ref.data
                            };
                            let (dict_entries, indices) =
                                compression::dictionary::decode_dict_and_indices(dict_data, non_null_count);

                            // Pre-warm regex cache from dict entries only — O(dict_size) calls
                            for &entry in &dict_entries {
                                let key = entry.to_string();
                                if !regex_cache.contains_key(&key) {
                                    for rgi in &regexp_group_infos {
                                        if group_specs[rgi.group_idx].col_idx as usize == col_idx {
                                            regex_cache_calls += 1;
                                            let input_datum = {
                                                let text = pg_sys::cstring_to_text_with_len(
                                                    entry.as_ptr() as *const _, entry.len() as i32,
                                                );
                                                pg_sys::Datum::from(text as usize)
                                            };
                                            let result_datum = pg_sys::OidFunctionCall3Coll(
                                                rgi.func_oid,
                                                rgi.collation,
                                                input_datum,
                                                rgi.pattern_datum,
                                                rgi.replacement_datum,
                                            );
                                            let cstr = pg_sys::text_to_cstring(result_datum.cast_mut_ptr());
                                            let s = std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned();
                                            pg_sys::pfree(cstr as *mut _);
                                            regex_cache.insert(key.clone(), s);
                                            break;
                                        }
                                    }
                                }
                            }

                            // Build per-row strings from cached regex results via dict index
                            let has_ne_empty = batch_quals.iter().any(|bq| {
                                bq.col_idx == col_idx
                                    && bq.text_const.as_deref() == Some("")
                                    && bq.op == BatchCompareOp::Ne
                            });
                            let ne_sel = if has_ne_empty {
                                compression::dictionary::check_ne_empty(dict_data, non_null_count)
                            } else {
                                Vec::new()
                            };

                            let nn_strings: Vec<String> = indices
                                .iter()
                                .map(|&idx| dict_entries[idx as usize].to_string())
                                .collect();

                            // Reinsert nulls
                            if cc_ref.null_bitmap.is_empty() {
                                let strings: Vec<Option<String>> = nn_strings.into_iter().map(Some).collect();
                                let datums: Vec<(pg_sys::Datum, bool)> = strings.iter().map(|s| {
                                    match s {
                                        Some(_) => (pg_sys::Datum::from(0usize), false),
                                        None => (pg_sys::Datum::from(0usize), true),
                                    }
                                }).collect();
                                decompressed.push(datums);
                                raw_strings.push(Some(strings));
                                if !ne_sel.is_empty() {
                                    if pre_selection.is_empty() {
                                        pre_selection = ne_sel;
                                    } else {
                                        for (ps, s) in pre_selection.iter_mut().zip(ne_sel.iter()) {
                                            *ps = *ps && *s;
                                        }
                                    }
                                }
                            } else {
                                let mut strings = Vec::with_capacity(total_count);
                                let mut sel = if has_ne_empty { Vec::with_capacity(total_count) } else { Vec::new() };
                                let mut val_idx = 0;
                                for i in 0..total_count {
                                    let is_null = (cc_ref.null_bitmap[i / 8] >> (i % 8)) & 1 == 1;
                                    if is_null {
                                        strings.push(None);
                                        if has_ne_empty { sel.push(false); }
                                    } else {
                                        strings.push(Some(nn_strings[val_idx].clone()));
                                        if has_ne_empty && !ne_sel.is_empty() { sel.push(ne_sel[val_idx]); }
                                        else if has_ne_empty { sel.push(true); }
                                        val_idx += 1;
                                    }
                                }
                                let datums: Vec<(pg_sys::Datum, bool)> = strings.iter().map(|s| {
                                    match s {
                                        Some(_) => (pg_sys::Datum::from(0usize), false),
                                        None => (pg_sys::Datum::from(0usize), true),
                                    }
                                }).collect();
                                decompressed.push(datums);
                                raw_strings.push(Some(strings));
                                if !sel.is_empty() {
                                    if pre_selection.is_empty() {
                                        pre_selection = sel;
                                    } else {
                                        for (ps, s) in pre_selection.iter_mut().zip(sel.iter()) {
                                            *ps = *ps && *s;
                                        }
                                    }
                                }
                            }
                        } else {
                            // Non-dictionary: fall back to existing path
                            let (strings, sel) = decompress_text_blob_to_raw_strings(blob, &batch_quals, col_idx);
                            let datums: Vec<(pg_sys::Datum, bool)> = strings.iter().map(|s| {
                                match s {
                                    Some(_) => (pg_sys::Datum::from(0usize), false),
                                    None => (pg_sys::Datum::from(0usize), true),
                                }
                            }).collect();
                            decompressed.push(datums);
                            raw_strings.push(Some(strings));
                            if !sel.is_empty() {
                                if pre_selection.is_empty() {
                                    pre_selection = sel;
                                } else {
                                    for (ps, s) in pre_selection.iter_mut().zip(sel.iter()) {
                                        *ps = *ps && *s;
                                    }
                                }
                            }
                        }
                    } else if length_cols[col_idx] {
                        // Length-only column: decompress as int4 lengths.
                        let has_ne_empty = batch_quals.iter().any(|bq| {
                            bq.col_idx == col_idx
                                && bq.text_const.as_deref() == Some("")
                                && bq.op == BatchCompareOp::Ne
                        });
                        let (datums, len_sel) = decompress_text_blob_to_lengths(blob, has_ne_empty);
                        decompressed.push(datums);
                        raw_strings.push(None);
                        if !len_sel.is_empty() {
                            if pre_selection.is_empty() {
                                pre_selection = len_sel;
                            } else {
                                for (ps, ls) in pre_selection.iter_mut().zip(len_sel.iter()) {
                                    *ps = *ps && *ls;
                                }
                            }
                        }
                    } else {
                        // Normal text/non-text column
                        let like_qual = batch_quals.iter().find(|bq| {
                            bq.col_idx == col_idx
                                && matches!(bq.op, BatchCompareOp::Like | BatchCompareOp::NotLike)
                        });

                        let text_eq_qual = batch_quals.iter().find(|bq| {
                            bq.col_idx == col_idx
                                && bq.text_const.is_some()
                                && matches!(bq.op, BatchCompareOp::Eq | BatchCompareOp::Ne)
                        });

                        if let Some(bq) = like_qual {
                            let strat = bq.like_strategy.as_ref().unwrap();
                            let neg = bq.op == BatchCompareOp::NotLike;
                            let (datums, like_sel) =
                                decompress_text_blob_with_like_filter(blob, type_oid, typmod, strat, neg);
                            decompressed.push(datums);
                            if pre_selection.is_empty() {
                                pre_selection = like_sel;
                            } else {
                                for (ps, ls) in pre_selection.iter_mut().zip(like_sel.iter()) {
                                    *ps = *ps && *ls;
                                }
                            }
                        } else if let Some(bq) = text_eq_qual {
                            let const_str = bq.text_const.as_ref().unwrap();
                            let is_ne = bq.op == BatchCompareOp::Ne;
                            let (datums, eq_sel) = decompress_text_blob_with_eq_filter(
                                blob, type_oid, typmod, const_str, is_ne,
                            );
                            decompressed.push(datums);
                            if pre_selection.is_empty() {
                                pre_selection = eq_sel;
                            } else {
                                for (ps, es) in pre_selection.iter_mut().zip(eq_sel.iter()) {
                                    *ps = *ps && *es;
                                }
                            }
                        } else {
                            let type_name = pg_type_name(type_oid);
                            let datums = decompress_blob_to_datums(blob, &type_name, type_oid, typmod);
                            decompressed.push(datums);
                        }
                        raw_strings.push(None);
                    }
                    blob_idx += 1;
                }
            }

            pg_sys::MemoryContextSwitchTo(old_ctx);
            decompress_us += t_dec.elapsed().as_micros() as u64;

            let row_count = seg.row_count as usize;

            // Extract text GROUP BY info: intern strings and build per-row u32 ID vectors.
            // Handles both dictionary-encoded and LZ4-encoded text columns.
            // Build per-segment text column data for GROUP BY.
            // Keeps decoded string data alive during the row loop for O(1) &str access.
            seg_text_columns.clear();
            seg_text_columns.resize_with(meta.col_names.len(), || None);
            {
                let mut blob_idx2 = 0;
                let mut seg_val_idx2 = 0;
                for (col_idx, col_name) in meta.col_names.iter().enumerate() {
                    if meta.segment_by.contains(col_name) {
                        if needed_cols[col_idx] && text_group_cols[col_idx] {
                            let val = &seg.segment_values[seg_val_idx2];
                            seg_text_columns[col_idx] = Some(SegTextColumn::SegBy(val.clone()));
                        }
                        seg_val_idx2 += 1;
                        continue;
                    }
                    if needed_cols[col_idx] && text_group_cols[col_idx] {
                        let blob = &seg.compressed_blobs[blob_idx2];
                        if !blob.is_empty() {
                            let cc_ref = compression::CompressedColumnRef::from_bytes(blob);
                            let total = cc_ref.row_count as usize;
                            let nn_count = count_non_null(cc_ref.null_bitmap, total);

                            let seg_col = match cc_ref.type_tag {
                                compression::CompressionType::Dictionary
                                | compression::CompressionType::DictionaryLz4 => {
                                    let norm_buf;
                                    let dict_data = if cc_ref.type_tag == compression::CompressionType::DictionaryLz4 {
                                        norm_buf = compression::dictionary::normalize_lz4(cc_ref.data);
                                        &norm_buf[..]
                                    } else {
                                        cc_ref.data
                                    };
                                    let (dict_entries, nn_indices) =
                                        compression::dictionary::decode_dict_and_indices(dict_data, nn_count);
                                    let entries: Vec<String> = dict_entries.iter().map(|&s| s.to_string()).collect();

                                    // Expand nn_indices to full-row indices (u32::MAX for nulls)
                                    let row_to_entry = if cc_ref.null_bitmap.is_empty() {
                                        nn_indices.iter().map(|&idx| idx as u32).collect()
                                    } else {
                                        let mut re = Vec::with_capacity(total);
                                        let mut vi = 0;
                                        for i in 0..total {
                                            let is_null = (cc_ref.null_bitmap[i / 8] >> (i % 8)) & 1 == 1;
                                            if is_null {
                                                re.push(u32::MAX);
                                            } else {
                                                re.push(nn_indices[vi] as u32);
                                                vi += 1;
                                            }
                                        }
                                        re
                                    };
                                    SegTextColumn::Dict { entries, row_to_entry }
                                }
                                compression::CompressionType::Lz4 | compression::CompressionType::Lz4Blocked => {
                                    let (buf, ranges) = if cc_ref.type_tag == compression::CompressionType::Lz4 {
                                        compression::lz4::decode_to_ranges(cc_ref.data, nn_count)
                                    } else {
                                        compression::lz4::decode_to_ranges_blocked(cc_ref.data, nn_count, None)
                                    };

                                    // Expand ranges to full-row ranges (u32::MAX for nulls)
                                    let row_to_range = if cc_ref.null_bitmap.is_empty() {
                                        ranges.iter().map(|&(off, len)| (off as u32, len as u16)).collect()
                                    } else {
                                        let mut rr = Vec::with_capacity(total);
                                        let mut vi = 0;
                                        for i in 0..total {
                                            let is_null = (cc_ref.null_bitmap[i / 8] >> (i % 8)) & 1 == 1;
                                            if is_null {
                                                rr.push((u32::MAX, 0u16));
                                            } else {
                                                let (off, len) = ranges[vi];
                                                rr.push((off as u32, len as u16));
                                                vi += 1;
                                            }
                                        }
                                        rr
                                    };
                                    SegTextColumn::Lz4 { buf, row_to_range }
                                }
                                _ => {
                                    blob_idx2 += 1;
                                    continue;
                                }
                            };
                            seg_text_columns[col_idx] = Some(seg_col);
                        }
                    }
                    blob_idx2 += 1;
                }
            }

            // Evaluate batch quals (WHERE) if any.
            // pre_selection seeds the selection vector so that rows already
            // filtered by LIKE during decompression are skipped (their dummy
            // datums are never dereferenced).
            let selection = evaluate_batch_quals(&decompressed, row_count, &batch_quals, pre_selection);




            // Fast path: when no GROUP BY and all agg specs are SUM/AVG on the
            // same column with Column or AddConst expr, compute base_sum once
            // and derive each result as base_sum + const_offset * non_null_count.
            // This turns O(N * num_aggs) into O(N + num_aggs).
            if !has_group_by && agg_specs.len() > 1 {
                let first_col = agg_specs[0].col_idx;
                let first_type = agg_specs[0].col_type_oid;
                let all_same_col_sum = agg_specs.iter().all(|s| {
                    s.col_idx == first_col
                        && (s.agg_type == AggType::Sum || s.agg_type == AggType::Avg)
                        && (s.expr_kind == AggExpr::Column || s.expr_kind == AggExpr::AddConst)
                });
                if all_same_col_sum {
                    let col = &decompressed[first_col as usize];
                    if !col.is_empty() {
                        let accumulators = global_accumulators.as_mut().unwrap();
                        let mut base_sum: i128 = 0;
                        let mut non_null_count: i64 = 0;
                        let use_float = matches!(first_type, pg_sys::FLOAT4OID | pg_sys::FLOAT8OID);
                        let mut base_sum_f: f64 = 0.0;
                        for row in 0..row_count {
                            if !selection.is_empty() && !selection[row] {
                                continue;
                            }
                            if !col[row].1 {
                                if use_float {
                                    base_sum_f += datum_to_f64(col[row].0, first_type);
                                } else {
                                    base_sum += datum_to_i128(col[row].0, first_type);
                                }
                                non_null_count += 1;
                            }
                        }
                        total_rows_processed += if selection.is_empty() {
                            row_count as u64
                        } else {
                            selection.iter().filter(|&&v| v).count() as u64
                        };
                        for (spec_idx, spec) in agg_specs.iter().enumerate() {
                            let acc = &mut accumulators[spec_idx];
                            if use_float {
                                if let AggAccumulator::SumFloat { sum, count } = acc {
                                    *sum += base_sum_f + spec.const_offset as f64 * non_null_count as f64;
                                    *count += non_null_count;
                                }
                            } else {
                                if let AggAccumulator::SumInt { sum, count } = acc {
                                    *sum += base_sum + spec.const_offset as i128 * non_null_count as i128;
                                    *count += non_null_count;
                                }
                            }
                        }
                        continue; // skip the generic aggregate loop for this segment
                    }
                }
            }

            // Reusable buffers for the aggregate loop (avoid per-row heap allocation)
            let mut key_ref: Vec<GroupKeyRef> = Vec::with_capacity(group_specs.len());
            let mut regex_results: Vec<Option<String>> = Vec::new();

            // Aggregate loop
            for row in 0..row_count {
                if !selection.is_empty() && !selection[row] {
                    continue;
                }

                total_rows_processed += 1;

                let accumulators = if has_group_by {
                    // Clear key_ref first to release borrows on regex_results
                    key_ref.clear();
                    // Pre-compute regex results for this row (needs mutable regex_cache,
                    // so must be done before building borrowed key_ref)
                    regex_results.clear();
                    if has_regexp_group {
                        for (gi, gs) in group_specs.iter().enumerate() {
                            if let GroupByExpr::RegexpReplace { .. } = &gs.expr {
                                let rs = raw_strings[gs.col_idx as usize].as_ref().unwrap();
                                if let Some(ref input_str) = rs[row] {
                                    let rgi = regexp_group_infos.iter().find(|r| r.group_idx == gi).unwrap();
                                    let result = regex_cache.entry(input_str.clone()).or_insert_with(|| {
                                        regex_cache_calls += 1;
                                        let input_datum = {
                                            let text = pg_sys::cstring_to_text_with_len(
                                                input_str.as_ptr() as *const _, input_str.len() as i32,
                                            );
                                            pg_sys::Datum::from(text as usize)
                                        };
                                        let result_datum = pg_sys::OidFunctionCall3Coll(
                                            rgi.func_oid,
                                            rgi.collation,
                                            input_datum,
                                            rgi.pattern_datum,
                                            rgi.replacement_datum,
                                        );
                                        let cstr = pg_sys::text_to_cstring(result_datum.cast_mut_ptr());
                                        let s = std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned();
                                        pg_sys::pfree(cstr as *mut _);
                                        s
                                    });
                                    regex_results.push(Some(result.clone()));
                                } else {
                                    regex_results.push(None);
                                }
                            }
                        }
                    }

                    // Build temporary borrowed key (reuse buffer, no heap alloc)
                    let mut regex_idx = 0;
                    for gs in group_specs.iter() {
                        let col = &decompressed[gs.col_idx as usize];
                        if col.is_empty() || col[row].1 {
                            key_ref.push(GroupKeyRef::Null);
                            if matches!(&gs.expr, GroupByExpr::RegexpReplace { .. }) {
                                regex_idx += 1;
                            }
                        } else {
                            match &gs.expr {
                                GroupByExpr::RegexpReplace { .. } => {
                                    match &regex_results[regex_idx] {
                                        Some(s) => key_ref.push(GroupKeyRef::from_str(s.as_str())),
                                        None => key_ref.push(GroupKeyRef::Null),
                                    }
                                    regex_idx += 1;
                                }
                                GroupByExpr::DateTrunc { unit_usecs, .. } => {
                                    let pg_usec = col[row].0.value() as i64;
                                    let truncated = pg_usec.div_euclid(*unit_usecs) * *unit_usecs;
                                    key_ref.push(GroupKeyRef::Int(truncated));
                                }
                                GroupByExpr::Extract { unit, .. } => {
                                    let pg_usec = col[row].0.value() as i64;
                                    let extracted = extract_field_from_usecs(pg_usec, unit);
                                    key_ref.push(GroupKeyRef::Int(extracted));
                                }
                                GroupByExpr::AddConst { offset, .. } => {
                                    let datum = col[row].0;
                                    let v = datum.value() as i64;
                                    key_ref.push(GroupKeyRef::Int(v + offset));
                                }
                                GroupByExpr::Column => {
                                    // Text GROUP BY: get &str from decoded segment data
                                    if let Some(ref seg_text) = seg_text_columns[gs.col_idx as usize] {
                                        match seg_text.get_str(row) {
                                            Some(s) => key_ref.push(GroupKeyRef::from_str(s)),
                                            None => key_ref.push(GroupKeyRef::Null),
                                        }
                                    } else {
                                        let datum = col[row].0;
                                        key_ref.push(GroupKeyRef::Int(datum.value() as i64));
                                    }
                                }
                            }
                        }
                    }

                    // Use hashbrown raw_entry to avoid cloning the key for existing groups
                    let h = hash_group_key_ref(&key_ref);
                    let group_idx = match group_map.raw_entry_mut().from_hash(h, |stored| keys_match(stored, &key_ref, &string_arena)) {
                        hashbrown::hash_map::RawEntryMut::Occupied(e) => {
                            *e.into_mut()
                        }
                        hashbrown::hash_map::RawEntryMut::Vacant(e) => {
                            let owned_key = if is_single_group_key {
                                GroupKey::Single(key_ref[0].resolve(&mut string_arena))
                            } else {
                                GroupKey::Multi(key_ref.iter().map(|r| r.resolve(&mut string_arena)).collect())
                            };
                            let idx = (flat_accs.len() / n_agg_specs) as u32;
                            for proto in &prototype_accumulators {
                                flat_accs.push(proto.clone_fresh());
                            }
                            e.insert_with_hasher(h, owned_key, idx, |k| hash_group_key(k, &string_arena));
                            idx
                        }
                    };
                    &mut flat_accs[group_idx as usize * n_agg_specs .. (group_idx as usize + 1) * n_agg_specs]
                } else {
                    global_accumulators.as_mut().unwrap().as_mut_slice()
                };

                for (spec_idx, spec) in agg_specs.iter().enumerate() {
                    let acc = &mut accumulators[spec_idx];
                    match spec.agg_type {
                        AggType::CountStar => {
                            if let AggAccumulator::Count { count } = acc {
                                *count += 1;
                            }
                        }
                        AggType::Count => {
                            let col = &decompressed[spec.col_idx as usize];
                            if !col.is_empty() && !col[row].1
                                && let AggAccumulator::Count { count } = acc
                            {
                                *count += 1;
                            }
                        }
                        AggType::Sum | AggType::Avg => {
                            // When LengthOf + raw_string_cols, compute length from raw strings
                            // (decompressed has dummy 0 datums for raw_string_cols columns)
                            if spec.expr_kind == AggExpr::LengthOf
                                && raw_string_cols.get(spec.col_idx as usize).copied().unwrap_or(false)
                            {
                                if let Some(ref rs) = raw_strings[spec.col_idx as usize]
                                    && let Some(ref s) = rs[row]
                                {
                                    match acc {
                                        AggAccumulator::SumInt { sum, count } => {
                                            *sum += s.chars().count() as i128;
                                            *count += 1;
                                        }
                                        AggAccumulator::SumFloat { sum, count } => {
                                            *sum += s.chars().count() as f64;
                                            *count += 1;
                                        }
                                        _ => {}
                                    }
                                }
                            } else {
                                let col = &decompressed[spec.col_idx as usize];
                                if !col.is_empty() && !col[row].1 {
                                    let datum = col[row].0;
                                    match acc {
                                        AggAccumulator::SumInt { sum, count } => {
                                            let v = datum_to_i128(datum, spec.col_type_oid);
                                            if spec.expr_kind == AggExpr::AddConst {
                                                *sum += v + spec.const_offset as i128;
                                            } else {
                                                *sum += v;
                                            }
                                            *count += 1;
                                        }
                                        AggAccumulator::SumFloat { sum, count } => {
                                            let v = datum_to_f64(datum, spec.col_type_oid);
                                            if spec.expr_kind == AggExpr::AddConst {
                                                *sum += v + spec.const_offset as f64;
                                            } else {
                                                *sum += v;
                                            }
                                            *count += 1;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        AggType::CountDistinct => {
                            let col = &decompressed[spec.col_idx as usize];
                            if !col.is_empty() && !col[row].1 {
                                let datum = col[row].0;
                                match acc {
                                    AggAccumulator::CountDistinctInt { seen } => {
                                        seen.insert(datum.value() as i64);
                                    }
                                    AggAccumulator::CountDistinctStr { seen } => {
                                        let cstr = pg_sys::text_to_cstring(datum.cast_mut_ptr());
                                        let s = std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned();
                                        pg_sys::pfree(cstr as *mut _);
                                        seen.insert(s);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        AggType::Min => {
                            // For text columns referenced by raw_string_cols, use raw strings
                            if raw_string_cols.get(spec.col_idx as usize).copied().unwrap_or(false) {
                                if let Some(ref rs) = raw_strings[spec.col_idx as usize]
                                    && let Some(ref s) = rs[row]
                                    && let AggAccumulator::MinStr { val } = acc
                                    && val.as_ref().is_none_or(|cur| collation_strcmp(s, cur) < 0)
                                {
                                    *val = Some(s.clone());
                                }
                            } else {
                                let col = &decompressed[spec.col_idx as usize];
                                if !col.is_empty() && !col[row].1 {
                                    let datum = col[row].0;
                                    match acc {
                                        AggAccumulator::MinInt { val } => {
                                            let v = datum.value() as i64;
                                            if val.is_none_or(|cur| v < cur) {
                                                *val = Some(v);
                                            }
                                        }
                                        AggAccumulator::MinFloat { val } => {
                                            let v = datum_to_f64(datum, spec.col_type_oid);
                                            if val.is_none_or(|cur| v < cur) {
                                                *val = Some(v);
                                            }
                                        }
                                        AggAccumulator::MinStr { val } => {
                                            let cstr = pg_sys::text_to_cstring(datum.cast_mut_ptr());
                                            let s = std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned();
                                            pg_sys::pfree(cstr as *mut _);
                                            if val.as_ref().is_none_or(|cur| collation_strcmp(&s, cur) < 0) {
                                                *val = Some(s);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        AggType::Max => {
                            if raw_string_cols.get(spec.col_idx as usize).copied().unwrap_or(false) {
                                if let Some(ref rs) = raw_strings[spec.col_idx as usize]
                                    && let Some(ref s) = rs[row]
                                    && let AggAccumulator::MaxStr { val } = acc
                                    && val.as_ref().is_none_or(|cur| collation_strcmp(s, cur) > 0)
                                {
                                    *val = Some(s.clone());
                                }
                            } else {
                                let col = &decompressed[spec.col_idx as usize];
                                if !col.is_empty() && !col[row].1 {
                                    let datum = col[row].0;
                                    match acc {
                                        AggAccumulator::MaxInt { val } => {
                                            let v = datum.value() as i64;
                                            if val.is_none_or(|cur| v > cur) {
                                                *val = Some(v);
                                            }
                                        }
                                        AggAccumulator::MaxFloat { val } => {
                                            let v = datum_to_f64(datum, spec.col_type_oid);
                                            if val.is_none_or(|cur| v > cur) {
                                                *val = Some(v);
                                            }
                                        }
                                        AggAccumulator::MaxStr { val } => {
                                            let cstr = pg_sys::text_to_cstring(datum.cast_mut_ptr());
                                            let s = std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned();
                                            pg_sys::pfree(cstr as *mut _);
                                            if val.as_ref().is_none_or(|cur| collation_strcmp(&s, cur) > 0) {
                                                *val = Some(s);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }



        }

        let agg_us = t2.elapsed().as_micros() as u64 - decompress_us;

        // Finalize results using output mapping, applying HAVING filters
        let mut result_rows = if has_group_by {
            let mut rows = Vec::new();
            // Pre-finalize all agg results keyed by group
            'group_loop: for (key, &group_idx) in &group_map {
                let accs = &flat_accs[group_idx as usize * n_agg_specs .. (group_idx as usize + 1) * n_agg_specs];
                let mut agg_results: Vec<(pg_sys::Datum, bool)> = Vec::new();
                for (spec_idx, spec) in agg_specs.iter().enumerate() {
                    agg_results.push(finalize_accumulator(&accs[spec_idx], spec));
                }

                // Apply HAVING filters on finalized aggregate values
                for hf in &having_filters {
                    let (datum, is_null) = agg_results[hf.agg_idx];
                    if is_null {
                        continue 'group_loop; // NULL doesn't satisfy HAVING
                    }
                    let val = datum.value() as i64;
                    let pass = match hf.op {
                        HavingOp::Gt => val > hf.const_val,
                        HavingOp::Lt => val < hf.const_val,
                        HavingOp::Ge => val >= hf.const_val,
                        HavingOp::Le => val <= hf.const_val,
                        HavingOp::Eq => val == hf.const_val,
                        HavingOp::Ne => val != hf.const_val,
                    };
                    if !pass {
                        continue 'group_loop;
                    }
                }

                let key_slice = key.as_slice();
                let mut row: Vec<(pg_sys::Datum, bool)> = Vec::with_capacity(num_result_cols);
                for entry in &output_map {
                    match entry {
                        OutputEntry::Agg(ai) => {
                            row.push(agg_results[*ai]);
                        }
                        OutputEntry::Group(gi) => {
                            match &key_slice[*gi] {
                                GroupKeyVal::Null => {
                                    row.push((pg_sys::Datum::from(0usize), true));
                                }
                                GroupKeyVal::Int(v) => {
                                    if matches!(group_specs[*gi].expr, GroupByExpr::Extract { .. }) {
                                        // extract() returns numeric — convert i64 to numeric datum
                                        row.push((i128_to_numeric_datum(*v as i128), false));
                                    } else {
                                        row.push((pg_sys::Datum::from(*v as usize), false));
                                    }
                                }
                                GroupKeyVal::Str(off, len) => {
                                    let s = string_arena.get(*off, *len);
                                    let datum = string_to_datum(s, group_specs[*gi].type_oid);
                                    row.push((datum, false));
                                }
                            }
                        }
                    }
                }
                rows.push(row);
            }
            rows
        } else if let Some(accumulators) = &global_accumulators {
            let mut agg_results: Vec<(pg_sys::Datum, bool)> = Vec::new();
            for (spec_idx, spec) in agg_specs.iter().enumerate() {
                agg_results.push(finalize_accumulator(&accumulators[spec_idx], spec));
            }
            let mut row: Vec<(pg_sys::Datum, bool)> = Vec::with_capacity(num_result_cols);
            for entry in &output_map {
                match entry {
                    OutputEntry::Agg(ai) => {
                        row.push(agg_results[*ai]);
                    }
                    OutputEntry::Group(_) => {
                        row.push((pg_sys::Datum::from(0usize), true));
                    }
                }
            }
            vec![row]
        } else {
            vec![]
        };

        // Apply top-N: sort by the specified output column and truncate
        let pre_topn_groups = result_rows.len();
        if topn_limit > 0 && has_group_by && result_rows.len() > topn_limit as usize {
            let si = topn_sort_col;
            if topn_ascending {
                result_rows.sort_by_key(|row| {
                    let (datum, is_null) = row[si];
                    if is_null { i64::MAX } else { datum.value() as i64 }
                });
            } else {
                result_rows.sort_by(|a, b| {
                    let (da, na) = a[si];
                    let (db, nb) = b[si];
                    let va = if na { i64::MIN } else { da.value() as i64 };
                    let vb = if nb { i64::MIN } else { db.value() as i64 };
                    vb.cmp(&va) // reverse order for DESC
                });
            }
            result_rows.truncate(topn_limit as usize);
        }

        // Clean up segment memory context
        if !segment_mcxt.is_null() {
            pg_sys::MemoryContextDelete(segment_mcxt);
        }

        let state = AggScanState {
            _agg_specs: agg_specs,
            _group_specs: group_specs,
            result_rows,
            result_idx: 0,
            _num_result_cols: num_result_cols,
            metadata_us,
            heap_scan_us,
            decompress_us,
            agg_us,
            total_segments,
            total_rows_processed,
            batch_quals_count: batch_quals.len(),
            where_quals_null: where_quals.is_null(),
            regex_cache_size: regex_cache.len() as u64,
            regex_cache_calls,
            topn_limit: if topn_limit > 0 { topn_limit as u64 } else { 0 },
            topn_sort_col: topn_sort_col as i64,
            topn_ascending,
            pre_topn_groups: pre_topn_groups as u64,
        };

        let state_box = Box::new(state);
        let state_ptr = Box::into_raw(state_box);
        (*node).custom_ps = state_ptr as *mut pg_sys::List;
    }
}

/// String arena: all group key strings packed into one Vec<u8>.
/// One deallocation instead of 275K individual String deallocations.
struct StringArena {
    buf: Vec<u8>,
}

impl StringArena {
    fn new() -> Self { Self { buf: Vec::new() } }

    fn alloc(&mut self, s: &str) -> (u32, u32) {
        let off = self.buf.len() as u32;
        let len = s.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        (off, len)
    }

    fn get(&self, off: u32, len: u32) -> &str {
        std::str::from_utf8(&self.buf[off as usize..off as usize + len as usize]).unwrap_or("")
    }
}

/// Group key value for HashMap key (owned).
/// Str variant stores (offset, len) into a StringArena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKeyVal {
    Null,
    Int(i64),
    Str(u32, u32), // (offset, len) into StringArena
}

/// Group key that avoids heap allocation for the common single-column case.
/// For high-cardinality GROUP BY (275K+ groups), eliminating per-key Vec
/// allocation saves ~130ms of cleanup overhead when the HashMap is dropped.
enum GroupKey {
    Single(GroupKeyVal),
    Multi(Box<[GroupKeyVal]>),
}

impl GroupKey {
    fn as_slice(&self) -> &[GroupKeyVal] {
        match self {
            GroupKey::Single(v) => std::slice::from_ref(v),
            GroupKey::Multi(v) => v,
        }
    }
}

/// Borrowed version of GroupKeyVal for hash lookups without allocation.
#[derive(Debug, Clone, Copy)]
/// Borrowed group key component without lifetime parameter.
/// Uses raw pointer for strings to avoid borrow-checker conflicts when reusing
/// the key buffer across loop iterations while mutating regex_results.
/// SAFETY: The pointed-to str data must outlive the current row iteration
/// (guaranteed by seg_text_columns and regex_results living across the loop).
enum GroupKeyRef {
    Null,
    Int(i64),
    Str(*const str),
}

impl GroupKeyRef {
    /// Create a Str variant from a &str. The caller must ensure the str outlives this GroupKeyRef.
    fn from_str(s: &str) -> Self {
        GroupKeyRef::Str(s as *const str)
    }

    fn resolve(&self, arena: &mut StringArena) -> GroupKeyVal {
        match self {
            GroupKeyRef::Null => GroupKeyVal::Null,
            GroupKeyRef::Int(v) => GroupKeyVal::Int(*v),
            GroupKeyRef::Str(p) => {
                // SAFETY: pointer is valid for the current row iteration
                let s = unsafe { &**p };
                let (off, len) = arena.alloc(s);
                GroupKeyVal::Str(off, len)
            }
        }
    }

    fn matches_owned(&self, owned: &GroupKeyVal, arena: &StringArena) -> bool {
        match (self, owned) {
            (GroupKeyRef::Null, GroupKeyVal::Null) => true,
            (GroupKeyRef::Int(a), GroupKeyVal::Int(b)) => a == b,
            (GroupKeyRef::Str(p), GroupKeyVal::Str(off, len)) => {
                // SAFETY: pointer is valid for the current row iteration
                let s = unsafe { &**p };
                s == arena.get(*off, *len)
            }
            _ => false,
        }
    }
}

/// Hash a group key component into a Hasher with a type discriminant.
fn hash_key_component<H: Hasher>(h: &mut H, val: &GroupKeyVal, arena: &StringArena) {
    match val {
        GroupKeyVal::Null => 0u8.hash(h),
        GroupKeyVal::Int(v) => { 1u8.hash(h); v.hash(h); }
        GroupKeyVal::Str(off, len) => { 2u8.hash(h); arena.get(*off, *len).hash(h); }
    }
}

fn hash_ref_component<H: Hasher>(h: &mut H, val: &GroupKeyRef) {
    match val {
        GroupKeyRef::Null => 0u8.hash(h),
        GroupKeyRef::Int(v) => { 1u8.hash(h); v.hash(h); }
        GroupKeyRef::Str(p) => {
            // SAFETY: pointer is valid for the current row iteration
            let s = unsafe { &**p };
            2u8.hash(h); s.hash(h);
        }
    }
}

/// Compute hash for an owned GroupKey (needs arena to resolve strings).
fn hash_group_key(key: &GroupKey, arena: &StringArena) -> u64 {
    let mut hasher = ahash::AHasher::default();
    for val in key.as_slice() {
        hash_key_component(&mut hasher, val, arena);
    }
    hasher.finish()
}

/// Compute hash for a borrowed group key slice (no allocation).
fn hash_group_key_ref(key: &[GroupKeyRef]) -> u64 {
    let mut hasher = ahash::AHasher::default();
    for val in key {
        hash_ref_component(&mut hasher, val);
    }
    hasher.finish()
}

/// Check if a stored owned key matches a temporary borrowed key.
fn keys_match(stored: &GroupKey, temp: &[GroupKeyRef], arena: &StringArena) -> bool {
    let s = stored.as_slice();
    s.len() == temp.len()
        && s.iter().zip(temp.iter()).all(|(s, t)| t.matches_owned(s, arena))
}

/// Per-segment decoded text data for GROUP BY columns.
/// Keeps decompressed string data alive during the row loop,
/// providing O(1) &str access per row without interning.
enum SegTextColumn {
    /// Dictionary-compressed: dict entries + per-row index (null-expanded).
    Dict {
        entries: Vec<String>,
        /// Per-row index into `entries`. u32::MAX = null.
        row_to_entry: Vec<u32>,
    },
    /// LZ4/LZ4Blocked: decompressed buffer + per-row range (null-expanded).
    Lz4 {
        buf: Vec<u8>,
        /// Per-row (offset, len). offset == u32::MAX means null.
        row_to_range: Vec<(u32, u16)>,
    },
    /// Segment-by column: same value for all rows.
    SegBy(Option<String>),
}

impl SegTextColumn {
    /// Get the string for a given row, or None if null.
    fn get_str(&self, row: usize) -> Option<&str> {
        match self {
            SegTextColumn::Dict { entries, row_to_entry } => {
                let idx = row_to_entry[row];
                if idx == u32::MAX { None } else { Some(&entries[idx as usize]) }
            }
            SegTextColumn::Lz4 { buf, row_to_range } => {
                let (off, len) = row_to_range[row];
                if off == u32::MAX {
                    None
                } else {
                    Some(std::str::from_utf8(&buf[off as usize..off as usize + len as usize]).unwrap_or(""))
                }
            }
            SegTextColumn::SegBy(opt) => opt.as_deref(),
        }
    }
}

/// Type alias for the group map using hashbrown with raw_entry support.
/// Maps group keys to indices into flat accumulator storage.
/// Using u32 index instead of Vec<AggAccumulator> eliminates per-group heap allocation
/// for accumulators, saving ~130ms cleanup for 275K groups.
type GroupMap = hashbrown::HashMap<GroupKey, u32, BuildHasherDefault<ahash::AHasher>>;

/// Convert a datum to i128 for SUM accumulation.
fn datum_to_i128(datum: pg_sys::Datum, type_oid: pg_sys::Oid) -> i128 {
    match type_oid {
        pg_sys::INT2OID => (datum.value() as i16) as i128,
        pg_sys::INT4OID => (datum.value() as i32) as i128,
        pg_sys::INT8OID => (datum.value() as i64) as i128,
        _ => datum.value() as i128,
    }
}

/// Convert a datum to f64 for float SUM/AVG.
fn datum_to_f64(datum: pg_sys::Datum, type_oid: pg_sys::Oid) -> f64 {
    match type_oid {
        pg_sys::FLOAT4OID => f32::from_bits(datum.value() as u32) as f64,
        pg_sys::FLOAT8OID => f64::from_bits(datum.value() as u64),
        _ => datum.value() as f64,
    }
}

/// Convert an i128 value to a PostgreSQL NUMERIC datum.
///
/// For values fitting in i64, uses the fast `int8_numeric` path.
/// For larger values, converts via string representation.
unsafe fn i128_to_numeric_datum(val: i128) -> pg_sys::Datum {
    unsafe {
        if val >= i64::MIN as i128 && val <= i64::MAX as i128 {
            pg_sys::OidFunctionCall1Coll(
                pg_sys::Oid::from(1781u32),  // int8_numeric
                pg_sys::InvalidOid,
                pg_sys::Datum::from(val as i64 as usize),
            )
        } else {
            let s = std::ffi::CString::new(val.to_string()).unwrap();
            pg_sys::OidFunctionCall3Coll(
                pg_sys::Oid::from(1701u32),  // numeric_in
                pg_sys::InvalidOid,
                pg_sys::Datum::from(s.as_ptr()),
                pg_sys::Datum::from(0usize),
                pg_sys::Datum::from(-1i32 as usize),
            )
        }
    }
}

/// Finalize an accumulator into a (Datum, is_null) result pair.
unsafe fn finalize_accumulator(acc: &AggAccumulator, spec: &AggExecSpec) -> (pg_sys::Datum, bool) {
    unsafe {
        match acc {
            AggAccumulator::SumInt { sum, count } => {
                if *count == 0 {
                    return (pg_sys::Datum::from(0usize), true);
                }
                match spec.agg_type {
                    AggType::Sum => {
                        // SUM(int2/int4) → INT8, SUM(int8) → NUMERIC
                        if spec.col_type_oid == pg_sys::INT8OID {
                            // Result is NUMERIC — use i128_to_numeric for full range
                            (i128_to_numeric_datum(*sum), false)
                        } else {
                            // Result is INT8
                            (pg_sys::Datum::from(*sum as i64 as usize), false)
                        }
                    }
                    AggType::Avg => {
                        // AVG(int*) → NUMERIC — use exact NUMERIC arithmetic
                        let sum_numeric = i128_to_numeric_datum(*sum);
                        let count_numeric = pg_sys::OidFunctionCall1Coll(
                            pg_sys::Oid::from(1781u32),  // int8_numeric
                            pg_sys::InvalidOid,
                            pg_sys::Datum::from(*count as usize),
                        );
                        let datum = pg_sys::OidFunctionCall2Coll(
                            pg_sys::Oid::from(1727u32),  // numeric_div
                            pg_sys::InvalidOid,
                            sum_numeric,
                            count_numeric,
                        );
                        (datum, false)
                    }
                    _ => (pg_sys::Datum::from(*sum as i64 as usize), false),
                }
            }
            AggAccumulator::SumFloat { sum, count } => {
                if *count == 0 {
                    return (pg_sys::Datum::from(0usize), true);
                }
                match spec.agg_type {
                    AggType::Sum => {
                        // SUM(float4) → FLOAT4, SUM(float8) → FLOAT8
                        if spec.col_type_oid == pg_sys::FLOAT4OID {
                            let f4 = *sum as f32;
                            (pg_sys::Datum::from(f4.to_bits() as usize), false)
                        } else {
                            (pg_sys::Datum::from(sum.to_bits() as usize), false)
                        }
                    }
                    AggType::Avg => {
                        // AVG(float*) → FLOAT8
                        let avg = *sum / *count as f64;
                        (pg_sys::Datum::from(avg.to_bits() as usize), false)
                    }
                    _ => (pg_sys::Datum::from(sum.to_bits() as usize), false),
                }
            }
            AggAccumulator::Count { count } => {
                (pg_sys::Datum::from(*count as usize), false)
            }
            AggAccumulator::CountDistinctInt { seen } => {
                (pg_sys::Datum::from(seen.len()), false)
            }
            AggAccumulator::CountDistinctStr { seen } => {
                (pg_sys::Datum::from(seen.len()), false)
            }
            AggAccumulator::MinInt { val } | AggAccumulator::MaxInt { val } => {
                match val {
                    Some(v) => (pg_sys::Datum::from(*v as usize), false),
                    None => (pg_sys::Datum::from(0usize), true),
                }
            }
            AggAccumulator::MinFloat { val } | AggAccumulator::MaxFloat { val } => {
                match val {
                    Some(v) => {
                        if spec.col_type_oid == pg_sys::FLOAT4OID {
                            let f4 = *v as f32;
                            (pg_sys::Datum::from(f4.to_bits() as usize), false)
                        } else {
                            (pg_sys::Datum::from(v.to_bits() as usize), false)
                        }
                    }
                    None => (pg_sys::Datum::from(0usize), true),
                }
            }
            AggAccumulator::MinStr { val } | AggAccumulator::MaxStr { val } => {
                match val {
                    Some(s) => {
                        let datum = string_to_datum(s, spec.col_type_oid);
                        (datum, false)
                    }
                    None => (pg_sys::Datum::from(0usize), true),
                }
            }
        }
    }
}

/// ExecCustomScan callback for DeltaXAgg: return result rows.
#[pg_guard]
pub(super) unsafe extern "C-unwind" fn exec_agg_scan(
    node: *mut pg_sys::CustomScanState,
) -> *mut pg_sys::TupleTableSlot {
    unsafe {
        let scan_slot = (*node).ss.ss_ScanTupleSlot;
        let state = &mut *((*node).custom_ps as *mut AggScanState);

        if state.result_idx < state.result_rows.len() {
            pg_sys::ExecClearTuple(scan_slot);
            let row = &state.result_rows[state.result_idx];
            for (i, &(datum, is_null)) in row.iter().enumerate() {
                (*scan_slot).tts_values.add(i).write(datum);
                (*scan_slot).tts_isnull.add(i).write(is_null);
            }
            pg_sys::ExecStoreVirtualTuple(scan_slot);
            state.result_idx += 1;
            return scan_slot;
        }

        // EOF
        pg_sys::ExecClearTuple(scan_slot);
        scan_slot
    }
}

/// EndCustomScan callback for DeltaXAgg.
#[pg_guard]
pub(super) unsafe extern "C-unwind" fn end_agg_scan(
    node: *mut pg_sys::CustomScanState,
) {
    unsafe {
        let state_ptr = (*node).custom_ps as *mut AggScanState;
        if !state_ptr.is_null() {
            let state = Box::from_raw(state_ptr);
            let total_us = state.metadata_us + state.heap_scan_us + state.decompress_us + state.agg_us;
            pgrx::log!(
                "pg_deltax DeltaXAgg timing: total={:.1}ms  metadata={:.1}ms  heap_scan={:.1}ms  \
                 decompress={:.1}ms  agg={:.1}ms  | \
                 segments={} rows_processed={} result_rows={}",
                total_us as f64 / 1000.0,
                state.metadata_us as f64 / 1000.0,
                state.heap_scan_us as f64 / 1000.0,
                state.decompress_us as f64 / 1000.0,
                state.agg_us as f64 / 1000.0,
                state.total_segments,
                state.total_rows_processed,
                state.result_rows.len(),
            );
            (*node).custom_ps = std::ptr::null_mut();
        }
    }
}

/// ReScanCustomScan callback for DeltaXAgg.
#[pg_guard]
pub(super) unsafe extern "C-unwind" fn rescan_agg_scan(
    node: *mut pg_sys::CustomScanState,
) {
    unsafe {
        let state = &mut *((*node).custom_ps as *mut AggScanState);
        state.result_idx = 0;
    }
}
