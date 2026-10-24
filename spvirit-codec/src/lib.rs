//! PVAccess protocol encode/decode and segment reassembly.
//!
//! This crate provides the low-level PVA wire-format codec (encode + decode),
//! the PVD (pvData) structure codec, monitor delta encoding, and sans-io
//! segment reassembly. It keeps no connection state.
//!
//! Commonly used types are re-exported at the crate root for convenience.
//! The full module paths remain available for less common items.

pub mod encode_common;
pub mod epics_decode;
pub mod error;
pub mod monitor;
pub mod segment;
pub mod spvd_decode;
pub mod spvd_encode;
pub mod spvirit_encode;

// --- Re-exports: PVA wire-format decode types ---
pub use epics_decode::{PvaHeader, PvaPacket, PvaPacketCommand, PvaStatus, decode_string};

// --- Re-exports: decode errors ---
pub use error::{DecodeError, DecodeResult};

// --- Re-exports: monitor deltas ---
pub use epics_decode::DecodeMode;
pub use monitor::{MonitorLayout, MonitorUpdate};

// --- Re-exports: segmentation ---
pub use segment::{DEFAULT_MAX_MESSAGE_BYTES, SegmentOutcome, SegmentReassembler};

// --- Re-exports: PVA wire-format encode helpers ---
pub use spvirit_encode::{
    encode_control_message, encode_header, format_pva_address, ip_from_bytes, ip_to_bytes,
};

// --- Re-exports: pvData structure decode ---
pub use spvd_decode::{
    DecodeLimits, DecodedValue, FieldDesc, FieldType, PvdDecoder, StructureDesc, TypeCode,
};

// --- Re-exports: pvData structure encode ---
pub use spvd_encode::{encode_decoded_value, encode_pv_request, encode_structure_desc};

// --- Re-export the types crate for convenience ---
pub use spvirit_types;
