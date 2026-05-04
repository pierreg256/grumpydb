//! CRDT merge helpers (Phase 46 runtime).
//!
//! Payload formats currently supported:
//! - `GCounter`: 8-byte little-endian `u64`.
//! - `PNCounter`: 16-byte little-endian `(u64 positive, u64 negative)`.
//! - `LwwSet`: JSON bytes of `{"adds": {"elem": ts}, "rems": {"elem": ts}}`.
//! - `OrSet`: JSON bytes of
//!   `{"adds": {"elem": ["tag"]}, "rems": {"elem": ["tag"]}}`.
//! - `Mvr`: JSON bytes of `{"values": ["base64value", ...]}`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::document::value::{CrdtKind, Value};

/// Merge two CRDT payloads for the same kind.
pub fn merge_payload(kind: CrdtKind, left: &[u8], right: &[u8]) -> Result<Vec<u8>, String> {
    match kind {
        CrdtKind::GCounter => {
            let l = decode_u64(left, "GCounter")?;
            let r = decode_u64(right, "GCounter")?;
            Ok(std::cmp::max(l, r).to_le_bytes().to_vec())
        }
        CrdtKind::PNCounter => {
            let (lp, ln) = decode_pn(left)?;
            let (rp, rn) = decode_pn(right)?;
            let mut out = Vec::with_capacity(16);
            out.extend_from_slice(&std::cmp::max(lp, rp).to_le_bytes());
            out.extend_from_slice(&std::cmp::max(ln, rn).to_le_bytes());
            Ok(out)
        }
        CrdtKind::LwwSet => merge_lwwset(left, right),
        CrdtKind::OrSet => merge_orset(left, right),
        CrdtKind::Mvr => merge_mvr(left, right),
    }
}

/// Merge two [`Value::Crdt`] values.
pub fn merge_values(left: &Value, right: &Value) -> Result<Value, String> {
    let (left_kind, left_payload) = left
        .as_crdt()
        .ok_or_else(|| "left value is not CRDT".to_string())?;
    let (right_kind, right_payload) = right
        .as_crdt()
        .ok_or_else(|| "right value is not CRDT".to_string())?;

    if left_kind != right_kind {
        return Err(format!(
            "CRDT kind mismatch: left={}, right={}",
            left_kind.as_str(),
            right_kind.as_str()
        ));
    }

    let merged = merge_payload(left_kind, left_payload, right_payload)?;
    Ok(Value::Crdt {
        kind: left_kind,
        payload: merged,
    })
}

fn decode_u64(bytes: &[u8], kind: &str) -> Result<u64, String> {
    if bytes.len() != 8 {
        return Err(format!(
            "invalid {kind} payload length: expected 8, got {}",
            bytes.len()
        ));
    }
    let mut arr = [0u8; 8];
    arr.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(arr))
}

fn decode_pn(bytes: &[u8]) -> Result<(u64, u64), String> {
    if bytes.len() != 16 {
        return Err(format!(
            "invalid PNCounter payload length: expected 16, got {}",
            bytes.len()
        ));
    }
    let mut p = [0u8; 8];
    let mut n = [0u8; 8];
    p.copy_from_slice(&bytes[..8]);
    n.copy_from_slice(&bytes[8..16]);
    Ok((u64::from_le_bytes(p), u64::from_le_bytes(n)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LwwSetPayload {
    #[serde(default)]
    adds: BTreeMap<String, u64>,
    #[serde(default)]
    rems: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrSetPayload {
    #[serde(default)]
    adds: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    rems: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MvrPayload {
    #[serde(default)]
    values: BTreeSet<String>,
}

fn merge_lwwset(left: &[u8], right: &[u8]) -> Result<Vec<u8>, String> {
    let left = decode_payload::<LwwSetPayload>(left, "LwwSet")?;
    let right = decode_payload::<LwwSetPayload>(right, "LwwSet")?;
    let out = LwwSetPayload {
        adds: merge_max_u64_maps(&left.adds, &right.adds),
        rems: merge_max_u64_maps(&left.rems, &right.rems),
    };
    encode_payload(&out, "LwwSet")
}

fn merge_orset(left: &[u8], right: &[u8]) -> Result<Vec<u8>, String> {
    let left = decode_payload::<OrSetPayload>(left, "OrSet")?;
    let right = decode_payload::<OrSetPayload>(right, "OrSet")?;
    let out = OrSetPayload {
        adds: merge_set_maps(&left.adds, &right.adds),
        rems: merge_set_maps(&left.rems, &right.rems),
    };
    encode_payload(&out, "OrSet")
}

fn merge_mvr(left: &[u8], right: &[u8]) -> Result<Vec<u8>, String> {
    let left = decode_payload::<MvrPayload>(left, "Mvr")?;
    let right = decode_payload::<MvrPayload>(right, "Mvr")?;
    let out = MvrPayload {
        values: left.values.union(&right.values).cloned().collect(),
    };
    encode_payload(&out, "Mvr")
}

fn merge_max_u64_maps(
    left: &BTreeMap<String, u64>,
    right: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    let mut out = left.clone();
    for (k, rv) in right {
        out.entry(k.clone())
            .and_modify(|lv| *lv = std::cmp::max(*lv, *rv))
            .or_insert(*rv);
    }
    out
}

fn merge_set_maps(
    left: &BTreeMap<String, BTreeSet<String>>,
    right: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut out = left.clone();
    for (k, rset) in right {
        out.entry(k.clone())
            .and_modify(|lset| {
                lset.extend(rset.iter().cloned());
            })
            .or_insert_with(|| rset.clone());
    }
    out
}

fn decode_payload<T>(bytes: &[u8], kind: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(bytes).map_err(|e| format!("invalid {kind} payload JSON: {e}"))
}

fn encode_payload<T>(value: &T, kind: &str) -> Result<Vec<u8>, String>
where
    T: Serialize,
{
    serde_json::to_vec(value).map_err(|e| format!("failed to encode {kind} payload JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_lwwset(adds: &[(&str, u64)], rems: &[(&str, u64)]) -> Vec<u8> {
        let adds = adds
            .iter()
            .map(|(k, ts)| ((*k).to_string(), *ts))
            .collect::<BTreeMap<_, _>>();
        let rems = rems
            .iter()
            .map(|(k, ts)| ((*k).to_string(), *ts))
            .collect::<BTreeMap<_, _>>();
        serde_json::to_vec(&LwwSetPayload { adds, rems }).expect("encode lwwset")
    }

    fn decode_lwwset(bytes: &[u8]) -> LwwSetPayload {
        serde_json::from_slice(bytes).expect("decode lwwset")
    }

    fn encode_orset(adds: &[(&str, &[&str])], rems: &[(&str, &[&str])]) -> Vec<u8> {
        let to_map = |entries: &[(&str, &[&str])]| {
            entries
                .iter()
                .map(|(k, tags)| {
                    (
                        (*k).to_string(),
                        tags.iter()
                            .map(|t| (*t).to_string())
                            .collect::<BTreeSet<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        serde_json::to_vec(&OrSetPayload {
            adds: to_map(adds),
            rems: to_map(rems),
        })
        .expect("encode orset")
    }

    fn decode_orset(bytes: &[u8]) -> OrSetPayload {
        serde_json::from_slice(bytes).expect("decode orset")
    }

    fn encode_mvr(values: &[&str]) -> Vec<u8> {
        let values = values
            .iter()
            .map(|v| (*v).to_string())
            .collect::<BTreeSet<_>>();
        serde_json::to_vec(&MvrPayload { values }).expect("encode mvr")
    }

    fn decode_mvr(bytes: &[u8]) -> MvrPayload {
        serde_json::from_slice(bytes).expect("decode mvr")
    }

    #[test]
    fn test_merge_gcounter_uses_max_component() {
        let out = merge_payload(CrdtKind::GCounter, &5u64.to_le_bytes(), &9u64.to_le_bytes())
            .expect("merge gcounter");
        assert_eq!(out, 9u64.to_le_bytes().to_vec());
    }

    #[test]
    fn test_merge_pncounter_uses_component_wise_max() {
        let mut l = Vec::new();
        l.extend_from_slice(&2u64.to_le_bytes());
        l.extend_from_slice(&7u64.to_le_bytes());

        let mut r = Vec::new();
        r.extend_from_slice(&9u64.to_le_bytes());
        r.extend_from_slice(&3u64.to_le_bytes());

        let out = merge_payload(CrdtKind::PNCounter, &l, &r).expect("merge pncounter");
        let mut expected = Vec::new();
        expected.extend_from_slice(&9u64.to_le_bytes());
        expected.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(out, expected);
    }

    #[test]
    fn test_merge_values_rejects_kind_mismatch() {
        let left = Value::Crdt {
            kind: CrdtKind::GCounter,
            payload: 1u64.to_le_bytes().to_vec(),
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u64.to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        let right = Value::Crdt {
            kind: CrdtKind::PNCounter,
            payload,
        };

        let err = merge_values(&left, &right).unwrap_err();
        assert!(err.contains("CRDT kind mismatch"));
    }

    #[test]
    fn test_merge_lwwset_uses_max_timestamp_per_entry() {
        let left = encode_lwwset(&[("a", 10), ("b", 3)], &[("b", 6)]);
        let right = encode_lwwset(&[("a", 12), ("c", 1)], &[("b", 4), ("c", 9)]);

        let merged = merge_payload(CrdtKind::LwwSet, &left, &right).expect("merge lwwset");
        let decoded = decode_lwwset(&merged);

        assert_eq!(decoded.adds.get("a"), Some(&12));
        assert_eq!(decoded.adds.get("b"), Some(&3));
        assert_eq!(decoded.adds.get("c"), Some(&1));
        assert_eq!(decoded.rems.get("b"), Some(&6));
        assert_eq!(decoded.rems.get("c"), Some(&9));
    }

    #[test]
    fn test_merge_orset_unions_tag_sets() {
        let left = encode_orset(&[("x", &["n1:1", "n2:1"])], &[]);
        let right = encode_orset(&[("x", &["n3:1"]), ("y", &["n1:2"])], &[("x", &["n2:1"])]);

        let merged = merge_payload(CrdtKind::OrSet, &left, &right).expect("merge orset");
        let decoded = decode_orset(&merged);

        assert_eq!(decoded.adds["x"].len(), 3);
        assert!(decoded.adds["x"].contains("n1:1"));
        assert!(decoded.adds["x"].contains("n2:1"));
        assert!(decoded.adds["x"].contains("n3:1"));
        assert!(decoded.rems["x"].contains("n2:1"));
        assert!(decoded.adds["y"].contains("n1:2"));
    }

    #[test]
    fn test_merge_mvr_unions_values() {
        let left = encode_mvr(&["dmFsdWUx", "dmFsdWUy"]);
        let right = encode_mvr(&["dmFsdWUy", "dmFsdWUz"]);

        let merged = merge_payload(CrdtKind::Mvr, &left, &right).expect("merge mvr");
        let decoded = decode_mvr(&merged);

        assert_eq!(decoded.values.len(), 3);
        assert!(decoded.values.contains("dmFsdWUx"));
        assert!(decoded.values.contains("dmFsdWUy"));
        assert!(decoded.values.contains("dmFsdWUz"));
    }

    #[test]
    fn test_merge_lwwset_rejects_invalid_payload() {
        let err = merge_payload(CrdtKind::LwwSet, b"{not-json", b"{}").unwrap_err();
        assert!(err.contains("invalid LwwSet payload JSON"));
    }
}
