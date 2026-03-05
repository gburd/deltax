/// Dictionary encoding for low-cardinality TEXT columns.
///
/// Stores a dictionary of unique strings + an array of indices.
///
/// Format:
///   4 bytes — dictionary size (number of unique strings, u32 LE)
///   For each dictionary entry:
///     4 bytes — string length (u32 LE)
///     N bytes — string UTF-8 data
///   Then for each value:
///     compressed index (varint, 0-based)
///
/// Use dictionary encoding when cardinality < 10% of row count AND < 65536 distinct values.
/// Otherwise fall back to LZ4.
use std::collections::HashMap;

fn write_varint(buf: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn read_varint(buf: &[u8]) -> (u32, usize) {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return (result, i + 1);
        }
        shift += 7;
    }
    (result, buf.len())
}

/// Returns true if dictionary encoding is suitable for the given values.
pub fn should_use_dictionary(values: &[&str]) -> bool {
    if values.is_empty() {
        return true;
    }
    let mut uniq = std::collections::HashSet::new();
    for v in values {
        uniq.insert(*v);
        // Early exit if cardinality too high
        if uniq.len() > 65535 {
            return false;
        }
    }
    let cardinality = uniq.len();
    cardinality < 65536 && cardinality <= (values.len() / 2).max(1)
}

pub fn encode(values: &[&str]) -> Vec<u8> {
    if values.is_empty() {
        return 0u32.to_le_bytes().to_vec();
    }

    let mut dict: HashMap<&str, u32> = HashMap::new();
    let mut dict_entries: Vec<&str> = Vec::new();

    for &v in values {
        if !dict.contains_key(v) {
            let idx = dict_entries.len() as u32;
            dict.insert(v, idx);
            dict_entries.push(v);
        }
    }

    let mut buf = Vec::new();

    // Dictionary size
    buf.extend_from_slice(&(dict_entries.len() as u32).to_le_bytes());

    // Dictionary entries
    for &entry in &dict_entries {
        let bytes = entry.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    // Indices
    for &v in values {
        let idx = dict[v];
        write_varint(&mut buf, idx);
    }

    buf
}

/// Decode dictionary-encoded data, returning borrowed &str slices.
/// Avoids N String allocations by referencing the dictionary entries in-place.
pub fn decode_to_slices(data: &[u8], count: usize) -> Vec<&str> {
    if count == 0 {
        return Vec::new();
    }

    let mut offset = 0;
    let dict_size = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;

    let mut dict: Vec<&str> = Vec::with_capacity(dict_size);
    for _ in 0..dict_size {
        let str_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let s = std::str::from_utf8(&data[offset..offset + str_len])
            .expect("invalid UTF-8 in dictionary");
        offset += str_len;
        dict.push(s);
    }

    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let (idx, consumed) = read_varint(&data[offset..]);
        offset += consumed;
        values.push(dict[idx as usize]);
    }
    values
}

/// Decode dictionary-encoded data, returning the dictionary entries and per-row indices separately.
/// This allows matching against only the dictionary entries (e.g. for LIKE filtering)
/// instead of resolving every row.
pub fn decode_dict_and_indices(data: &[u8], count: usize) -> (Vec<&str>, Vec<u32>) {
    if count == 0 {
        return (Vec::new(), Vec::new());
    }

    let mut offset = 0;
    let dict_size = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;

    let mut dict: Vec<&str> = Vec::with_capacity(dict_size);
    for _ in 0..dict_size {
        let str_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let s = std::str::from_utf8(&data[offset..offset + str_len])
            .expect("invalid UTF-8 in dictionary");
        offset += str_len;
        dict.push(s);
    }

    let mut indices = Vec::with_capacity(count);
    for _ in 0..count {
        let (idx, consumed) = read_varint(&data[offset..]);
        offset += consumed;
        indices.push(idx);
    }

    (dict, indices)
}

pub fn decode(data: &[u8], count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }

    let mut offset = 0;

    // Read dictionary
    let dict_size = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;

    let mut dict = Vec::with_capacity(dict_size);
    for _ in 0..dict_size {
        let str_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let s = std::str::from_utf8(&data[offset..offset + str_len])
            .expect("invalid UTF-8 in dictionary")
            .to_string();
        offset += str_len;
        dict.push(s);
    }

    // Read indices
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let (idx, consumed) = read_varint(&data[offset..]);
        offset += consumed;
        values.push(dict[idx as usize].clone());
    }

    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_basic() {
        let values = vec!["hello", "world", "hello", "world", "hello"];
        let encoded = encode(&values);
        let decoded = decode(&encoded, values.len());
        let expected: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn test_roundtrip_empty() {
        let encoded = encode(&[]);
        let decoded = decode(&encoded, 0);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_roundtrip_single() {
        let values = vec!["test"];
        let encoded = encode(&values);
        let decoded = decode(&encoded, 1);
        assert_eq!(decoded, vec!["test".to_string()]);
    }

    #[test]
    fn test_compression_ratio() {
        // Low cardinality: 10 device IDs repeated 1000 times
        let devices: Vec<String> = (0..10).map(|i| format!("device-{:04}", i)).collect();
        let values: Vec<&str> = (0..10000).map(|i| devices[i % 10].as_str()).collect();

        let raw_size: usize = values.iter().map(|s| s.len()).sum();
        let encoded = encode(&values);
        let decoded = decode(&encoded, values.len());

        let expected: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        assert_eq!(decoded, expected);

        let ratio = raw_size as f64 / encoded.len() as f64;
        assert!(ratio > 5.0, "low-cardinality text should compress >5x, got {:.1}x", ratio);
    }

    #[test]
    fn test_should_use_dictionary() {
        // Low cardinality
        let values: Vec<&str> = vec!["a", "b", "c", "a", "b", "c", "a", "b", "c", "a", "b"];
        assert!(should_use_dictionary(&values));

        // High cardinality — every value unique
        let strings: Vec<String> = (0..100).map(|i| format!("unique-{}", i)).collect();
        let values: Vec<&str> = strings.iter().map(|s| s.as_str()).collect();
        assert!(!should_use_dictionary(&values));
    }

    #[test]
    fn test_utf8_strings() {
        let values = vec!["héllo", "wörld", "日本語", "🎉"];
        let encoded = encode(&values);
        let decoded = decode(&encoded, values.len());
        let expected: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        assert_eq!(decoded, expected);
    }
}
