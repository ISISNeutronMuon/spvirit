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

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_types::{NtPayload, ScalarArrayValue, ScalarValue};

use crate::apply::{
    apply_alarm_update, apply_control_update, apply_display_update, apply_scalar_array_put,
    apply_value_update,
};
use crate::monitor::MonitorRegistry;
use crate::pvstore::{PvInfo, Source};
use crate::types::{RecordData, RecordInstance};

/// Callback invoked after a PUT value is applied to a record.
pub type OnPutCallback = Arc<dyn Fn(&str, &DecodedValue) + Send + Sync>;

/// Callback invoked by the scan scheduler; returns the new value for the PV.
pub type ScanCallback = Arc<dyn Fn(&str) -> ScalarValue + Send + Sync>;

/// Callback that computes a derived PV value from its input values.
pub type LinkCallback = Arc<dyn Fn(&[ScalarValue]) -> ScalarValue + Send + Sync>;

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
            .map(|(name, record)| {
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
        }
    }

    /// Attach the [`MonitorRegistry`] so that `set_value` can push updates
    /// to PVAccess monitor clients.  Called automatically by [`PvaServer::run`].
    pub async fn set_registry(&self, registry: Arc<MonitorRegistry>) {
        *self.registry.write().await = Some(registry);
    }

    /// Insert or replace a PV record at runtime.
    pub async fn insert(&self, name: String, record: RecordInstance) {
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
                        .retain(|tx| tx.try_send(payload.clone()).is_ok());
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
                        .retain(|tx| tx.try_send(payload.clone()).is_ok());
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
                        .retain(|tx| tx.try_send(payload.clone()).is_ok());
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
            let result = {
                let mut pvs = self.pvs.write().await;
                let entry = pvs
                    .get_mut(&name)
                    .ok_or_else(|| format!("PV '{}' not found", name))?;

                if !entry.record.writable() {
                    return Err(format!("PV '{}' is not writable", name));
                }

                let prev_severity = entry.record.to_ntscalar().alarm_severity;
                let changed = apply_put_to_record(&mut entry.record, &value, self.compute_alarms);
                if !changed {
                    return Ok(vec![]);
                }

                if should_post_update(entry, prev_severity) {
                    let payload = entry.record.to_ntpayload();
                    entry
                        .subscribers
                        .retain(|tx| tx.try_send(payload.clone()).is_ok());
                    Some((name.clone(), payload))
                } else {
                    // Within the MDEL monitor deadband — value applied, no post.
                    None
                }
            }; // pvs lock dropped

            // Fire on_put callback (non-blocking).
            if let Some(cb) = self.on_put.get(&name) {
                let cb = cb.clone();
                let n = name.clone();
                let v = value.clone();
                tokio::spawn(async move { cb(&n, &v) });
            }

            // Propagate linked PV updates.
            self.evaluate_links(&name).await;

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

/// Apply a decoded PUT value to a RecordInstance, returning whether it changed.
fn apply_put_to_record(
    record: &mut RecordInstance,
    value: &DecodedValue,
    compute_alarms: bool,
) -> bool {
    let fields = match value {
        DecodedValue::Structure(f) => f,
        other => {
            // Bare scalar — wrap as value field.
            return apply_put_to_record(
                record,
                &DecodedValue::Structure(vec![("value".to_string(), other.clone())]),
                compute_alarms,
            );
        }
    };

    let mut changed = false;

    match &mut record.data {
        RecordData::Ai { nt, .. }
        | RecordData::Ao { nt, .. }
        | RecordData::Bi { nt, .. }
        | RecordData::Bo { nt, .. }
        | RecordData::StringIn { nt, .. }
        | RecordData::StringOut { nt, .. } => {
            for (name, val) in fields {
                match name.as_str() {
                    "value" => {
                        changed |= apply_value_update(nt, val, compute_alarms);
                    }
                    "alarm" => {
                        changed |= apply_alarm_update(nt, val);
                    }
                    "display" => {
                        changed |= apply_display_update(nt, val);
                    }
                    "control" => {
                        changed |= apply_control_update(nt, val);
                    }
                    _ => {}
                }
            }
        }
        RecordData::Waveform { nt, nord, .. }
        | RecordData::Aai { nt, nord, .. }
        | RecordData::Aao { nt, nord, .. }
        | RecordData::SubArray { nt, nord, .. } => {
            changed = apply_scalar_array_put(nt, nord, value);
        }
        RecordData::NtTable { .. } | RecordData::NtNdArray { .. } => {
            // Table/NdArray PUT not supported via high-level API yet.
            debug!("PUT to NtTable/NtNdArray not yet supported in SimplePvStore");
        }
        RecordData::NtEnum { nt, .. } => {
            // Accept index updates for NtEnum PVs.
            for (name, val) in fields {
                if name == "value" {
                    let idx = match val {
                        DecodedValue::Int32(v) => Some(*v),
                        DecodedValue::Int64(v) => Some(*v as i32),
                        DecodedValue::Int16(v) => Some(*v as i32),
                        DecodedValue::Int8(v) => Some(*v as i32),
                        DecodedValue::Float64(v) => Some(*v as i32),
                        _ => None,
                    };
                    if let Some(idx) = idx {
                        if nt.index != idx {
                            nt.index = idx;
                            changed = true;
                        }
                    }
                }
            }
        }
        RecordData::Generic { .. } => {
            debug!("PUT to Generic not yet supported in SimplePvStore");
        }
    }

    changed
}

// ── NtPayload → StructureDesc ────────────────────────────────────────────

pub fn descriptor_for_payload(payload: &NtPayload) -> StructureDesc {
    match payload {
        NtPayload::Scalar(nt) => nt_scalar_desc(&nt.value),
        NtPayload::ScalarArray(arr) => nt_scalar_array_desc(&arr.value),
        _ => StructureDesc::new(),
    }
}

fn value_type_code(sv: &ScalarValue) -> TypeCode {
    match sv {
        ScalarValue::Bool(_) => TypeCode::Boolean,
        ScalarValue::I8(_) => TypeCode::Int8,
        ScalarValue::I16(_) => TypeCode::Int16,
        ScalarValue::I32(_) => TypeCode::Int32,
        ScalarValue::I64(_) => TypeCode::Int64,
        ScalarValue::U8(_) => TypeCode::UInt8,
        ScalarValue::U16(_) => TypeCode::UInt16,
        ScalarValue::U32(_) => TypeCode::UInt32,
        ScalarValue::U64(_) => TypeCode::UInt64,
        ScalarValue::F32(_) => TypeCode::Float32,
        ScalarValue::F64(_) => TypeCode::Float64,
        ScalarValue::Str(_) => TypeCode::String,
    }
}

fn array_type_code(sav: &ScalarArrayValue) -> TypeCode {
    match sav {
        ScalarArrayValue::Bool(_) => TypeCode::Boolean,
        ScalarArrayValue::I8(_) => TypeCode::Int8,
        ScalarArrayValue::I16(_) => TypeCode::Int16,
        ScalarArrayValue::I32(_) => TypeCode::Int32,
        ScalarArrayValue::I64(_) => TypeCode::Int64,
        ScalarArrayValue::U8(_) => TypeCode::UInt8,
        ScalarArrayValue::U16(_) => TypeCode::UInt16,
        ScalarArrayValue::U32(_) => TypeCode::UInt32,
        ScalarArrayValue::U64(_) => TypeCode::UInt64,
        ScalarArrayValue::F32(_) => TypeCode::Float32,
        ScalarArrayValue::F64(_) => TypeCode::Float64,
        ScalarArrayValue::Str(_) => TypeCode::String,
    }
}

fn nt_scalar_desc(sv: &ScalarValue) -> StructureDesc {
    let tc = value_type_code(sv);
    StructureDesc {
        struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "value".to_string(),
                field_type: FieldType::Scalar(tc),
            },
            FieldDesc {
                name: "alarm".to_string(),
                field_type: FieldType::Structure(alarm_desc()),
            },
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
            FieldDesc {
                name: "display".to_string(),
                field_type: FieldType::Structure(display_desc()),
            },
            FieldDesc {
                name: "control".to_string(),
                field_type: FieldType::Structure(control_desc()),
            },
            FieldDesc {
                name: "valueAlarm".to_string(),
                field_type: FieldType::Structure(value_alarm_desc()),
            },
        ],
    }
}

fn nt_scalar_array_desc(sav: &ScalarArrayValue) -> StructureDesc {
    let tc = array_type_code(sav);
    StructureDesc {
        struct_id: Some("epics:nt/NTScalarArray:1.0".to_string()),
        fields: vec![
            FieldDesc {
                name: "value".to_string(),
                field_type: FieldType::ScalarArray(tc),
            },
            FieldDesc {
                name: "alarm".to_string(),
                field_type: FieldType::Structure(alarm_desc()),
            },
            FieldDesc {
                name: "timeStamp".to_string(),
                field_type: FieldType::Structure(timestamp_desc()),
            },
            FieldDesc {
                name: "display".to_string(),
                field_type: FieldType::Structure(display_desc()),
            },
            FieldDesc {
                name: "control".to_string(),
                field_type: FieldType::Structure(control_desc()),
            },
        ],
    }
}

fn alarm_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("alarm_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "severity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "status".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "message".to_string(),
                field_type: FieldType::String,
            },
        ],
    }
}

fn timestamp_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("time_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "secondsPastEpoch".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int64),
            },
            FieldDesc {
                name: "nanoseconds".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "userTag".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
        ],
    }
}

fn display_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("display_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "limitLow".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "limitHigh".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "description".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "units".to_string(),
                field_type: FieldType::String,
            },
            FieldDesc {
                name: "precision".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "form".to_string(),
                field_type: FieldType::Structure(StructureDesc {
                    struct_id: Some("enum_t".to_string()),
                    fields: vec![
                        FieldDesc {
                            name: "index".to_string(),
                            field_type: FieldType::Scalar(TypeCode::Int32),
                        },
                        FieldDesc {
                            name: "choices".to_string(),
                            field_type: FieldType::StringArray,
                        },
                    ],
                }),
            },
        ],
    }
}

fn control_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("control_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "limitLow".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "limitHigh".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "minStep".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
        ],
    }
}

fn value_alarm_desc() -> StructureDesc {
    StructureDesc {
        struct_id: Some("valueAlarm_t".to_string()),
        fields: vec![
            FieldDesc {
                name: "active".to_string(),
                field_type: FieldType::Scalar(TypeCode::Boolean),
            },
            FieldDesc {
                name: "lowAlarmLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "lowWarningLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "highWarningLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "highAlarmLimit".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            },
            FieldDesc {
                name: "lowAlarmSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "lowWarningSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "highWarningSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "highAlarmSeverity".to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            },
            FieldDesc {
                name: "hysteresis".to_string(),
                field_type: FieldType::Scalar(TypeCode::UInt8),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DbCommonState, RecordType};
    use spvirit_types::{
        NdCodec, NdDimension, NtNdArray, NtPayload, NtScalar, NtScalarArray, NtTable,
        NtTableColumn, ScalarArrayValue, ScalarValue,
    };

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
        let mut rx = Source::subscribe(&store, "DB:AO").await.expect("subscribed");

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
        assert_eq!(
            store.get_value("DB:AO").await,
            Some(ScalarValue::F64(0.9))
        );
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

        match store.get_nt("TEST:TBL").await.unwrap() {
            NtPayload::Table(nt) => assert_eq!(nt, table),
            _ => panic!("expected table payload"),
        }
        match store.get_nt("TEST:NDA").await.unwrap() {
            NtPayload::NdArray(nt) => assert_eq!(nt, ndarray),
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
}
