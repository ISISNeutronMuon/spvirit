//! Group PV configuration: parse JSON definitions that compose multiple
//! individual PVs into a single structured PVA channel.
//!
//! Corresponds to the C++ QSRV `group` info-tag / JSON config format.
//! A group PV is a composite PVA channel whose top-level fields each map
//! to a different backing PV ("member").
//!
//! # JSON format
//!
//! ```json
//! {
//!     "GRP:name": {
//!         "+id": "epics:nt/NTTable:1.0",
//!         "+atomic": true,
//!         "fieldA": {
//!             "+channel": "RECORD:A",
//!             "+type": "scalar",
//!             "+trigger": "*",
//!             "+putorder": 0
//!         },
//!         "fieldB": { "+channel": "RECORD:B" }
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How a member PV's value is mapped into the group structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldMapping {
    /// NTScalar/NTScalarArray with full metadata (alarm, timestamp, display, control).
    Scalar,
    /// Value only, no metadata.
    Plain,
    /// Alarm + timestamp only, no value transfer.
    Meta,
    /// Variant-union wrapping (pass-through).
    Any,
    /// Process-only: put triggers record processing, no value transfer.
    Proc,
}

/// When a member's value changes, which group fields should be re-published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerDef {
    /// Trigger all fields (`"*"`).
    All,
    /// Trigger only the named fields.
    Fields(Vec<String>),
    /// Never trigger (dead member).
    None,
}

/// Definition of one field inside a group PV.
#[derive(Debug, Clone)]
pub struct GroupMember {
    /// Field name in the group structure.
    pub field_name: String,
    /// Backing channel / PV name.
    pub channel: String,
    /// How the PV value is mapped.
    pub mapping: FieldMapping,
    /// Which fields a change to this member triggers.
    pub triggers: TriggerDef,
    /// Ordering for put operations (lower = earlier).
    pub put_order: i32,
    /// Optional struct-id override for this field.
    pub struct_id: Option<String>,
}

/// A group PV definition: a structured channel composed of several member PVs.
#[derive(Debug, Clone)]
pub struct GroupPvDef {
    /// Channel name for the group.
    pub name: String,
    /// Optional struct-id (e.g. `"epics:nt/NTTable:1.0"`).
    pub struct_id: Option<String>,
    /// Whether GET/PUT/MONITOR operate atomically across all members.
    pub atomic: bool,
    /// Ordered list of member field definitions.
    pub members: Vec<GroupMember>,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct GroupConfigError(String);

impl fmt::Display for GroupConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "group config: {}", self.0)
    }
}

impl std::error::Error for GroupConfigError {}

type Result<T> = std::result::Result<T, GroupConfigError>;

fn err(msg: impl Into<String>) -> GroupConfigError {
    GroupConfigError(msg.into())
}

// ---------------------------------------------------------------------------
// Serde helpers (intermediate JSON representation)
// ---------------------------------------------------------------------------

/// Top-level JSON: `{ "GROUP:name": { ...members... } }`
type RawConfig = HashMap<String, RawGroupDef>;

/// One group definition: meta keys (`+id`, `+atomic`) mixed with member defs.
#[derive(Deserialize)]
struct RawGroupDef {
    #[serde(rename = "+id", default)]
    id: Option<String>,
    #[serde(rename = "+atomic", default)]
    atomic: Option<bool>,
    #[serde(flatten)]
    members: HashMap<String, RawMember>,
}

#[derive(Deserialize)]
struct RawMember {
    #[serde(rename = "+channel", default)]
    channel: Option<String>,
    #[serde(rename = "+type", default)]
    mapping: Option<String>,
    #[serde(rename = "+trigger", default)]
    trigger: Option<String>,
    #[serde(rename = "+putorder", default)]
    putorder: Option<i32>,
    #[serde(rename = "+id", default)]
    id: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a JSON string describing one or more group PVs.
///
/// Returns a `Vec<GroupPvDef>` — one entry per top-level key.
pub fn parse_group_config(json: &str) -> Result<Vec<GroupPvDef>> {
    let raw: RawConfig =
        serde_json::from_str(json).map_err(|e| err(format!("invalid JSON: {e}")))?;

    let mut groups: Vec<GroupPvDef> = Vec::with_capacity(raw.len());
    for (name, raw_group) in raw {
        groups.push(raw_to_group_def(name, raw_group)?);
    }
    // Sort for deterministic order.
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

/// Parse a record's `info(Q:group, ...)` JSON tag.
///
/// Bare channel names (no `:`) are prefixed with `"{record_name}."`.
pub fn parse_info_group(record_name: &str, json: &str) -> Result<Vec<GroupPvDef>> {
    let raw: RawConfig =
        serde_json::from_str(json).map_err(|e| err(format!("invalid JSON: {e}")))?;

    let mut groups: Vec<GroupPvDef> = Vec::with_capacity(raw.len());
    for (name, mut raw_group) in raw {
        // Prefix bare channel names.
        for member in raw_group.members.values_mut() {
            if let Some(ref mut ch) = member.channel {
                if !ch.contains(':') {
                    *ch = format!("{record_name}.{ch}");
                }
            }
        }
        groups.push(raw_to_group_def(name, raw_group)?);
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

/// Merge newly-parsed group defs into an existing map (accumulates members).
///
/// This supports the C++ pattern where multiple records each contribute
/// members to the same group name.
pub fn merge_group_defs(existing: &mut HashMap<String, GroupPvDef>, new_defs: Vec<GroupPvDef>) {
    for def in new_defs {
        existing
            .entry(def.name.clone())
            .and_modify(|e| {
                // Merge struct_id — last one wins (like C++ QSRV).
                if def.struct_id.is_some() {
                    e.struct_id.clone_from(&def.struct_id);
                }
                e.atomic |= def.atomic;
                e.members.extend(def.members.iter().cloned());
            })
            .or_insert(def);
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn raw_to_group_def(name: String, raw: RawGroupDef) -> Result<GroupPvDef> {
    let mut members = Vec::with_capacity(raw.members.len());

    // Collect all field names first (for trigger validation).
    let field_names: Vec<&str> = raw.members.keys().map(|s| s.as_str()).collect();

    for (field_name, raw_member) in &raw.members {
        members.push(parse_member(field_name, raw_member, &field_names)?);
    }

    // Sort members by field name for deterministic ordering.
    members.sort_by(|a, b| a.field_name.cmp(&b.field_name));

    Ok(GroupPvDef {
        name,
        struct_id: raw.id,
        atomic: raw.atomic.unwrap_or(false),
        members,
    })
}

fn parse_member(field_name: &str, raw: &RawMember, all_fields: &[&str]) -> Result<GroupMember> {
    let channel = raw
        .channel
        .clone()
        .ok_or_else(|| err(format!("member '{field_name}' missing +channel")))?;

    let mapping = match raw.mapping.as_deref() {
        None | Some("plain") => FieldMapping::Plain,
        Some("scalar") => FieldMapping::Scalar,
        Some("meta") => FieldMapping::Meta,
        Some("any") => FieldMapping::Any,
        Some("proc") => FieldMapping::Proc,
        Some(other) => {
            return Err(err(format!(
                "member '{field_name}': unknown +type '{other}'"
            )));
        }
    };

    let triggers = match raw.trigger.as_deref() {
        None => TriggerDef::None,
        Some("*") => TriggerDef::All,
        Some("") => TriggerDef::None,
        Some(spec) => {
            let names: Vec<String> = spec.split(',').map(|s| s.trim().to_owned()).collect();
            // Validate that every named trigger refers to a known field.
            for n in &names {
                if !all_fields.contains(&n.as_str()) {
                    return Err(err(format!(
                        "member '{field_name}': trigger references unknown field '{n}'"
                    )));
                }
            }
            TriggerDef::Fields(names)
        }
    };

    Ok(GroupMember {
        field_name: field_name.to_owned(),
        channel,
        mapping,
        triggers,
        put_order: raw.putorder.unwrap_or(0),
        struct_id: raw.id.clone(),
    })
}

// ---------------------------------------------------------------------------
// GroupSource — Source implementation that serves group PVs
// ---------------------------------------------------------------------------

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_types::{NtPayload, PvValue, ScalarArrayValue, ScalarValue};

use crate::pvstore::{PvInfo, Source, SourceRegistry, TryClaim};
use crate::simple_store::descriptor_for_payload;

/// A [`Source`] that serves group PVs composed from multiple member PVs.
///
/// Group PV names are resolved by fetching each member PV from the
/// underlying [`SourceRegistry`] and composing them into an
/// [`NtPayload::Generic`]. Non-group names are not claimed.
pub struct GroupSource {
    sources: Arc<SourceRegistry>,
    groups: HashMap<String, GroupPvDef>,
}

impl GroupSource {
    /// Create a new group source backed by `sources` with the given group definitions.
    pub fn new(sources: Arc<SourceRegistry>, groups: HashMap<String, GroupPvDef>) -> Self {
        Self { sources, groups }
    }

    /// Build a composite snapshot for a group PV.
    async fn group_snapshot(&self, def: &GroupPvDef) -> NtPayload {
        let mut fields = Vec::with_capacity(def.members.len());
        for member in &def.members {
            if member.mapping == FieldMapping::Proc {
                continue; // Proc members don't contribute a value field.
            }
            let pv_val = match self.sources.get(&member.channel).await {
                Some(snap) => payload_to_pv_value(&snap, member.mapping),
                None => PvValue::Scalar(ScalarValue::I32(0)), // disconnected fallback
            };
            fields.push((member.field_name.clone(), pv_val));
        }
        NtPayload::Generic {
            struct_id: def
                .struct_id
                .clone()
                .unwrap_or_else(|| "structure".to_string()),
            fields,
        }
    }

    /// Build a structure descriptor for a group PV.
    async fn group_descriptor(&self, def: &GroupPvDef) -> StructureDesc {
        let mut field_descs = Vec::with_capacity(def.members.len());
        for member in &def.members {
            if member.mapping == FieldMapping::Proc {
                continue;
            }
            let field_type = match self.sources.get(&member.channel).await {
                Some(snap) => payload_field_type(&snap, member.mapping),
                None => FieldType::Scalar(TypeCode::Int32), // disconnected fallback
            };
            field_descs.push(FieldDesc {
                name: member.field_name.clone(),
                field_type,
            });
        }
        StructureDesc {
            struct_id: def.struct_id.clone(),
            fields: field_descs,
        }
    }
}

impl Source for GroupSource {
    /// Decisive both ways, from the same `groups` map `claim` consults.
    ///
    /// `claim`'s only fallible step is `self.groups.get(&name)?` — everything
    /// after it (the descriptor build, the writability probe) always yields a
    /// `PvInfo`. `groups` is a plain `HashMap`, fixed at construction, so a
    /// `contains_key` is a lock-free, allocation-free, exact mirror of that
    /// predicate: `Yes` really will be served, and `No` really can never be.
    fn try_claim(&self, name: &str) -> TryClaim {
        if self.groups.contains_key(name) {
            TryClaim::Yes
        } else {
            TryClaim::No
        }
    }

    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let def = self.groups.get(&name)?;
            let descriptor = self.group_descriptor(def).await;
            // A group PV is writable if any non-proc non-meta member is writable.
            let mut writable = false;
            for member in &def.members {
                if member.mapping == FieldMapping::Proc || member.mapping == FieldMapping::Meta {
                    continue;
                }
                if self.sources.is_writable(&member.channel).await {
                    writable = true;
                    break;
                }
            }
            Some(PvInfo {
                descriptor,
                writable,
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let def = self.groups.get(&name)?;
            Some(self.group_snapshot(def).await)
        })
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<Vec<(String, NtPayload)>, String>> + Send + '_>,
    > {
        let name = name.to_string();
        let value = value.clone();
        Box::pin(async move {
            if let Some(def) = self.groups.get(&name) {
                // Dispatch sub-field puts to member channels.
                let fields = match &value {
                    DecodedValue::Structure(f) => f,
                    _ => return Err("group PUT requires a structure".to_string()),
                };

                // Sort members by put_order.
                let mut ordered: Vec<&GroupMember> = def.members.iter().collect();
                ordered.sort_by_key(|m| m.put_order);

                let mut results = Vec::new();
                for member in ordered {
                    if member.mapping == FieldMapping::Proc || member.mapping == FieldMapping::Meta
                    {
                        continue;
                    }
                    // Find the sub-field matching this member.
                    if let Some((_, sub_val)) = fields.iter().find(|(n, _)| n == &member.field_name)
                    {
                        match self.sources.put(&member.channel, sub_val).await {
                            Ok(mut r) => results.append(&mut r),
                            Err(e) => {
                                tracing::warn!(
                                    "group PUT {}: member {} failed: {e}",
                                    name,
                                    member.field_name
                                );
                            }
                        }
                    }
                }
                Ok(results)
            } else {
                Err(format!("group PV '{}' not found", name))
            }
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let def = self.groups.get(&name)?;
            self.subscribe_group(def).await
        })
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async move { self.groups.keys().cloned().collect() })
    }
}

impl GroupSource {
    /// Subscribe to a group PV by fanning-in member subscriptions.
    ///
    /// When any member PV updates, the trigger rules are evaluated and if
    /// triggered, a full group snapshot is composed and sent to the subscriber.
    async fn subscribe_group(&self, def: &GroupPvDef) -> Option<mpsc::Receiver<NtPayload>> {
        let (tx, rx) = mpsc::channel(64);
        let sources = self.sources.clone();
        let def = def.clone();

        // Collect member subscriptions.
        let mut member_rxs: Vec<(String, mpsc::Receiver<NtPayload>)> = Vec::new();
        for member in &def.members {
            if let Some(member_rx) = sources.subscribe(&member.channel).await {
                member_rxs.push((member.field_name.clone(), member_rx));
            }
        }
        if member_rxs.is_empty() {
            return None;
        }

        // Build trigger map: field_name → set of field names that get triggered.
        let trigger_map = build_trigger_map(&def);

        tokio::spawn(async move {
            // Use a select!-like loop: poll all member receivers.
            // We use tokio::select! with a macro-generated match is complex for
            // dynamic member sets, so use a simpler polling approach.
            loop {
                // Wait for any member to produce a value.
                let src_field = match poll_any_member(&mut member_rxs).await {
                    Some(field_name) => field_name,
                    None => break, // All members closed.
                };

                {
                    // Check trigger rules.
                    let should_send = match trigger_map.get(&src_field) {
                        Some(targets) => !targets.is_empty(),
                        None => false,
                    };

                    if should_send {
                        // Compose a full group snapshot.
                        let mut fields = Vec::with_capacity(def.members.len());
                        for member in &def.members {
                            if member.mapping == FieldMapping::Proc {
                                continue;
                            }
                            let pv_val = match sources.get(&member.channel).await {
                                Some(snap) => payload_to_pv_value(&snap, member.mapping),
                                None => PvValue::Scalar(ScalarValue::I32(0)),
                            };
                            fields.push((member.field_name.clone(), pv_val));
                        }
                        let payload = NtPayload::Generic {
                            struct_id: def
                                .struct_id
                                .clone()
                                .unwrap_or_else(|| "structure".to_string()),
                            fields,
                        };
                        if tx.send(payload).await.is_err() {
                            break; // Subscriber dropped.
                        }
                    }
                }
            }
        });

        Some(rx)
    }
}

/// Build a map from source field_name → set of triggered field names.
fn build_trigger_map(def: &GroupPvDef) -> HashMap<String, Vec<String>> {
    let all_fields: Vec<String> = def.members.iter().map(|m| m.field_name.clone()).collect();
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    for member in &def.members {
        let targets = match &member.triggers {
            TriggerDef::All => all_fields.clone(),
            TriggerDef::Fields(names) => names.clone(),
            TriggerDef::None => Vec::new(),
        };
        map.insert(member.field_name.clone(), targets);
    }
    map
}

/// Wait for any member receiver to produce a value, returning the field name
/// of the member that updated. Returns `None` when all channels are closed.
async fn poll_any_member(members: &mut Vec<(String, mpsc::Receiver<NtPayload>)>) -> Option<String> {
    if members.is_empty() {
        return None;
    }

    // Create pinned futures for each member.
    let futs: Vec<_> = members
        .iter_mut()
        .map(|(name, rx)| {
            let name = name.clone();
            Box::pin(async move { (name, rx.recv().await) })
                as std::pin::Pin<Box<dyn Future<Output = (String, Option<NtPayload>)> + Send + '_>>
        })
        .collect();

    let (field_name, payload) = race_all(futs).await;

    if payload.is_none() {
        // This member channel is closed; remove it.
        members.retain(|(n, _)| n != &field_name);
        if members.is_empty() {
            return None;
        }
        return Box::pin(poll_any_member(members)).await;
    }

    Some(field_name)
}

/// Race a vec of pinned futures, returning the result of the first to complete.
async fn race_all<T>(futs: Vec<std::pin::Pin<Box<dyn Future<Output = T> + Send + '_>>>) -> T {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    assert!(!futs.is_empty());

    struct RaceAll<'a, T> {
        futs: Vec<Pin<Box<dyn Future<Output = T> + Send + 'a>>>,
    }

    impl<T> Future for RaceAll<'_, T> {
        type Output = T;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            for fut in &mut self.futs {
                if let Poll::Ready(val) = fut.as_mut().poll(cx) {
                    return Poll::Ready(val);
                }
            }
            Poll::Pending
        }
    }

    RaceAll { futs }.await
}

// ---------------------------------------------------------------------------
// NtPayload → PvValue conversion helpers
// ---------------------------------------------------------------------------

/// Convert an NtPayload (member snapshot) to a PvValue for embedding in a
/// group structure, respecting the FieldMapping.
fn payload_to_pv_value(payload: &NtPayload, mapping: FieldMapping) -> PvValue {
    match mapping {
        FieldMapping::Scalar => payload_to_full_structure(payload),
        FieldMapping::Plain => payload_to_value_only(payload),
        FieldMapping::Meta => payload_to_meta_only(payload),
        FieldMapping::Any => payload_to_full_structure(payload),
        FieldMapping::Proc => PvValue::Scalar(ScalarValue::I32(0)), // shouldn't be called
    }
}

/// Full NT structure as PvValue (Scalar mapping).
fn payload_to_full_structure(payload: &NtPayload) -> PvValue {
    match payload {
        NtPayload::Scalar(nt) => {
            let mut fields = vec![
                ("value".to_string(), PvValue::Scalar(nt.value.clone())),
                (
                    "alarm".to_string(),
                    alarm_to_pv_value(nt.alarm_severity, nt.alarm_status, &nt.alarm_message),
                ),
                ("timeStamp".to_string(), timestamp_to_pv_value_default()),
            ];
            fields.push((
                "display".to_string(),
                PvValue::Structure {
                    struct_id: "display_t".to_string(),
                    fields: vec![
                        (
                            "limitLow".to_string(),
                            PvValue::Scalar(ScalarValue::F64(nt.display_low)),
                        ),
                        (
                            "limitHigh".to_string(),
                            PvValue::Scalar(ScalarValue::F64(nt.display_high)),
                        ),
                        (
                            "description".to_string(),
                            PvValue::Scalar(ScalarValue::Str(nt.display_description.clone())),
                        ),
                        (
                            "units".to_string(),
                            PvValue::Scalar(ScalarValue::Str(nt.units.clone())),
                        ),
                        (
                            "precision".to_string(),
                            PvValue::Scalar(ScalarValue::I32(nt.display_precision)),
                        ),
                    ],
                },
            ));
            fields.push((
                "control".to_string(),
                PvValue::Structure {
                    struct_id: "control_t".to_string(),
                    fields: vec![
                        (
                            "limitLow".to_string(),
                            PvValue::Scalar(ScalarValue::F64(nt.control_low)),
                        ),
                        (
                            "limitHigh".to_string(),
                            PvValue::Scalar(ScalarValue::F64(nt.control_high)),
                        ),
                        (
                            "minStep".to_string(),
                            PvValue::Scalar(ScalarValue::F64(nt.control_min_step)),
                        ),
                    ],
                },
            ));
            PvValue::Structure {
                struct_id: "epics:nt/NTScalar:1.0".to_string(),
                fields,
            }
        }
        NtPayload::ScalarArray(nt) => {
            let fields = vec![
                ("value".to_string(), PvValue::ScalarArray(nt.value.clone())),
                (
                    "alarm".to_string(),
                    alarm_to_pv_value(nt.alarm.severity, nt.alarm.status, &nt.alarm.message),
                ),
                ("timeStamp".to_string(), timestamp_pv(&nt.time_stamp)),
            ];
            PvValue::Structure {
                struct_id: "epics:nt/NTScalarArray:1.0".to_string(),
                fields,
            }
        }
        NtPayload::Enum(nt) => {
            let fields = vec![
                (
                    "value".to_string(),
                    PvValue::Structure {
                        struct_id: "enum_t".to_string(),
                        fields: vec![
                            (
                                "index".to_string(),
                                PvValue::Scalar(ScalarValue::I32(nt.index)),
                            ),
                            (
                                "choices".to_string(),
                                PvValue::ScalarArray(ScalarArrayValue::Str(nt.choices.clone())),
                            ),
                        ],
                    },
                ),
                ("alarm".to_string(), alarm_pv(&nt.alarm)),
                ("timeStamp".to_string(), timestamp_pv(&nt.time_stamp)),
            ];
            PvValue::Structure {
                struct_id: "epics:nt/NTEnum:1.0".to_string(),
                fields,
            }
        }
        NtPayload::Generic { struct_id, fields } => PvValue::Structure {
            struct_id: struct_id.clone(),
            fields: fields.clone(),
        },
        _ => PvValue::Scalar(ScalarValue::I32(0)),
    }
}

/// Value-only (Plain mapping) — extract just the primary value.
fn payload_to_value_only(payload: &NtPayload) -> PvValue {
    match payload {
        NtPayload::Scalar(nt) => PvValue::Scalar(nt.value.clone()),
        NtPayload::ScalarArray(nt) => PvValue::ScalarArray(nt.value.clone()),
        NtPayload::Enum(nt) => PvValue::Scalar(ScalarValue::I32(nt.index)),
        NtPayload::Generic { fields, .. } => {
            // Try to find a "value" field.
            fields
                .iter()
                .find(|(n, _)| n == "value")
                .map(|(_, v)| v.clone())
                .unwrap_or(PvValue::Scalar(ScalarValue::I32(0)))
        }
        _ => PvValue::Scalar(ScalarValue::I32(0)),
    }
}

/// Meta-only (Meta mapping) — alarm + timestamp only.
fn payload_to_meta_only(payload: &NtPayload) -> PvValue {
    match payload {
        NtPayload::Scalar(nt) => PvValue::Structure {
            struct_id: String::new(),
            fields: vec![
                (
                    "alarm".to_string(),
                    alarm_to_pv_value(nt.alarm_severity, nt.alarm_status, &nt.alarm_message),
                ),
                ("timeStamp".to_string(), timestamp_to_pv_value_default()),
            ],
        },
        NtPayload::ScalarArray(nt) => PvValue::Structure {
            struct_id: String::new(),
            fields: vec![
                ("alarm".to_string(), alarm_pv(&nt.alarm)),
                ("timeStamp".to_string(), timestamp_pv(&nt.time_stamp)),
            ],
        },
        NtPayload::Enum(nt) => PvValue::Structure {
            struct_id: String::new(),
            fields: vec![
                ("alarm".to_string(), alarm_pv(&nt.alarm)),
                ("timeStamp".to_string(), timestamp_pv(&nt.time_stamp)),
            ],
        },
        _ => PvValue::Structure {
            struct_id: String::new(),
            fields: vec![],
        },
    }
}

/// Build a FieldType for a member's contribution to the group descriptor.
fn payload_field_type(payload: &NtPayload, mapping: FieldMapping) -> FieldType {
    match mapping {
        FieldMapping::Scalar | FieldMapping::Any => {
            FieldType::Structure(descriptor_for_payload(payload))
        }
        FieldMapping::Plain => match payload {
            NtPayload::Scalar(nt) => value_field_type(&nt.value),
            NtPayload::ScalarArray(nt) => array_field_type(&nt.value),
            NtPayload::Enum(_) => FieldType::Scalar(TypeCode::Int32),
            _ => FieldType::Scalar(TypeCode::Int32),
        },
        FieldMapping::Meta => FieldType::Structure(StructureDesc {
            struct_id: None,
            fields: vec![
                FieldDesc {
                    name: "alarm".to_string(),
                    field_type: FieldType::Structure(alarm_struct_desc()),
                },
                FieldDesc {
                    name: "timeStamp".to_string(),
                    field_type: FieldType::Structure(timestamp_struct_desc()),
                },
            ],
        }),
        FieldMapping::Proc => FieldType::Scalar(TypeCode::Int32),
    }
}

fn value_field_type(sv: &ScalarValue) -> FieldType {
    match sv {
        ScalarValue::Str(_) => FieldType::String,
        sv => {
            let tc = match sv {
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
                ScalarValue::Str(_) => unreachable!(),
            };
            FieldType::Scalar(tc)
        }
    }
}

fn array_field_type(sav: &ScalarArrayValue) -> FieldType {
    match sav {
        ScalarArrayValue::Str(_) => FieldType::StringArray,
        sav => {
            let tc = match sav {
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
                ScalarArrayValue::Str(_) => unreachable!(),
            };
            FieldType::ScalarArray(tc)
        }
    }
}

fn alarm_struct_desc() -> StructureDesc {
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

fn timestamp_struct_desc() -> StructureDesc {
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

fn alarm_to_pv_value(severity: i32, status: i32, message: &str) -> PvValue {
    PvValue::Structure {
        struct_id: "alarm_t".to_string(),
        fields: vec![
            (
                "severity".to_string(),
                PvValue::Scalar(ScalarValue::I32(severity)),
            ),
            (
                "status".to_string(),
                PvValue::Scalar(ScalarValue::I32(status)),
            ),
            (
                "message".to_string(),
                PvValue::Scalar(ScalarValue::Str(message.to_string())),
            ),
        ],
    }
}

fn alarm_pv(alarm: &spvirit_types::NtAlarm) -> PvValue {
    alarm_to_pv_value(alarm.severity, alarm.status, &alarm.message)
}

fn timestamp_pv(ts: &spvirit_types::NtTimeStamp) -> PvValue {
    PvValue::Structure {
        struct_id: "time_t".to_string(),
        fields: vec![
            (
                "secondsPastEpoch".to_string(),
                PvValue::Scalar(ScalarValue::I64(ts.seconds_past_epoch)),
            ),
            (
                "nanoseconds".to_string(),
                PvValue::Scalar(ScalarValue::I32(ts.nanoseconds)),
            ),
            (
                "userTag".to_string(),
                PvValue::Scalar(ScalarValue::I32(ts.user_tag)),
            ),
        ],
    }
}

fn timestamp_to_pv_value_default() -> PvValue {
    timestamp_pv(&spvirit_types::NtTimeStamp::default())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_group() {
        let json = r#"{
            "GRP:test": {
                "+id": "epics:nt/NTTable:1.0",
                "+atomic": true,
                "fieldA": {
                    "+channel": "REC:A",
                    "+type": "scalar",
                    "+trigger": "*"
                },
                "fieldB": {
                    "+channel": "REC:B",
                    "+type": "plain"
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.name, "GRP:test");
        assert_eq!(g.struct_id.as_deref(), Some("epics:nt/NTTable:1.0"));
        assert!(g.atomic);
        assert_eq!(g.members.len(), 2);

        let a = g.members.iter().find(|m| m.field_name == "fieldA").unwrap();
        assert_eq!(a.channel, "REC:A");
        assert_eq!(a.mapping, FieldMapping::Scalar);
        assert_eq!(a.triggers, TriggerDef::All);

        let b = g.members.iter().find(|m| m.field_name == "fieldB").unwrap();
        assert_eq!(b.channel, "REC:B");
        assert_eq!(b.mapping, FieldMapping::Plain);
    }

    #[test]
    fn parse_minimal_member() {
        let json = r#"{
            "GRP:min": {
                "x": { "+channel": "R:x" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert_eq!(m.mapping, FieldMapping::Plain); // default
        assert_eq!(m.triggers, TriggerDef::None); // default
        assert_eq!(m.put_order, 0);
    }

    #[test]
    fn parse_proc_mapping() {
        let json = r#"{
            "GRP:proc": {
                "go": {
                    "+channel": "REC:PROC",
                    "+type": "proc",
                    "+trigger": "go",
                    "+putorder": 99
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert_eq!(m.mapping, FieldMapping::Proc);
        assert_eq!(m.put_order, 99);
        assert_eq!(m.triggers, TriggerDef::Fields(vec!["go".into()]));
    }

    #[test]
    fn parse_error_missing_channel() {
        let json = r#"{
            "GRP:bad": {
                "x": { "+type": "scalar" }
            }
        }"#;

        assert!(parse_group_config(json).is_err());
    }

    #[test]
    fn parse_multiple_groups() {
        let json = r#"{
            "G:a": { "x": { "+channel": "R:x" } },
            "G:b": { "y": { "+channel": "R:y" } }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn parse_member_id() {
        let json = r#"{
            "GRP:id": {
                "val": {
                    "+channel": "R:val",
                    "+id": "custom_t"
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups[0].members[0].struct_id.as_deref(), Some("custom_t"));
    }

    #[test]
    fn parse_member_no_id() {
        let json = r#"{
            "GRP:noid": {
                "v": { "+channel": "R:v" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert!(groups[0].members[0].struct_id.is_none());
    }

    #[test]
    fn parse_info_group_prefix() {
        let json = r#"{
            "TEMP:group": {
                "VAL": {
                    "+channel": "VAL",
                    "+type": "plain",
                    "+trigger": "*"
                }
            }
        }"#;

        let groups = parse_info_group("TEMP:sensor", json).unwrap();
        // Bare field "VAL" should become "TEMP:sensor.VAL"
        assert_eq!(groups[0].members[0].channel, "TEMP:sensor.VAL");
    }

    #[test]
    fn parse_info_group_absolute_channel() {
        let json = r#"{
            "TEMP:group": {
                "pressure": {
                    "+channel": "PRESS:ai",
                    "+type": "scalar"
                }
            }
        }"#;

        let groups = parse_info_group("TEMP:sensor", json).unwrap();
        // Absolute channel (contains ':') should be kept as-is
        assert_eq!(groups[0].members[0].channel, "PRESS:ai");
    }

    #[test]
    fn merge_groups() {
        let mut existing = HashMap::new();
        let defs1 = parse_group_config(r#"{ "GRP:a": { "x": { "+channel": "R1:x" } } }"#).unwrap();
        merge_group_defs(&mut existing, defs1);

        let defs2 = parse_group_config(r#"{ "GRP:a": { "y": { "+channel": "R2:y" } } }"#).unwrap();
        merge_group_defs(&mut existing, defs2);

        let grp = existing.get("GRP:a").unwrap();
        assert_eq!(grp.members.len(), 2);
    }

    #[test]
    fn trigger_validation_unknown_field() {
        let json = r#"{
            "GRP:bad": {
                "x": {
                    "+channel": "R:x",
                    "+trigger": "y,z"
                },
                "y": { "+channel": "R:y" }
            }
        }"#;

        // y exists but z doesn't — should fail.
        let result = parse_group_config(json);
        assert!(result.is_err());
        let e = format!("{}", result.unwrap_err());
        assert!(e.contains("'z'"), "expected error about 'z': {e}");
    }

    #[test]
    fn trigger_validation_self_reference() {
        let json = r#"{
            "GRP:ok": {
                "a": { "+channel": "R:a", "+trigger": "a,b" },
                "b": { "+channel": "R:b", "+trigger": "a" }
            }
        }"#;

        // Self-reference and cross-reference are both valid.
        assert!(parse_group_config(json).is_ok());
    }

    #[test]
    fn trigger_validation_star_passes() {
        let json = r#"{
            "GRP:ok": {
                "a": { "+channel": "R:a", "+trigger": "*" }
            }
        }"#;

        // "*" doesn't go through field validation.
        assert!(parse_group_config(json).is_ok());
    }

    #[tokio::test]
    async fn try_claim_agrees_with_claim_for_the_group_source() {
        let json = r#"{
            "GRP:present": {
                "a": { "+channel": "R:a", "+type": "plain" }
            }
        }"#;
        let groups: HashMap<String, GroupPvDef> = parse_group_config(json)
            .unwrap()
            .into_iter()
            .map(|g| (g.name.clone(), g))
            .collect();
        let source = GroupSource::new(Arc::new(SourceRegistry::new()), groups);

        assert_eq!(source.try_claim("GRP:present"), TryClaim::Yes);
        assert!(Source::claim(&source, "GRP:present").await.is_some());

        assert_eq!(source.try_claim("GRP:absent"), TryClaim::No);
        assert!(Source::claim(&source, "GRP:absent").await.is_none());
    }
}
