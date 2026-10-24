//! PVD (pvData) Type Introspection and Value Decoding
//!
//! Implements parsing of PVAccess field descriptions and value decoding
//! according to the pvData serialization specification.

use std::fmt;
use tracing::debug;

use crate::error::{
    DecodeError, DecodeResult, Utf8Policy, decode_size_prefixed, decode_string_prefixed,
};

/// Re-export the free-standing `decode_string` from `epics_decode` for
/// discoverability alongside the other decode helpers in this module.
pub use crate::epics_decode::decode_string;

/// Ceilings on decoded array lengths, per array kind.
///
/// These are a backstop against corrupt or hostile element counts. Exceeding
/// one is [`DecodeError::ArrayTooLarge`] — the decoder never returns a
/// silently shortened array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_scalar_array: usize,
    pub max_string_array: usize,
    pub max_struct_array: usize,
    pub max_union_array: usize,
    pub max_variant_array: usize,
    /// Ceiling on the field count in one structure descriptor. Matches the
    /// non-scalar array ceilings above; real NT structures have ~30 fields.
    /// Checked *before* the `Vec::with_capacity`, because a `Vec` allocation
    /// failure aborts the process rather than unwinding.
    pub max_struct_fields: usize,
    /// Ceiling on introspection / value nesting depth. Unbounded recursion
    /// here overflows the native stack -- an uncatchable process kill. The
    /// deepest normative NT type nests fewer than ten levels; 32 leaves a 3x
    /// margin for vendor extensions.
    pub max_depth: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_scalar_array: 4_000_000,
            max_string_array: 65_536,
            max_struct_array: 65_536,
            max_union_array: 65_536,
            max_variant_array: 65_536,
            max_struct_fields: 65_536,
            max_depth: 32,
        }
    }
}

/// PVD type codes from the specification
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeCode {
    Null = 0xFF,
    Boolean = 0x00,
    Int8 = 0x20,
    Int16 = 0x21,
    Int32 = 0x22,
    Int64 = 0x23,
    UInt8 = 0x24,
    UInt16 = 0x25,
    UInt32 = 0x26,
    UInt64 = 0x27,
    Float32 = 0x42,
    Float64 = 0x43,
    String = 0x60,
    // Bounded string has 0x83 prefix followed by size
    Variant = 0xFE, // Union with no fixed type (0xFF is Null)
}

impl TypeCode {
    pub fn from_byte(b: u8) -> Option<Self> {
        // Clear scalar-array mode bits (variable/bounded/fixed array)
        let base = b & 0xE7;
        match base {
            0x00 => Some(TypeCode::Boolean),
            0x20 => Some(TypeCode::Int8),
            0x21 => Some(TypeCode::Int16),
            0x22 => Some(TypeCode::Int32),
            0x23 => Some(TypeCode::Int64),
            0x24 => Some(TypeCode::UInt8),
            0x25 => Some(TypeCode::UInt16),
            0x26 => Some(TypeCode::UInt32),
            0x27 => Some(TypeCode::UInt64),
            0x42 => Some(TypeCode::Float32),
            0x43 => Some(TypeCode::Float64),
            0x60 => Some(TypeCode::String),
            _ => None,
        }
    }

    pub fn size(&self) -> Option<usize> {
        match self {
            TypeCode::Boolean | TypeCode::Int8 | TypeCode::UInt8 => Some(1),
            TypeCode::Int16 | TypeCode::UInt16 => Some(2),
            TypeCode::Int32 | TypeCode::UInt32 | TypeCode::Float32 => Some(4),
            TypeCode::Int64 | TypeCode::UInt64 | TypeCode::Float64 => Some(8),
            TypeCode::String | TypeCode::Null | TypeCode::Variant => None,
        }
    }
}

/// Field type description
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Scalar(TypeCode),
    ScalarArray(TypeCode),
    String,
    StringArray,
    Structure(StructureDesc),
    StructureArray(StructureDesc),
    Union(Vec<FieldDesc>),
    UnionArray(Vec<FieldDesc>),
    Variant,
    VariantArray,
    BoundedString(u32),
}

impl FieldType {
    pub fn type_name(&self) -> &'static str {
        match self {
            FieldType::Scalar(tc) => match tc {
                TypeCode::Boolean => "boolean",
                TypeCode::Int8 => "byte",
                TypeCode::Int16 => "short",
                TypeCode::Int32 => "int",
                TypeCode::Int64 => "long",
                TypeCode::UInt8 => "ubyte",
                TypeCode::UInt16 => "ushort",
                TypeCode::UInt32 => "uint",
                TypeCode::UInt64 => "ulong",
                TypeCode::Float32 => "float",
                TypeCode::Float64 => "double",
                TypeCode::String => "string",
                _ => "unknown",
            },
            FieldType::ScalarArray(tc) => match tc {
                TypeCode::Float64 => "double[]",
                TypeCode::Float32 => "float[]",
                TypeCode::Int64 => "long[]",
                TypeCode::Int32 => "int[]",
                _ => "array",
            },
            FieldType::String => "string",
            FieldType::StringArray => "string[]",
            FieldType::Structure(_) => "structure",
            FieldType::StructureArray(_) => "structure[]",
            FieldType::Union(_) => "union",
            FieldType::UnionArray(_) => "union[]",
            FieldType::Variant => "any",
            FieldType::VariantArray => "any[]",
            FieldType::BoundedString(_) => "string",
        }
    }
}

/// Field description (name + type)
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDesc {
    pub name: String,
    pub field_type: FieldType,
}

/// Structure description with optional ID
#[derive(Debug, Clone, PartialEq)]
pub struct StructureDesc {
    pub struct_id: Option<String>,
    pub fields: Vec<FieldDesc>,
}

impl StructureDesc {
    pub fn new() -> Self {
        Self {
            struct_id: None,
            fields: Vec::new(),
        }
    }

    /// Look up a field by name.
    pub fn field(&self, name: &str) -> Option<&FieldDesc> {
        self.fields.iter().find(|f| f.name == name)
    }
}

impl Default for StructureDesc {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for StructureDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn write_indent(f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
            for _ in 0..depth {
                write!(f, "    ")?;
            }
            Ok(())
        }

        fn write_field_type(
            f: &mut fmt::Formatter<'_>,
            ft: &FieldType,
            depth: usize,
        ) -> fmt::Result {
            match ft {
                FieldType::Structure(desc) => write_structure(f, desc, depth),
                FieldType::StructureArray(desc) => {
                    write_structure(f, desc, depth)?;
                    write!(f, "[]")
                }
                FieldType::Union(fields) => {
                    writeln!(f, "union")?;
                    for field in fields {
                        write_indent(f, depth + 1)?;
                        write!(f, "{} ", field.name)?;
                        write_field_type(f, &field.field_type, depth + 1)?;
                        writeln!(f)?;
                    }
                    Ok(())
                }
                FieldType::UnionArray(fields) => {
                    writeln!(f, "union[]")?;
                    for field in fields {
                        write_indent(f, depth + 1)?;
                        write!(f, "{} ", field.name)?;
                        write_field_type(f, &field.field_type, depth + 1)?;
                        writeln!(f)?;
                    }
                    Ok(())
                }
                other => write!(f, "{}", other.type_name()),
            }
        }

        fn write_structure(
            f: &mut fmt::Formatter<'_>,
            desc: &StructureDesc,
            depth: usize,
        ) -> fmt::Result {
            if let Some(id) = &desc.struct_id {
                write!(f, "structure «{}»", id)?;
            } else {
                write!(f, "structure")?;
            }
            if desc.fields.is_empty() {
                return Ok(());
            }
            writeln!(f)?;
            for field in &desc.fields {
                write_indent(f, depth + 1)?;
                write!(f, "{} ", field.name)?;
                write_field_type(f, &field.field_type, depth + 1)?;
                writeln!(f)?;
            }
            Ok(())
        }

        write_structure(f, self, 0)
    }
}

/// Decoded value
#[derive(Debug, Clone)]
pub enum DecodedValue {
    Null,
    Boolean(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    UInt8(u8),
    UInt16(u16),
    UInt32(u32),
    UInt64(u64),
    Float32(f32),
    Float64(f64),
    String(String),
    Array(Vec<DecodedValue>),
    Structure(Vec<(String, DecodedValue)>),
    Raw(Vec<u8>),
}

impl fmt::Display for DecodedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodedValue::Null => write!(f, "null"),
            DecodedValue::Boolean(v) => write!(f, "{}", v),
            DecodedValue::Int8(v) => write!(f, "{}", v),
            DecodedValue::Int16(v) => write!(f, "{}", v),
            DecodedValue::Int32(v) => write!(f, "{}", v),
            DecodedValue::Int64(v) => write!(f, "{}", v),
            DecodedValue::UInt8(v) => write!(f, "{}", v),
            DecodedValue::UInt16(v) => write!(f, "{}", v),
            DecodedValue::UInt32(v) => write!(f, "{}", v),
            DecodedValue::UInt64(v) => write!(f, "{}", v),
            DecodedValue::Float32(v) => write!(f, "{:.6}", v),
            DecodedValue::Float64(v) => write!(f, "{:.6}", v),
            DecodedValue::String(v) => write!(f, "\"{}\"", v),
            DecodedValue::Array(arr) => {
                write!(f, "[")?;
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
            DecodedValue::Structure(fields) => {
                write!(f, "{{")?;
                for (i, (name, val)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}={}", name, val)?;
                }
                write!(f, "}}")
            }
            DecodedValue::Raw(data) => {
                if data.len() <= 8 {
                    write!(f, "<{} bytes: {}>", data.len(), hex::encode(data))
                } else {
                    write!(f, "<{} bytes>", data.len())
                }
            }
        }
    }
}

/// Height of a type tree in `PvdDecoder::enter` units: a leaf is 1, a
/// structure or union is 1 + its tallest field. Only called on trees the
/// decoder has already built under the depth limit, so the recursion is
/// bounded by `DecodeLimits::max_depth`.
fn type_height(ft: &FieldType) -> usize {
    let fields = match ft {
        FieldType::Structure(d) | FieldType::StructureArray(d) => &d.fields,
        FieldType::Union(f) | FieldType::UnionArray(f) => f,
        _ => return 1,
    };
    1 + fields
        .iter()
        .map(|f| type_height(&f.field_type))
        .max()
        .unwrap_or(0)
}

/// Node count of a type tree: every structure, union and leaf is one node.
/// Like `type_height`, only called on trees already built under the depth
/// limit and the node budget, so it is bounded by both.
fn type_nodes(ft: &FieldType) -> usize {
    let fields = match ft {
        FieldType::Structure(d) | FieldType::StructureArray(d) => &d.fields,
        FieldType::Union(f) | FieldType::UnionArray(f) => f,
        _ => return 1,
    };
    1 + fields
        .iter()
        .map(|f| type_nodes(&f.field_type))
        .sum::<usize>()
}

/// The `DecodeError::Malformed` text for a descriptor whose resolved type
/// tree passes the node budget. A named constant so the lenient field loop
/// in `parse_structure_desc` can surface it instead of swallowing it.
const NODE_BUDGET_EXCEEDED: &str = "introspection expands past the node budget";

/// Resolved type nodes allowed per input byte, on top of the
/// `DecodeLimits::max_struct_fields` floor. The threat is *amplification*
/// (`0xFE` splicing a stored tree in for 3 bytes), not size: a literal type
/// node costs at least one wire byte and a legitimate `0xFE` repeat (pvxs
/// caches types and resends them by id) carries its element's value bytes,
/// so real payloads stay far below 64 nodes per byte. A 4-way registry chain
/// is ~4^n nodes from ~23n bytes and passes it within a few levels.
const NODES_PER_BYTE: usize = 64;

/// The errors that mean "this input is hostile", as opposed to "this input
/// is short or garbled". The lenient field loops stop quietly on the latter
/// but must surface these, or the guard becomes a silent truncation.
///
/// `Malformed(NODE_BUDGET_EXCEEDED)` is a *value* comparison of the `&str`
/// payload against the constant: any other `Malformed` text never matches,
/// so the constant must be the only way this error is ever built.
fn is_hostile(e: &DecodeError) -> bool {
    matches!(
        e,
        DecodeError::DepthExceeded { .. } | DecodeError::Malformed(NODE_BUDGET_EXCEEDED)
    )
}

/// PVD Decoder state
pub struct PvdDecoder {
    is_be: bool,
    limits: DecodeLimits,
    /// IntrospectionRegistry: maps int16 keys to previously seen FieldTypes.
    /// Populated when parsing `0xFD` (full-with-id) entries, looked up on `0xFE` (only-id).
    /// Each entry carries the stored type's height (see `type_height`) and
    /// node count (see `type_nodes`), recorded once on insert so a `0xFE`
    /// reference can be depth- and budget-checked in O(1) instead of
    /// re-walking the tree.
    registry: std::cell::RefCell<std::collections::HashMap<u16, (FieldType, usize, usize)>>,
    /// Current recursion depth. A `Cell` rather than a `&mut self` parameter
    /// so no public signature changes (which would ripple into the frozen
    /// `spvirit-py` surface).
    depth: std::cell::Cell<usize>,
    /// Type nodes built by the current top-level decode. The depth limit
    /// bounds height but not breadth: a structure whose fields all
    /// `0xFE`-reference the previous level grows 4^n nodes from O(n) bytes.
    /// Each parsed type charges 1, each `0xFE` charges the stored node count,
    /// and passing `node_limit` fails the decode. Reset at every top-level
    /// entry (`node_budget_scope`).
    nodes: std::cell::Cell<usize>,
    /// The current top-level decode's node ceiling:
    /// `max_struct_fields + NODES_PER_BYTE * input_len`, so the budget bounds
    /// the amplification ratio rather than the size of legitimate data.
    node_limit: std::cell::Cell<usize>,
    /// Nesting of public decode entry points, so only the outermost one
    /// resets `nodes`. Not `depth`: `decode_structure` does not `enter()`,
    /// so its fields' `decode_value` calls run at depth 0 and would each
    /// get a fresh budget.
    budget_scope: std::cell::Cell<usize>,
}

/// Decrements the decoder's depth counter (or node-budget scope counter) on
/// *every* exit path, including the `?` early returns that are the whole
/// point of the recursion being dangerous.
struct DepthGuard<'a>(&'a std::cell::Cell<usize>);

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

impl PvdDecoder {
    pub fn new(is_be: bool) -> Self {
        Self::with_limits(is_be, DecodeLimits::default())
    }

    /// Build a decoder with non-default array-length ceilings.
    pub fn with_limits(is_be: bool, limits: DecodeLimits) -> Self {
        Self {
            is_be,
            limits,
            registry: std::cell::RefCell::new(std::collections::HashMap::new()),
            depth: std::cell::Cell::new(0),
            nodes: std::cell::Cell::new(0),
            node_limit: std::cell::Cell::new(limits.max_struct_fields),
            budget_scope: std::cell::Cell::new(0),
        }
    }

    /// The array-length ceilings this decoder was built with.
    pub fn limits(&self) -> &DecodeLimits {
        &self.limits
    }

    /// Decode a size value (PVA variable-length encoding).
    ///
    /// Delegates to the shared [`decode_size_prefixed`] core.
    pub fn decode_size(&self, data: &[u8]) -> DecodeResult<(usize, usize)> {
        decode_size_prefixed(data, self.is_be)
    }

    /// Decode a string.
    ///
    /// STRICT UTF-8: these are structured pvData values, where a non-UTF-8
    /// value is a genuine protocol error (contrast the LOSSY policy used at the
    /// outer epics-protocol framing boundary in `epics_decode`).
    pub fn decode_string(&self, data: &[u8]) -> DecodeResult<(String, usize)> {
        decode_string_prefixed(data, self.is_be, Utf8Policy::Strict)
    }

    /// Parse field description from introspection data
    pub fn parse_field_desc(&self, data: &[u8]) -> DecodeResult<(FieldDesc, usize)> {
        let _scope = self.node_budget_scope(data.len());
        if data.is_empty() {
            return Err(DecodeError::Truncated {
                needed: 1,
                available: 0,
            });
        }

        let mut offset = 0;

        // Parse field name
        let (name, consumed) = self.decode_string(&data[offset..])?;
        offset += consumed;

        if offset >= data.len() {
            return Err(DecodeError::Truncated {
                needed: offset + 1,
                available: data.len(),
            });
        }

        // Parse type descriptor
        let (field_type, type_consumed) = self.parse_type_desc(&data[offset..])?;
        offset += type_consumed;

        Ok((FieldDesc { name, field_type }, offset))
    }

    /// Parse type descriptor
    fn parse_type_desc(&self, data: &[u8]) -> DecodeResult<(FieldType, usize)> {
        let _depth = self.enter()?;
        if data.is_empty() {
            return Err(DecodeError::Truncated {
                needed: 1,
                available: 0,
            });
        }

        let type_byte = data[0];
        let mut offset = 1;

        // 0xFD charges through its recursive parse, 0xFE the stored count.
        if type_byte != 0xFD && type_byte != 0xFE {
            self.charge_nodes(1)?;
        }

        // Check for NULL type
        if type_byte == 0xFF {
            return Ok((FieldType::Variant, 1));
        }

        // Full-with-id from IntrospectionRegistry:
        // 0xFD + int16 key + type descriptor payload.
        if type_byte == 0xFD {
            if data.len() < 3 {
                return Err(DecodeError::Truncated {
                    needed: 3,
                    available: data.len(),
                });
            }
            let key = if self.is_be {
                u16::from_be_bytes([data[1], data[2]])
            } else {
                u16::from_le_bytes([data[1], data[2]])
            };
            let (field_type, consumed) = self.parse_type_desc(&data[3..])?;
            let height = type_height(&field_type);
            let nodes = type_nodes(&field_type);
            self.registry
                .borrow_mut()
                .insert(key, (field_type.clone(), height, nodes));
            return Ok((field_type, 3 + consumed));
        }

        // Only-id from IntrospectionRegistry:
        // 0xFE + int16 key — reference to a previously seen type.
        if type_byte == 0xFE {
            if data.len() < 3 {
                return Err(DecodeError::Truncated {
                    needed: 3,
                    available: data.len(),
                });
            }
            let key = if self.is_be {
                u16::from_be_bytes([data[1], data[2]])
            } else {
                u16::from_le_bytes([data[1], data[2]])
            };
            if let Some((ft, height, nodes)) = self.registry.borrow().get(&key) {
                self.check_resolved_height(*height)?;
                self.charge_nodes(*nodes)?;
                return Ok((ft.clone(), 3));
            }
            debug!(
                "Type descriptor ONLY_ID (0xFE) key={} not found in registry",
                key
            );
            return Err(DecodeError::UnresolvedTypeId(key));
        }

        // Check for structure (0x80) or structure array (0x88)
        if type_byte == 0x80 || type_byte == 0x88 {
            let is_array = (type_byte & 0x08) != 0;
            if is_array {
                // Skip the inner structure element tag (0x80)
                if offset >= data.len() {
                    return Err(DecodeError::Truncated {
                        needed: offset + 1,
                        available: data.len(),
                    });
                }
                if data[offset] != 0x80 {
                    return Err(DecodeError::UnknownTypeTag(data[offset]));
                }
                offset += 1;
            }
            let (struct_desc, consumed) = self.parse_structure_desc(&data[offset..])?;
            offset += consumed;
            if is_array {
                return Ok((FieldType::StructureArray(struct_desc), offset));
            } else {
                return Ok((FieldType::Structure(struct_desc), offset));
            }
        }

        // Check for union (0x81) or union array (0x89)
        if type_byte == 0x81 || type_byte == 0x89 {
            let is_array = (type_byte & 0x08) != 0;
            if is_array {
                // Skip the inner union element tag (0x81)
                if offset >= data.len() {
                    return Err(DecodeError::Truncated {
                        needed: offset + 1,
                        available: data.len(),
                    });
                }
                if data[offset] != 0x81 {
                    return Err(DecodeError::UnknownTypeTag(data[offset]));
                }
                offset += 1;
            }
            // Parse union fields (same as structure)
            let (struct_desc, consumed) = self.parse_structure_desc(&data[offset..])?;
            offset += consumed;
            if is_array {
                return Ok((FieldType::UnionArray(struct_desc.fields), offset));
            } else {
                return Ok((FieldType::Union(struct_desc.fields), offset));
            }
        }

        // Check for variant/any (0x82) or variant array (0x8A)
        if type_byte == 0x82 {
            return Ok((FieldType::Variant, 1));
        }
        if type_byte == 0x8A {
            return Ok((FieldType::VariantArray, 1));
        }

        // Check for bounded string (0x83, legacy 0x86 accepted for compatibility)
        if type_byte == 0x83 || type_byte == 0x86 {
            let (bound, consumed) = self.decode_size(&data[offset..])?;
            offset += consumed;
            return Ok((FieldType::BoundedString(bound as u32), offset));
        }

        // Scalar / scalar-array with mode bits:
        // 0x00=not-array, 0x08=variable, 0x10=bounded, 0x18=fixed
        let scalar_or_array = type_byte & 0x18;
        let is_array = scalar_or_array != 0;
        if is_array && scalar_or_array != 0x08 {
            // Consume bounded/fixed max length for alignment, even if we don't model it.
            let (_bound, consumed) = self.decode_size(&data[offset..])?;
            offset += consumed;
        }
        let base_type = type_byte & 0xE7;

        // String type
        if base_type == 0x60 {
            if is_array {
                return Ok((FieldType::StringArray, offset));
            } else {
                return Ok((FieldType::String, offset));
            }
        }

        // Numeric types
        if let Some(tc) = TypeCode::from_byte(base_type) {
            if is_array {
                return Ok((FieldType::ScalarArray(tc), offset));
            } else {
                return Ok((FieldType::Scalar(tc), offset));
            }
        }

        debug!("Unknown type byte: 0x{:02x}", type_byte);
        Err(DecodeError::UnknownTypeTag(type_byte))
    }

    /// Parse structure description
    fn parse_structure_desc(&self, data: &[u8]) -> DecodeResult<(StructureDesc, usize)> {
        let mut offset = 0;

        // Parse optional struct ID
        let (struct_id, consumed) = self.decode_string(&data[offset..])?;
        offset += consumed;

        let struct_id = if struct_id.is_empty() {
            None
        } else {
            Some(struct_id)
        };

        // Parse field count
        let (field_count, consumed) = self.decode_size(&data[offset..])?;
        offset += consumed;

        // Reject on the configured ceiling *before* reserving -- a failed
        // `Vec` allocation aborts the process instead of unwinding.
        //
        // Deliberately NOT a bytes-remaining check (`check_array_count`'s
        // second half): a descriptor cut short by a third-party IOC must keep
        // parsing leniently via the `break` in the loop below, and the codec
        // plan pins that contract with `truncated_descriptor_parses_leniently_by_design`.
        // The reserve is instead capped at the number of two-byte fields
        // (one-byte empty-name prefix + one-byte type code) the remaining
        // bytes could possibly hold, so a legal-but-oversized count cannot
        // over-allocate either.
        if field_count > self.limits.max_struct_fields {
            return Err(DecodeError::ArrayTooLarge {
                kind: "structure fields",
                count: field_count,
                limit: self.limits.max_struct_fields,
            });
        }

        let mut fields =
            Vec::with_capacity(field_count.min(data.len().saturating_sub(offset) / 2));

        // Lenient by design. A descriptor that announces more fields than the
        // buffer holds returns `Ok` with the fields that did parse rather than
        // an error: spvirit is a passive observer and a gateway, and one
        // malformed descriptor from a third-party IOC must not drop a
        // connection carrying other channels. Callers therefore cannot tell
        // "2 fields" from "6 announced, 2 salvaged" - if you need that
        // distinction, add a returned count rather than making this strict.
        // Pinned by `truncated_descriptor_parses_leniently_by_design`.
        // Same choice, same reason, in `decode_structure` below.
        for _ in 0..field_count {
            if offset >= data.len() {
                break;
            }
            match self.parse_field_desc(&data[offset..]) {
                Ok((field, consumed)) => {
                    offset += consumed;
                    fields.push(field);
                }
                // Truncated/garbage input stays lenient: stop and return what
                // parsed. A depth or node-budget violation does NOT --
                // swallowing it here would turn the guard into a silent
                // truncation and the caller would never learn the descriptor
                // was hostile. See `is_hostile`.
                Err(e) if is_hostile(&e) => return Err(e),
                Err(_) => break,
            }
        }

        Ok((StructureDesc { struct_id, fields }, offset))
    }

    /// Parse the full type introspection from INIT response
    pub fn parse_introspection(&self, data: &[u8]) -> DecodeResult<StructureDesc> {
        self.parse_introspection_with_len(data)
            .map(|(desc, _)| desc)
    }

    /// Parse full type introspection and return consumed bytes.
    pub fn parse_introspection_with_len(&self, data: &[u8]) -> DecodeResult<(StructureDesc, usize)> {
        let _scope = self.node_budget_scope(data.len());
        let _depth = self.enter()?;
        if data.is_empty() {
            return Err(DecodeError::Truncated {
                needed: 1,
                available: 0,
            });
        }

        // The introspection starts with a type byte
        let type_byte = data[0];

        // Should be a structure (0x80)
        if type_byte == 0x80 {
            self.charge_nodes(1)?;
            let (desc, consumed) = self.parse_structure_desc(&data[1..])?;
            return Ok((desc, 1 + consumed));
        }

        // Full-with-id from IntrospectionRegistry:
        // 0xFD + int16 key + field type descriptor payload.
        if type_byte == 0xFD {
            if data.len() < 3 {
                return Err(DecodeError::Truncated {
                    needed: 3,
                    available: data.len(),
                });
            }
            let key = if self.is_be {
                u16::from_be_bytes([data[1], data[2]])
            } else {
                u16::from_le_bytes([data[1], data[2]])
            };
            let (desc, consumed) = self.parse_introspection_with_len(&data[3..])?;
            // Register this structure type for later 0xFE references
            let field_type = FieldType::Structure(desc.clone());
            let height = type_height(&field_type);
            let nodes = type_nodes(&field_type);
            self.registry
                .borrow_mut()
                .insert(key, (field_type, height, nodes));
            return Ok((desc, 3 + consumed));
        }

        // Only-id from IntrospectionRegistry:
        // 0xFE + int16 key — reference to a previously seen type.
        if type_byte == 0xFE {
            if data.len() < 3 {
                return Err(DecodeError::Truncated {
                    needed: 3,
                    available: data.len(),
                });
            }
            let key = if self.is_be {
                u16::from_be_bytes([data[1], data[2]])
            } else {
                u16::from_le_bytes([data[1], data[2]])
            };
            if let Some((ft, height, nodes)) = self.registry.borrow().get(&key)
                && let FieldType::Structure(desc) = ft {
                    self.check_resolved_height(*height)?;
                    self.charge_nodes(*nodes)?;
                    return Ok((desc.clone(), 3));
                }
            debug!(
                "Introspection ONLY_ID (0xFE) key={} not found in registry",
                key
            );
            return Err(DecodeError::UnresolvedTypeId(key));
        }

        debug!("Unexpected introspection type byte: 0x{:02x}", type_byte);
        Err(DecodeError::UnknownTypeTag(type_byte))
    }

    /// Decode a scalar value
    fn decode_scalar(&self, data: &[u8], tc: TypeCode) -> DecodeResult<(DecodedValue, usize)> {
        let size = tc
            .size()
            .ok_or(DecodeError::Malformed("type code has no fixed scalar size"))?;
        if data.len() < size {
            return Err(DecodeError::Truncated {
                needed: size,
                available: data.len(),
            });
        }

        let value = match tc {
            TypeCode::Boolean => DecodedValue::Boolean(data[0] != 0),
            TypeCode::Int8 => DecodedValue::Int8(data[0] as i8),
            TypeCode::UInt8 => DecodedValue::UInt8(data[0]),
            TypeCode::Int16 => {
                let v = if self.is_be {
                    i16::from_be_bytes([data[0], data[1]])
                } else {
                    i16::from_le_bytes([data[0], data[1]])
                };
                DecodedValue::Int16(v)
            }
            TypeCode::UInt16 => {
                let v = if self.is_be {
                    u16::from_be_bytes([data[0], data[1]])
                } else {
                    u16::from_le_bytes([data[0], data[1]])
                };
                DecodedValue::UInt16(v)
            }
            TypeCode::Int32 => {
                let v = if self.is_be {
                    i32::from_be_bytes(data[0..4].try_into().unwrap())
                } else {
                    i32::from_le_bytes(data[0..4].try_into().unwrap())
                };
                DecodedValue::Int32(v)
            }
            TypeCode::UInt32 => {
                let v = if self.is_be {
                    u32::from_be_bytes(data[0..4].try_into().unwrap())
                } else {
                    u32::from_le_bytes(data[0..4].try_into().unwrap())
                };
                DecodedValue::UInt32(v)
            }
            TypeCode::Int64 => {
                let v = if self.is_be {
                    i64::from_be_bytes(data[0..8].try_into().unwrap())
                } else {
                    i64::from_le_bytes(data[0..8].try_into().unwrap())
                };
                DecodedValue::Int64(v)
            }
            TypeCode::UInt64 => {
                let v = if self.is_be {
                    u64::from_be_bytes(data[0..8].try_into().unwrap())
                } else {
                    u64::from_le_bytes(data[0..8].try_into().unwrap())
                };
                DecodedValue::UInt64(v)
            }
            TypeCode::Float32 => {
                let v = if self.is_be {
                    f32::from_be_bytes(data[0..4].try_into().unwrap())
                } else {
                    f32::from_le_bytes(data[0..4].try_into().unwrap())
                };
                DecodedValue::Float32(v)
            }
            TypeCode::Float64 => {
                let v = if self.is_be {
                    f64::from_be_bytes(data[0..8].try_into().unwrap())
                } else {
                    f64::from_le_bytes(data[0..8].try_into().unwrap())
                };
                DecodedValue::Float64(v)
            }
            _ => {
                return Err(DecodeError::Malformed(
                    "type code is not a decodable scalar",
                ))
            }
        };

        Ok((value, size))
    }

    /// Reject an array count that exceeds its configured limit, or that could
    /// not possibly fit in the bytes that remain.
    ///
    /// `min_elem` is the smallest number of wire bytes one element can
    /// occupy: the fixed width for scalars, 1 for strings, structures and
    /// unions. The limit is checked first, so a count that violates both
    /// yields `ArrayTooLarge`.
    fn check_array_count(
        &self,
        count: usize,
        min_elem: usize,
        available: usize,
        kind: &'static str,
        limit: usize,
    ) -> DecodeResult<()> {
        if count > limit {
            return Err(DecodeError::ArrayTooLarge { kind, count, limit });
        }
        let min_bytes = count.saturating_mul(min_elem);
        if min_bytes > available {
            return Err(DecodeError::CountExceedsBuffer {
                count,
                min_bytes,
                available,
            });
        }
        Ok(())
    }

    /// Enter one level of recursion, or fail. Hold the returned guard for the
    /// duration of the recursive call.
    fn enter(&self) -> DecodeResult<DepthGuard<'_>> {
        let next = self.depth.get() + 1;
        if next > self.limits.max_depth {
            return Err(DecodeError::DepthExceeded {
                depth: next,
                limit: self.limits.max_depth,
            });
        }
        self.depth.set(next);
        Ok(DepthGuard(&self.depth))
    }

    /// A `0xFE` reference splices a stored type of `height` levels in at
    /// the current depth with O(1) wire recursion, so `enter()` alone never
    /// sees its nesting. Reject it if the resolved tree would pass
    /// `max_depth`; otherwise chained `0xFD`/`0xFE` entries build an
    /// unboundedly deep tree (and overflow the stack in the derived
    /// `Clone`/`Drop`) from wire nesting that never exceeds the limit.
    fn check_resolved_height(&self, height: usize) -> DecodeResult<()> {
        let depth = (self.depth.get() + height).saturating_sub(1);
        if depth > self.limits.max_depth {
            return Err(DecodeError::DepthExceeded {
                depth,
                limit: self.limits.max_depth,
            });
        }
        Ok(())
    }

    /// Enter a node-budget scope; the outermost one starts a fresh budget
    /// sized to its input (`input_len` = the bytes available to that call).
    /// A nested entry (`0xFD` recursion, a structure's fields, a variant's
    /// descriptor, every element of an array) is charged to the decode
    /// already in progress. Hold the returned guard for the duration of the
    /// call.
    fn node_budget_scope(&self, input_len: usize) -> DepthGuard<'_> {
        let scope = self.budget_scope.get();
        if scope == 0 {
            self.nodes.set(0);
            self.node_limit.set(
                self.limits
                    .max_struct_fields
                    .saturating_add(NODES_PER_BYTE.saturating_mul(input_len)),
            );
        }
        self.budget_scope.set(scope + 1);
        DepthGuard(&self.budget_scope)
    }

    /// Charge `n` type nodes to the current decode's budget, or fail once
    /// the resolved nodes pass `node_limit`. The `max_struct_fields` floor
    /// (one structure's field-count ceiling) covers any single descriptor;
    /// the per-byte share lets linear-size values (an `any[]` of thousands of
    /// NTScalars, literal or `0xFE`-cached) through while capping the
    /// amplification a few bytes of `0xFE` references can buy.
    fn charge_nodes(&self, n: usize) -> DecodeResult<()> {
        let used = self.nodes.get().saturating_add(n);
        self.nodes.set(used);
        if used > self.node_limit.get() {
            return Err(DecodeError::Malformed(NODE_BUDGET_EXCEEDED));
        }
        Ok(())
    }

    /// Decode value according to field type
    pub fn decode_value(
        &self,
        data: &[u8],
        field_type: &FieldType,
    ) -> DecodeResult<(DecodedValue, usize)> {
        // A `Variant` value carries its own type descriptor, charged to the
        // node budget. One budget per top-level value: a long-lived decoder
        // never accumulates charges across values, and one value cannot
        // splice more than the budget's worth of `0xFE` clones.
        let _scope = self.node_budget_scope(data.len());
        let _depth = self.enter()?;
        match field_type {
            FieldType::Scalar(tc) => self.decode_scalar(data, *tc),
            FieldType::String | FieldType::BoundedString(_) => {
                let (s, consumed) = self.decode_string(data)?;
                Ok((DecodedValue::String(s), consumed))
            }
            FieldType::ScalarArray(tc) => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                let elem_size = tc.size().unwrap_or(1);
                self.check_array_count(
                    count,
                    elem_size,
                    data.len() - offset,
                    "scalar array",
                    self.limits.max_scalar_array,
                )?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let (val, consumed) = self.decode_scalar(&data[offset..], *tc)?;
                    values.push(val);
                    offset += consumed;
                }
                Ok((DecodedValue::Array(values), offset))
            }
            FieldType::StringArray => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                self.check_array_count(
                    count,
                    1,
                    data.len() - offset,
                    "string array",
                    self.limits.max_string_array,
                )?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let (s, consumed) = self.decode_string(&data[offset..])?;
                    values.push(DecodedValue::String(s));
                    offset += consumed;
                }
                Ok((DecodedValue::Array(values), offset))
            }
            FieldType::Structure(desc) => self.decode_structure(data, desc),
            FieldType::StructureArray(desc) => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                self.check_array_count(
                    count,
                    1,
                    data.len() - offset,
                    "structure array",
                    self.limits.max_struct_array,
                )?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    // Read per-element null indicator (0 = null, non-zero = present)
                    if offset >= data.len() {
                        return Err(DecodeError::Truncated {
                            needed: offset + 1,
                            available: data.len(),
                        });
                    }
                    let null_indicator = data[offset];
                    offset += 1;
                    if null_indicator == 0 {
                        // null element – push empty structure placeholder
                        values.push(DecodedValue::Structure(Vec::new()));
                        continue;
                    }
                    let (item, consumed) = self.decode_structure(&data[offset..], desc)?;
                    values.push(item);
                    offset += consumed;
                }
                Ok((DecodedValue::Array(values), offset))
            }
            FieldType::Union(fields) => {
                let (selector, consumed) = self.decode_size(data)?;
                let field = fields.get(selector).ok_or(DecodeError::UnknownUnionSelector {
                    selector,
                    len: fields.len(),
                })?;
                let (value, val_consumed) =
                    self.decode_value(&data[consumed..], &field.field_type)?;
                Ok((
                    DecodedValue::Structure(vec![(field.name.clone(), value)]),
                    consumed + val_consumed,
                ))
            }
            FieldType::UnionArray(fields) => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                self.check_array_count(
                    count,
                    1,
                    data.len() - offset,
                    "union array",
                    self.limits.max_union_array,
                )?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let (selector, consumed) = self.decode_size(&data[offset..])?;
                    offset += consumed;
                    let field =
                        fields
                            .get(selector)
                            .ok_or(DecodeError::UnknownUnionSelector {
                                selector,
                                len: fields.len(),
                            })?;
                    let (value, val_consumed) =
                        self.decode_value(&data[offset..], &field.field_type)?;
                    offset += val_consumed;
                    values.push(DecodedValue::Structure(vec![(field.name.clone(), value)]));
                }
                Ok((DecodedValue::Array(values), offset))
            }
            FieldType::Variant => {
                if data.is_empty() {
                    return Err(DecodeError::Truncated {
                        needed: 1,
                        available: 0,
                    });
                }
                if data[0] == 0xFF {
                    return Ok((DecodedValue::Null, 1));
                }
                let (variant_type, type_consumed) = self.parse_type_desc(data)?;
                let (variant_value, value_consumed) =
                    self.decode_value(&data[type_consumed..], &variant_type)?;
                Ok((variant_value, type_consumed + value_consumed))
            }
            FieldType::VariantArray => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                self.check_array_count(
                    count,
                    1,
                    data.len() - offset,
                    "variant array",
                    self.limits.max_variant_array,
                )?;
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let (v, consumed) = self.decode_value(&data[offset..], &FieldType::Variant)?;
                    values.push(v);
                    offset += consumed;
                }
                Ok((DecodedValue::Array(values), offset))
            }
        }
    }

    /// Decode a structure value using the field descriptions
    pub fn decode_structure(
        &self,
        data: &[u8],
        desc: &StructureDesc,
    ) -> DecodeResult<(DecodedValue, usize)> {
        let _scope = self.node_budget_scope(data.len());
        let mut offset = 0;
        let mut fields: Vec<(String, DecodedValue)> = Vec::new();

        // Lenient by design - see the note in `parse_structure_desc`.
        for field in &desc.fields {
            if offset >= data.len() {
                break;
            }
            match self.decode_value(&data[offset..], &field.field_type) {
                Ok((value, consumed)) => {
                    fields.push((field.name.clone(), value));
                    offset += consumed;
                }
                // Same rule as `parse_structure_desc`: a depth or node-budget
                // violation is surfaced, everything else stops the loop as before.
                Err(e) if is_hostile(&e) => return Err(e),
                Err(_) => break,
            }
        }

        Ok((DecodedValue::Structure(fields), offset))
    }

    /// Decode a structure with a bitset indicating which fields are present
    /// This is used for delta updates in MONITOR
    pub fn decode_structure_with_bitset(
        &self,
        data: &[u8],
        desc: &StructureDesc,
    ) -> DecodeResult<(DecodedValue, usize)> {
        if data.is_empty() {
            return Err(DecodeError::Truncated {
                needed: 1,
                available: 0,
            });
        }

        let mut offset = 0;

        // Parse the bitset - PVA uses size-encoded bitset
        let (bitset_size, size_consumed) = self.decode_size(data)?;
        offset += size_consumed;

        if bitset_size == 0 || offset + bitset_size > data.len() {
            return Ok((DecodedValue::Structure(vec![]), offset));
        }

        let bitset = &data[offset..offset + bitset_size];
        offset += bitset_size;

        let (value, consumed) =
            self.decode_structure_with_bitset_body(&data[offset..], desc, bitset)?;
        Ok((value, offset + consumed))
    }

    pub(crate) fn decode_structure_with_bitset_body(
        &self,
        data: &[u8],
        desc: &StructureDesc,
        bitset: &[u8],
    ) -> DecodeResult<(DecodedValue, usize)> {
        let _scope = self.node_budget_scope(data.len());
        // Bit 0 is for the whole structure, field bits start at bit 1
        debug!(
            "Bitset: {:02x?} (size={}), total_fields={}",
            bitset,
            bitset.len(),
            count_structure_fields(desc)
        );
        debug!(
            "Structure fields: {:?}",
            desc.fields.iter().map(|f| &f.name).collect::<Vec<_>>()
        );

        // Special case: bitset contains only bit0 (whole structure) and no field bits.
        let mut has_field_bits = false;
        if !bitset.is_empty() {
            for (i, b) in bitset.iter().enumerate() {
                let mask = if i == 0 { *b & !0x01 } else { *b };
                if mask != 0 {
                    has_field_bits = true;
                    break;
                }
            }
        }
        if !has_field_bits && !bitset.is_empty() && (bitset[0] & 0x01) != 0 {
            match self.decode_structure(data, desc) {
                Ok((value, consumed)) => return Ok((value, consumed)),
                // A hostile input is not retried through the per-field walk.
                Err(e) if is_hostile(&e) => return Err(e),
                Err(_) => {}
            }
        }

        let mut fields: Vec<(String, DecodedValue)> = Vec::new();
        let mut offset = 0usize;

        // Recursive decoder threads its whole state explicitly; a context struct would widen this byte-critical path.
        #[allow(clippy::too_many_arguments)]
        fn decode_with_bitset_recursive(
            decoder: &PvdDecoder,
            data: &[u8],
            offset: &mut usize,
            desc: &StructureDesc,
            bitset: &[u8],
            bit_offset: &mut usize,
            counts: &[usize],
            fields: &mut Vec<(String, DecodedValue)>,
        ) -> DecodeResult<bool> {
            // `Ok(false)` stops the walk leniently (short/garbled data, the
            // partial structure is returned); a hostile error (`is_hostile`)
            // is propagated so the guard is never a silent truncation.
            for field in &desc.fields {
                let byte_idx = *bit_offset / 8;
                let bit_idx = *bit_offset % 8;
                let current_bit = *bit_offset;
                *bit_offset += 1;

                let field_present = if byte_idx < bitset.len() {
                    (bitset[byte_idx] & (1 << bit_idx)) != 0
                } else {
                    false
                };

                debug!(
                    "Field '{}' at bit {}: present={}",
                    field.name, current_bit, field_present
                );

                if let FieldType::Structure(nested_desc) = &field.field_type {
                    let child_start_bit = *bit_offset;
                    let child_field_count = counts[current_bit - 1];

                    let mut any_child_bits_set = false;
                    for i in 0..child_field_count {
                        let check_byte = (child_start_bit + i) / 8;
                        let check_bit = (child_start_bit + i) % 8;
                        if check_byte < bitset.len() && (bitset[check_byte] & (1 << check_bit)) != 0
                        {
                            any_child_bits_set = true;
                            break;
                        }
                    }

                    debug!(
                        "Nested structure '{}': parent_present={}, child_start_bit={}, child_count={}, any_child_bits_set={}",
                        field.name,
                        field_present,
                        child_start_bit,
                        child_field_count,
                        any_child_bits_set
                    );

                    if field_present && !any_child_bits_set {
                        *bit_offset += child_field_count;
                        if *offset < data.len() {
                            match decoder.decode_structure(&data[*offset..], nested_desc) {
                                Ok((value, consumed)) => {
                                    debug!(
                                        "Decoded full nested structure '{}', consumed {} bytes",
                                        field.name, consumed
                                    );
                                    fields.push((field.name.clone(), value));
                                    *offset += consumed;
                                }
                                Err(e) if is_hostile(&e) => return Err(e),
                                Err(_) => {
                                    debug!(
                                        "Failed to decode full nested structure '{}'",
                                        field.name
                                    );
                                    return Ok(false);
                                }
                            }
                        }
                    } else if any_child_bits_set {
                        let mut nested_fields: Vec<(String, DecodedValue)> = Vec::new();
                        if !decode_with_bitset_recursive(
                            decoder,
                            data,
                            offset,
                            nested_desc,
                            bitset,
                            bit_offset,
                            counts,
                            &mut nested_fields,
                        )? {
                            return Ok(false);
                        }
                        debug!(
                            "Nested structure '{}' decoded {} fields",
                            field.name,
                            nested_fields.len()
                        );
                        if !nested_fields.is_empty() {
                            fields
                                .push((field.name.clone(), DecodedValue::Structure(nested_fields)));
                        }
                    } else {
                        *bit_offset += child_field_count;
                    }
                } else if field_present {
                    if *offset >= data.len() {
                        debug!(
                            "Data exhausted at offset {} for field '{}'",
                            *offset, field.name
                        );
                        return Ok(false);
                    }
                    match decoder.decode_value(&data[*offset..], &field.field_type) {
                        Ok((value, consumed)) => {
                            fields.push((field.name.clone(), value));
                            *offset += consumed;
                        }
                        Err(e) if is_hostile(&e) => return Err(e),
                        Err(_) => return Ok(false),
                    }
                }
            }
            Ok(true)
        }

        let counts = subtree_field_counts(desc);
        let mut bit_offset = 1;
        decode_with_bitset_recursive(
            self,
            data,
            &mut offset,
            desc,
            bitset,
            &mut bit_offset,
            &counts,
            &mut fields,
        )?;
        Ok((DecodedValue::Structure(fields), offset))
    }
}

/// Count total fields in a structure (including nested).
///
/// Self-then-nested, depth-first. This is the order the MONITOR bitset bits
/// are numbered in, so `monitor::flatten_field_paths` must walk it identically.
pub(crate) fn count_structure_fields(desc: &StructureDesc) -> usize {
    let mut count = 0;
    for field in &desc.fields {
        count += 1;
        if let FieldType::Structure(nested) = &field.field_type {
            count += count_structure_fields(nested);
        }
    }
    count
}

/// Descendant field-bit counts in bitset-bit order.
///
/// `counts[n]` is the number of bits owned by the *subtree* of the field
/// addressed by bit `n + 1` (bit 0 is the whole structure and has no entry).
/// Built once per decode so the bitset walk stops re-running
/// `count_structure_fields` for every nested field of every frame.
pub(crate) fn subtree_field_counts(desc: &StructureDesc) -> Vec<usize> {
    let mut out = Vec::with_capacity(count_structure_fields(desc));
    push_subtree_counts(desc, &mut out);
    out
}

fn push_subtree_counts(desc: &StructureDesc, out: &mut Vec<usize>) {
    for field in &desc.fields {
        let slot = out.len();
        out.push(0);
        if let FieldType::Structure(nested) = &field.field_type {
            push_subtree_counts(nested, out);
        }
        out[slot] = out.len() - slot - 1;
    }
}

/// Extract a sub-field from a StructureDesc by dot-separated path.
/// Returns the sub-field as an owned StructureDesc. For leaf (non-structure)
/// fields, returns a single-field StructureDesc wrapping the matched field.
/// Returns the full desc if path is empty.
pub fn extract_subfield_desc(desc: &StructureDesc, path: &str) -> Option<StructureDesc> {
    if path.is_empty() {
        return Some(desc.clone());
    }
    let mut parts = path.splitn(2, '.');
    let head = parts.next()?;
    let tail = parts.next().unwrap_or("");
    for field in &desc.fields {
        if field.name == head {
            match &field.field_type {
                FieldType::Structure(nested) | FieldType::StructureArray(nested) => {
                    return extract_subfield_desc(nested, tail);
                }
                _ => {
                    if tail.is_empty() {
                        return Some(StructureDesc {
                            struct_id: None,
                            fields: vec![field.clone()],
                        });
                    }
                    return None;
                }
            }
        }
    }
    None
}

/// Format a structure description for display
pub fn format_structure_desc(desc: &StructureDesc) -> String {
    let mut parts = Vec::new();
    if let Some(ref id) = desc.struct_id {
        parts.push(id.clone());
    }
    for field in &desc.fields {
        parts.push(format!("{}:{}", field.name, field.field_type.type_name()));
    }
    parts.join(", ")
}

pub fn format_structure_tree(desc: &StructureDesc) -> String {
    fn push_fields(out: &mut Vec<String>, fields: &[FieldDesc], indent: usize) {
        let prefix = "  ".repeat(indent);
        for field in fields {
            match &field.field_type {
                FieldType::Structure(nested) => {
                    out.push(format!("{}{}: structure", prefix, field.name));
                    push_fields(out, &nested.fields, indent + 1);
                }
                FieldType::StructureArray(nested) => {
                    out.push(format!("{}{}: structure[]", prefix, field.name));
                    push_fields(out, &nested.fields, indent + 1);
                }
                FieldType::Union(variants) => {
                    out.push(format!("{}{}: union", prefix, field.name));
                    push_fields(out, variants, indent + 1);
                }
                FieldType::UnionArray(variants) => {
                    out.push(format!("{}{}: union[]", prefix, field.name));
                    push_fields(out, variants, indent + 1);
                }
                FieldType::BoundedString(bound) => {
                    out.push(format!("{}{}: string<={}", prefix, field.name, bound));
                }
                _ => {
                    out.push(format!(
                        "{}{}: {}",
                        prefix,
                        field.name,
                        field.field_type.type_name()
                    ));
                }
            }
        }
    }

    let mut lines = Vec::new();
    if let Some(id) = &desc.struct_id {
        lines.push(format!("struct {}", id));
    } else {
        lines.push("struct <anonymous>".to_string());
    }
    push_fields(&mut lines, &desc.fields, 0);
    lines.join("\n")
}

/// Extract the "value" field from a decoded NTScalar structure
pub fn extract_nt_scalar_value(decoded: &DecodedValue) -> Option<&DecodedValue> {
    if let DecodedValue::Structure(fields) = decoded {
        for (name, value) in fields {
            if name == "value" {
                return Some(value);
            }
        }
    }
    None
}

/// Compact display of decoded value for logging - shows only updated fields concisely
pub fn format_compact_value(decoded: &DecodedValue) -> String {
    match decoded {
        DecodedValue::Structure(fields) => {
            if fields.is_empty() {
                return "{}".to_string();
            }

            let mut parts = Vec::new();

            for (name, val) in fields {
                let formatted = format_field_value_compact(name, val);
                if !formatted.is_empty() {
                    parts.push(formatted);
                }
            }

            parts.join(", ")
        }
        _ => format!("{}", decoded),
    }
}

/// Format a single field value compactly - shows key info for known structures
fn format_field_value_compact(name: &str, val: &DecodedValue) -> String {
    match val {
        DecodedValue::Structure(fields) => {
            // For known EPICS NTScalar structures, show only key fields
            match name {
                "alarm" => {
                    // Show severity and message if non-zero/non-empty
                    let severity = fields.iter().find(|(n, _)| n == "severity");
                    let message = fields.iter().find(|(n, _)| n == "message");
                    let mut parts = Vec::new();
                    if let Some((_, DecodedValue::Int32(s))) = severity
                        && *s != 0 {
                            parts.push(format!("sev={}", s));
                        }
                    if let Some((_, DecodedValue::String(m))) = message
                        && !m.is_empty() {
                            parts.push(format!("\"{}\"", m));
                        }
                    if parts.is_empty() {
                        String::new() // Don't show alarm if it's OK
                    } else {
                        format!("alarm={{{}}}", parts.join(", "))
                    }
                }
                "timeStamp" => {
                    // Show just seconds or skip entirely for brevity
                    let secs = fields.iter().find(|(n, _)| n == "secondsPastEpoch");
                    if let Some((_, DecodedValue::Int64(s))) = secs {
                        format!("ts={}", s)
                    } else {
                        String::new()
                    }
                }
                "display" | "control" | "valueAlarm" => {
                    // Skip verbose metadata structures in compact view
                    String::new()
                }
                _ => {
                    // For other structures, show all fields
                    let nested: Vec<String> = fields
                        .iter()
                        .map(|(n, v)| format!("{}={}", n, format_scalar_value(v)))
                        .collect();

                    if nested.is_empty() {
                        String::new()
                    } else {
                        format!("{}={{{}}}", name, nested.join(", "))
                    }
                }
            }
        }
        _ => {
            format!("{}={}", name, format_scalar_value(val))
        }
    }
}

/// Format a scalar value concisely
fn format_scalar_value(val: &DecodedValue) -> String {
    match val {
        DecodedValue::Null => "null".to_string(),
        DecodedValue::Boolean(v) => format!("{}", v),
        DecodedValue::Int8(v) => format!("{}", v),
        DecodedValue::Int16(v) => format!("{}", v),
        DecodedValue::Int32(v) => format!("{}", v),
        DecodedValue::Int64(v) => format!("{}", v),
        DecodedValue::UInt8(v) => format!("{}", v),
        DecodedValue::UInt16(v) => format!("{}", v),
        DecodedValue::UInt32(v) => format!("{}", v),
        DecodedValue::UInt64(v) => format!("{}", v),
        DecodedValue::Float32(v) => format!("{:.4}", v),
        DecodedValue::Float64(v) => format!("{:.6}", v),
        DecodedValue::String(v) => format!("\"{}\"", v),
        DecodedValue::Array(arr) => {
            if arr.is_empty() {
                "[]".to_string()
            } else {
                let items: Vec<String> = arr.iter().map(format_scalar_value).collect();
                format!("[{}]", items.join(", "))
            }
        }
        DecodedValue::Structure(fields) => {
            let nested: Vec<String> = fields
                .iter()
                .map(|(n, v)| format!("{}={}", n, format_scalar_value(v)))
                .collect();
            format!("{{{}}}", nested.join(", "))
        }
        DecodedValue::Raw(data) => {
            if data.len() <= 4 {
                format!("<{}>", hex::encode(data))
            } else {
                format!("<{}B>", data.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lenient contract: a descriptor that announces more fields than the
    /// buffer holds yields `Ok` with the fields that did parse. Deliberate - see
    /// the comment at the `break`. Change this test only with a protocol decision.
    #[test]
    fn truncated_descriptor_parses_leniently_by_design() {
        let decoder = PvdDecoder::new(false);
        // Build a well-formed 3-field structure descriptor, then cut it short so
        // only the first field survives.
        let mut desc = StructureDesc::new();
        for name in ["a", "b", "c"] {
            desc.fields.push(FieldDesc {
                name: name.to_string(),
                field_type: FieldType::Scalar(TypeCode::Int32),
            });
        }
        // `encode_structure_desc` emits 11 bytes here: 1 (empty struct id) +
        // 1 (field count 3) + 3 fields x 3 bytes (1-byte length + 1 name byte +
        // 1 type tag). Dropping the last 6 leaves exactly the header plus field
        // "a", so the loop parses one field and then breaks on `offset >= len`.
        let full = crate::spvd_encode::encode_structure_desc(&desc, false);
        assert_eq!(full.len(), 11, "descriptor layout changed; recompute the cut");
        let cut = &full[..full.len() - 6];

        let (parsed, _consumed) = decoder
            .parse_structure_desc(cut)
            .expect("lenient parse returns Ok, not Err");
        // 11 bytes = 1 (0x80) + 1 (field count 3) + 3 x 3-byte fields; cutting
        // 6 leaves exactly one complete field, so the lenient loop must stop
        // after it. Pinned exactly: salvaging a second field would be a change.
        assert_eq!(
            parsed.fields.len(),
            1,
            "the test buffer must actually be short"
        );
        assert_eq!(parsed.fields[0].name, "a");
    }

    /// `counts[n]` must equal what `count_structure_fields` returns for the field
    /// addressed by bit `n + 1`. Walk the descriptor independently and compare.
    #[test]
    fn subtree_field_counts_match_count_structure_fields() {
        fn walk(desc: &StructureDesc, out: &mut Vec<usize>) {
            for field in &desc.fields {
                match &field.field_type {
                    FieldType::Structure(nested) => {
                        out.push(count_structure_fields(nested));
                        walk(nested, out);
                    }
                    _ => out.push(0),
                }
            }
        }

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
        let mut top = StructureDesc::new();
        top.fields.push(FieldDesc {
            name: "mid".to_string(),
            field_type: FieldType::Structure(mid),
        });
        top.fields.push(FieldDesc {
            name: "z".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });

        let mut expected = Vec::new();
        walk(&top, &mut expected);
        assert_eq!(subtree_field_counts(&top), expected);
        assert_eq!(subtree_field_counts(&top).len(), count_structure_fields(&top));
    }

    #[test]
    fn test_decode_size() {
        let decoder = PvdDecoder::new(false);

        // Small size (single byte)
        assert_eq!(decoder.decode_size(&[5]), Ok((5, 1)));
        assert_eq!(decoder.decode_size(&[253]), Ok((253, 1)));

        // Medium/large size (5 bytes, 254 prefix + uint32)
        assert_eq!(
            decoder.decode_size(&[254, 0x00, 0x01, 0x00, 0x00]),
            Ok((256, 5))
        );
    }

    #[test]
    fn decode_size_reports_truncation_rather_than_none() {
        let decoder = PvdDecoder::new(false);
        // 254 announces a 4-byte length that is not present.
        let err = decoder.decode_size(&[254, 0x01]).unwrap_err();
        assert_eq!(
            err,
            DecodeError::Truncated {
                needed: 5,
                available: 2
            }
        );
    }

    #[test]
    fn decode_size_on_empty_buffer_is_truncated() {
        let decoder = PvdDecoder::new(false);
        assert_eq!(
            decoder.decode_size(&[]).unwrap_err(),
            DecodeError::Truncated {
                needed: 1,
                available: 0
            }
        );
    }

    #[test]
    fn unknown_type_tag_is_named() {
        let decoder = PvdDecoder::new(false);
        // Empty field name (0x00), then 0x40 — not a valid type descriptor
        // byte. (0x7F, as the plan suggested, is consumed as a 127-byte field
        // name and reports Truncated before reaching the type tag.)
        assert_eq!(
            decoder.parse_field_desc(&[0x00, 0x40]).unwrap_err(),
            DecodeError::UnknownTypeTag(0x40)
        );
    }

    #[test]
    fn limits_are_configurable_and_readable() {
        let limits = DecodeLimits {
            max_string_array: 7,
            ..DecodeLimits::default()
        };
        let decoder = PvdDecoder::with_limits(false, limits);
        assert_eq!(decoder.limits().max_string_array, 7);
        assert_eq!(decoder.limits().max_scalar_array, 4_000_000);
    }

    #[test]
    fn default_limits_match_the_documented_values() {
        let d = DecodeLimits::default();
        assert_eq!(d.max_scalar_array, 4_000_000);
        assert_eq!(d.max_string_array, 65_536);
        assert_eq!(d.max_struct_array, 65_536);
        assert_eq!(d.max_union_array, 65_536);
        assert_eq!(d.max_variant_array, 65_536);
        assert_eq!(d.max_struct_fields, 65_536);
        assert_eq!(d.max_depth, 32);
    }

    /// Site 5. `parse_structure_desc` reserved `Vec::with_capacity(count)`
    /// straight off the wire. `Vec`'s allocation failure calls
    /// `handle_alloc_error`, which aborts the process -- it is not a
    /// catchable panic, so this must be rejected before the reserve.
    ///
    /// Driven with a *configured* small ceiling so the test never asks for a
    /// real over-large allocation. The u32::MAX case is
    /// `an_enormous_field_count_is_rejected_before_the_reserve` below.
    #[test]
    fn a_field_count_over_the_configured_limit_is_rejected() {
        let decoder = PvdDecoder::with_limits(
            false,
            DecodeLimits {
                max_struct_fields: 4,
                ..DecodeLimits::default()
            },
        );
        // 0x80 structure, empty struct id, nine fields declared, no field
        // bytes at all.
        let data = [0x80u8, 0x00, 0x09];
        let err = decoder
            .parse_introspection(&data)
            .expect_err("a field count over the limit must be an error");
        match err {
            DecodeError::ArrayTooLarge { kind, count, limit } => {
                assert_eq!(kind, "structure fields");
                assert_eq!(count, 9);
                assert_eq!(limit, 4);
            }
            other => panic!("expected ArrayTooLarge, got {other:?}"),
        }
    }

    /// A realistic structure must still parse. The guard is a backstop, not a
    /// new ceiling on real data.
    #[test]
    fn an_ordinary_structure_still_parses_under_the_field_limit() {
        let decoder = PvdDecoder::new(false);
        let mut data = vec![0x80u8, 0x00, 0x03];
        for name in ["a", "bb", "ccc"] {
            data.push(name.len() as u8);
            data.extend_from_slice(name.as_bytes());
            data.push(0x43); // float64
        }
        let desc = decoder.parse_introspection(&data).expect("parses");
        assert_eq!(desc.fields.len(), 3);
    }

    /// The actual bomb from the review: a seven-byte blob declaring
    /// `u32::MAX` fields. Pre-fix this reached
    /// `Vec::<FieldDesc>::with_capacity(4_294_967_295)`, which aborts the
    /// process; post-fix it is rejected on the count alone, with no
    /// allocation at all.
    #[test]
    fn an_enormous_field_count_is_rejected_before_the_reserve() {
        let decoder = PvdDecoder::new(false);
        let mut data = vec![0x80u8, 0x00, 254];
        data.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(data.len(), 7);
        match decoder.parse_introspection(&data) {
            Err(DecodeError::ArrayTooLarge { kind, count, limit }) => {
                assert_eq!(kind, "structure fields");
                assert_eq!(count, u32::MAX as usize);
                assert_eq!(limit, 65_536);
            }
            other => panic!("expected ArrayTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_introspection_full_with_id() {
        let decoder = PvdDecoder::new(false);
        let data = vec![
            0xFD, // FULL_WITH_ID
            0x06, 0x00, // registry key (little-endian)
            0x80, // structure type follows
            0x00, // empty struct id
            0x01, // one field
            0x05, b'v', b'a', b'l', b'u', b'e', // field name
            0x43, // float64
        ];
        let desc = decoder
            .parse_introspection(&data)
            .expect("parsed introspection");
        assert_eq!(desc.fields.len(), 1);
        assert_eq!(desc.fields[0].name, "value");
        match desc.fields[0].field_type {
            FieldType::Scalar(TypeCode::Float64) => {}
            _ => panic!("expected float64 value field"),
        }
    }

    #[test]
    fn test_decode_string() {
        let decoder = PvdDecoder::new(false);

        // Empty string
        assert_eq!(decoder.decode_string(&[0]), Ok((String::new(), 1)));

        // "hello"
        let data = [5, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(decoder.decode_string(&data), Ok(("hello".to_string(), 6)));
    }

    #[test]
    fn decode_variant_accepts_full_with_id_type_tag() {
        let decoder = PvdDecoder::new(false);
        // Variant payload: 0xFD + int16 key + string type + "ok"
        let data = [0xFD, 0x02, 0x00, 0x60, 0x02, b'o', b'k'];
        let (value, consumed) = decoder
            .decode_value(&data, &FieldType::Variant)
            .expect("decode variant");
        assert_eq!(consumed, data.len());
        assert!(matches!(value, DecodedValue::String(ref s) if s == "ok"));
    }

    #[test]
    fn test_decode_bitset_whole_structure() {
        let decoder = PvdDecoder::new(false);
        let desc = StructureDesc {
            struct_id: None,
            fields: vec![FieldDesc {
                name: "value".to_string(),
                field_type: FieldType::Scalar(TypeCode::Float64),
            }],
        };
        // bitset_size=1, bitset=0x01 (whole structure), then float64 value.
        let mut data = Vec::new();
        data.push(0x01);
        data.push(0x01);
        data.extend_from_slice(&1.25f64.to_le_bytes());

        let (decoded, _consumed) = decoder
            .decode_structure_with_bitset(&data, &desc)
            .expect("decoded");
        if let DecodedValue::Structure(fields) = decoded {
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].0, "value");
        } else {
            panic!("expected structure");
        }
    }

    #[test]
    fn format_structure_tree_includes_nested_fields() {
        let desc = StructureDesc {
            struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
            fields: vec![
                FieldDesc {
                    name: "value".to_string(),
                    field_type: FieldType::Scalar(TypeCode::Float64),
                },
                FieldDesc {
                    name: "alarm".to_string(),
                    field_type: FieldType::Structure(StructureDesc {
                        struct_id: None,
                        fields: vec![
                            FieldDesc {
                                name: "severity".to_string(),
                                field_type: FieldType::Scalar(TypeCode::Int32),
                            },
                            FieldDesc {
                                name: "message".to_string(),
                                field_type: FieldType::String,
                            },
                        ],
                    }),
                },
            ],
        };

        let rendered = format_structure_tree(&desc);
        assert!(rendered.contains("struct epics:nt/NTScalar:1.0"));
        assert!(rendered.contains("value: double"));
        assert!(rendered.contains("alarm: structure"));
        assert!(rendered.contains("severity: int"));
        assert!(rendered.contains("message: string"));
    }

    #[test]
    fn decode_string_array_not_capped_at_100_items() {
        fn encode_size(size: usize) -> Vec<u8> {
            if size == 0 {
                return vec![0x00];
            }
            if size < 254 {
                return vec![size as u8];
            }
            let mut out = vec![0xFE];
            out.extend_from_slice(&(size as u32).to_le_bytes());
            out
        }

        let item_count = 150usize;
        let mut raw = encode_size(item_count);
        for idx in 0..item_count {
            let s = format!("PV:{}", idx);
            raw.extend_from_slice(&encode_size(s.len()));
            raw.extend_from_slice(s.as_bytes());
        }

        let decoder = PvdDecoder::new(false);
        let (decoded, _consumed) = decoder
            .decode_value(&raw, &FieldType::StringArray)
            .expect("decoded");

        let DecodedValue::Array(items) = decoded else {
            panic!("expected decoded array");
        };
        assert_eq!(items.len(), item_count);
    }

    /// Encode a PVA size prefix for `n` using the 4-byte form.
    fn size_prefix(n: u32) -> Vec<u8> {
        let mut v = vec![254u8];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }

    #[test]
    fn oversized_string_array_errors_instead_of_truncating() {
        let limits = DecodeLimits {
            max_string_array: 4,
            ..DecodeLimits::default()
        };
        let decoder = PvdDecoder::with_limits(false, limits);
        let mut data = size_prefix(9);
        for _ in 0..9 {
            data.push(1); // one-byte string, length 1
            data.push(b'x');
        }
        assert_eq!(
            decoder
                .decode_value(&data, &FieldType::StringArray)
                .unwrap_err(),
            DecodeError::ArrayTooLarge {
                kind: "string array",
                count: 9,
                limit: 4
            }
        );
    }

    #[test]
    fn count_larger_than_the_buffer_is_rejected_before_allocating() {
        let decoder = PvdDecoder::new(false);
        // Claims 4 billion int32s in a 5-byte buffer.
        let data = size_prefix(4_000_000_000);
        let err = decoder
            .decode_value(&data, &FieldType::ScalarArray(TypeCode::Int32))
            .unwrap_err();
        // The limit is checked first, so this is ArrayTooLarge, not
        // CountExceedsBuffer. See the Global Constraints.
        assert_eq!(
            err,
            DecodeError::ArrayTooLarge {
                kind: "scalar array",
                count: 4_000_000_000,
                limit: 4_000_000,
            }
        );
    }

    #[test]
    fn count_within_the_limit_but_beyond_the_buffer_is_rejected() {
        let decoder = PvdDecoder::new(false);
        // 1000 int32s = 4000 bytes claimed, 8 supplied.
        let mut data = size_prefix(1000);
        data.extend_from_slice(&[0u8; 8]);
        assert_eq!(
            decoder
                .decode_value(&data, &FieldType::ScalarArray(TypeCode::Int32))
                .unwrap_err(),
            DecodeError::CountExceedsBuffer {
                count: 1000,
                min_bytes: 4000,
                available: 8
            }
        );
    }

    #[test]
    fn oversized_struct_array_errors() {
        let limits = DecodeLimits {
            max_struct_array: 2,
            ..DecodeLimits::default()
        };
        let decoder = PvdDecoder::with_limits(false, limits);
        let mut inner = StructureDesc::new();
        inner.fields.push(FieldDesc {
            name: "a".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int8),
        });
        let mut data = size_prefix(5);
        for _ in 0..5 {
            data.push(1); // present
            data.push(0); // the int8
        }
        assert_eq!(
            decoder
                .decode_value(&data, &FieldType::StructureArray(inner))
                .unwrap_err(),
            DecodeError::ArrayTooLarge {
                kind: "structure array",
                count: 5,
                limit: 2
            }
        );
    }

    /// The regression this whole task exists for. Before the change, an
    /// over-cap string array returned a short Vec *and* left the offset
    /// short, so the following int32 decoded from the wrong bytes and the
    /// caller got a plausible, wrong answer.
    #[test]
    fn truncated_array_no_longer_desyncs_the_following_field() {
        let limits = DecodeLimits {
            max_string_array: 2,
            ..DecodeLimits::default()
        };
        let decoder = PvdDecoder::with_limits(false, limits);

        let mut desc = StructureDesc::new();
        desc.fields.push(FieldDesc {
            name: "names".to_string(),
            field_type: FieldType::StringArray,
        });
        desc.fields.push(FieldDesc {
            name: "count".to_string(),
            field_type: FieldType::Scalar(TypeCode::Int32),
        });

        let mut data = size_prefix(4);
        for _ in 0..4 {
            data.push(1);
            data.push(b'x');
        }
        data.extend_from_slice(&7i32.to_le_bytes());

        // decode_structure stops at the first field it cannot decode and
        // returns what it has, so "count" must be absent rather than wrong.
        let (value, _) = decoder.decode_structure(&data, &desc).unwrap();
        let DecodedValue::Structure(fields) = value else {
            panic!("expected a structure");
        };
        assert!(
            fields.iter().all(|(name, _)| name != "count"),
            "a desynced 'count' must not be reported: got {fields:?}"
        );

        // And with a limit that admits the array, the following field decodes
        // correctly — proving the payload above really does carry a readable
        // `count` that only the desync could have mangled.
        let loose = PvdDecoder::new(false);
        let (value, consumed) = loose.decode_structure(&data, &desc).unwrap();
        let DecodedValue::Structure(fields) = value else {
            panic!("expected a structure");
        };
        assert_eq!(consumed, data.len());
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[1].0, "count");
        assert!(
            matches!(fields[1].1, DecodedValue::Int32(7)),
            "expected count == 7, got {:?}",
            fields[1].1
        );
    }

    #[test]
    fn raising_the_limit_lets_the_same_payload_decode() {
        let mut data = size_prefix(5);
        for _ in 0..5 {
            data.push(1);
            data.push(b'x');
        }
        let strict = PvdDecoder::with_limits(
            false,
            DecodeLimits {
                max_string_array: 2,
                ..DecodeLimits::default()
            },
        );
        assert!(strict.decode_value(&data, &FieldType::StringArray).is_err());

        let loose = PvdDecoder::new(false);
        let (value, _) = loose.decode_value(&data, &FieldType::StringArray).unwrap();
        let DecodedValue::Array(items) = value else {
            panic!("expected an array");
        };
        assert_eq!(items.len(), 5);
    }

    /// Build a descriptor that nests `levels` structures inside each other.
    ///
    /// One level on the wire is: `0x80` (structure) + `0x00` (empty struct
    /// id) + `0x01` (one field) + `0x00` (empty field name), then the next
    /// level's type byte. The innermost structure is `0x80 0x00 0x00`
    /// (empty id, zero fields).
    fn nested_structure_desc(levels: usize) -> Vec<u8> {
        assert!(levels >= 1);
        let mut data = vec![0x80u8];
        for _ in 0..levels - 1 {
            data.extend_from_slice(&[0x00, 0x01, 0x00, 0x80]);
        }
        data.extend_from_slice(&[0x00, 0x00]);
        data
    }

    /// Site 6. Nothing bounded the introspection recursion, so a crafted
    /// descriptor could nest until the native stack overflowed -- an
    /// immediate process kill, not an unwind.
    ///
    /// 65 levels is deliberately shallow enough that the *pre-fix* decoder
    /// returns `Ok` rather than actually crashing the test runner: the test
    /// fails cleanly and instantly on the missing error.
    #[test]
    fn a_deeply_nested_descriptor_is_rejected_at_the_depth_limit() {
        let decoder = PvdDecoder::new(false);
        let data = nested_structure_desc(65);
        assert_eq!(data.len(), 259);
        match decoder.parse_introspection(&data) {
            Err(DecodeError::DepthExceeded { depth, limit }) => {
                assert_eq!(limit, 32);
                assert!(depth > limit, "depth {depth} should exceed limit {limit}");
            }
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }

    /// The bound must clear every real type. The deepest normative NT type
    /// nests fewer than ten levels.
    #[test]
    fn a_realistically_nested_descriptor_still_parses() {
        let decoder = PvdDecoder::new(false);
        let desc = decoder
            .parse_introspection(&nested_structure_desc(10))
            .expect("ten levels is well inside the limit");
        assert_eq!(desc.fields.len(), 1);
    }

    /// The counter is per-decoder state, and a `PvdDecoder` is reused for the
    /// whole connection. If a rejected parse left the depth counter raised,
    /// every subsequent decode on that connection would fail -- one hostile
    /// frame would poison the channel.
    #[test]
    fn a_rejected_deep_parse_does_not_poison_the_decoder() {
        let decoder = PvdDecoder::new(false);
        assert!(decoder.parse_introspection(&nested_structure_desc(65)).is_err());
        decoder
            .parse_introspection(&nested_structure_desc(3))
            .expect("the decoder must be usable again after a rejected parse");
    }

    /// The `Variant` arm of `decode_value` re-enters `parse_type_desc` on
    /// wire bytes, so the same nesting arrives through the *value* path too.
    #[test]
    fn a_deeply_nested_variant_value_is_rejected() {
        let decoder = PvdDecoder::new(false);
        let data = nested_structure_desc(65);
        match decoder.decode_value(&data, &FieldType::Variant) {
            Err(DecodeError::DepthExceeded { .. }) => {}
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }

    /// Depth of a decoded type tree, counted the way the wire recursion
    /// counts it: a leaf is 1, a structure/union is 1 + its deepest field.
    fn tree_depth(ft: &FieldType) -> usize {
        match ft {
            FieldType::Structure(d) | FieldType::StructureArray(d) => {
                1 + d.fields.iter().map(|f| tree_depth(&f.field_type)).max().unwrap_or(0)
            }
            FieldType::Union(f) | FieldType::UnionArray(f) => {
                1 + f.iter().map(|f| tree_depth(&f.field_type)).max().unwrap_or(0)
            }
            _ => 1,
        }
    }

    /// Top-level structure with `fields` fields. Field `i` is
    /// `0xFD key_i` + 25 one-field wrapper structures + `0xFE key_{i-1}`
    /// (field 0 ends in an empty structure instead). Each field stays well
    /// inside the wire depth limit, but every `0xFE` splices in the whole
    /// previous chain, so the *resolved* tree grows by ~25 levels per field.
    fn registry_chain_desc(fields: u8) -> Vec<u8> {
        let mut data = vec![0x80u8, 0x00, fields];
        for i in 0..fields {
            data.push(0x00); // empty field name
            data.push(0xFD);
            data.extend_from_slice(&u16::from(i).to_le_bytes());
            for _ in 0..25 {
                data.extend_from_slice(&[0x80, 0x00, 0x01, 0x00]);
            }
            if i == 0 {
                data.extend_from_slice(&[0x80, 0x00, 0x00]);
            } else {
                data.push(0xFE);
                data.extend_from_slice(&u16::from(i - 1).to_le_bytes());
            }
        }
        data
    }

    /// Review finding on site 6: a `0xFE` only-id reference clones a stored
    /// type with O(1) wire recursion, so chaining `0xFD`/`0xFE` built an
    /// unboundedly deep tree (and a stack overflow in the derived
    /// `Clone`/`Drop`) while the wire nesting never passed 32.
    #[test]
    fn registry_references_cannot_build_a_tree_deeper_than_the_limit() {
        let decoder = PvdDecoder::new(false);
        match decoder.parse_introspection(&registry_chain_desc(3)) {
            Err(DecodeError::DepthExceeded { depth, limit }) => {
                assert_eq!(limit, 32);
                assert!(depth > limit, "depth {depth} should exceed limit {limit}");
            }
            Ok(desc) => panic!(
                "expected DepthExceeded, got Ok with a tree {} levels deep",
                tree_depth(&FieldType::Structure(desc))
            ),
            Err(other) => panic!("expected DepthExceeded, got {other:?}"),
        }
        // The rejection must not poison the decoder either.
        decoder
            .parse_introspection(&nested_structure_desc(3))
            .expect("the decoder must be usable again after a rejected parse");
    }

    /// Legitimate registry reuse inside the limit still resolves.
    #[test]
    fn a_shallow_registry_reference_still_resolves() {
        let decoder = PvdDecoder::new(false);
        let data = [
            0x80, 0x00, 0x02, // top structure, two fields
            0x01, b'a', 0xFD, 0x07, 0x00, 0x80, 0x00, 0x01, 0x01, b'x', 0x43,
            0x01, b'b', 0xFE, 0x07, 0x00,
        ];
        let desc = decoder.parse_introspection(&data).expect("parses");
        assert_eq!(desc.fields.len(), 2);
        assert_eq!(desc.fields[0].field_type, desc.fields[1].field_type);
        assert_eq!(tree_depth(&FieldType::Structure(desc)), 3);
    }

    /// Top-level structure whose field `i` is `0xFD key_i` + a structure
    /// of four fields that all reference `key_{i-1}` with `0xFE` (field 0's
    /// four fields are scalars). Each level costs ~23 wire bytes and adds one
    /// level of height, but resolving the references multiplies the tree by
    /// four, so the decoded tree has ~4^levels nodes.
    fn fourway_chain_desc(levels: u8) -> Vec<u8> {
        let mut data = vec![0x80u8, 0x00, levels];
        for i in 0..levels {
            data.push(0x00); // empty field name
            data.push(0xFD);
            data.extend_from_slice(&u16::from(i).to_le_bytes());
            data.extend_from_slice(&[0x80, 0x00, 0x04]);
            for _ in 0..4 {
                if i == 0 {
                    data.extend_from_slice(&[0x00, 0x43]); // empty name, float64
                } else {
                    data.extend_from_slice(&[0x00, 0xFE]); // empty name, only-id
                    data.extend_from_slice(&u16::from(i - 1).to_le_bytes());
                }
            }
        }
        data
    }

    /// TRIAGE F1: `check_resolved_height` bounds depth but not breadth, so
    /// a few hundred bytes of `0xFE` references expanded to 4^n type nodes
    /// (1.41 s at n=11 in release, x4 per further 23 bytes). The per-decode
    /// node budget must reject it promptly.
    #[test]
    fn a_fourway_registry_chain_is_rejected_by_the_node_budget() {
        let decoder = PvdDecoder::new(false);
        let data = fourway_chain_desc(12);
        let start = std::time::Instant::now();
        let result = decoder.parse_introspection(&data);
        let elapsed = start.elapsed();
        match result {
            Err(DecodeError::Malformed(_)) => {}
            Err(other) => panic!("expected Malformed (node budget), got {other:?}"),
            Ok(desc) => panic!(
                "expected the node budget to reject the chain, got Ok with {} fields after {elapsed:?}",
                count_structure_fields(&desc)
            ),
        }
        // The rejection itself is microseconds; 500 ms (not the triage's
        // 50 ms) because this runs in an unoptimised debug build on loaded,
        // shared CI hosts. BASE took seconds at n=11 and is ~x4 per level,
        // so any regression blows far past this bound.
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "rejection took {elapsed:?}"
        );
        // The rejection must not poison the decoder.
        decoder
            .parse_introspection(&nested_structure_desc(3))
            .expect("the decoder must be usable again after a rejected parse");
    }

    /// The node budget is a backstop, not a ceiling on real types. Flat
    /// structures up to the field-count ceiling (a very wide NTTable) and a
    /// 7-level 4-way registry chain (~29K resolved nodes, every `0xFE`
    /// legitimately reused) still decode.
    #[test]
    fn large_legitimate_structures_fit_the_node_budget() {
        let decoder = PvdDecoder::new(false);

        let flat = |fields: u32| {
            let mut data = vec![0x80u8, 0x00, 254];
            data.extend_from_slice(&fields.to_le_bytes());
            for _ in 0..fields {
                data.extend_from_slice(&[0x00, 0x43]); // empty name, float64
            }
            data
        };
        let desc = decoder
            .parse_introspection(&flat(65_535))
            .expect("a 65,535-field structure is inside the node budget");
        assert_eq!(desc.fields.len(), 65_535);
        // Literal nodes cost wire bytes, so the budget scales past the
        // floor: the field-count ceiling (65,536) is the binding limit here.
        let desc = decoder
            .parse_introspection(&flat(65_536))
            .expect("a 65,536-field structure is at the field-count ceiling");
        assert_eq!(desc.fields.len(), 65_536);

        let desc = decoder
            .parse_introspection(&fourway_chain_desc(7))
            .expect("a 7-level 4-way chain is inside the node budget");
        let nodes = type_nodes(&FieldType::Structure(desc));
        assert!(
            (20_000..=65_536).contains(&nodes),
            "expected a near-budget tree, got {nodes} nodes"
        );
    }

    /// `0xFD key` + a structure of `fields` empty structures: `fields + 1`
    /// nodes whose value is zero bytes, so every `0xFE` reference to it is
    /// pure amplification (3 wire bytes, `fields + 1` nodes, no value bytes).
    fn register_empty_struct_type(decoder: &PvdDecoder, key: u16, fields: u32) {
        let mut data = vec![0xFDu8];
        data.extend_from_slice(&key.to_le_bytes());
        data.extend_from_slice(&[0x80, 0x00, 254]);
        data.extend_from_slice(&fields.to_le_bytes());
        for _ in 0..fields {
            data.extend_from_slice(&[0x00, 0x80, 0x00, 0x00]); // "" : struct {}
        }
        decoder.parse_introspection(&data).expect("registers the type");
    }

    /// The budget covers a whole value, not each top-level field: a
    /// structure whose many `Variant` fields each `0xFE`-reference one large
    /// stored type must not get a fresh budget per field.
    #[test]
    fn variant_fields_share_one_node_budget_per_value() {
        let decoder = PvdDecoder::new(false);
        // Key 0 is 20,001 nodes with a zero-byte value.
        register_empty_struct_type(&decoder, 0, 20_000);

        // A value of four variant fields, each typed `0xFE 0x00 0x00`:
        // 4 x 20K nodes from 12 bytes passes the ~66K budget.
        let desc = StructureDesc {
            struct_id: None,
            fields: (0..4)
                .map(|i| FieldDesc {
                    name: format!("v{i}"),
                    field_type: FieldType::Variant,
                })
                .collect(),
        };
        let value = [0xFEu8, 0x00, 0x00].repeat(4);
        match decoder.decode_structure(&value, &desc) {
            Err(DecodeError::Malformed(what)) => assert_eq!(what, NODE_BUDGET_EXCEEDED),
            other => panic!("expected the node budget to reject, got {:?}", other.map(|(_, n)| n)),
        }
        // Three references fit, and the decoder is not poisoned.
        let three = StructureDesc {
            struct_id: None,
            fields: desc.fields[..3].to_vec(),
        };
        decoder
            .decode_structure(&value[..9], &three)
            .expect("three references are inside the budget");
    }

    /// Review Important 1: a legitimate `any[]` whose node count is linear
    /// in its bytes must decode. 22,000 elements, each a literal
    /// `{double a; double b}` type (3 nodes) plus 16 value bytes, is 66K
    /// nodes from ~550 KB; an absolute 64K budget rejected it.
    #[test]
    fn a_large_any_array_of_literal_structures_decodes() {
        let decoder = PvdDecoder::new(false);
        let n: u32 = 22_000;
        let mut data = vec![254u8];
        data.extend_from_slice(&n.to_le_bytes());
        for _ in 0..n {
            data.extend_from_slice(&[0x80, 0x00, 0x02, 0x01, b'a', 0x43, 0x01, b'b', 0x43]);
            data.extend_from_slice(&1.5f64.to_le_bytes());
            data.extend_from_slice(&2.5f64.to_le_bytes());
        }
        let (value, consumed) = decoder
            .decode_value(&data, &FieldType::VariantArray)
            .expect("a linear-size any[] is not amplification");
        assert_eq!(consumed, data.len());
        let DecodedValue::Array(items) = value else {
            panic!("expected an array");
        };
        assert_eq!(items.len(), n as usize);
    }

    /// pvxs caches types and resends repeats as `0xFE`: an NTMultiChannel-like
    /// `any[]` of 3,000 elements, the first defining a 25-node structure with
    /// `0xFD` and the rest referencing it, charges ~75K nodes legitimately.
    #[test]
    fn a_large_any_array_of_cached_type_references_decodes() {
        let decoder = PvdDecoder::new(false);
        let n: u32 = 3_000;
        let mut data = vec![254u8];
        data.extend_from_slice(&n.to_le_bytes());
        for i in 0..n {
            if i == 0 {
                data.extend_from_slice(&[0xFD, 0x05, 0x00, 0x80, 0x00, 24]);
                for f in 0..24u8 {
                    data.extend_from_slice(&[0x01, b'a' + f, 0x43]);
                }
            } else {
                data.extend_from_slice(&[0xFE, 0x05, 0x00]);
            }
            data.extend(std::iter::repeat_n(0u8, 24 * 8));
        }
        let (value, consumed) = decoder
            .decode_value(&data, &FieldType::VariantArray)
            .expect("cached-type repeats carry their value bytes");
        assert_eq!(consumed, data.len());
        let DecodedValue::Array(items) = value else {
            panic!("expected an array");
        };
        assert_eq!(items.len(), n as usize);
    }

    /// The amplification the budget exists for: a stored ~65K-node type
    /// (built within budget) referenced by 1,000 consecutive `0xFE` refs in
    /// one value, ~20,000x nodes per byte, is rejected.
    #[test]
    fn repeated_references_to_a_huge_stored_type_are_rejected() {
        let decoder = PvdDecoder::new(false);
        register_empty_struct_type(&decoder, 9, 65_535);
        let n: u32 = 1_000;
        let mut data = vec![254u8];
        data.extend_from_slice(&n.to_le_bytes());
        for _ in 0..n {
            data.extend_from_slice(&[0xFE, 0x09, 0x00]);
        }
        let start = std::time::Instant::now();
        match decoder.decode_value(&data, &FieldType::VariantArray) {
            Err(DecodeError::Malformed(what)) => assert_eq!(what, NODE_BUDGET_EXCEEDED),
            other => panic!(
                "expected the node budget to reject, got {:?}",
                other.map(|(_, n)| n)
            ),
        }
        // Same debug/CI margin as the 4-way chain test.
        assert!(start.elapsed() < std::time::Duration::from_millis(500));
    }

    /// Review Minor 1: the MONITOR delta walk turned a hostile error into a
    /// silently truncated `Ok`, and the bit-0 fast path swallowed it and
    /// retried. Both must surface it.
    #[test]
    fn delta_decode_surfaces_a_node_budget_error() {
        let decoder = PvdDecoder::new(false);
        register_empty_struct_type(&decoder, 0, 40_000);
        let desc = StructureDesc {
            struct_id: None,
            fields: vec![
                FieldDesc {
                    name: "a".into(),
                    field_type: FieldType::Variant,
                },
                FieldDesc {
                    name: "b".into(),
                    field_type: FieldType::Variant,
                },
            ],
        };
        // Two refs: 80K nodes from 6 bytes passes the ~66K budget.
        let data = [0xFEu8, 0x00, 0x00].repeat(2);
        for bitset in [[0b0000_0110u8], [0b0000_0001u8]] {
            match decoder.decode_structure_with_bitset_body(&data, &desc, &bitset) {
                Err(DecodeError::Malformed(what)) => assert_eq!(what, NODE_BUDGET_EXCEEDED),
                other => panic!(
                    "bitset {bitset:02x?}: expected the node budget error, got {:?}",
                    other.map(|(_, n)| n)
                ),
            }
        }
        // One ref fits, and the walk still decodes it.
        let (value, _) = decoder
            .decode_structure_with_bitset_body(&data[..3], &desc, &[0b0000_0010])
            .expect("one reference is inside the budget");
        let DecodedValue::Structure(fields) = value else {
            panic!("expected a structure");
        };
        assert_eq!(fields.len(), 1);
    }
}
