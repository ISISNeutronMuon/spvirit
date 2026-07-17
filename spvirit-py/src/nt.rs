//! Python wrappers for Normative Type structs.

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use spvirit_types::{
    NtAlarm, NtControl, NtDisplay, NtNdArray, NtPayload, NtScalar, NtScalarArray, NtTable,
    NtTimeStamp, PvValue,
};

use crate::convert::{
    py_to_scalar_array_maybe_typed, py_to_scalar_maybe_typed, scalar_array_to_py,
    scalar_array_type_code, scalar_to_py, scalar_value_type_code, wire_type_name,
};

// ─── PyAlarm ─────────────────────────────────────────────────────────────────

/// Alarm substructure: severity (0=NO_ALARM, 1=MINOR, 2=MAJOR, 3=INVALID),
/// status, message. Read-only properties.
#[pyclass(name = "Alarm")]
#[derive(Clone)]
pub struct PyAlarm {
    /// Alarm severity: 0=NO_ALARM, 1=MINOR, 2=MAJOR, 3=INVALID.
    #[pyo3(get)]
    pub severity: i32,
    /// Alarm status code.
    #[pyo3(get)]
    pub status: i32,
    /// Alarm message text.
    #[pyo3(get)]
    pub message: String,
}

impl From<&NtAlarm> for PyAlarm {
    fn from(a: &NtAlarm) -> Self {
        Self {
            severity: a.severity,
            status: a.status,
            message: a.message.clone(),
        }
    }
}

#[pymethods]
impl PyAlarm {
    #[new]
    #[pyo3(signature = (severity=0, status=0, message=String::new()))]
    fn py_new(severity: i32, status: i32, message: String) -> Self {
        Self {
            severity,
            status,
            message,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Alarm(severity={}, status={}, message={:?})",
            self.severity, self.status, self.message
        )
    }
}

// ─── PyTimeStamp ─────────────────────────────────────────────────────────────

/// Time stamp substructure: seconds_past_epoch, nanoseconds, user_tag.
#[pyclass(name = "TimeStamp")]
#[derive(Clone)]
pub struct PyTimeStamp {
    /// Seconds since the Unix epoch.
    #[pyo3(get)]
    pub seconds_past_epoch: i64,
    /// Nanoseconds part of the time stamp.
    #[pyo3(get)]
    pub nanoseconds: i32,
    /// Application-defined user tag.
    #[pyo3(get)]
    pub user_tag: i32,
}

impl From<&NtTimeStamp> for PyTimeStamp {
    fn from(ts: &NtTimeStamp) -> Self {
        Self {
            seconds_past_epoch: ts.seconds_past_epoch,
            nanoseconds: ts.nanoseconds,
            user_tag: ts.user_tag,
        }
    }
}

#[pymethods]
impl PyTimeStamp {
    #[new]
    #[pyo3(signature = (seconds_past_epoch=0, nanoseconds=0, user_tag=0))]
    fn py_new(seconds_past_epoch: i64, nanoseconds: i32, user_tag: i32) -> Self {
        Self {
            seconds_past_epoch,
            nanoseconds,
            user_tag,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "TimeStamp(seconds={}, ns={})",
            self.seconds_past_epoch, self.nanoseconds
        )
    }
}

// ─── PyDisplay ───────────────────────────────────────────────────────────────

/// Display metadata substructure: limits, description, units, precision.
#[pyclass(name = "Display")]
#[derive(Clone)]
pub struct PyDisplay {
    /// Lower display limit.
    #[pyo3(get)]
    pub limit_low: f64,
    /// Upper display limit.
    #[pyo3(get)]
    pub limit_high: f64,
    /// Human-readable description.
    #[pyo3(get)]
    pub description: String,
    /// Engineering units string.
    #[pyo3(get)]
    pub units: String,
    /// Display precision (decimal places).
    #[pyo3(get)]
    pub precision: i32,
}

impl From<&NtDisplay> for PyDisplay {
    fn from(d: &NtDisplay) -> Self {
        Self {
            limit_low: d.limit_low,
            limit_high: d.limit_high,
            description: d.description.clone(),
            units: d.units.clone(),
            precision: d.precision,
        }
    }
}

#[pymethods]
impl PyDisplay {
    #[new]
    #[pyo3(signature = (limit_low=0.0, limit_high=0.0, description=String::new(), units=String::new(), precision=0))]
    fn py_new(
        limit_low: f64,
        limit_high: f64,
        description: String,
        units: String,
        precision: i32,
    ) -> Self {
        Self {
            limit_low,
            limit_high,
            description,
            units,
            precision,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Display(low={}, high={}, units={:?})",
            self.limit_low, self.limit_high, self.units
        )
    }
}

// ─── PyControl ───────────────────────────────────────────────────────────────

/// Control metadata substructure: write limits and minimum step.
#[pyclass(name = "Control")]
#[derive(Clone)]
pub struct PyControl {
    /// Lower control (write) limit.
    #[pyo3(get)]
    pub limit_low: f64,
    /// Upper control (write) limit.
    #[pyo3(get)]
    pub limit_high: f64,
    /// Minimum step between accepted values.
    #[pyo3(get)]
    pub min_step: f64,
}

impl From<&NtControl> for PyControl {
    fn from(c: &NtControl) -> Self {
        Self {
            limit_low: c.limit_low,
            limit_high: c.limit_high,
            min_step: c.min_step,
        }
    }
}

#[pymethods]
impl PyControl {
    #[new]
    #[pyo3(signature = (limit_low=0.0, limit_high=0.0, min_step=0.0))]
    fn py_new(limit_low: f64, limit_high: f64, min_step: f64) -> Self {
        Self {
            limit_low,
            limit_high,
            min_step,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Control(low={}, high={}, min_step={})",
            self.limit_low, self.limit_high, self.min_step
        )
    }
}

// ─── PyNtScalar ──────────────────────────────────────────────────────────────

/// Full-fidelity NTScalar payload: value plus alarm, display, and control
/// metadata. Used with `Store.get_nt`/`Store.put_nt`, Python sources, and
/// `Notifier.notify`.
#[pyclass(name = "NtScalar")]
pub struct PyNtScalar {
    inner: NtScalar,
}

impl PyNtScalar {
    pub fn new(inner: NtScalar) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyNtScalar {
    /// Create an NtScalar from a Python value with optional metadata.
    #[new]
    #[pyo3(signature = (value, units=String::new(), display_low=0.0, display_high=0.0, display_description=String::new(), display_precision=0, control_low=0.0, control_high=0.0, control_min_step=0.0, alarm_severity=0, alarm_status=0, alarm_message=String::new(), r#type=None))]
    #[allow(clippy::too_many_arguments)]
    fn py_new(
        value: &Bound<'_, PyAny>,
        units: String,
        display_low: f64,
        display_high: f64,
        display_description: String,
        display_precision: i32,
        control_low: f64,
        control_high: f64,
        control_min_step: f64,
        alarm_severity: i32,
        alarm_status: i32,
        alarm_message: String,
        r#type: Option<String>,
    ) -> PyResult<Self> {
        let sv = py_to_scalar_maybe_typed(value, r#type.as_deref())?;
        let mut nt = NtScalar::from_value(sv);
        nt.units = units;
        nt.display_low = display_low;
        nt.display_high = display_high;
        nt.display_description = display_description;
        nt.display_precision = display_precision;
        nt.control_low = control_low;
        nt.control_high = control_high;
        nt.control_min_step = control_min_step;
        nt.alarm_severity = alarm_severity;
        nt.alarm_status = alarm_status;
        nt.alarm_message = alarm_message;
        Ok(Self { inner: nt })
    }

    /// Scalar value as a Python object.
    #[getter]
    fn value(&self, py: Python<'_>) -> PyObject {
        scalar_to_py(py, &self.inner.value)
    }

    /// Canonical wire value-type name, e.g. "double", "ushort", "string".
    #[getter]
    fn value_type(&self) -> &'static str {
        wire_type_name(scalar_value_type_code(&self.inner.value))
    }

    /// Alarm severity: 0=NO_ALARM, 1=MINOR, 2=MAJOR, 3=INVALID.
    #[getter]
    fn alarm_severity(&self) -> i32 {
        self.inner.alarm_severity
    }

    /// Alarm status code.
    #[getter]
    fn alarm_status(&self) -> i32 {
        self.inner.alarm_status
    }

    /// Alarm message text.
    #[getter]
    fn alarm_message(&self) -> &str {
        &self.inner.alarm_message
    }

    /// Engineering units string.
    #[getter]
    fn units(&self) -> &str {
        &self.inner.units
    }

    /// Lower display limit.
    #[getter]
    fn display_low(&self) -> f64 {
        self.inner.display_low
    }

    /// Upper display limit.
    #[getter]
    fn display_high(&self) -> f64 {
        self.inner.display_high
    }

    /// Display description text.
    #[getter]
    fn display_description(&self) -> &str {
        &self.inner.display_description
    }

    /// Display precision (decimal places).
    #[getter]
    fn display_precision(&self) -> i32 {
        self.inner.display_precision
    }

    /// Lower control (write) limit.
    #[getter]
    fn control_low(&self) -> f64 {
        self.inner.control_low
    }

    /// Upper control (write) limit.
    #[getter]
    fn control_high(&self) -> f64 {
        self.inner.control_high
    }

    /// Minimum step between accepted values.
    #[getter]
    fn control_min_step(&self) -> f64 {
        self.inner.control_min_step
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let val = scalar_to_py(py, &self.inner.value);
        format!("NtScalar(value={}, units={:?})", val, self.inner.units)
    }
}

// ─── PyNtScalarArray ─────────────────────────────────────────────────────────

/// Full-fidelity NTScalarArray payload: array value plus alarm, time stamp,
/// display, and control metadata.
#[pyclass(name = "NtScalarArray")]
pub struct PyNtScalarArray {
    inner: NtScalarArray,
}

impl PyNtScalarArray {
    pub fn new(inner: NtScalarArray) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyNtScalarArray {
    /// Create an NtScalarArray from a Python list/bytes.
    #[new]
    #[pyo3(signature = (value, r#type=None))]
    fn py_new(value: &Bound<'_, PyAny>, r#type: Option<String>) -> PyResult<Self> {
        let arr = py_to_scalar_array_maybe_typed(value, r#type.as_deref())?;
        Ok(Self {
            inner: NtScalarArray::from_value(arr),
        })
    }

    /// Array value as a Python list (or bytes for byte arrays).
    #[getter]
    fn value(&self, py: Python<'_>) -> PyObject {
        scalar_array_to_py(py, &self.inner.value)
    }

    /// Canonical wire element-type name, e.g. "double", "ushort".
    #[getter]
    fn value_type(&self) -> &'static str {
        wire_type_name(scalar_array_type_code(&self.inner.value))
    }

    /// Alarm substructure.
    #[getter]
    fn alarm(&self) -> PyAlarm {
        PyAlarm::from(&self.inner.alarm)
    }

    /// Time stamp substructure.
    #[getter]
    fn time_stamp(&self) -> PyTimeStamp {
        PyTimeStamp::from(&self.inner.time_stamp)
    }

    /// Display metadata substructure.
    #[getter]
    fn display(&self) -> PyDisplay {
        PyDisplay::from(&self.inner.display)
    }

    /// Control metadata substructure.
    #[getter]
    fn control(&self) -> PyControl {
        PyControl::from(&self.inner.control)
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let val = scalar_array_to_py(py, &self.inner.value);
        format!("NtScalarArray(value={})", val)
    }
}

// ─── PyNtTable ───────────────────────────────────────────────────────────────

/// NTTable payload: column labels, columns, and optional metadata. Has no
/// Python constructor — returned by reads such as `Store.get_nt`.
#[pyclass(name = "NtTable")]
pub struct PyNtTable {
    inner: NtTable,
}

impl PyNtTable {
    pub fn new(inner: NtTable) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyNtTable {
    /// Column labels as a list of strings.
    #[getter]
    fn labels(&self, py: Python<'_>) -> PyResult<PyObject> {
        let items: Vec<PyObject> = self
            .inner
            .labels
            .iter()
            .map(|s| pyo3::types::PyString::new(py, s).into_any().unbind())
            .collect();
        Ok(PyList::new(py, &items)?.into_any().unbind())
    }

    /// Return columns as a dict of {name: list}.
    fn columns(&self, py: Python<'_>) -> PyResult<PyObject> {
        let dict = pyo3::types::PyDict::new(py);
        for col in &self.inner.columns {
            dict.set_item(&col.name, scalar_array_to_py(py, &col.values))?;
        }
        Ok(dict.into_any().unbind())
    }

    /// Optional descriptor string, or None.
    #[getter]
    fn descriptor(&self) -> Option<&str> {
        self.inner.descriptor.as_deref()
    }

    /// Optional alarm substructure, or None.
    #[getter]
    fn alarm(&self) -> Option<PyAlarm> {
        self.inner.alarm.as_ref().map(PyAlarm::from)
    }

    /// Optional time stamp substructure, or None.
    #[getter]
    fn time_stamp(&self) -> Option<PyTimeStamp> {
        self.inner.time_stamp.as_ref().map(PyTimeStamp::from)
    }

    fn __repr__(&self) -> String {
        format!(
            "NtTable(labels={:?}, columns={})",
            self.inner.labels,
            self.inner.columns.len()
        )
    }
}

// ─── PyNtNdArray ─────────────────────────────────────────────────────────────

/// NTNDArray payload: flat array data plus dimensions and image metadata.
/// Has no Python constructor — returned by reads such as `Store.get_nt`.
#[pyclass(name = "NtNdArray")]
pub struct PyNtNdArray {
    inner: NtNdArray,
}

impl PyNtNdArray {
    pub fn new(inner: NtNdArray) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyNtNdArray {
    /// Flat array data as a Python list (or bytes for byte arrays).
    #[getter]
    fn value(&self, py: Python<'_>) -> PyObject {
        scalar_array_to_py(py, &self.inner.value)
    }

    /// Unique frame identifier.
    #[getter]
    fn unique_id(&self) -> i32 {
        self.inner.unique_id
    }

    /// Compressed data size in bytes.
    #[getter]
    fn compressed_size(&self) -> i64 {
        self.inner.compressed_size
    }

    /// Uncompressed data size in bytes.
    #[getter]
    fn uncompressed_size(&self) -> i64 {
        self.inner.uncompressed_size
    }

    /// Return dimensions as a list of dicts, one per dimension, with keys
    /// `size`, `offset`, `full_size`, `binning`, and `reverse`.
    fn dimensions(&self, py: Python<'_>) -> PyResult<PyObject> {
        let items: Vec<PyObject> = self
            .inner
            .dimension
            .iter()
            .map(|d| {
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("size", d.size).expect("set");
                dict.set_item("offset", d.offset).expect("set");
                dict.set_item("full_size", d.full_size).expect("set");
                dict.set_item("binning", d.binning).expect("set");
                dict.set_item("reverse", d.reverse).expect("set");
                dict.into_any().unbind()
            })
            .collect();
        Ok(PyList::new(py, &items)?.into_any().unbind())
    }

    /// Time stamp of the data acquisition.
    #[getter]
    fn data_time_stamp(&self) -> PyTimeStamp {
        PyTimeStamp::from(&self.inner.data_time_stamp)
    }

    fn __repr__(&self) -> String {
        let dims: Vec<i32> = self.inner.dimension.iter().map(|d| d.size).collect();
        format!(
            "NtNdArray(unique_id={}, dims={:?})",
            self.inner.unique_id, dims
        )
    }
}

// ─── NtPayload → Python wrapper ──────────────────────────────────────────────

/// Extract the Rust `NtPayload` from a Python NT object.
pub fn py_to_nt_payload(obj: &Bound<'_, PyAny>) -> PyResult<NtPayload> {
    if let Ok(s) = obj.downcast::<PyNtScalar>() {
        Ok(NtPayload::Scalar(s.borrow().inner.clone()))
    } else if let Ok(a) = obj.downcast::<PyNtScalarArray>() {
        Ok(NtPayload::ScalarArray(a.borrow().inner.clone()))
    } else if let Ok(t) = obj.downcast::<PyNtTable>() {
        Ok(NtPayload::Table(t.borrow().inner.clone()))
    } else if let Ok(n) = obj.downcast::<PyNtNdArray>() {
        Ok(NtPayload::NdArray(n.borrow().inner.clone()))
    } else {
        Err(pyo3::exceptions::PyTypeError::new_err(
            "expected NtScalar, NtScalarArray, NtTable, or NtNdArray",
        ))
    }
}

pub fn nt_payload_to_py(py: Python<'_>, payload: NtPayload) -> PyObject {
    match payload {
        NtPayload::Scalar(s) => PyNtScalar::new(s)
            .into_pyobject(py)
            .expect("NtScalar")
            .into_any()
            .unbind(),
        NtPayload::ScalarArray(a) => PyNtScalarArray::new(a)
            .into_pyobject(py)
            .expect("NtScalarArray")
            .into_any()
            .unbind(),
        NtPayload::Table(t) => PyNtTable::new(t)
            .into_pyobject(py)
            .expect("NtTable")
            .into_any()
            .unbind(),
        NtPayload::NdArray(n) => PyNtNdArray::new(n)
            .into_pyobject(py)
            .expect("NtNdArray")
            .into_any()
            .unbind(),
        NtPayload::Enum(e) => {
            let d = PyDict::new(py);
            d.set_item("index", e.index).ok();
            d.set_item("choices", &e.choices).ok();
            d.set_item("selected", e.selected().unwrap_or("")).ok();
            d.unbind().into_any()
        }
        NtPayload::Generic { struct_id, fields } => {
            let d = PyDict::new(py);
            d.set_item("struct_id", &struct_id).ok();
            for (name, val) in &fields {
                d.set_item(name, pvvalue_to_py(py, val)).ok();
            }
            d.unbind().into_any()
        }
    }
}

fn pvvalue_to_py(py: Python<'_>, val: &PvValue) -> PyObject {
    match val {
        PvValue::Scalar(s) => scalar_to_py(py, s),
        PvValue::ScalarArray(a) => scalar_array_to_py(py, a),
        PvValue::Structure { struct_id, fields } => {
            let d = PyDict::new(py);
            d.set_item("struct_id", struct_id).ok();
            for (name, v) in fields {
                d.set_item(name, pvvalue_to_py(py, v)).ok();
            }
            d.unbind().into_any()
        }
    }
}
