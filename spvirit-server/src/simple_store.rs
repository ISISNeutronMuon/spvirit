//! A simple in-memory [`Source`] implementation backed by `RecordInstance`.
//!
//! Used by [`PvaServer`](crate::pva_server::PvaServer) to serve PVs without
//! requiring an external database.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{RwLock, mpsc};
use tracing::debug;

use std::future::Future;
use std::pin::Pin;

use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};
use spvirit_types::{NtPayload, ScalarArrayValue, ScalarValue};

use crate::monitor::MonitorRegistry;
use crate::pvstore::{PvInfo, Source};
use crate::types::{RecordData, RecordInstance};

/// Callback invoked after a PUT value is applied to a record.
pub type OnPutCallback = Arc<dyn Fn(&str, &DecodedValue) + Send + Sync>;

/// Callback invoked by the scan scheduler; returns the new value for the PV.
pub type ScanCallback = Arc<dyn Fn(&str) -> ScalarValue + Send + Sync>;

/// Callback that computes a derived PV value from its input values.
pub type LinkCallback = Arc<dyn Fn(&[ScalarValue]) -> ScalarValue + Send + Sync>;

/// Pre-apply PUT validator: `Err(msg)` rejects the PUT (error on the wire).
pub(crate) type PutValidator = Arc<dyn Fn(&str, &DecodedValue) -> Result<(), String> + Send + Sync>;

/// A link from one or more input PVs to a computed output PV.
pub(crate) struct LinkDef {
    pub output: String,
    pub inputs: Vec<String>,
    pub compute: LinkCallback,
}

struct PvEntry {
    record: RecordInstance,
    subscribers: Vec<mpsc::Sender<NtPayload>>,
    /// Value carried by the last update posted to subscribers/monitors —
    /// the reference point for the MDEL monitor deadband. `None` until the
    /// first post (so the first change always posts).
    last_posted: Option<f64>,
}

/// A simple in-memory PV store.
pub struct SimplePvStore {
    pvs: RwLock<HashMap<String, PvEntry>>,
    on_put: HashMap<String, OnPutCallback>,
    links: Vec<LinkDef>,
    compute_alarms: bool,
    registry: RwLock<Option<Arc<MonitorRegistry>>>,
    validators: RwLock<HashMap<String, PutValidator>>,
}

impl SimplePvStore {
    pub(crate) fn new(
        records: HashMap<String, RecordInstance>,
        on_put: HashMap<String, OnPutCallback>,
        links: Vec<LinkDef>,
        compute_alarms: bool,
    ) -> Self {
        let pvs = records
            .into_iter()
            .map(|(name, mut record)| {
                record.stamp_missing_timestamps();
                let last_posted = initial_posted(&record);
                (
                    name,
                    PvEntry {
                        record,
                        subscribers: Vec::new(),
                        last_posted,
                    },
                )
            })
            .collect();
        Self {
            pvs: RwLock::new(pvs),
            on_put,
            links,
            compute_alarms,
            registry: RwLock::new(None),
            validators: RwLock::new(HashMap::new()),
        }
    }

    /// Attach the [`MonitorRegistry`] so that `set_value` can push updates
    /// to PVAccess monitor clients.  Called automatically by [`PvaServer::run`].
    pub async fn set_registry(&self, registry: Arc<MonitorRegistry>) {
        *self.registry.write().await = Some(registry);
    }

    /// Register a pre-apply PUT validator for a PV.
    pub(crate) async fn set_validator(&self, name: String, v: PutValidator) {
        self.validators.write().await.insert(name, v);
    }

    /// Insert or replace a PV record at runtime.
    pub async fn insert(&self, name: String, mut record: RecordInstance) {
        record.stamp_missing_timestamps();
        let mut pvs = self.pvs.write().await;
        let last_posted = initial_posted(&record);
        pvs.insert(
            name,
            PvEntry {
                record,
                subscribers: Vec::new(),
                last_posted,
            },
        );
    }

    /// Remove a PV record at runtime. Returns `true` if a record was removed,
    /// `false` if no record with that name existed. Dropping the entry drops
    /// its subscriber senders, which closes any active monitor channels for
    /// that PV.
    pub async fn remove(&self, name: &str) -> bool {
        self.pvs.write().await.remove(name).is_some()
    }

    /// Read the current [`ScalarValue`] of a PV.
    pub async fn get_value(&self, name: &str) -> Option<ScalarValue> {
        let pvs = self.pvs.read().await;
        pvs.get(name).map(|e| e.record.current_value())
    }

    /// Read a clone of the full [`RecordInstance`] backing a PV.
    ///
    /// Used by [`RecordFieldSource`](crate::record_fields::RecordFieldSource)
    /// to serve `<name>.<FIELD>` channels.
    pub async fn get_record(&self, name: &str) -> Option<RecordInstance> {
        let pvs = self.pvs.read().await;
        pvs.get(name).map(|e| e.record.clone())
    }

    /// Read the full [`NtPayload`] of a PV.
    pub async fn get_nt(&self, name: &str) -> Option<NtPayload> {
        let pvs = self.pvs.read().await;
        pvs.get(name).map(|e| e.record.to_ntpayload())
    }

    /// Write a [`ScalarValue`] to a PV (bypasses on_put).
    pub async fn set_value(&self, name: &str, value: ScalarValue) -> bool {
        if self.set_value_inner(name, value).await {
            self.evaluate_links(name).await;
            true
        } else {
            false
        }
    }

    /// Write a [`ScalarArrayValue`] to an array PV (bypasses on_put).
    pub async fn set_array_value(&self, name: &str, value: ScalarArrayValue) -> bool {
        if self.set_array_value_inner(name, value).await {
            self.evaluate_links(name).await;
            true
        } else {
            false
        }
    }

    /// Write a full [`NtPayload`] to a PV (bypasses on_put).
    pub async fn put_nt(&self, name: &str, payload: NtPayload) -> bool {
        if self.put_nt_inner(name, payload).await {
            self.evaluate_links(name).await;
            true
        } else {
            false
        }
    }

    /// Explicitly set a record's alarm fields (severity/status/message),
    /// independent of its value. Unlike [`SimplePvStore::set_value`], alarm
    /// transitions always post — there is no MDEL deadband gating and no
    /// link evaluation (alarm changes don't propagate links). Returns
    /// `false` if the alarm state is unchanged (idempotent) or the record
    /// doesn't support alarm fields (`Table`/`NdArray`/`Generic`) or doesn't
    /// exist.
    pub async fn set_alarm(&self, name: &str, severity: i32, status: i32, message: &str) -> bool {
        let payload = {
            let mut pvs = self.pvs.write().await;
            let Some(entry) = pvs.get_mut(name) else {
                return false;
            };
            let alarm = if let Some(nt) = entry.record.nt_scalar_mut() {
                (
                    &mut nt.alarm_severity,
                    &mut nt.alarm_status,
                    &mut nt.alarm_message,
                )
            } else {
                match &mut entry.record.data {
                    RecordData::NtEnum { nt, .. } => (
                        &mut nt.alarm.severity,
                        &mut nt.alarm.status,
                        &mut nt.alarm.message,
                    ),
                    RecordData::Waveform { nt, .. }
                    | RecordData::Aai { nt, .. }
                    | RecordData::Aao { nt, .. }
                    | RecordData::SubArray { nt, .. } => (
                        &mut nt.alarm.severity,
                        &mut nt.alarm.status,
                        &mut nt.alarm.message,
                    ),
                    _ => return false,
                }
            };
            let (sev, sta, msg) = alarm;
            let changed = *sev != severity || *sta != status || msg.as_str() != message;
            if !changed {
                return false;
            }
            *sev = severity;
            *sta = status;
            *msg = message.to_string();

            let payload = entry.record.to_ntpayload();
            entry
                .subscribers
                .retain(|tx| deliver_or_keep(tx, &payload));
            payload
        };

        let reg = self.registry.read().await;
        if let Some(registry) = reg.as_ref() {
            registry.notify_monitors(name, &payload).await;
        }
        true
    }

    /// Core write logic — updates the value, notifies subscribers and monitors,
    /// but does **not** trigger link evaluation (to avoid recursion).
    async fn set_value_inner(&self, name: &str, value: ScalarValue) -> bool {
        let payload = {
            let mut pvs = self.pvs.write().await;
            if let Some(entry) = pvs.get_mut(name) {
                let prev_severity = entry.record.to_ntscalar().alarm_severity;
                let changed = entry.record.set_scalar_value(value, self.compute_alarms);
                if changed {
                    if !should_post_update(entry, prev_severity) {
                        // Changed, but within the MDEL monitor deadband:
                        // the record holds the new value (GETs see it), just
                        // no update is posted to subscribers/monitors.
                        return true;
                    }
                    let payload = entry.record.to_ntpayload();
                    entry
                        .subscribers
                        .retain(|tx| deliver_or_keep(tx, &payload));
                    Some(payload)
                } else {
                    None
                }
            } else {
                return false;
            }
        };

        if let Some(payload) = payload {
            // Notify PVAccess monitor clients (if the registry is attached).
            let reg = self.registry.read().await;
            if let Some(registry) = reg.as_ref() {
                registry.notify_monitors(name, &payload).await;
            }
            true
        } else {
            false
        }
    }

    /// Core array write logic — updates the value, notifies subscribers and monitors,
    /// but does **not** trigger link evaluation (to avoid recursion).
    async fn set_array_value_inner(&self, name: &str, value: ScalarArrayValue) -> bool {
        let payload = {
            let mut pvs = self.pvs.write().await;
            if let Some(entry) = pvs.get_mut(name) {
                let changed = entry.record.set_array_value(value);
                if changed {
                    let payload = entry.record.to_ntpayload();
                    entry
                        .subscribers
                        .retain(|tx| deliver_or_keep(tx, &payload));
                    Some(payload)
                } else {
                    None
                }
            } else {
                return false;
            }
        };

        if let Some(payload) = payload {
            // Notify PVAccess monitor clients (if the registry is attached).
            let reg = self.registry.read().await;
            if let Some(registry) = reg.as_ref() {
                registry.notify_monitors(name, &payload).await;
            }
            true
        } else {
            false
        }
    }

    /// Core NtPayload write logic — updates the payload, notifies subscribers
    /// and monitors, but does **not** trigger link evaluation.
    async fn put_nt_inner(&self, name: &str, payload: NtPayload) -> bool {
        let payload = {
            let mut pvs = self.pvs.write().await;
            if let Some(entry) = pvs.get_mut(name) {
                let changed = entry.record.set_nt_payload(payload);
                if changed {
                    let payload = entry.record.to_ntpayload();
                    entry
                        .subscribers
                        .retain(|tx| deliver_or_keep(tx, &payload));
                    Some(payload)
                } else {
                    None
                }
            } else {
                return false;
            }
        };

        if let Some(payload) = payload {
            // Notify PVAccess monitor clients (if the registry is attached).
            let reg = self.registry.read().await;
            if let Some(registry) = reg.as_ref() {
                registry.notify_monitors(name, &payload).await;
            }
            true
        } else {
            false
        }
    }

    /// Walk every link whose inputs include `changed_pv`, compute the output,
    /// and propagate (BFS with cycle detection).
    async fn evaluate_links(&self, changed_pv: &str) {
        if self.links.is_empty() {
            return;
        }
        let mut queue = vec![changed_pv.to_string()];
        let mut visited = HashSet::new();

        while let Some(pv) = queue.pop() {
            if !visited.insert(pv.clone()) {
                debug!("Circular link detected for PV '{}', skipping", pv);
                continue;
            }
            for link in &self.links {
                if !link.inputs.iter().any(|i| i == &pv) {
                    continue;
                }
                // Gather current values of all inputs.
                let values = {
                    let pvs = self.pvs.read().await;
                    link.inputs
                        .iter()
                        .map(|n| {
                            pvs.get(n)
                                .map(|e| e.record.current_value())
                                .unwrap_or(ScalarValue::F64(0.0))
                        })
                        .collect::<Vec<_>>()
                };
                let new_val = (link.compute)(&values);
                if self.set_value_inner(&link.output, new_val).await {
                    queue.push(link.output.clone());
                }
            }
        }
    }

    /// List all PV names.
    pub async fn pv_names(&self) -> Vec<String> {
        let pvs = self.pvs.read().await;
        pvs.keys().cloned().collect()
    }
}

impl Source for SimplePvStore {
    /// This store self-notifies: every write both sends to its per-PV
    /// `subscribers` and calls `registry.notify_monitors`, so the monitor
    /// handler must not also pump `subscribe` — doing so would double-deliver.
    fn pushes_own_updates(&self) -> bool {
        true
    }

    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let pvs = self.pvs.read().await;
            let entry = pvs.get(&name)?;
            let descriptor = descriptor_for_payload(&entry.record.to_ntpayload());
            Some(PvInfo {
                descriptor,
                writable: entry.record.writable(),
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let pvs = self.pvs.read().await;
            pvs.get(&name).map(|e| e.record.to_ntpayload())
        })
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let name = name.to_string();
        let value = value.clone();
        Box::pin(async move {
            // Clone the validator out inside a tight scope so the read guard
            // drops before the user callback runs — otherwise temporary
            // lifetime extension holds the lock across the call, blocking
            // concurrent set_validator for the duration of every PUT.
            let validator = {
                let guard = self.validators.read().await;
                guard.get(&name).cloned()
            };
            if let Some(v) = validator {
                v(&name, &value)?;
            }

            let result = {
                let mut pvs = self.pvs.write().await;
                let entry = pvs
                    .get_mut(&name)
                    .ok_or_else(|| format!("PV '{}' not found", name))?;

                if !entry.record.writable() {
                    return Err(format!("PV '{}' is not writable", name));
                }

                let prev_severity = entry.record.to_ntscalar().alarm_severity;
                let outcome = entry.record.apply_put(&value, self.compute_alarms);

                // A client-supplied timeStamp is new information even when the
                // value is identical, so it posts. A server-generated stamp on
                // an unchanged value updates the record silently — GETs and the
                // next real post carry it. should_post_update is only consulted
                // on a value change, so the MDEL reference point is untouched
                // by timestamp-only updates.
                let post = if outcome.value_changed {
                    should_post_update(entry, prev_severity)
                } else {
                    outcome.client_stamped
                };

                if post {
                    let payload = entry.record.to_ntpayload();
                    entry
                        .subscribers
                        .retain(|tx| deliver_or_keep(tx, &payload));
                    (Some((name.clone(), payload)), outcome)
                } else {
                    (None, outcome)
                }
            }; // pvs lock dropped
            let (result, outcome) = result;

            // EPICS runs the forward link on every record process, so on_put
            // fires for every accepted PUT — including one that did not change
            // the value.
            if let Some(cb) = self.on_put.get(&name) {
                let cb = cb.clone();
                let n = name.clone();
                let v = value.clone();
                tokio::spawn(async move { cb(&n, &v) });
            }

            // Links are change-driven here, not post-driven: a PUT suppressed
            // by the MDEL deadband still propagates, matching set_value.
            if outcome.value_changed {
                self.evaluate_links(&name).await;
            }

            Ok(result.into_iter().collect())
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let mut pvs = self.pvs.write().await;
            let entry = pvs.get_mut(&name)?;
            let (tx, rx) = mpsc::channel(64);
            entry.subscribers.push(tx);
            Some(rx)
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async move {
            let pvs = self.pvs.read().await;
            pvs.keys().cloned().collect()
        })
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Try to deliver `payload` to one subscriber, returning whether the sender
/// should be retained.
///
/// Distinguishes a *full* channel from a *closed* one — the crate-wide
/// prune-on-Full convention (same shape as the committed gateway `dispatch`
/// fix, item 0a): `Full` keeps the subscriber and drops only this update (a
/// slow-but-live receiver must not be silently unsubscribed), `Closed` removes
/// it. The bare `try_send(..).is_ok()` this replaces dropped both cases,
/// which for any future subscriber that could actually fill the buffer would be
/// a latent unsubscribe-on-a-single-slow-tick trap. (Unreachable via the server
/// path today, where `pushes_own_updates()==true`.)
fn deliver_or_keep(tx: &mpsc::Sender<NtPayload>, payload: &NtPayload) -> bool {
    match tx.try_send(payload.clone()) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

/// Numeric view of a scalar value, for deadband arithmetic.
fn scalar_as_f64(v: &ScalarValue) -> Option<f64> {
    Some(match v {
        ScalarValue::I8(x) => *x as f64,
        ScalarValue::I16(x) => *x as f64,
        ScalarValue::I32(x) => *x as f64,
        ScalarValue::I64(x) => *x as f64,
        ScalarValue::U8(x) => *x as f64,
        ScalarValue::U16(x) => *x as f64,
        ScalarValue::U32(x) => *x as f64,
        ScalarValue::U64(x) => *x as f64,
        ScalarValue::F32(x) => *x as f64,
        ScalarValue::F64(x) => *x,
        ScalarValue::Bool(_) | ScalarValue::Str(_) => return None,
    })
}

/// Initial MDEL deadband reference: the record's starting value (EPICS
/// initialises MLST from the initial VAL, so the first small change is
/// already subject to the deadband).
fn initial_posted(record: &RecordInstance) -> Option<f64> {
    match record.to_ntpayload() {
        NtPayload::Scalar(nt) => scalar_as_f64(&nt.value),
        _ => None,
    }
}

/// MDEL monitor-deadband gate, called after a record changed.
///
/// Returns `true` when the update must be posted to subscribers/monitors
/// (and records it as the new deadband reference point). An update is
/// suppressed only when the record is a numeric scalar with MDEL > 0, the
/// alarm severity did not change, and the value moved less than MDEL from
/// the last *posted* value — EPICS monitor-deadband semantics.
fn should_post_update(entry: &mut PvEntry, prev_severity: i32) -> bool {
    let new_f = match entry.record.to_ntpayload() {
        NtPayload::Scalar(nt) => match scalar_as_f64(&nt.value) {
            Some(f) => f,
            None => return true,
        },
        _ => return true,
    };
    let mdel = crate::record_fields::mdel_of(&entry.record);
    let severity_changed = entry.record.to_ntscalar().alarm_severity != prev_severity;
    let within_deadband = mdel > 0.0
        && !severity_changed
        && entry
            .last_posted
            .is_some_and(|last| (new_f - last).abs() < mdel);
    if within_deadband {
        return false;
    }
    entry.last_posted = Some(new_f);
    true
}

// ── NtPayload → StructureDesc ────────────────────────────────────────────

/// The descriptor the server advertises at GET/MONITOR INIT.
///
/// This delegates to the codec for every payload shape. It used to
/// reimplement the NTScalar and NTScalarArray trees locally, and the two
/// copies drifted: the local one spelled a string value
/// `Scalar(TypeCode::String)` where the codec spells it `String`, and gave
/// `timeStamp`/`display` struct ids the codec omitted. Both spellings put the
/// same byte on the wire, but the descriptor is not only advertised — it is
/// also what PUT bodies are decoded against, and `decode_scalar` rejects
/// `TypeCode::String` because it has no fixed size. The result was that a PUT
/// to any string-valued PV failed to decode. Keeping one builder is what stops
/// that class of drift.
pub fn descriptor_for_payload(payload: &NtPayload) -> StructureDesc {
    spvirit_codec::spvd_encode::nt_payload_desc(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DbCommonState, RecordType};
    use spvirit_codec::spvd_decode::{FieldType, TypeCode};
    use spvirit_types::{
        NdCodec, NdDimension, NtNdArray, NtPayload, NtScalar, NtScalarArray, NtTable,
        NtTableColumn, ScalarArrayValue, ScalarValue,
    };

    /// Every payload shape the server can advertise a descriptor for.
    fn descriptor_payload_samples() -> Vec<(&'static str, NtPayload)> {
        vec![
            (
                "NTScalar/f64",
                NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(1.5))),
            ),
            (
                "NTScalar/i32",
                NtPayload::Scalar(NtScalar::from_value(ScalarValue::I32(7))),
            ),
            (
                "NTScalar/bool",
                NtPayload::Scalar(NtScalar::from_value(ScalarValue::Bool(true))),
            ),
            (
                "NTScalar/string",
                NtPayload::Scalar(NtScalar::from_value(ScalarValue::Str("hi".to_string()))),
            ),
            (
                "NTScalarArray/f64",
                NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::F64(vec![
                    1.0, 2.0,
                ]))),
            ),
            (
                "NTScalarArray/string",
                NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::Str(vec![
                    "a".to_string(),
                    "b".to_string(),
                ]))),
            ),
        ]
    }

    /// The server advertises `descriptor_for_payload` at GET/MONITOR INIT, but
    /// the value bytes come from the codec. `simple_store` reimplements the
    /// NTScalar and NTScalarArray descriptors instead of calling the codec, so
    /// the two trees can drift apart — and when they do, a strict client
    /// (pvxs) decodes past the end of the frame and drops the whole TCP
    /// connection. That is exactly how the missing `display.form` shipped.
    ///
    /// Assert the two descriptor trees are identical, so a future edit to
    /// either one cannot silently reintroduce that class of defect.
    #[test]
    fn server_descriptor_matches_the_codec_descriptor() {
        for (label, payload) in descriptor_payload_samples() {
            let ours = descriptor_for_payload(&payload);
            let codec = spvirit_codec::spvd_encode::nt_payload_desc(&payload);
            assert_eq!(
                ours, codec,
                "server and codec descriptors disagree for {label}"
            );
        }
    }

    /// The bytes we put on the wire must decode against the descriptor we
    /// advertised, consuming it exactly — no more, no less.
    #[test]
    fn advertised_descriptor_decodes_the_encoded_bytes_exactly() {
        for (label, payload) in descriptor_payload_samples() {
            for is_be in [false, true] {
                let desc = descriptor_for_payload(&payload);
                let bytes = spvirit_codec::spvd_encode::encode_nt_payload_values_for_desc(
                    &payload, &desc, is_be,
                );
                let decoder = spvirit_codec::spvd_decode::PvdDecoder::new(is_be);
                let (decoded, consumed) = decoder
                    .decode_structure(&bytes, &desc)
                    .unwrap_or_else(|e| panic!("{label} (be={is_be}): decode failed: {e:?}"));
                assert_eq!(
                    consumed,
                    bytes.len(),
                    "{label} (be={is_be}): advertised descriptor consumed {consumed} of {} bytes",
                    bytes.len()
                );
                let DecodedValue::Structure(fields) = &decoded else {
                    panic!("{label}: top level is not a structure");
                };
                assert_eq!(
                    fields.len(),
                    desc.fields.len(),
                    "{label} (be={is_be}): decoded {} fields, descriptor declares {}",
                    fields.len(),
                    desc.fields.len()
                );
            }
        }
    }

    /// A PUT body is decoded against the descriptor the server advertised at
    /// INIT (`ioid_to_desc`), so that descriptor has to be one our own decoder
    /// can read. When `simple_store` built its own tree it spelled a string
    /// value `Scalar(TypeCode::String)`, which `decode_scalar` rejects because
    /// that type code has no fixed size — so every PUT to a string-valued PV
    /// silently failed to decode while numeric PVs worked.
    #[test]
    fn put_bodies_decode_against_the_advertised_descriptor() {
        for (label, payload) in descriptor_payload_samples() {
            let desc = descriptor_for_payload(&payload);
            let (bits, vals) =
                spvirit_codec::spvd_encode::encode_nt_payload_bitset_parts(&payload, false);
            let mut body = bits;
            body.extend_from_slice(&vals);
            assert!(
                crate::decode::decode_put_body(&body, &desc, false).is_some(),
                "{label}: PUT body did not decode against the advertised descriptor"
            );
        }
    }

    fn make_ai(name: &str, val: f64) -> RecordInstance {
        RecordInstance {
            name: name.to_string(),
            record_type: RecordType::Ai,
            common: DbCommonState::default(),
            data: RecordData::Ai {
                nt: NtScalar::from_value(ScalarValue::F64(val)),
                inp: None,
                siml: None,
                siol: None,
                simm: false,
            },
            raw_fields: HashMap::new(),
        }
    }

    fn make_mbbo(name: &str, choices: Vec<String>, initial: i32) -> RecordInstance {
        RecordInstance {
            name: name.to_string(),
            record_type: RecordType::Mbbo,
            common: DbCommonState::default(),
            data: RecordData::NtEnum {
                nt: spvirit_types::NtEnum::new(initial, choices),
                inp: None,
                out: None,
                omsl: crate::types::OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        }
    }

    fn make_ao(name: &str, val: f64) -> RecordInstance {
        RecordInstance {
            name: name.to_string(),
            record_type: RecordType::Ao,
            common: DbCommonState::default(),
            data: RecordData::Ao {
                nt: NtScalar::from_value(ScalarValue::F64(val)),
                out: None,
                dol: None,
                omsl: crate::types::OutputMode::Supervisory,
                drvl: None,
                drvh: None,
                oroc: None,
                siml: None,
                siol: None,
                simm: false,
            },
            raw_fields: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn mdel_deadband_suppresses_small_monitor_updates() {
        let recs = crate::db::parse_db(
            r#"
record(ao, "DB:AO") {
    field(VAL, "0.0")
    field(MDEL, "0.5")
}"#,
        )
        .expect("parse");
        let store = SimplePvStore::new(recs, HashMap::new(), Vec::new(), false);
        let mut rx = Source::subscribe(&store, "DB:AO")
            .await
            .expect("subscribed");

        // |Δ| = 0.2 < MDEL 0.5 → value updates but no monitor post.
        assert!(store.set_value("DB:AO", ScalarValue::F64(0.2)).await);
        // |Δ| = 0.9 ≥ MDEL 0.5 → posted.
        assert!(store.set_value("DB:AO", ScalarValue::F64(0.9)).await);

        match rx.recv().await.expect("posted update") {
            NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::F64(0.9)),
            other => panic!("expected scalar, got {other:?}"),
        }
        // The suppressed 0.2 update must not be queued behind it.
        assert!(rx.try_recv().is_err());
        // GETs always see the latest value regardless of the deadband.
        assert_eq!(store.get_value("DB:AO").await, Some(ScalarValue::F64(0.9)));
    }

    fn make_waveform(name: &str, value: ScalarArrayValue) -> RecordInstance {
        let nelm = value.len();
        RecordInstance {
            name: name.to_string(),
            record_type: RecordType::Waveform,
            common: DbCommonState::default(),
            data: RecordData::Waveform {
                nt: NtScalarArray::from_value(value),
                inp: None,
                ftvl: "DOUBLE".to_string(),
                nelm,
                nord: nelm,
            },
            raw_fields: HashMap::new(),
        }
    }

    fn make_nt_table(name: &str) -> RecordInstance {
        RecordInstance {
            name: name.to_string(),
            record_type: RecordType::NtTable,
            common: DbCommonState::default(),
            data: RecordData::NtTable {
                nt: NtTable {
                    labels: vec!["X".to_string(), "Y".to_string()],
                    columns: vec![
                        NtTableColumn {
                            name: "x".to_string(),
                            values: ScalarArrayValue::F64(vec![1.0, 2.0]),
                        },
                        NtTableColumn {
                            name: "y".to_string(),
                            values: ScalarArrayValue::F64(vec![10.0, 20.0]),
                        },
                    ],
                    descriptor: Some("table".to_string()),
                    alarm: None,
                    time_stamp: None,
                },
                inp: None,
                out: None,
                omsl: crate::types::OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        }
    }

    fn make_nt_ndarray(name: &str) -> RecordInstance {
        RecordInstance {
            name: name.to_string(),
            record_type: RecordType::NtNdArray,
            common: DbCommonState::default(),
            data: RecordData::NtNdArray {
                nt: NtNdArray {
                    value: ScalarArrayValue::U8(vec![0; 4]),
                    codec: NdCodec {
                        name: "none".to_string(),
                        parameters: HashMap::new(),
                    },
                    compressed_size: 4,
                    uncompressed_size: 4,
                    dimension: vec![NdDimension {
                        size: 2,
                        offset: 0,
                        full_size: 2,
                        binning: 1,
                        reverse: false,
                    }],
                    unique_id: 1,
                    data_time_stamp: Default::default(),
                    attribute: vec![],
                    descriptor: Some("ndarray".to_string()),
                    alarm: None,
                    time_stamp: None,
                    display: None,
                },
                inp: None,
                out: None,
                omsl: crate::types::OutputMode::Supervisory,
            },
            raw_fields: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn store_stamps_initial_timestamps_on_static_records() {
        // Records that are never updated after creation (e.g. a static
        // NTTable) must still carry a valid timestamp — clients like the
        // EPICS Archiver Appliance reject epoch-0 events.
        let mut records = HashMap::new();
        records.insert("TEST:TBL".into(), make_nt_table("TEST:TBL"));
        records.insert("TEST:NDA".into(), make_nt_ndarray("TEST:NDA"));
        records.insert(
            "TEST:ENUM".into(),
            make_mbbo("TEST:ENUM", vec!["A".into(), "B".into()], 0),
        );
        records.insert(
            "TEST:WF".into(),
            make_waveform("TEST:WF", ScalarArrayValue::F64(vec![0.0])),
        );
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 1.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        match store.get_nt("TEST:TBL").await.unwrap() {
            NtPayload::Table(nt) => {
                assert!(nt.time_stamp.expect("table stamped").seconds_past_epoch > 0)
            }
            _ => panic!("expected table"),
        }
        match store.get_nt("TEST:NDA").await.unwrap() {
            NtPayload::NdArray(nt) => {
                assert!(nt.time_stamp.expect("ndarray stamped").seconds_past_epoch > 0);
                assert!(nt.data_time_stamp.seconds_past_epoch > 0);
            }
            _ => panic!("expected ndarray"),
        }
        match store.get_nt("TEST:ENUM").await.unwrap() {
            NtPayload::Enum(nt) => assert!(nt.time_stamp.seconds_past_epoch > 0),
            _ => panic!("expected enum"),
        }
        match store.get_nt("TEST:WF").await.unwrap() {
            NtPayload::ScalarArray(nt) => assert!(nt.time_stamp.seconds_past_epoch > 0),
            _ => panic!("expected scalar array"),
        }
        match store.get_nt("TEST:AI").await.unwrap() {
            NtPayload::Scalar(nt) => {
                assert!(nt.time_stamp.expect("scalar stamped").seconds_past_epoch > 0)
            }
            _ => panic!("expected scalar"),
        }
    }

    #[tokio::test]
    async fn has_pv_returns_true_for_existing() {
        let mut records = HashMap::new();
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 1.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);
        assert!(store.claim("TEST:AI").await.is_some());
        assert!(store.claim("MISSING").await.is_none());
    }

    #[tokio::test]
    async fn get_snapshot_returns_payload() {
        let mut records = HashMap::new();
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 42.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);
        let snap = store.get("TEST:AI").await.unwrap();
        match snap {
            NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::F64(42.0)),
            _ => panic!("expected scalar"),
        }
    }

    #[tokio::test]
    async fn put_value_updates_writable_record() {
        let mut records = HashMap::new();
        records.insert("TEST:AO".into(), make_ao("TEST:AO", 0.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        let val = DecodedValue::Structure(vec![("value".to_string(), DecodedValue::Float64(99.5))]);
        let result = store.put("TEST:AO", &val).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "TEST:AO");

        let snap = store.get("TEST:AO").await.unwrap();
        match snap {
            NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::F64(99.5)),
            _ => panic!("expected scalar"),
        }
    }

    #[tokio::test]
    async fn put_wire_rejects_out_of_range_enum_index() {
        let mut records = HashMap::new();
        records.insert(
            "E".into(),
            make_mbbo("E", vec!["A".into(), "B".into(), "C".into()], 0),
        );
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        // Out-of-range index — must be a no-op (Ok, no changed PVs), index unchanged.
        let result = Source::put(&store, "E", &DecodedValue::Int32(7))
            .await
            .unwrap();
        assert!(result.is_empty());
        assert_eq!(store.get_value("E").await.unwrap(), ScalarValue::I32(0));

        // In-range index — applied.
        let result = Source::put(&store, "E", &DecodedValue::Int32(2))
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(store.get_value("E").await.unwrap(), ScalarValue::I32(2));
    }

    #[tokio::test]
    async fn put_value_rejects_readonly() {
        let mut records = HashMap::new();
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 1.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        let val = DecodedValue::Float64(5.0);
        let err = store.put("TEST:AI", &val).await.unwrap_err();
        assert!(err.contains("not writable"));
    }

    #[tokio::test]
    async fn set_value_bypasses_writable_check() {
        let mut records = HashMap::new();
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 1.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);
        assert!(store.set_value("TEST:AI", ScalarValue::F64(10.0)).await);
        let val = store.get_value("TEST:AI").await.unwrap();
        assert_eq!(val, ScalarValue::F64(10.0));
    }

    #[tokio::test]
    async fn set_array_value_updates_all_scalar_array_types() {
        let cases: Vec<ScalarArrayValue> = vec![
            ScalarArrayValue::Bool(vec![false, true]),
            ScalarArrayValue::I8(vec![1, 2]),
            ScalarArrayValue::I16(vec![1, 2]),
            ScalarArrayValue::I32(vec![1, 2]),
            ScalarArrayValue::I64(vec![1, 2]),
            ScalarArrayValue::U8(vec![1, 2]),
            ScalarArrayValue::U16(vec![1, 2]),
            ScalarArrayValue::U32(vec![1, 2]),
            ScalarArrayValue::U64(vec![1, 2]),
            ScalarArrayValue::F32(vec![1.0, 2.0]),
            ScalarArrayValue::F64(vec![1.0, 2.0]),
            ScalarArrayValue::Str(vec!["a".to_string(), "b".to_string()]),
        ];

        for (idx, updated) in cases.into_iter().enumerate() {
            let pv = format!("TEST:WF:{idx}");
            let mut records = HashMap::new();
            records.insert(pv.clone(), make_waveform(&pv, updated.clone()));
            let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

            assert!(!store.set_array_value(&pv, updated.clone()).await);

            let second = match updated {
                ScalarArrayValue::Bool(_) => ScalarArrayValue::Bool(vec![true, false]),
                ScalarArrayValue::I8(_) => ScalarArrayValue::I8(vec![3, 4]),
                ScalarArrayValue::I16(_) => ScalarArrayValue::I16(vec![3, 4]),
                ScalarArrayValue::I32(_) => ScalarArrayValue::I32(vec![3, 4]),
                ScalarArrayValue::I64(_) => ScalarArrayValue::I64(vec![3, 4]),
                ScalarArrayValue::U8(_) => ScalarArrayValue::U8(vec![3, 4]),
                ScalarArrayValue::U16(_) => ScalarArrayValue::U16(vec![3, 4]),
                ScalarArrayValue::U32(_) => ScalarArrayValue::U32(vec![3, 4]),
                ScalarArrayValue::U64(_) => ScalarArrayValue::U64(vec![3, 4]),
                ScalarArrayValue::F32(_) => ScalarArrayValue::F32(vec![3.0, 4.0]),
                ScalarArrayValue::F64(_) => ScalarArrayValue::F64(vec![3.0, 4.0]),
                ScalarArrayValue::Str(_) => {
                    ScalarArrayValue::Str(vec!["x".to_string(), "y".to_string()])
                }
            };

            assert!(store.set_array_value(&pv, second.clone()).await);
            let snap = store.get(&pv).await.unwrap();
            match snap {
                NtPayload::ScalarArray(nt) => assert_eq!(nt.value, second),
                _ => panic!("expected scalar array"),
            }
        }
    }

    #[tokio::test]
    async fn get_nt_returns_full_payload() {
        let mut records = HashMap::new();
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 12.5));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        let nt = store.get_nt("TEST:AI").await.unwrap();
        match nt {
            NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::F64(12.5)),
            _ => panic!("expected scalar payload"),
        }
    }

    #[tokio::test]
    async fn put_nt_updates_scalar_array_table_and_ndarray() {
        let mut records = HashMap::new();
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 1.0));
        records.insert(
            "TEST:WF".into(),
            make_waveform("TEST:WF", ScalarArrayValue::F64(vec![0.0, 0.0])),
        );
        records.insert("TEST:TBL".into(), make_nt_table("TEST:TBL"));
        records.insert("TEST:NDA".into(), make_nt_ndarray("TEST:NDA"));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        assert!(
            store
                .put_nt(
                    "TEST:AI",
                    NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(5.0))),
                )
                .await
        );
        assert!(
            store
                .put_nt(
                    "TEST:WF",
                    NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::F64(vec![
                        3.0, 4.0
                    ],))),
                )
                .await
        );

        let table = NtTable {
            labels: vec!["X".to_string(), "Y".to_string()],
            columns: vec![
                NtTableColumn {
                    name: "x".to_string(),
                    values: ScalarArrayValue::F64(vec![2.0, 3.0]),
                },
                NtTableColumn {
                    name: "y".to_string(),
                    values: ScalarArrayValue::F64(vec![20.0, 30.0]),
                },
            ],
            descriptor: Some("updated table".to_string()),
            alarm: None,
            time_stamp: None,
        };
        assert!(
            store
                .put_nt("TEST:TBL", NtPayload::Table(table.clone()))
                .await
        );

        let ndarray = NtNdArray {
            value: ScalarArrayValue::U8(vec![1, 2, 3, 4]),
            codec: NdCodec {
                name: "none".to_string(),
                parameters: HashMap::new(),
            },
            compressed_size: 4,
            uncompressed_size: 4,
            dimension: vec![NdDimension {
                size: 4,
                offset: 0,
                full_size: 4,
                binning: 1,
                reverse: false,
            }],
            unique_id: 2,
            data_time_stamp: Default::default(),
            attribute: vec![],
            descriptor: Some("updated ndarray".to_string()),
            alarm: None,
            time_stamp: None,
            display: None,
        };
        assert!(
            store
                .put_nt("TEST:NDA", NtPayload::NdArray(ndarray.clone()))
                .await
        );

        assert!(
            !store
                .put_nt(
                    "TEST:AI",
                    NtPayload::ScalarArray(NtScalarArray::from_value(ScalarArrayValue::F64(vec![
                        1.0
                    ]))),
                )
                .await
        );

        // The caller supplied no timestamps, so the store stamps the update
        // time — compare everything else verbatim.
        match store.get_nt("TEST:TBL").await.unwrap() {
            NtPayload::Table(mut nt) => {
                let ts = nt.time_stamp.take().expect("table put must be stamped");
                assert!(ts.seconds_past_epoch > 0);
                assert_eq!(nt, table);
            }
            _ => panic!("expected table payload"),
        }
        match store.get_nt("TEST:NDA").await.unwrap() {
            NtPayload::NdArray(mut nt) => {
                let ts = nt.time_stamp.take().expect("ndarray put must be stamped");
                assert!(ts.seconds_past_epoch > 0);
                assert!(nt.data_time_stamp.seconds_past_epoch > 0);
                nt.data_time_stamp = Default::default();
                assert_eq!(nt, ndarray);
            }
            _ => panic!("expected ndarray payload"),
        }
    }

    #[tokio::test]
    async fn descriptor_matches_value_type() {
        let mut records = HashMap::new();
        records.insert("TEST:AI".into(), make_ai("TEST:AI", 0.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);
        let info = store.claim("TEST:AI").await.unwrap();
        assert_eq!(
            info.descriptor.struct_id.as_deref(),
            Some("epics:nt/NTScalar:1.0")
        );
        let desc = info.descriptor;
        let value_field = desc.field("value").unwrap();
        assert!(matches!(
            value_field.field_type,
            FieldType::Scalar(TypeCode::Float64)
        ));
    }

    #[tokio::test]
    async fn subscribe_receives_updates() {
        let mut records = HashMap::new();
        records.insert("TEST:AO".into(), make_ao("TEST:AO", 0.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        let mut rx = Source::subscribe(&store, "TEST:AO").await.unwrap();

        let val = DecodedValue::Structure(vec![("value".to_string(), DecodedValue::Float64(7.7))]);
        store.put("TEST:AO", &val).await.unwrap();

        let update = rx.recv().await.unwrap();
        match update {
            NtPayload::Scalar(nt) => assert_eq!(nt.value, ScalarValue::F64(7.7)),
            _ => panic!("expected scalar"),
        }
    }

    #[tokio::test]
    async fn on_put_callback_is_invoked() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let called = Arc::new(AtomicBool::new(false));
        let called2 = called.clone();

        let mut records = HashMap::new();
        records.insert("CB:AO".into(), make_ao("CB:AO", 0.0));

        let mut on_put = HashMap::new();
        let cb: OnPutCallback = Arc::new(move |_name, _val| {
            called2.store(true, Ordering::SeqCst);
        });
        on_put.insert("CB:AO".into(), cb);

        let store = SimplePvStore::new(records, on_put, vec![], false);
        let val = DecodedValue::Structure(vec![("value".to_string(), DecodedValue::Float64(1.0))]);
        store.put("CB:AO", &val).await.unwrap();

        // Give the spawned task time to run.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert!(called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn validator_rejects_put_before_apply() {
        let mut records = std::collections::HashMap::new();
        records.insert(
            "V".to_string(),
            crate::pva_server::make_output_record(
                "V",
                crate::types::RecordType::Ao,
                ScalarValue::F64(1.0),
            ),
        );
        let store =
            SimplePvStore::new(records, std::collections::HashMap::new(), Vec::new(), false);
        store
            .set_validator(
                "V".to_string(),
                std::sync::Arc::new(|_name, _val| Err("nope".to_string())),
            )
            .await;

        let dv = DecodedValue::Float64(2.0);
        let res = Source::put(&store, "V", &dv).await;
        assert_eq!(res, Err("nope".to_string()));
        // value unchanged — validator ran BEFORE apply
        assert_eq!(store.get_value("V").await, Some(ScalarValue::F64(1.0)));
    }

    #[tokio::test]
    async fn remove_deletes_record_and_is_idempotent() {
        let mut records = std::collections::HashMap::new();
        records.insert(
            "T:GONE".to_string(),
            crate::pva_server::make_scalar_record("T:GONE", RecordType::Ai, ScalarValue::F64(1.0)),
        );
        let store = SimplePvStore::new(records, Default::default(), Vec::new(), false);

        assert!(store.get_value("T:GONE").await.is_some());
        assert!(store.remove("T:GONE").await, "first remove returns true");
        assert!(store.get_value("T:GONE").await.is_none(), "record is gone");
        assert!(!store.remove("T:GONE").await, "second remove returns false");
        assert!(store.claim("T:GONE").await.is_none(), "claim no longer matches");
    }

    #[tokio::test]
    async fn put_advances_the_timestamp() {
        let mut records = HashMap::new();
        records.insert("TEST:AO".into(), make_ao("TEST:AO", 1.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);

        let first = match store.get_nt("TEST:AO").await.unwrap() {
            NtPayload::Scalar(nt) => nt.time_stamp.unwrap(),
            other => panic!("expected scalar, got {other:?}"),
        };

        store
            .put("TEST:AO", &DecodedValue::Float64(2.0))
            .await
            .unwrap();

        let second = match store.get_nt("TEST:AO").await.unwrap() {
            NtPayload::Scalar(nt) => nt.time_stamp.unwrap(),
            other => panic!("expected scalar, got {other:?}"),
        };
        assert!(
            (second.seconds_past_epoch, second.nanoseconds)
                > (first.seconds_past_epoch, first.nanoseconds),
            "timestamp did not advance: {first:?} -> {second:?}"
        );
    }

    #[tokio::test]
    async fn put_of_identical_value_restamps_without_posting() {
        let mut records = HashMap::new();
        records.insert("TEST:AO".into(), make_ao("TEST:AO", 1.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);
        let mut rx = Source::subscribe(&store, "TEST:AO").await.unwrap();

        let before = match store.get_nt("TEST:AO").await.unwrap() {
            NtPayload::Scalar(nt) => nt.time_stamp.unwrap(),
            other => panic!("expected scalar, got {other:?}"),
        };

        let posted = store
            .put("TEST:AO", &DecodedValue::Float64(1.0))
            .await
            .unwrap();

        assert!(posted.is_empty(), "no-op PUT must not report a change");
        assert!(rx.try_recv().is_err(), "no-op PUT must not post to monitors");

        let after = match store.get_nt("TEST:AO").await.unwrap() {
            NtPayload::Scalar(nt) => nt.time_stamp.unwrap(),
            other => panic!("expected scalar, got {other:?}"),
        };
        assert!(
            (after.seconds_past_epoch, after.nanoseconds)
                > (before.seconds_past_epoch, before.nanoseconds),
            "record was not restamped: {before:?} -> {after:?}"
        );
    }

    #[tokio::test]
    async fn client_supplied_timestamp_posts_even_when_value_is_unchanged() {
        let mut records = HashMap::new();
        records.insert("TEST:AO".into(), make_ao("TEST:AO", 1.0));
        let store = SimplePvStore::new(records, HashMap::new(), vec![], false);
        let mut rx = Source::subscribe(&store, "TEST:AO").await.unwrap();

        let body = DecodedValue::Structure(vec![
            ("value".to_string(), DecodedValue::Float64(1.0)),
            (
                "timeStamp".to_string(),
                DecodedValue::Structure(vec![
                    (
                        "secondsPastEpoch".to_string(),
                        DecodedValue::Int64(9_000),
                    ),
                    ("nanoseconds".to_string(), DecodedValue::Int32(0)),
                    ("userTag".to_string(), DecodedValue::Int32(0)),
                ]),
            ),
        ]);
        let posted = store.put("TEST:AO", &body).await.unwrap();

        assert_eq!(posted.len(), 1, "client-stamped PUT must post");
        assert!(rx.try_recv().is_ok(), "subscriber must receive the update");
        match store.get_nt("TEST:AO").await.unwrap() {
            NtPayload::Scalar(nt) => {
                assert_eq!(nt.time_stamp.unwrap().seconds_past_epoch, 9_000)
            }
            other => panic!("expected scalar, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn on_put_fires_for_a_value_unchanged_put() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();

        let mut records = HashMap::new();
        records.insert("TEST:AO".into(), make_ao("TEST:AO", 1.0));
        let mut on_put: HashMap<String, OnPutCallback> = HashMap::new();
        on_put.insert(
            "TEST:AO".into(),
            Arc::new(move |_name, _val| {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let store = SimplePvStore::new(records, on_put, vec![], false);

        store
            .put("TEST:AO", &DecodedValue::Float64(1.0))
            .await
            .unwrap();

        // on_put is spawned; give the task a chance to run.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn validator_allows_structure_wrapped_put_through() {
        // Real puts to scalar records arrive wrapped as a Structure with a
        // "value" field (see apply_put_to_record's bare-scalar-wrapping).
        // The validator itself only sees the raw DecodedValue as given to
        // `put`; this test documents that a validator returning Ok lets a
        // structure-wrapped put proceed and apply normally.
        let mut records = std::collections::HashMap::new();
        records.insert("W".to_string(), make_ao("W", 1.0));
        let store =
            SimplePvStore::new(records, std::collections::HashMap::new(), Vec::new(), false);
        store
            .set_validator("W".to_string(), std::sync::Arc::new(|_name, _val| Ok(())))
            .await;

        let dv = DecodedValue::Structure(vec![("value".to_string(), DecodedValue::Float64(5.0))]);
        let res = Source::put(&store, "W", &dv).await;
        assert!(res.is_ok());
        assert_eq!(store.get_value("W").await, Some(ScalarValue::F64(5.0)));
    }
}
