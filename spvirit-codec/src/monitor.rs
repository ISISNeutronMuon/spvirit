//! Monitor delta decoding: changed bitset, value, overrun bitset.
//!
//! The pvAccess specification puts a MONITOR update's changed bitset first,
//! then the delta data, then the overrun bitset. That order is what
//! [`PvdDecoder::decode_monitor_update`] implements, and it is what every live
//! connection in this workspace uses.
//!
//! Some implementations disagree about where the overrun bitset goes (or omit
//! it entirely). [`PvdDecoder::decode_monitor_update_lenient`] tries every
//! known layout and reports which one it matched; it exists for mid-stream
//! packet captures, where the introspection was never seen and the peer's
//! layout has to be inferred.

use crate::error::{DecodeError, DecodeResult};
use crate::spvd_decode::{DecodedValue, FieldType, PvdDecoder, StructureDesc};

/// A decoded MONITOR update: the delta value plus both bitsets.
#[derive(Debug, Clone)]
pub struct MonitorUpdate {
    pub value: DecodedValue,
    /// Raw changed bitset. Bit 0 is the whole structure; field bits start at 1.
    pub changed: Vec<u8>,
    /// Raw overrun bitset, same numbering. A set bit means the server dropped
    /// at least one update for that field before this one.
    pub overrun: Vec<u8>,
    /// Bytes consumed from the body.
    pub consumed: usize,
    /// Bit-indexed field paths for this update's introspection: index 0 is
    /// `"<whole structure>"`, index *n* is the field addressed by bit *n*.
    ///
    /// Captured at decode time so a callback can name the set bits without
    /// also being handed the [`StructureDesc`].
    pub paths: Vec<String>,
}

/// Which wire layout a lenient decode matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorLayout {
    /// changed bitset, data, overrun bitset. The pvAccess specification order.
    SpecOrder,
    /// changed bitset, overrun bitset, data.
    OverrunBeforeData,
    /// changed bitset, data, with no overrun bitset.
    ChangedOnly,
}

impl MonitorUpdate {
    /// True if any overrun bit is set.
    pub fn has_overrun(&self) -> bool {
        self.overrun.iter().any(|b| *b != 0)
    }

    /// Dotted paths of the fields whose overrun bits are set.
    ///
    /// Bit 0 yields the literal `"<whole structure>"`. Takes the descriptor
    /// explicitly; [`MonitorUpdate::overrun_paths`] uses the paths the decoder
    /// already captured.
    pub fn overrun_fields(&self, desc: &StructureDesc) -> Vec<String> {
        select_paths(&bit_paths(desc), &self.overrun)
    }

    /// Dotted paths of the fields marked changed in this update.
    ///
    /// Bit 0 yields the literal `"<whole structure>"`.
    pub fn changed_paths(&self) -> Vec<String> {
        select_paths(&self.paths, &self.changed)
    }

    /// Dotted paths of the fields whose overrun bits are set — the server
    /// dropped at least one earlier update for each of them.
    pub fn overrun_paths(&self) -> Vec<String> {
        select_paths(&self.paths, &self.overrun)
    }
}

/// Bit-indexed paths for a descriptor: bit 0 is the whole structure, field
/// bits follow in `flatten_field_paths` order.
pub(crate) fn bit_paths(desc: &StructureDesc) -> Vec<String> {
    let mut paths = vec!["<whole structure>".to_string()];
    flatten_field_paths(desc, "", &mut paths);
    paths
}

/// The subset of `paths` whose bit is set in `bits`.
fn select_paths(paths: &[String], bits: &[u8]) -> Vec<String> {
    paths
        .iter()
        .enumerate()
        .filter(|(bit, _)| {
            let byte = bit / 8;
            byte < bits.len() && (bits[byte] & (1 << (bit % 8))) != 0
        })
        .map(|(_, path)| path.clone())
        .collect()
}

/// Depth-first, self-then-nested. Must stay in step with
/// `count_structure_fields` in `spvd_decode.rs`, which numbers the bits.
fn flatten_field_paths(desc: &StructureDesc, prefix: &str, out: &mut Vec<String>) {
    for field in &desc.fields {
        let path = if prefix.is_empty() {
            field.name.clone()
        } else {
            format!("{prefix}.{}", field.name)
        };
        out.push(path.clone());
        if let FieldType::Structure(nested) = &field.field_type {
            flatten_field_paths(nested, &path, out);
        }
    }
}

impl PvdDecoder {
    /// Decode a MONITOR update in specification order: changed bitset, then
    /// the delta data, then the overrun bitset.
    pub fn decode_monitor_update(
        &self,
        data: &[u8],
        desc: &StructureDesc,
    ) -> DecodeResult<MonitorUpdate> {
        let (changed, mut offset) = self.read_bitset(data, 0)?;
        let (value, consumed) =
            self.decode_structure_with_bitset_body(&data[offset..], desc, &changed)?;
        offset += consumed;
        let (overrun, next) = self.read_bitset(data, offset)?;
        Ok(MonitorUpdate {
            value,
            changed,
            overrun,
            consumed: next,
            paths: bit_paths(desc),
        })
    }

    /// Try all three known layouts and report which one won.
    ///
    /// For mid-stream packet captures, where the introspection was missed and
    /// the peer's layout is unknown. Live connections should use
    /// [`PvdDecoder::decode_monitor_update`].
    pub fn decode_monitor_update_lenient(
        &self,
        data: &[u8],
        desc: &StructureDesc,
    ) -> DecodeResult<(MonitorUpdate, MonitorLayout)> {
        let candidates = [
            (
                MonitorLayout::SpecOrder,
                self.decode_monitor_update(data, desc),
            ),
            (
                MonitorLayout::OverrunBeforeData,
                self.decode_overrun_before_data(data, desc),
            ),
            (
                MonitorLayout::ChangedOnly,
                self.decode_changed_only(data, desc),
            ),
        ];

        let mut best: Option<(MonitorUpdate, MonitorLayout, i32)> = None;
        let mut last_err = DecodeError::Malformed("no monitor layout matched");
        for (layout, result) in candidates {
            match result {
                Ok(update) => {
                    let score = score_decoded(&update.value);
                    let better = match &best {
                        None => true,
                        Some((prev, _, prev_score)) => {
                            score > *prev_score
                                || (score == *prev_score && update.consumed > prev.consumed)
                        }
                    };
                    if better {
                        best = Some((update, layout, score));
                    }
                }
                Err(e) => last_err = e,
            }
        }

        best.map(|(u, l, _)| (u, l)).ok_or(last_err)
    }

    /// changed bitset, overrun bitset, data.
    fn decode_overrun_before_data(
        &self,
        data: &[u8],
        desc: &StructureDesc,
    ) -> DecodeResult<MonitorUpdate> {
        let (changed, offset) = self.read_bitset(data, 0)?;
        let (overrun, mut offset) = self.read_bitset(data, offset)?;
        let (value, consumed) =
            self.decode_structure_with_bitset_body(&data[offset..], desc, &changed)?;
        offset += consumed;
        Ok(MonitorUpdate {
            value,
            changed,
            overrun,
            consumed: offset,
            paths: bit_paths(desc),
        })
    }

    /// changed bitset, data, nothing else.
    fn decode_changed_only(
        &self,
        data: &[u8],
        desc: &StructureDesc,
    ) -> DecodeResult<MonitorUpdate> {
        let (changed, mut offset) = self.read_bitset(data, 0)?;
        let (value, consumed) =
            self.decode_structure_with_bitset_body(&data[offset..], desc, &changed)?;
        offset += consumed;
        Ok(MonitorUpdate {
            value,
            changed,
            overrun: Vec::new(),
            consumed: offset,
            paths: bit_paths(desc),
        })
    }

    /// Read a size-prefixed bitset starting at `offset`; returns it and the
    /// offset just past it.
    fn read_bitset(&self, data: &[u8], offset: usize) -> DecodeResult<(Vec<u8>, usize)> {
        if offset > data.len() {
            return Err(DecodeError::Truncated {
                needed: offset,
                available: data.len(),
            });
        }
        let (size, consumed) = self.decode_size(&data[offset..])?;
        let start = offset + consumed;
        let end = start + size;
        if end > data.len() {
            return Err(DecodeError::Truncated {
                needed: end,
                available: data.len(),
            });
        }
        Ok((data[start..end].to_vec(), end))
    }
}

/// Plausibility score for a candidate decode. Higher is better.
///
/// Moved verbatim from `epics_decode.rs`, where it drove the old
/// three-way "try everything" monitor decode. Only
/// [`PvdDecoder::decode_monitor_update_lenient`] uses it now.
fn score_decoded(value: &DecodedValue) -> i32 {
    let DecodedValue::Structure(fields) = value else {
        return -1;
    };

    let mut score = fields.len() as i32;

    let mut has_value = false;
    let mut has_alarm = false;
    let mut has_ts = false;

    for (name, val) in fields {
        match name.as_str() {
            "value" => {
                has_value = true;
                score += 4;
                match val {
                    DecodedValue::Array(items) => {
                        if items.is_empty() {
                            score -= 2;
                        } else {
                            score += 6 + (items.len().min(8) as i32);
                        }
                    }
                    DecodedValue::Structure(_) => score += 1,
                    _ => score += 2,
                }
            }
            "alarm" => {
                has_alarm = true;
                score += 2;
            }
            "timeStamp" => {
                has_ts = true;
                score += 2;
                if let DecodedValue::Structure(ts_fields) = val {
                    if let Some(secs) = ts_fields.iter().find_map(|(n, v)| {
                        if n == "secondsPastEpoch" {
                            if let DecodedValue::Int64(s) = v {
                                return Some(*s);
                            }
                        }
                        None
                    }) {
                        if (0..=4_000_000_000i64).contains(&secs) {
                            score += 2;
                        } else if secs.abs() > 10_000_000_000i64 {
                            score -= 2;
                        }
                    }
                }
            }
            "display" | "control" => {
                score += 1;
            }
            _ => {}
        }
    }

    if !has_value {
        score -= 2;
    }
    if !has_alarm {
        score -= 1;
    }
    if !has_ts {
        score -= 1;
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spvd_decode::{FieldDesc, FieldType, PvdDecoder, StructureDesc, TypeCode};

    fn nt_scalar_desc() -> StructureDesc {
        let mut alarm = StructureDesc::new();
        alarm.fields.push(FieldDesc {
            name: "severity".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });

        let mut desc = StructureDesc::new();
        desc.fields.push(FieldDesc {
            name: "value".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });
        desc.fields.push(FieldDesc {
            name: "alarm".to_string(),
            field_type: FieldType::Structure(alarm),
        });
        desc
    }

    /// changed bitset (1 byte), value data, overrun bitset (1 byte).
    fn spec_order_body(changed: u8, value: i32, overrun: u8) -> Vec<u8> {
        let mut b = vec![1, changed];
        b.extend_from_slice(&value.to_le_bytes());
        b.extend_from_slice(&[1, overrun]);
        b
    }

    #[test]
    fn decodes_spec_order_and_reports_consumed() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        // bit 1 = "value".
        let body = spec_order_body(0b0000_0010, 42, 0);
        let update = decoder.decode_monitor_update(&body, &desc).unwrap();

        assert_eq!(update.changed, vec![0b0000_0010]);
        assert_eq!(update.overrun, vec![0]);
        assert!(!update.has_overrun());
        assert_eq!(update.consumed, body.len());
        let DecodedValue::Structure(fields) = &update.value else {
            panic!("expected a structure");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "value");
    }

    #[test]
    fn overrun_bits_resolve_to_field_paths() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        // Overrun on bit 1 ("value") and bit 3 ("alarm.severity").
        let body = spec_order_body(0b0000_0010, 42, 0b0000_1010);
        let update = decoder.decode_monitor_update(&body, &desc).unwrap();

        assert!(update.has_overrun());
        assert_eq!(
            update.overrun_fields(&desc),
            vec!["value", "alarm.severity"]
        );
    }

    #[test]
    fn bit_zero_overrun_reports_the_whole_structure() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        let body = spec_order_body(0b0000_0010, 42, 0b0000_0001);
        let update = decoder.decode_monitor_update(&body, &desc).unwrap();
        assert_eq!(update.overrun_fields(&desc), vec!["<whole structure>"]);
    }

    /// The bit numbering `overrun_fields` walks must be the same numbering
    /// `decode_structure_with_bitset_body` uses, which is
    /// `count_structure_fields`' self-then-nested depth-first order. If the
    /// two ever diverge, overrun bits map to the wrong field names.
    #[test]
    fn flatten_field_paths_agrees_with_count_structure_fields() {
        let mut leaf = StructureDesc::new();
        leaf.fields.push(FieldDesc {
            name: "deep".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });

        let mut mid = StructureDesc::new();
        mid.fields.push(FieldDesc {
            name: "a".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });
        mid.fields.push(FieldDesc {
            name: "leaf".to_string(),
            field_type: FieldType::Structure(leaf),
        });
        mid.fields.push(FieldDesc {
            name: "b".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });

        let mut root = StructureDesc::new();
        root.fields.push(FieldDesc {
            name: "value".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });
        root.fields.push(FieldDesc {
            name: "mid".to_string(),
            field_type: FieldType::Structure(mid),
        });
        root.fields.push(FieldDesc {
            name: "tail".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });

        let mut paths = Vec::new();
        flatten_field_paths(&root, "", &mut paths);

        assert_eq!(
            paths,
            vec![
                "value",
                "mid",
                "mid.a",
                "mid.leaf",
                "mid.leaf.deep",
                "mid.b",
                "tail",
            ]
        );
        assert_eq!(
            paths.len(),
            crate::spvd_decode::count_structure_fields(&root),
            "one path per bit, in the same order the bits are numbered"
        );

        // And the paths line up with the bits when read through an update:
        // bit 0 is the whole structure, so field bits start at 1.
        let mut overrun = vec![0u8; 2];
        let bit = 1 + 4; // "mid.leaf.deep"
        overrun[bit / 8] |= 1 << (bit % 8);
        let update = MonitorUpdate {
            value: DecodedValue::Structure(Vec::new()),
            changed: Vec::new(),
            overrun,
            consumed: 0,
            paths: bit_paths(&root),
        };
        assert_eq!(update.overrun_fields(&root), vec!["mid.leaf.deep"]);
        assert_eq!(update.overrun_paths(), vec!["mid.leaf.deep"]);
    }

    /// The shape the client monitor callback relies on: a decoded update can
    /// name its own changed and overrun bits without the caller holding the
    /// descriptor.
    #[test]
    fn monitor_update_reports_overrun_paths() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        // changed = bit 1 ("value"), overrun = bit 3 ("alarm.severity").
        let body = spec_order_body(0b0000_0010, 42, 0b0000_1000);
        let update = decoder.decode_monitor_update(&body, &desc).unwrap();

        assert_eq!(update.changed_paths(), vec!["value"]);
        assert_eq!(update.overrun_paths(), vec!["alarm.severity"]);
        assert!(update.has_overrun());
    }

    #[test]
    fn missing_overrun_bitset_is_truncated_not_silently_accepted() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        // changed bitset and value, then nothing.
        let mut body = vec![1, 0b0000_0010];
        body.extend_from_slice(&42i32.to_le_bytes());
        assert!(matches!(
            decoder.decode_monitor_update(&body, &desc).unwrap_err(),
            DecodeError::Truncated { .. }
        ));
    }

    #[test]
    fn lenient_identifies_the_spec_layout() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        let body = spec_order_body(0b0000_0010, 42, 0);
        let (_, layout) = decoder.decode_monitor_update_lenient(&body, &desc).unwrap();
        assert_eq!(layout, MonitorLayout::SpecOrder);
    }

    #[test]
    fn lenient_recovers_the_overrun_before_data_layout() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        // changed bitset, overrun bitset, then the value.
        let mut body = vec![1, 0b0000_0010, 1, 0];
        body.extend_from_slice(&42i32.to_le_bytes());

        // Strict must not accept it: it would read the value from the
        // overrun bitset's bytes.
        let strict = decoder.decode_monitor_update(&body, &desc);
        let strict_ok = strict.map(|u| u.value).ok();
        assert_ne!(
            strict_ok.as_ref().and_then(scalar_value_of),
            Some(42),
            "strict must not accidentally decode the non-spec layout"
        );

        let (update, layout) = decoder.decode_monitor_update_lenient(&body, &desc).unwrap();
        assert_eq!(layout, MonitorLayout::OverrunBeforeData);
        assert_eq!(scalar_value_of(&update.value), Some(42));
    }

    fn scalar_value_of(v: &DecodedValue) -> Option<i32> {
        let DecodedValue::Structure(fields) = v else {
            return None;
        };
        fields
            .iter()
            .find_map(|(name, val)| match (name.as_str(), val) {
                ("value", DecodedValue::Int32(n)) => Some(*n),
                _ => None,
            })
    }

    #[test]
    fn lenient_falls_back_to_changed_only() {
        let decoder = PvdDecoder::new(false);
        let desc = nt_scalar_desc();
        // changed bitset and value, no overrun bitset at all.
        let mut body = vec![1, 0b0000_0010];
        body.extend_from_slice(&42i32.to_le_bytes());
        let (update, layout) = decoder.decode_monitor_update_lenient(&body, &desc).unwrap();
        assert_eq!(layout, MonitorLayout::ChangedOnly);
        assert!(update.overrun.is_empty());
    }

    #[test]
    fn round_trips_an_encoded_delta() {
        use crate::spvd_encode::{compute_changed_bits, encode_nt_payload_delta, nt_payload_desc};
        use spvirit_types::{NtPayload, NtScalar, ScalarValue};

        let prev = NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(1.0)));
        let next = NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(3.5)));
        let desc = nt_payload_desc(&next);

        let (bitset, values) =
            encode_nt_payload_delta(&prev, &next, &desc, false).expect("value changed");

        // Spec order: changed bitset, data, then an empty overrun bitset.
        let mut body = bitset.clone();
        body.extend_from_slice(&values);
        body.extend_from_slice(&[0u8]); // zero-length overrun bitset

        let decoder = PvdDecoder::new(false);
        let update = decoder.decode_monitor_update(&body, &desc).unwrap();

        assert_eq!(update.consumed, body.len());
        assert!(!update.has_overrun());

        // The changed bitset the decoder reports is exactly what
        // compute_changed_bits produced for this pair.
        let bits =
            compute_changed_bits(&projection(&prev, &desc), &projection(&next, &desc), &desc)
                .expect("value changed");
        let mut expected = vec![0u8; bits.len().div_ceil(8)];
        for (i, b) in bits.iter().enumerate() {
            if *b {
                expected[i / 8] |= 1 << (i % 8);
            }
        }
        assert_eq!(update.changed, expected);
        assert!(bits[1], "bit 1 is 'value', which is what changed");

        // And the value round-trips.
        let DecodedValue::Structure(fields) = &update.value else {
            panic!("expected a structure");
        };
        let value = fields
            .iter()
            .find(|(n, _)| n == "value")
            .map(|(_, v)| v)
            .expect("value field present");
        match value {
            DecodedValue::Float64(v) => assert!((v - 3.5).abs() < 1e-9),
            other => panic!("unexpected value {other:?}"),
        }
    }

    /// `compute_changed_bits` compares two `DecodedValue`s; `encode_nt_payload_delta`
    /// projects each payload onto the descriptor first. Reproduce that
    /// projection by encoding and decoding against the same descriptor.
    fn projection(payload: &spvirit_types::NtPayload, desc: &StructureDesc) -> DecodedValue {
        let bytes = crate::spvd_encode::encode_nt_payload_values_for_desc(payload, desc, false);
        PvdDecoder::new(false)
            .decode_structure(&bytes, desc)
            .expect("projection")
            .0
    }
}
