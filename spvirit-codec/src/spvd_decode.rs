//! PVD (pvData) Type Introspection and Value Decoding
//!
//! Implements parsing of PVAccess field descriptions and value decoding
//! according to the pvData serialization specification.

use std::fmt;
use tracing::debug;

use crate::error::{DecodeError, DecodeResult};

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
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_scalar_array: 4_000_000,
            max_string_array: 65_536,
            max_struct_array: 65_536,
            max_union_array: 65_536,
            max_variant_array: 65_536,
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct FieldDesc {
    pub name: String,
    pub field_type: FieldType,
}

/// Structure description with optional ID
#[derive(Debug, Clone)]
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

/// PVD Decoder state
pub struct PvdDecoder {
    is_be: bool,
    limits: DecodeLimits,
    /// IntrospectionRegistry: maps int16 keys to previously seen FieldTypes.
    /// Populated when parsing `0xFD` (full-with-id) entries, looked up on `0xFE` (only-id).
    registry: std::cell::RefCell<std::collections::HashMap<u16, FieldType>>,
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
        }
    }

    /// The array-length ceilings this decoder was built with.
    pub fn limits(&self) -> &DecodeLimits {
        &self.limits
    }

    /// Decode a size value (PVA variable-length encoding)
    pub fn decode_size(&self, data: &[u8]) -> DecodeResult<(usize, usize)> {
        if data.is_empty() {
            return Err(DecodeError::Truncated {
                needed: 1,
                available: 0,
            });
        }
        let first = data[0];
        if first == 0xFF {
            // Special: -1 (null)
            return Ok((0, 1)); // Treat as 0 for simplicity
        }
        if first < 254 {
            return Ok((first as usize, 1));
        }
        if first == 254 {
            // 4-byte size follows
            if data.len() < 5 {
                return Err(DecodeError::Truncated {
                    needed: 5,
                    available: data.len(),
                });
            }
            let size = if self.is_be {
                u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize
            } else {
                u32::from_le_bytes([data[1], data[2], data[3], data[4]]) as usize
            };
            return Ok((size, 5));
        }
        // first == 255 is null marker, handled above.
        Err(DecodeError::Malformed("invalid size prefix 255"))
    }

    /// Decode a string
    pub fn decode_string(&self, data: &[u8]) -> DecodeResult<(String, usize)> {
        let (size, size_bytes) = self.decode_size(data)?;
        if size == 0 {
            return Ok((String::new(), size_bytes));
        }
        if data.len() < size_bytes + size {
            return Err(DecodeError::Truncated {
                needed: size_bytes + size,
                available: data.len(),
            });
        }
        let s = std::str::from_utf8(&data[size_bytes..size_bytes + size])
            .map_err(|_| DecodeError::Malformed("string is not valid UTF-8"))?;
        Ok((s.to_string(), size_bytes + size))
    }

    /// Parse field description from introspection data
    pub fn parse_field_desc(&self, data: &[u8]) -> DecodeResult<(FieldDesc, usize)> {
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
        if data.is_empty() {
            return Err(DecodeError::Truncated {
                needed: 1,
                available: 0,
            });
        }

        let type_byte = data[0];
        let mut offset = 1;

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
            self.registry.borrow_mut().insert(key, field_type.clone());
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
            if let Some(ft) = self.registry.borrow().get(&key) {
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

        let mut fields = Vec::with_capacity(field_count);

        for _ in 0..field_count {
            if offset >= data.len() {
                break;
            }
            if let Ok((field, consumed)) = self.parse_field_desc(&data[offset..]) {
                offset += consumed;
                fields.push(field);
            } else {
                break;
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
            self.registry
                .borrow_mut()
                .insert(key, FieldType::Structure(desc.clone()));
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
            if let Some(ft) = self.registry.borrow().get(&key) {
                if let FieldType::Structure(desc) = ft {
                    return Ok((desc.clone(), 3));
                }
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

    /// Decode value according to field type
    pub fn decode_value(
        &self,
        data: &[u8],
        field_type: &FieldType,
    ) -> DecodeResult<(DecodedValue, usize)> {
        match field_type {
            FieldType::Scalar(tc) => self.decode_scalar(data, *tc),
            FieldType::String | FieldType::BoundedString(_) => {
                let (s, consumed) = self.decode_string(data)?;
                Ok((DecodedValue::String(s), consumed))
            }
            FieldType::ScalarArray(tc) => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                let limit = count.min(4_000_000);
                let mut values = Vec::with_capacity(limit);
                let elem_size = tc.size().unwrap_or(1);
                for _ in 0..limit {
                    if let Ok((val, consumed)) = self.decode_scalar(&data[offset..], *tc) {
                        values.push(val);
                        offset += consumed;
                    } else {
                        break;
                    }
                }
                // Skip past any remaining elements we didn't store, so the
                // stream stays aligned for the next field.
                let remaining = count.saturating_sub(limit);
                offset += remaining * elem_size;
                Ok((DecodedValue::Array(values), offset))
            }
            FieldType::StringArray => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                let max_items = count.min(4096);
                let mut values = Vec::with_capacity(max_items);
                for _ in 0..max_items {
                    if let Ok((s, consumed)) = self.decode_string(&data[offset..]) {
                        values.push(DecodedValue::String(s));
                        offset += consumed;
                    } else {
                        break;
                    }
                }
                Ok((DecodedValue::Array(values), offset))
            }
            FieldType::Structure(desc) => self.decode_structure(data, desc),
            FieldType::StructureArray(desc) => {
                let (count, size_consumed) = self.decode_size(data)?;
                let mut offset = size_consumed;
                let mut values = Vec::with_capacity(count.min(256));
                for _ in 0..count.min(256) {
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
                let mut values = Vec::with_capacity(count.min(128));
                for _ in 0..count.min(128) {
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
                let mut values = Vec::with_capacity(count.min(128));
                for _ in 0..count.min(128) {
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
        let mut offset = 0;
        let mut fields: Vec<(String, DecodedValue)> = Vec::new();

        for field in &desc.fields {
            if offset >= data.len() {
                break;
            }
            if let Ok((value, consumed)) = self.decode_value(&data[offset..], &field.field_type) {
                fields.push((field.name.clone(), value));
                offset += consumed;
            } else {
                // Can't decode this field, stop
                break;
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

    /// Decode a structure with changed and overrun bitsets (MONITOR updates)
    pub fn decode_structure_with_bitset_and_overrun(
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
        let mut offset = 0usize;
        let (changed_size, consumed1) = self.decode_size(&data[offset..])?;
        offset += consumed1;
        if offset + changed_size > data.len() {
            return Err(DecodeError::Truncated {
                needed: offset + changed_size,
                available: data.len(),
            });
        }
        let changed = &data[offset..offset + changed_size];
        offset += changed_size;

        let (overrun_size, consumed2) = self.decode_size(&data[offset..])?;
        offset += consumed2;
        if offset + overrun_size > data.len() {
            return Err(DecodeError::Truncated {
                needed: offset + overrun_size,
                available: data.len(),
            });
        }
        offset += overrun_size;

        let (value, consumed) =
            self.decode_structure_with_bitset_body(&data[offset..], desc, changed)?;
        Ok((value, offset + consumed))
    }

    /// Decode a structure with changed bitset, data, then overrun bitset (spec order)
    pub fn decode_structure_with_bitset_then_overrun(
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
        let mut offset = 0usize;
        let (changed_size, consumed1) = self.decode_size(&data[offset..])?;
        offset += consumed1;
        if offset + changed_size > data.len() {
            return Err(DecodeError::Truncated {
                needed: offset + changed_size,
                available: data.len(),
            });
        }
        let changed = &data[offset..offset + changed_size];
        offset += changed_size;

        let (value, consumed) =
            self.decode_structure_with_bitset_body(&data[offset..], desc, changed)?;
        offset += consumed;

        let (overrun_size, consumed2) = self.decode_size(&data[offset..])?;
        offset += consumed2;
        if offset + overrun_size > data.len() {
            return Err(DecodeError::Truncated {
                needed: offset + overrun_size,
                available: data.len(),
            });
        }
        offset += overrun_size;

        Ok((value, offset))
    }

    pub(crate) fn decode_structure_with_bitset_body(
        &self,
        data: &[u8],
        desc: &StructureDesc,
        bitset: &[u8],
    ) -> DecodeResult<(DecodedValue, usize)> {
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
            if let Ok((value, consumed)) = self.decode_structure(data, desc) {
                return Ok((value, consumed));
            }
        }

        let mut fields: Vec<(String, DecodedValue)> = Vec::new();
        let mut offset = 0usize;

        fn decode_with_bitset_recursive(
            decoder: &PvdDecoder,
            data: &[u8],
            offset: &mut usize,
            desc: &StructureDesc,
            bitset: &[u8],
            bit_offset: &mut usize,
            fields: &mut Vec<(String, DecodedValue)>,
        ) -> bool {
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
                    let child_field_count = count_structure_fields(nested_desc);

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
                            if let Ok((value, consumed)) =
                                decoder.decode_structure(&data[*offset..], nested_desc)
                            {
                                debug!(
                                    "Decoded full nested structure '{}', consumed {} bytes",
                                    field.name, consumed
                                );
                                fields.push((field.name.clone(), value));
                                *offset += consumed;
                            } else {
                                debug!("Failed to decode full nested structure '{}'", field.name);
                                return false;
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
                            &mut nested_fields,
                        ) {
                            return false;
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
                        return false;
                    }
                    if let Ok((value, consumed)) =
                        decoder.decode_value(&data[*offset..], &field.field_type)
                    {
                        fields.push((field.name.clone(), value));
                        *offset += consumed;
                    } else {
                        return false;
                    }
                }
            }
            true
        }

        let mut bit_offset = 1;
        decode_with_bitset_recursive(
            self,
            data,
            &mut offset,
            desc,
            bitset,
            &mut bit_offset,
            &mut fields,
        );
        Ok((DecodedValue::Structure(fields), offset))
    }
}

/// Count total fields in a structure (including nested)
fn count_structure_fields(desc: &StructureDesc) -> usize {
    let mut count = 0;
    for field in &desc.fields {
        count += 1;
        if let FieldType::Structure(nested) = &field.field_type {
            count += count_structure_fields(nested);
        }
    }
    count
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
                    if let Some((_, DecodedValue::Int32(s))) = severity {
                        if *s != 0 {
                            parts.push(format!("sev={}", s));
                        }
                    }
                    if let Some((_, DecodedValue::String(m))) = message {
                        if !m.is_empty() {
                            parts.push(format!("\"{}\"", m));
                        }
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
                let items: Vec<String> = arr.iter().map(|v| format_scalar_value(v)).collect();
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
}
