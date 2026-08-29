//! Python wrappers for the standalone `spvirit-codec` encoder/decoder.
//!
//! Exposes introspection types (`FieldDesc`, `StructureDesc`), packet-level
//! decoding of `pvData` buffers, and encoding helpers usable without any
//! network IO.

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use spvirit_client::put_encode::encode_put_payload as client_encode_put_payload;
use spvirit_codec::epics_decode::PvaPacket;
use spvirit_codec::spvd_decode::{
    DecodedValue, FieldDesc, FieldType, PvdDecoder, StructureDesc, TypeCode,
    extract_nt_scalar_value, format_compact_value,
};
use spvirit_codec::spvd_encode::encode_pv_request as codec_encode_pv_request;

use crate::convert::{decoded_to_py, py_to_json};
use crate::errors::{decode_msg_to_py_err, protocol_msg_to_py_err};

// ─── FieldDesc / StructureDesc wrappers ──────────────────────────────────────

/// A single field description: name and a type string (e.g. "int32",
/// "double[]", "struct", "struct[]", "union", "any", "string[5]").
#[pyclass(name = "FieldDesc", frozen, module = "spvirit.codec")]
#[derive(Clone)]
pub struct PyFieldDesc {
    inner: FieldDesc,
}

impl PyFieldDesc {
    pub(crate) fn from_inner(inner: FieldDesc) -> Self {
        Self { inner }
    }

    #[allow(dead_code)]
    pub(crate) fn inner(&self) -> &FieldDesc {
        &self.inner
    }
}

#[pymethods]
impl PyFieldDesc {
    /// Field name.
    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    /// Human-readable type string (same as `FieldType::type_name`).
    #[getter]
    fn field_type(&self) -> &'static str {
        self.inner.field_type.type_name()
    }

    /// Underlying scalar/array element type code, e.g. `"int32"`, or None
    /// for non-scalar fields.
    #[getter]
    fn type_code(&self) -> Option<&'static str> {
        match &self.inner.field_type {
            FieldType::Scalar(tc) | FieldType::ScalarArray(tc) => Some(type_code_name(*tc)),
            FieldType::String | FieldType::StringArray | FieldType::BoundedString(_) => {
                Some("string")
            }
            _ => None,
        }
    }

    /// True if this field represents any kind of array.
    #[getter]
    fn is_array(&self) -> bool {
        matches!(
            self.inner.field_type,
            FieldType::ScalarArray(_)
                | FieldType::StringArray
                | FieldType::StructureArray(_)
                | FieldType::UnionArray(_)
                | FieldType::VariantArray
        )
    }

    /// Nested `StructureDesc` when this field is a structure or structure
    /// array; otherwise None.
    #[getter]
    fn struct_desc(&self) -> Option<PyStructureDesc> {
        match &self.inner.field_type {
            FieldType::Structure(s) | FieldType::StructureArray(s) => {
                Some(PyStructureDesc::from_inner(s.clone()))
            }
            _ => None,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "FieldDesc(name={:?}, field_type={:?})",
            self.inner.name,
            self.inner.field_type.type_name()
        )
    }
}

fn type_code_name(tc: TypeCode) -> &'static str {
    match tc {
        TypeCode::Null => "null",
        TypeCode::Boolean => "boolean",
        TypeCode::Int8 => "int8",
        TypeCode::Int16 => "int16",
        TypeCode::Int32 => "int32",
        TypeCode::Int64 => "int64",
        TypeCode::UInt8 => "uint8",
        TypeCode::UInt16 => "uint16",
        TypeCode::UInt32 => "uint32",
        TypeCode::UInt64 => "uint64",
        TypeCode::Float32 => "float32",
        TypeCode::Float64 => "float64",
        TypeCode::String => "string",
        TypeCode::Variant => "any",
    }
}

/// Structure type description: optional struct ID plus ordered fields.
#[pyclass(name = "StructureDesc", frozen, module = "spvirit.codec")]
#[derive(Clone)]
pub struct PyStructureDesc {
    inner: StructureDesc,
}

impl PyStructureDesc {
    pub(crate) fn from_inner(inner: StructureDesc) -> Self {
        Self { inner }
    }

    pub(crate) fn inner(&self) -> &StructureDesc {
        &self.inner
    }
}

#[pymethods]
impl PyStructureDesc {
    /// Structure type ID (e.g. `"epics:nt/NTScalar:1.0"`), or None.
    #[getter]
    fn struct_id(&self) -> Option<&str> {
        self.inner.struct_id.as_deref()
    }

    /// Ordered list of `FieldDesc` entries for this structure.
    #[getter]
    fn fields(&self) -> Vec<PyFieldDesc> {
        self.inner
            .fields
            .iter()
            .cloned()
            .map(PyFieldDesc::from_inner)
            .collect()
    }

    /// Look up a field by name, returning None when absent.
    fn field(&self, name: &str) -> Option<PyFieldDesc> {
        self.inner.field(name).cloned().map(PyFieldDesc::from_inner)
    }

    fn __len__(&self) -> usize {
        self.inner.fields.len()
    }

    fn __contains__(&self, name: &str) -> bool {
        self.inner.field(name).is_some()
    }

    fn __repr__(&self) -> String {
        format!(
            "StructureDesc(struct_id={:?}, fields={})",
            self.inner.struct_id,
            self.inner.fields.len()
        )
    }

    /// Multi-line human-readable dump matching `Display for StructureDesc`.
    fn dump(&self) -> String {
        format!("{}", self.inner)
    }
}

// ─── Free functions ──────────────────────────────────────────────────────────

/// Decode a `StructureDesc` from raw introspection bytes (the PVD field
/// description as seen on the wire).
#[pyfunction]
#[pyo3(signature = (data, is_be=false))]
pub fn decode_introspection(data: &[u8], is_be: bool) -> PyResult<PyStructureDesc> {
    let decoder = PvdDecoder::new(is_be);
    decoder
        .parse_introspection(data)
        .map(PyStructureDesc::from_inner)
        .map_err(|_| decode_msg_to_py_err("failed to parse introspection"))
}

/// Decode a pvData value given the accompanying `StructureDesc`.
#[pyfunction]
#[pyo3(signature = (data, desc, is_be=false))]
pub fn decode_value(
    py: Python<'_>,
    data: &[u8],
    desc: &PyStructureDesc,
    is_be: bool,
) -> PyResult<PyObject> {
    let decoder = PvdDecoder::new(is_be);
    // The top-level introspection is a structure; decode it as such.
    let ft = FieldType::Structure(desc.inner().clone());
    let (decoded, _consumed) = decoder
        .decode_value(data, &ft)
        .map_err(|_| decode_msg_to_py_err("failed to decode value"))?;
    Ok(decoded_to_py(py, &decoded))
}

/// Canonical "all fields" pvRequest body.
///
/// Selecting no specific top-level fields is encoded as this fixed 6-byte
/// mask. Defined once here and reused by the codec submodule and the
/// low-level channel module (via [`build_pv_request`](crate::channel)) so the
/// literal is never duplicated.
pub(crate) const ALL_FIELDS_PV_REQUEST: [u8; 6] = [0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];

/// Encode a pvRequest mask selecting specific top-level fields.  Pass an
/// empty list or None to request all fields.
#[pyfunction]
#[pyo3(signature = (fields=None, is_be=false))]
pub fn encode_pv_request(
    py: Python<'_>,
    fields: Option<Vec<String>>,
    is_be: bool,
) -> PyResult<PyObject> {
    let fields = fields.unwrap_or_default();
    let bytes = if fields.is_empty() {
        ALL_FIELDS_PV_REQUEST.to_vec()
    } else {
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        codec_encode_pv_request(&refs, is_be)
    };
    Ok(PyBytes::new(py, &bytes).into_any().unbind())
}

/// Encode a PUT payload (size-prefixed bitset + field data) for the given
/// structure description and value dict.  The bitset is built from the
/// field keys present in `value` so callers only transmit changed fields.
#[pyfunction]
#[pyo3(signature = (desc, value, is_be=false))]
pub fn encode_put_payload(
    py: Python<'_>,
    desc: &PyStructureDesc,
    value: PyObject,
    is_be: bool,
) -> PyResult<PyObject> {
    let json = py_to_json(value.bind(py))?;
    let bytes =
        client_encode_put_payload(desc.inner(), &json, is_be).map_err(protocol_msg_to_py_err)?;
    Ok(PyBytes::new(py, &bytes).into_any().unbind())
}

/// Compact single-line representation of a decoded value.  Accepts any
/// Python object that was produced by [`decode_value`] or by this crate's
/// GET result conversion; non-NT inputs are round-tripped via their
/// string form.
#[pyfunction]
pub fn format_value(py: Python<'_>, value: PyObject) -> PyResult<String> {
    let bound = value.bind(py);
    let decoded = py_to_decoded(bound)?;
    Ok(format_compact_value(&decoded))
}

/// Extract the inner `value` field from an NT structure dict.  Returns
/// None if the input is not an NT structure.
#[pyfunction]
pub fn extract_nt_value(py: Python<'_>, value: PyObject) -> PyResult<Option<PyObject>> {
    let bound = value.bind(py);
    let decoded = py_to_decoded(bound)?;
    Ok(extract_nt_scalar_value(&decoded).map(|v| decoded_to_py(py, v)))
}

/// Best-effort inspection of a full PVA packet (header + payload).
/// Returns a dict with `command`, `command_name`, `version`, `flags`, and
/// a `details` sub-dict describing the command-specific payload.
#[pyfunction]
pub fn decode_packet<'py>(py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, PyDict>> {
    use spvirit_codec::epics_decode::command_name;

    if data.len() < 8 {
        return Err(decode_msg_to_py_err("packet shorter than 8-byte header"));
    }

    let mut pkt = PvaPacket::new(data);
    let out = PyDict::new(py);
    out.set_item("magic", pkt.header.magic)?;
    out.set_item("version", pkt.header.version)?;
    out.set_item("command", pkt.header.command)?;
    out.set_item("command_name", command_name(pkt.header.command))?;
    out.set_item("payload_length", pkt.header.payload_length)?;

    let flags = PyDict::new(py);
    flags.set_item("raw", pkt.header.flags.raw)?;
    flags.set_item("is_application", pkt.header.flags.is_application)?;
    flags.set_item("is_control", pkt.header.flags.is_control)?;
    flags.set_item("is_segmented", pkt.header.flags.is_segmented)?;
    flags.set_item("is_client", pkt.header.flags.is_client)?;
    flags.set_item("is_server", pkt.header.flags.is_server)?;
    flags.set_item("is_msb", pkt.header.flags.is_msb)?;
    out.set_item("flags", flags)?;

    out.set_item("payload", PyBytes::new(py, &data[8.min(data.len())..]))?;

    let details = PyDict::new(py);
    if let Some(cmd) = pkt.decode_payload() {
        fill_command_details(py, &cmd, &details)?;
    }
    out.set_item("details", details)?;
    Ok(out)
}

pub(crate) fn fill_command_details(
    py: Python<'_>,
    cmd: &spvirit_codec::epics_decode::PvaPacketCommand,
    details: &Bound<'_, PyDict>,
) -> PyResult<()> {
    use spvirit_codec::epics_decode::PvaPacketCommand as C;

    match cmd {
        C::Control(p) => {
            details.set_item("kind", "control")?;
            details.set_item("command", p.command)?;
            details.set_item("data", p.data)?;
        }
        C::Search(_) => {
            details.set_item("kind", "search")?;
        }
        C::SearchResponse(p) => {
            details.set_item("kind", "search_response")?;
            details.set_item("guid", PyBytes::new(py, &p.guid))?;
            details.set_item("seq", p.seq)?;
            details.set_item("port", p.port)?;
            details.set_item("protocol", &p.protocol)?;
            details.set_item("found", p.found)?;
            details.set_item("cids", PyList::new(py, p.cids.iter().cloned())?)?;
        }
        C::Beacon(_) => {
            details.set_item("kind", "beacon")?;
        }
        C::ConnectionValidation(_) => {
            details.set_item("kind", "connection_validation")?;
        }
        C::ConnectionValidated(_) => {
            details.set_item("kind", "connection_validated")?;
        }
        C::AuthNZ(_) => {
            details.set_item("kind", "authnz")?;
        }
        C::AclChange(_) => {
            details.set_item("kind", "acl_change")?;
        }
        C::Op(p) => {
            details.set_item("kind", "op")?;
            details.set_item("command", p.command)?;
            details.set_item("sid_or_cid", p.sid_or_cid)?;
            details.set_item("ioid", p.ioid)?;
            details.set_item("subcmd", p.subcmd)?;
            details.set_item("is_server", p.is_server)?;
            if let Some(status) = &p.status {
                let s = PyDict::new(py);
                s.set_item("code", status.code)?;
                s.set_item("message", status.message.as_deref())?;
                s.set_item("is_error", status.is_error())?;
                details.set_item("status", s)?;
            }
            if let Some(intro) = &p.introspection {
                details.set_item("introspection", PyStructureDesc::from_inner(intro.clone()))?;
            }
        }
        C::CreateChannel(p) => {
            details.set_item("kind", "create_channel")?;
            details.set_item("is_server", p.is_server)?;
            details.set_item("cid", p.cid)?;
            details.set_item("sid", p.sid)?;
            if !p.channels.is_empty() {
                let pairs: Vec<(u32, String)> = p.channels.clone();
                details.set_item("channels", pairs)?;
            }
            if let Some(status) = &p.status {
                let s = PyDict::new(py);
                s.set_item("code", status.code)?;
                s.set_item("message", status.message.as_deref())?;
                s.set_item("is_error", status.is_error())?;
                details.set_item("status", s)?;
            }
        }
        C::DestroyChannel(p) => {
            details.set_item("kind", "destroy_channel")?;
            details.set_item("sid", p.sid)?;
            details.set_item("cid", p.cid)?;
        }
        C::GetField(p) => {
            details.set_item("kind", "get_field")?;
            details.set_item("is_server", p.is_server)?;
            details.set_item("sid", p.sid)?;
            details.set_item("ioid", p.ioid)?;
            details.set_item("field_name", p.field_name.as_deref())?;
        }
        C::Message(_) => {
            details.set_item("kind", "message")?;
        }
        C::MultipleData(_) => {
            details.set_item("kind", "multiple_data")?;
        }
        C::CancelRequest(_) => {
            details.set_item("kind", "cancel_request")?;
        }
        C::DestroyRequest(_) => {
            details.set_item("kind", "destroy_request")?;
        }
        C::OriginTag(_) => {
            details.set_item("kind", "origin_tag")?;
        }
        C::Echo(data) => {
            details.set_item("kind", "echo")?;
            details.set_item("data", PyBytes::new(py, data))?;
        }
        C::Unknown(_) => {
            details.set_item("kind", "unknown")?;
        }
    }
    Ok(())
}

/// Convert a Python value (dict/list/scalar) back into `DecodedValue` for
/// the formatter helpers.  Used only for single-shot formatting — avoids
/// needing a round-trip through serde_json where possible.
fn py_to_decoded(obj: &Bound<'_, PyAny>) -> PyResult<DecodedValue> {
    use pyo3::types::{PyBool, PyFloat, PyInt, PyString};

    if obj.is_none() {
        return Ok(DecodedValue::Null);
    }
    if let Ok(b) = obj.downcast::<PyBool>() {
        return Ok(DecodedValue::Boolean(b.is_true()));
    }
    if obj.is_instance_of::<PyInt>() {
        let v: i64 = obj.extract()?;
        return Ok(DecodedValue::Int64(v));
    }
    if obj.is_instance_of::<PyFloat>() {
        let v: f64 = obj.extract()?;
        return Ok(DecodedValue::Float64(v));
    }
    if obj.is_instance_of::<PyString>() {
        let v: String = obj.extract()?;
        return Ok(DecodedValue::String(v));
    }
    if let Ok(b) = obj.downcast::<pyo3::types::PyBytes>() {
        return Ok(DecodedValue::Raw(b.as_bytes().to_vec()));
    }
    if let Ok(list) = obj.downcast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(py_to_decoded(&item)?);
        }
        return Ok(DecodedValue::Array(out));
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut out = Vec::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            out.push((key, py_to_decoded(&v)?));
        }
        return Ok(DecodedValue::Structure(out));
    }
    Err(decode_msg_to_py_err(format!(
        "cannot convert {} to DecodedValue",
        obj.get_type().name()?
    )))
}

/// Register the `spvirit.codec` submodule.
pub fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = parent.py();
    let m = PyModule::new(py, "codec")?;
    m.add_class::<PyFieldDesc>()?;
    m.add_class::<PyStructureDesc>()?;
    m.add_function(wrap_pyfunction!(decode_introspection, &m)?)?;
    m.add_function(wrap_pyfunction!(decode_value, &m)?)?;
    m.add_function(wrap_pyfunction!(encode_pv_request, &m)?)?;
    m.add_function(wrap_pyfunction!(encode_put_payload, &m)?)?;
    m.add_function(wrap_pyfunction!(format_value, &m)?)?;
    m.add_function(wrap_pyfunction!(extract_nt_value, &m)?)?;
    m.add_function(wrap_pyfunction!(decode_packet, &m)?)?;
    parent.add_submodule(&m)?;
    // Also expose under the dotted name so `import spvirit.codec` works.
    py.import("sys")?
        .getattr("modules")?
        .set_item("spvirit.codec", &m)?;
    Ok(())
}
