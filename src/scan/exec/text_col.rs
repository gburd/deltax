//! Shared text column primitives for parallel decompress and aggregation paths.
//!
//! All types and functions here are pure Rust (no PG API calls) and thread-safe,
//! suitable for use in `std::thread::scope` workers.

use crate::compression;
use super::datum_utils::count_non_null;
use super::batch_qual::LikeStrategy;

/// Keeps decompressed string data alive during the row loop,
/// providing O(1) &str access per row without interning.
pub(super) enum SegTextColumn {
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
    pub(super) fn get_str(&self, row: usize) -> Option<&str> {
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

/// Pre-extracted text qual info for worker threads.
#[derive(Clone)]
pub(super) enum TextQualInfo {
    EqNe { col_idx: usize, const_str: String, is_ne: bool },
    Like { col_idx: usize, strategy: LikeStrategy, negate: bool },
}

/// Decompress a text column blob into a SegTextColumn (pure Rust, thread-safe).
pub(super) fn decompress_text_to_seg_col(blob: &[u8]) -> Option<SegTextColumn> {
    if blob.is_empty() {
        return None;
    }
    let cc = compression::CompressedColumnRef::from_bytes(blob);
    let total = cc.row_count as usize;
    let nn_count = count_non_null(cc.null_bitmap, total);

    match cc.type_tag {
        compression::CompressionType::Dictionary
        | compression::CompressionType::DictionaryLz4 => {
            let norm_buf;
            let dict_data = if cc.type_tag == compression::CompressionType::DictionaryLz4 {
                norm_buf = compression::dictionary::normalize_lz4(cc.data);
                &norm_buf[..]
            } else {
                cc.data
            };
            let (dict_entries, nn_indices) =
                compression::dictionary::decode_dict_and_indices(dict_data, nn_count);
            let entries: Vec<String> = dict_entries.iter().map(|&s| s.to_string()).collect();

            let row_to_entry = if cc.null_bitmap.is_empty() {
                nn_indices.iter().map(|&idx| idx as u32).collect()
            } else {
                let mut re = Vec::with_capacity(total);
                let mut vi = 0;
                for i in 0..total {
                    let is_null = (cc.null_bitmap[i / 8] >> (i % 8)) & 1 == 1;
                    if is_null {
                        re.push(u32::MAX);
                    } else {
                        re.push(nn_indices[vi] as u32);
                        vi += 1;
                    }
                }
                re
            };
            Some(SegTextColumn::Dict { entries, row_to_entry })
        }
        compression::CompressionType::Lz4 | compression::CompressionType::Lz4Blocked => {
            let (buf, ranges) = if cc.type_tag == compression::CompressionType::Lz4 {
                compression::lz4::decode_to_ranges(cc.data, nn_count)
            } else {
                compression::lz4::decode_to_ranges_blocked(cc.data, nn_count, None)
            };

            let row_to_range = if cc.null_bitmap.is_empty() {
                ranges.iter().map(|&(off, len)| (off as u32, len as u16)).collect()
            } else {
                let mut rr = Vec::with_capacity(total);
                let mut vi = 0;
                for i in 0..total {
                    let is_null = (cc.null_bitmap[i / 8] >> (i % 8)) & 1 == 1;
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
            Some(SegTextColumn::Lz4 { buf, row_to_range })
        }
        _ => None,
    }
}

/// Apply a text EQ/NE filter to a SegTextColumn, producing a selection bitmap.
pub(super) fn apply_text_eq_filter(seg_col: &SegTextColumn, const_str: &str, is_ne: bool, row_count: usize) -> Vec<bool> {
    let mut sel = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let s = seg_col.get_str(row);
        let pass = match s {
            Some(s) => {
                let eq = s == const_str;
                if is_ne { !eq } else { eq }
            }
            None => false, // NULL doesn't pass
        };
        sel.push(pass);
    }
    sel
}

/// Apply a text LIKE filter to a SegTextColumn, producing a selection bitmap.
pub(super) fn apply_text_like_filter(seg_col: &SegTextColumn, strategy: &LikeStrategy, negate: bool, row_count: usize) -> Vec<bool> {
    use super::batch_qual::sql_like_match;

    let matches_like = |text: &str| -> bool {
        let matched = match strategy {
            LikeStrategy::Contains(s) => text.contains(s.as_str()),
            LikeStrategy::StartsWith(s) => text.starts_with(s.as_str()),
            LikeStrategy::EndsWith(s) => text.ends_with(s.as_str()),
            LikeStrategy::Exact(s) => text == s.as_str(),
            LikeStrategy::General(p) => sql_like_match(text, p),
        };
        if negate { !matched } else { matched }
    };

    let mut sel = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let pass = match seg_col.get_str(row) {
            Some(s) => matches_like(s),
            None => false,
        };
        sel.push(pass);
    }
    sel
}

/// Collation-aware string comparison using libc `strcoll`.
///
/// Safe to call from non-PG worker threads. On glibc, `strcoll` is MT-Safe
/// and uses the process-wide locale set by `setlocale(LC_COLLATE, ...)`.
/// Strings are null-terminated via a small stack buffer or heap fallback.
pub(super) fn strcoll_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    // Fast path: if both are equal bytes, they're equal in any collation
    if a.as_bytes() == b.as_bytes() {
        return std::cmp::Ordering::Equal;
    }

    unsafe extern "C" {
        fn strcoll(s1: *const std::ffi::c_char, s2: *const std::ffi::c_char) -> std::ffi::c_int;
    }

    // Null-terminate strings for strcoll.
    // Use stack buffers for short strings to avoid allocation in the hot path.
    const STACK_BUF: usize = 512;
    let mut buf_a = [0u8; STACK_BUF];
    let mut buf_b = [0u8; STACK_BUF];
    let mut heap_a: Vec<u8>;
    let mut heap_b: Vec<u8>;

    let ptr_a = if a.len() < STACK_BUF {
        buf_a[..a.len()].copy_from_slice(a.as_bytes());
        buf_a[a.len()] = 0;
        buf_a.as_ptr() as *const std::ffi::c_char
    } else {
        heap_a = Vec::with_capacity(a.len() + 1);
        heap_a.extend_from_slice(a.as_bytes());
        heap_a.push(0);
        heap_a.as_ptr() as *const std::ffi::c_char
    };

    let ptr_b = if b.len() < STACK_BUF {
        buf_b[..b.len()].copy_from_slice(b.as_bytes());
        buf_b[b.len()] = 0;
        buf_b.as_ptr() as *const std::ffi::c_char
    } else {
        heap_b = Vec::with_capacity(b.len() + 1);
        heap_b.extend_from_slice(b.as_bytes());
        heap_b.push(0);
        heap_b.as_ptr() as *const std::ffi::c_char
    };

    let result = unsafe { strcoll(ptr_a, ptr_b) };
    if result < 0 {
        std::cmp::Ordering::Less
    } else if result > 0 {
        std::cmp::Ordering::Greater
    } else {
        // Tie-break: byte comparison (matches PG's deterministic collation behavior)
        a.as_bytes().cmp(b.as_bytes())
    }
}
