//! Typed PV handles for Python — wraps `spvirit_server::pv::Pv<T>`.

use std::sync::Mutex;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyKeyError, PyRuntimeError, PyTypeError};
use pyo3::prelude::*;

use spvirit_codec::spvd_decode::TypeCode;
use spvirit_server::pv::{AnyPv, Pv, PvArray, PvError};
use spvirit_types::ScalarValue;

use crate::convert::{
    parse_scalar_type, py_to_scalar_array, py_to_scalar_typed, scalar_array_to_py, scalar_to_py,
    wire_type_name,
};
use crate::runtime::{block_on_py, future_into_py};

pub(crate) fn pv_err(e: PvError) -> PyErr {
    match e {
        PvError::Unbound => PyRuntimeError::new_err(e.to_string()),
        PvError::NotFound(_) => PyKeyError::new_err(e.to_string()),
        PvError::TypeMismatch { .. } => PyTypeError::new_err(e.to_string()),
        PvError::PutRejected(_) => crate::errors::PutRejectedError::new_err(e.to_string()),
    }
}

#[derive(Clone)]
pub(crate) enum PvKind {
    F64(Pv<f64>),
    Bool(Pv<bool>),
    I32(Pv<i32>),
    Str(Pv<String>),
    /// Array handle plus its element `TypeCode`, captured at construction.
    /// A record's element type is fixed when the record is created and never
    /// changes, so carrying it here lets `set`/`set_async` coerce Python
    /// values strictly without a GET round-trip to re-learn the type.
    Array(PvArray, TypeCode),
    /// Dynamically typed scalar — covers all twelve NTScalar wire types.
    /// The TypeCode is the record's wire type; Python values are strictly
    /// coerced against it at the boundary.
    Typed(Pv<ScalarValue>, TypeCode),
}

/// Typed handle to a PV record. Create with `spvirit.ai(...)`, `spvirit.ao(...)`,
/// etc.; serve with `spvirit.Server(pvs=[...])`; then `set()`/`get()` freely.
#[pyclass(name = "Pv")]
#[derive(Clone)]
pub struct PyPv {
    pub(crate) kind: PvKind,
}

impl PyPv {
    pub(crate) fn any(&self) -> AnyPv {
        match &self.kind {
            PvKind::F64(p) => AnyPv::from(p.clone()),
            PvKind::Bool(p) => AnyPv::from(p.clone()),
            PvKind::I32(p) => AnyPv::from(p.clone()),
            PvKind::Str(p) => AnyPv::from(p.clone()),
            PvKind::Array(a, _) => AnyPv::from(a.clone()),
            PvKind::Typed(p, _) => AnyPv::from(p.clone()),
        }
    }
}

#[pymethods]
impl PyPv {
    /// PV name.
    #[getter]
    fn name(&self) -> &str {
        match &self.kind {
            PvKind::F64(p) => p.name(),
            PvKind::Bool(p) => p.name(),
            PvKind::I32(p) => p.name(),
            PvKind::Str(p) => p.name(),
            PvKind::Array(p, _) => p.name(),
            PvKind::Typed(p, _) => p.name(),
        }
    }

    fn __repr__(&self) -> String {
        let ty: String = match &self.kind {
            PvKind::F64(_) => "float".into(),
            PvKind::Bool(_) => "bool".into(),
            PvKind::I32(_) => "int".into(),
            PvKind::Str(_) => "str".into(),
            PvKind::Array(..) => "array".into(),
            PvKind::Typed(_, code) => wire_type_name(*code).into(),
        };
        format!("<spvirit.Pv '{}' ({ty})>", self.name())
    }

    /// Write a value through the full posting pipeline (blocking, GIL released).
    fn set(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<()> {
        match &self.kind {
            PvKind::F64(p) => {
                let v: f64 = value.extract()?;
                block_on_py(py, p.set(v)).map_err(pv_err)
            }
            PvKind::Bool(p) => {
                let v: bool = value.extract()?;
                block_on_py(py, p.set(v)).map_err(pv_err)
            }
            PvKind::I32(p) => {
                let v: i32 = value.extract()?;
                block_on_py(py, p.set(v)).map_err(pv_err)
            }
            PvKind::Str(p) => {
                let v: String = value.extract()?;
                block_on_py(py, p.set(v)).map_err(pv_err)
            }
            PvKind::Array(p, code) => {
                // Coerce strictly to the record's element type, captured at
                // construction. No GET round-trip: the element type is fixed
                // when the record is created and cannot change.
                let v = crate::convert::py_to_scalar_array_typed(value, *code)?;
                block_on_py(py, p.set(v)).map_err(pv_err)
            }
            PvKind::Typed(p, code) => {
                let v = py_to_scalar_typed(value, *code)?;
                block_on_py(py, p.set(v)).map_err(pv_err)
            }
        }
    }

    /// Read the current value, typed (blocking, GIL released).
    fn get(&self, py: Python<'_>) -> PyResult<PyObject> {
        match &self.kind {
            PvKind::F64(p) => {
                let v = block_on_py(py, p.get()).map_err(pv_err)?;
                v.into_py_any(py)
            }
            PvKind::Bool(p) => {
                let v = block_on_py(py, p.get()).map_err(pv_err)?;
                v.into_py_any(py)
            }
            PvKind::I32(p) => {
                let v = block_on_py(py, p.get()).map_err(pv_err)?;
                v.into_py_any(py)
            }
            PvKind::Str(p) => {
                let v = block_on_py(py, p.get()).map_err(pv_err)?;
                v.into_py_any(py)
            }
            PvKind::Array(p, _) => {
                let v = block_on_py(py, p.get()).map_err(pv_err)?;
                Ok(scalar_array_to_py(py, &v))
            }
            PvKind::Typed(p, _) => {
                let v = block_on_py(py, p.get()).map_err(pv_err)?;
                Ok(scalar_to_py(py, &v))
            }
        }
    }

    /// Write a value through the full posting pipeline (async variant of `set`).
    fn set_async<'py>(
        &self,
        py: Python<'py>,
        value: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        match &self.kind {
            PvKind::F64(p) => {
                let v: f64 = value.extract()?;
                let handle = p.clone();
                future_into_py(py, async move {
                    handle.set(v).await.map_err(pv_err)?;
                    Python::with_gil(|py| py.None().into_py_any(py))
                })
            }
            PvKind::Bool(p) => {
                let v: bool = value.extract()?;
                let handle = p.clone();
                future_into_py(py, async move {
                    handle.set(v).await.map_err(pv_err)?;
                    Python::with_gil(|py| py.None().into_py_any(py))
                })
            }
            PvKind::I32(p) => {
                let v: i32 = value.extract()?;
                let handle = p.clone();
                future_into_py(py, async move {
                    handle.set(v).await.map_err(pv_err)?;
                    Python::with_gil(|py| py.None().into_py_any(py))
                })
            }
            PvKind::Str(p) => {
                let v: String = value.extract()?;
                let handle = p.clone();
                future_into_py(py, async move {
                    handle.set(v).await.map_err(pv_err)?;
                    Python::with_gil(|py| py.None().into_py_any(py))
                })
            }
            PvKind::Array(p, code) => {
                // Coerce against the construction-time element type — no GET.
                let v = crate::convert::py_to_scalar_array_typed(value, *code)?;
                let handle = p.clone();
                future_into_py(py, async move {
                    handle.set(v).await.map_err(pv_err)?;
                    Python::with_gil(|py| py.None().into_py_any(py))
                })
            }
            PvKind::Typed(p, code) => {
                let v = py_to_scalar_typed(value, *code)?;
                let handle = p.clone();
                future_into_py(py, async move {
                    handle.set(v).await.map_err(pv_err)?;
                    Python::with_gil(|py| py.None().into_py_any(py))
                })
            }
        }
    }

    /// Read the current value, typed (async variant of `get`).
    fn get_async<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.kind {
            PvKind::F64(p) => {
                let handle = p.clone();
                future_into_py(py, async move {
                    let v = handle.get().await.map_err(pv_err)?;
                    Python::with_gil(|py| v.into_py_any(py))
                })
            }
            PvKind::Bool(p) => {
                let handle = p.clone();
                future_into_py(py, async move {
                    let v = handle.get().await.map_err(pv_err)?;
                    Python::with_gil(|py| v.into_py_any(py))
                })
            }
            PvKind::I32(p) => {
                let handle = p.clone();
                future_into_py(py, async move {
                    let v = handle.get().await.map_err(pv_err)?;
                    Python::with_gil(|py| v.into_py_any(py))
                })
            }
            PvKind::Str(p) => {
                let handle = p.clone();
                future_into_py(py, async move {
                    let v = handle.get().await.map_err(pv_err)?;
                    Python::with_gil(|py| v.into_py_any(py))
                })
            }
            PvKind::Array(p, _) => {
                let handle = p.clone();
                future_into_py(py, async move {
                    let v = handle.get().await.map_err(pv_err)?;
                    Ok(Python::with_gil(|py| scalar_array_to_py(py, &v)))
                })
            }
            PvKind::Typed(p, _) => {
                let handle = p.clone();
                future_into_py(py, async move {
                    let v = handle.get().await.map_err(pv_err)?;
                    Ok(Python::with_gil(|py| scalar_to_py(py, &v)))
                })
            }
        }
    }

    /// Explicitly set the record's alarm severity/status/message, independent
    /// of its value.
    #[pyo3(signature = (severity, status, message=""))]
    fn set_alarm(&self, py: Python<'_>, severity: i32, status: i32, message: &str) -> PyResult<()> {
        match &self.kind {
            PvKind::F64(p) => {
                block_on_py(py, p.set_alarm(severity, status, message)).map_err(pv_err)
            }
            PvKind::Bool(p) => {
                block_on_py(py, p.set_alarm(severity, status, message)).map_err(pv_err)
            }
            PvKind::I32(p) => {
                block_on_py(py, p.set_alarm(severity, status, message)).map_err(pv_err)
            }
            PvKind::Str(p) => {
                block_on_py(py, p.set_alarm(severity, status, message)).map_err(pv_err)
            }
            PvKind::Array(p, _) => {
                block_on_py(py, p.set_alarm(severity, status, message)).map_err(pv_err)
            }
            PvKind::Typed(p, _) => {
                block_on_py(py, p.set_alarm(severity, status, message)).map_err(pv_err)
            }
        }
    }

    /// Attach a PUT handler: `pv.on_put(fn)` or `@pv.on_put`.
    ///
    /// The callback is invoked as `callback(pv, value)` whenever a PVAccess
    /// client writes to this PV, BEFORE the value is applied. Returning
    /// `False` or raising rejects the PUT on the wire (the client's `put`
    /// raises); any other return accepts it. Returns the callback unchanged
    /// (decorator protocol), so it works both as a plain method call and as
    /// `@pv.on_put`.
    fn on_put(&self, py: Python<'_>, callback: PyObject) -> PyResult<PyObject> {
        if matches!(&self.kind, PvKind::Array(..)) {
            return Err(PyTypeError::new_err(
                "on_put/scan not supported on array PVs",
            ));
        }
        match &self.kind {
            PvKind::F64(p) => {
                let handle = p.clone();
                let cb = callback.clone_ref(py);
                let _ = p.clone().on_put(move |_pv, v: f64| {
                    py_on_put(&cb, PvKind::F64(handle.clone()), PutVal::F64(v))
                });
            }
            PvKind::Bool(p) => {
                let handle = p.clone();
                let cb = callback.clone_ref(py);
                let _ = p.clone().on_put(move |_pv, v: bool| {
                    py_on_put(&cb, PvKind::Bool(handle.clone()), PutVal::Bool(v))
                });
            }
            PvKind::I32(p) => {
                let handle = p.clone();
                let cb = callback.clone_ref(py);
                let _ = p.clone().on_put(move |_pv, v: i32| {
                    py_on_put(&cb, PvKind::I32(handle.clone()), PutVal::I32(v))
                });
            }
            PvKind::Str(p) => {
                let handle = p.clone();
                let cb = callback.clone_ref(py);
                let _ = p.clone().on_put(move |_pv, v: String| {
                    py_on_put(&cb, PvKind::Str(handle.clone()), PutVal::Str(v))
                });
            }
            PvKind::Typed(p, code) => {
                let handle = p.clone();
                let c = *code;
                let cb = callback.clone_ref(py);
                let _ = p.clone().on_put(move |_pv, v: ScalarValue| {
                    py_on_put(&cb, PvKind::Typed(handle.clone(), c), PutVal::Scalar(v))
                });
            }
            PvKind::Array(..) => unreachable!("Array on_put rejected above"),
        }
        Ok(callback)
    }

    /// Periodic scan: `pv.scan(period, fn)` or `@pv.scan(period=0.1)`.
    ///
    /// `fn(pv)` returns the new value. Returning `None` re-posts the last
    /// value this scan produced (the type default — 0.0/false/0/"" — before
    /// the first scanned value); it does not read whatever the PV's current
    /// value happens to be (e.g. from `pv.set(...)` called elsewhere). Prefer
    /// returning a value from the callback, or calling `pv.set()` explicitly.
    /// Must be attached BEFORE the PV is served (`Server(...)`); attaching
    /// afterwards is a silent no-op (core logs a warning).
    #[pyo3(signature = (period, callback=None))]
    fn scan(&self, py: Python<'_>, period: f64, callback: Option<PyObject>) -> PyResult<PyObject> {
        if matches!(&self.kind, PvKind::Array(..)) {
            return Err(PyTypeError::new_err(
                "on_put/scan not supported on array PVs",
            ));
        }
        match callback {
            Some(cb) => {
                register_scan(self, period, cb.clone_ref(py));
                Ok(cb)
            }
            None => {
                let dec = ScanDecorator {
                    pv: self.clone(),
                    period,
                };
                dec.into_py_any(py)
            }
        }
    }
}

/// Decorator-factory returned by `pv.scan(period=...)` when called without a
/// callback: `@pv.scan(period=0.1)` then registers the decorated function.
#[pyclass]
pub struct ScanDecorator {
    pv: PyPv,
    period: f64,
}

#[pymethods]
impl ScanDecorator {
    fn __call__(&self, py: Python<'_>, callback: PyObject) -> PyResult<PyObject> {
        register_scan(&self.pv, self.period, callback.clone_ref(py));
        Ok(callback)
    }
}

fn register_scan(pv: &PyPv, period_secs: f64, cb: PyObject) {
    let dur = std::time::Duration::from_secs_f64(period_secs);
    match &pv.kind {
        PvKind::F64(p) => {
            let cache = Mutex::new(None);
            let _ = p
                .clone()
                .scan(dur, move |h| scan_bridge_f64(&cb, &cache, h));
        }
        PvKind::Bool(p) => {
            let cache = Mutex::new(None);
            let _ = p
                .clone()
                .scan(dur, move |h| scan_bridge_bool(&cb, &cache, h));
        }
        PvKind::I32(p) => {
            let cache = Mutex::new(None);
            let _ = p
                .clone()
                .scan(dur, move |h| scan_bridge_i32(&cb, &cache, h));
        }
        PvKind::Str(p) => {
            let cache = Mutex::new(None);
            let _ = p
                .clone()
                .scan(dur, move |h| scan_bridge_str(&cb, &cache, h));
        }
        PvKind::Typed(p, code) => {
            let cache = Mutex::new(None);
            let c = *code;
            let _ = p
                .clone()
                .scan(dur, move |h| scan_bridge_typed(&cb, &cache, h, c));
        }
        PvKind::Array(..) => unreachable!("Array scan rejected in PyPv::scan"),
    }
}

/// Shared scan-bridge shape, one per scalar type (kept as small distinct fns
/// rather than a fully generic helper — `PyPv`'s `PvKind` wrapping and the
/// per-type `extract::<T>` calls don't factor cleanly through a trait without
/// more machinery than four ~10-line functions warrant).
///
/// `Handle::current().block_on` is NOT usable here to read "the current
/// value": the scan closure runs synchronously inside the async scan task,
/// and blocking on it panics. Instead each closure owns a
/// `Mutex<Option<T>>` cache: a value returned by the Python callback is
/// cached and returned; `None`/an unextractable return/an exception falls
/// back to the cached last value, or a type default (0.0/false/0/"") on the
/// very first call. This keeps the `None`-means-"leave PV alone" contract
/// honest without ever blocking inside the runtime.
macro_rules! scan_bridge_fn {
    ($fname:ident, $ty:ty, $kind:ident, $default:expr) => {
        fn $fname(cb: &PyObject, cache: &Mutex<Option<$ty>>, h: &Pv<$ty>) -> $ty {
            Python::with_gil(|py| {
                let pv = PyPv {
                    kind: PvKind::$kind(h.clone()),
                };
                let result = match cb.call1(py, (pv,)) {
                    Ok(ret) if ret.is_none(py) => None,
                    Ok(ret) => ret.extract::<$ty>(py).ok(),
                    Err(e) => {
                        tracing::error!("scan callback error: {e}");
                        None
                    }
                };
                let mut guard = cache.lock().unwrap();
                match result {
                    Some(v) => {
                        *guard = Some(v.clone());
                        v
                    }
                    None => guard.clone().unwrap_or_else(|| $default),
                }
            })
        }
    };
}

scan_bridge_fn!(scan_bridge_f64, f64, F64, 0.0f64);
scan_bridge_fn!(scan_bridge_bool, bool, Bool, false);
scan_bridge_fn!(scan_bridge_i32, i32, I32, 0i32);
scan_bridge_fn!(scan_bridge_str, String, Str, String::new());

/// Scan bridge for dynamically typed scalars: the Python return value is
/// strictly coerced to the record's wire type; failures fall back to the
/// cached last value or the type's zero default (same contract as the four
/// monomorphic bridges above).
fn scan_bridge_typed(
    cb: &PyObject,
    cache: &Mutex<Option<ScalarValue>>,
    h: &Pv<ScalarValue>,
    code: TypeCode,
) -> ScalarValue {
    Python::with_gil(|py| {
        let pv = PyPv {
            kind: PvKind::Typed(h.clone(), code),
        };
        let result = match cb.call1(py, (pv,)) {
            Ok(ret) if ret.is_none(py) => None,
            Ok(ret) => py_to_scalar_typed(ret.bind(py), code).ok(),
            Err(e) => {
                tracing::error!("scan callback error: {e}");
                None
            }
        };
        let mut guard = cache.lock().unwrap();
        match result {
            Some(v) => {
                *guard = Some(v.clone());
                v
            }
            None => guard.clone().unwrap_or_else(|| default_scalar(code)),
        }
    })
}

/// Zero/empty default for each wire type (scan fallback before first tick).
fn default_scalar(code: TypeCode) -> ScalarValue {
    match code {
        TypeCode::Boolean => ScalarValue::Bool(false),
        TypeCode::Int8 => ScalarValue::I8(0),
        TypeCode::Int16 => ScalarValue::I16(0),
        TypeCode::Int32 => ScalarValue::I32(0),
        TypeCode::Int64 => ScalarValue::I64(0),
        TypeCode::UInt8 => ScalarValue::U8(0),
        TypeCode::UInt16 => ScalarValue::U16(0),
        TypeCode::UInt32 => ScalarValue::U32(0),
        TypeCode::UInt64 => ScalarValue::U64(0),
        TypeCode::Float32 => ScalarValue::F32(0.0),
        TypeCode::String => ScalarValue::Str(String::new()),
        _ => ScalarValue::F64(0.0),
    }
}

pub(crate) enum PutVal {
    F64(f64),
    Bool(bool),
    I32(i32),
    Str(String),
    Scalar(ScalarValue),
}

/// Bridge a wire PUT into a Python callback. Exception or `False` → reject.
fn py_on_put(cb: &PyObject, kind: PvKind, val: PutVal) -> Result<(), String> {
    Python::with_gil(|py| {
        let pv = PyPv { kind };
        let arg = match val {
            PutVal::F64(v) => v.into_py_any(py),
            PutVal::Bool(v) => v.into_py_any(py),
            PutVal::I32(v) => v.into_py_any(py),
            PutVal::Str(v) => v.into_py_any(py),
            PutVal::Scalar(v) => Ok(scalar_to_py(py, &v)),
        }
        .map_err(|e| e.to_string())?;
        match cb.call1(py, (pv, arg)) {
            Err(e) => Err(e.to_string()),
            Ok(ret) => {
                if matches!(ret.extract::<bool>(py), Ok(false)) {
                    Err("rejected by on_put".to_string())
                } else {
                    Ok(())
                }
            }
        }
    })
}

/// Shared keyword options applied to any typed handle.
#[allow(clippy::too_many_arguments)]
fn apply_opts<T: spvirit_server::pv::PvScalar>(
    mut pv: Pv<T>,
    units: Option<String>,
    prec: Option<i32>,
    desc: Option<String>,
    adel: Option<f64>,
    mdel: Option<f64>,
    drive_limits: Option<(f64, f64)>,
    alarm_limits: Option<(f64, f64, f64, f64)>,
) -> Pv<T> {
    if let Some(u) = units {
        pv = pv.units(u);
    }
    if let Some(p) = prec {
        pv = pv.prec(p);
    }
    if let Some(d) = desc {
        pv = pv.desc(d);
    }
    if let Some(a) = adel {
        pv = pv.adel(a);
    }
    if let Some(m) = mdel {
        pv = pv.mdel(m);
    }
    if let Some((lo, hi)) = drive_limits {
        pv = pv.drive_limits(lo, hi);
    }
    if let Some((lolo, low, high, hihi)) = alarm_limits {
        pv = pv.alarm_limits(lolo, low, high, hihi);
    }
    pv
}

macro_rules! pv_ctor {
    ($fname:ident, $ctor:path, $init_ty:ty, $kind:ident, $doc:literal) => {
        #[pyfunction]
        #[pyo3(signature = (name, initial, *, units=None, prec=None, desc=None,
                                    adel=None, mdel=None, drive_limits=None, alarm_limits=None))]
        #[doc = $doc]
        #[allow(clippy::too_many_arguments)]
        pub fn $fname(
            name: String,
            initial: $init_ty,
            units: Option<String>,
            prec: Option<i32>,
            desc: Option<String>,
            adel: Option<f64>,
            mdel: Option<f64>,
            drive_limits: Option<(f64, f64)>,
            alarm_limits: Option<(f64, f64, f64, f64)>,
        ) -> PyPv {
            let pv = apply_opts(
                $ctor(name, initial),
                units,
                prec,
                desc,
                adel,
                mdel,
                drive_limits,
                alarm_limits,
            );
            PyPv {
                kind: PvKind::$kind(pv),
            }
        }
    };
}

pv_ctor!(
    ai,
    Pv::ai,
    f64,
    F64,
    "Analog input (read-only over the wire)."
);
pv_ctor!(ao, Pv::ao, f64, F64, "Analog output (writable).");
pv_ctor!(
    bi,
    Pv::bi,
    bool,
    Bool,
    "Binary input (read-only over the wire)."
);
pv_ctor!(bo, Pv::bo, bool, Bool, "Binary output (writable).");
pv_ctor!(
    string_in,
    Pv::string_in,
    String,
    Str,
    "String input (read-only over the wire)."
);
pv_ctor!(
    string_out,
    Pv::string_out,
    String,
    Str,
    "String output (writable)."
);
pv_ctor!(
    longin,
    Pv::longin,
    i32,
    I32,
    "32-bit integer input (read-only over the wire)."
);
pv_ctor!(
    longout,
    Pv::longout,
    i32,
    I32,
    "32-bit integer output (writable)."
);

/// Multi-bit binary input (enum, read-only). Value is the choice index;
/// out-of-range writes are rejected. Enum records ignore the common
/// `NtScalar` options (`units`/`prec`/`adel`/`mdel`/limits) — only `desc`
/// is accepted.
#[pyfunction]
#[pyo3(signature = (name, choices, initial, *, desc=None))]
pub fn mbbi(name: String, choices: Vec<String>, initial: i32, desc: Option<String>) -> PyPv {
    let mut pv = Pv::mbbi(name, choices, initial);
    if let Some(d) = desc {
        pv = pv.desc(d);
    }
    PyPv {
        kind: PvKind::I32(pv),
    }
}

/// Multi-bit binary output (enum, writable). Value is the choice index;
/// out-of-range writes are rejected. Enum records ignore the common
/// `NtScalar` options (`units`/`prec`/`adel`/`mdel`/limits) — only `desc`
/// is accepted.
#[pyfunction]
#[pyo3(signature = (name, choices, initial, *, desc=None))]
pub fn mbbo(name: String, choices: Vec<String>, initial: i32, desc: Option<String>) -> PyPv {
    let mut pv = Pv::mbbo(name, choices, initial);
    if let Some(d) = desc {
        pv = pv.desc(d);
    }
    PyPv {
        kind: PvKind::I32(pv),
    }
}

/// Array record (writable over the wire). `data` is a list of bool/int/
/// float/str, or `bytes` for a `U8` array. `type=` selects the element
/// type explicitly (e.g. "ushort", "float").
#[pyfunction]
#[pyo3(signature = (name, data, *, r#type=None))]
pub fn waveform(name: String, data: &Bound<'_, PyAny>, r#type: Option<String>) -> PyResult<PyPv> {
    let arr = crate::convert::py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
    let code = crate::convert::scalar_array_type_code(&arr);
    Ok(PyPv {
        kind: PvKind::Array(PvArray::waveform(name, arr), code),
    })
}

/// Analog array input (read-only over the wire). `data` is a list of bool/
/// int/float/str, or `bytes` for a `U8` array. `type=` selects the element
/// type explicitly (e.g. "ushort", "float").
#[pyfunction]
#[pyo3(signature = (name, data, *, r#type=None))]
pub fn aai(name: String, data: &Bound<'_, PyAny>, r#type: Option<String>) -> PyResult<PyPv> {
    let arr = crate::convert::py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
    let code = crate::convert::scalar_array_type_code(&arr);
    Ok(PyPv {
        kind: PvKind::Array(PvArray::aai(name, arr), code),
    })
}

/// Analog array output (writable). `data` is a list of bool/int/float/str,
/// or `bytes` for a `U8` array. `type=` selects the element type explicitly
/// (e.g. "ushort", "float").
#[pyfunction]
#[pyo3(signature = (name, data, *, r#type=None))]
pub fn aao(name: String, data: &Bound<'_, PyAny>, r#type: Option<String>) -> PyResult<PyPv> {
    let arr = crate::convert::py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
    let code = crate::convert::scalar_array_type_code(&arr);
    Ok(PyPv {
        kind: PvKind::Array(PvArray::aao(name, arr), code),
    })
}

/// A derived (read-only) float PV recomputed whenever any input changes.
///
/// `inputs` must all be float PVs (`ai`/`ao`); anything else raises
/// `TypeError`. `callback(values: list[float]) -> float` is invoked with the
/// inputs' current values in order; exceptions or non-float returns are
/// logged and treated as `0.0`. Must be attached BEFORE the PVs are served
/// (`Server(...)`); attaching afterwards is a silent no-op (core logs a
/// warning).
#[pyfunction]
pub fn calc(py: Python<'_>, name: String, inputs: Vec<PyPv>, callback: PyObject) -> PyResult<PyPv> {
    let mut fs: Vec<Pv<f64>> = Vec::with_capacity(inputs.len());
    for p in &inputs {
        match &p.kind {
            PvKind::F64(h) => fs.push(h.clone()),
            _ => {
                return Err(PyTypeError::new_err(
                    "calc inputs must all be float PVs (ai/ao)",
                ));
            }
        }
    }
    let refs: Vec<&Pv<f64>> = fs.iter().collect();
    let cb = callback.clone_ref(py);
    let out = Pv::calc(name, &refs, move |vals: &[f64]| {
        Python::with_gil(|py| {
            let called = pyo3::types::PyList::new(py, vals)
                .and_then(|l| cb.call1(py, (l,)))
                .and_then(|ret| ret.extract::<f64>(py));
            match called {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("calc callback failed, posting 0.0: {e}");
                    0.0
                }
            }
        })
    });
    Ok(PyPv {
        kind: PvKind::F64(out),
    })
}

/// Build a typed PV, inferring the record type from `initial`'s Python type:
/// `bool` -> `bo`, `int` -> `longout`, `float` -> `ao`, `str` -> `string_out`,
/// `list`/`bytes` -> `waveform`. Note `bool` is checked before `int` since
/// `isinstance(True, int)` is `True` in Python. Any other type raises
/// `TypeError`.
///
/// `type=` overrides inference and picks the wire value type explicitly
/// (same type-string convention as `spvirit.scalar(...)`): `double`,
/// `boolean`, `int`, and `string` map onto the same native handle kinds as
/// the inferred cases above (`bo`/`longout`/`ao`/`string_out` respectively);
/// any other scalar type (`long`, the unsigned variants, `float`) produces a
/// dynamically typed handle. A `list`/`bytes` `initial` combined with
/// `type=` produces a typed waveform instead of inferring the element type.
/// Metadata options (`units`/`prec`/`desc`/`adel`/`mdel`/`drive_limits`/
/// `alarm_limits`) are rejected with `TypeError` for array PVs, whether the
/// element type was inferred or given via `type=`.
#[pyfunction]
#[pyo3(signature = (name, initial, *, units=None, prec=None, desc=None,
                    adel=None, mdel=None, drive_limits=None, alarm_limits=None, r#type=None))]
#[allow(clippy::too_many_arguments)]
pub fn pv(
    name: String,
    initial: &Bound<'_, PyAny>,
    units: Option<String>,
    prec: Option<i32>,
    desc: Option<String>,
    adel: Option<f64>,
    mdel: Option<f64>,
    drive_limits: Option<(f64, f64)>,
    alarm_limits: Option<(f64, f64, f64, f64)>,
    r#type: Option<String>,
) -> PyResult<PyPv> {
    use pyo3::types::{PyBool, PyBytes, PyFloat, PyInt, PyList, PyString};

    if let Some(t) = &r#type {
        let code = parse_scalar_type(t)?;
        // Arrays: typed waveform (same metadata-option rejection as inferred arrays).
        if initial.is_instance_of::<PyList>() || initial.is_instance_of::<PyBytes>() {
            if units.is_some()
                || prec.is_some()
                || desc.is_some()
                || adel.is_some()
                || mdel.is_some()
                || drive_limits.is_some()
                || alarm_limits.is_some()
            {
                return Err(PyTypeError::new_err(
                    "metadata options (units/prec/desc/adel/mdel/drive_limits/alarm_limits) \
                     are not supported for array PVs",
                ));
            }
            let arr = crate::convert::py_to_scalar_array_typed(initial, code)?;
            let elem_code = crate::convert::scalar_array_type_code(&arr);
            return Ok(PyPv {
                kind: PvKind::Array(PvArray::waveform(name, arr), elem_code),
            });
        }
        let sv = py_to_scalar_typed(initial, code)?;
        // The four native kinds keep their monomorphic handles (calc() inputs,
        // existing repr contracts); everything else rides PvKind::Typed.
        let kind = match sv {
            ScalarValue::F64(v) => PvKind::F64(apply_opts(
                Pv::ao(name, v),
                units, prec, desc, adel, mdel, drive_limits, alarm_limits,
            )),
            ScalarValue::Bool(v) => PvKind::Bool(apply_opts(
                Pv::bo(name, v),
                units, prec, desc, adel, mdel, drive_limits, alarm_limits,
            )),
            ScalarValue::I32(v) => PvKind::I32(apply_opts(
                Pv::longout(name, v),
                units, prec, desc, adel, mdel, drive_limits, alarm_limits,
            )),
            ScalarValue::Str(v) => PvKind::Str(apply_opts(
                Pv::string_out(name, v),
                units, prec, desc, adel, mdel, drive_limits, alarm_limits,
            )),
            other => PvKind::Typed(
                apply_opts(
                    Pv::<ScalarValue>::scalar_out(name, other),
                    units, prec, desc, adel, mdel, drive_limits, alarm_limits,
                ),
                code,
            ),
        };
        return Ok(PyPv { kind });
    }

    let kind = if initial.is_instance_of::<PyBool>() {
        PvKind::Bool(apply_opts(
            Pv::bo(name, initial.extract::<bool>()?),
            units,
            prec,
            desc,
            adel,
            mdel,
            drive_limits,
            alarm_limits,
        ))
    } else if initial.is_instance_of::<PyInt>() {
        PvKind::I32(apply_opts(
            Pv::longout(name, initial.extract::<i32>()?),
            units,
            prec,
            desc,
            adel,
            mdel,
            drive_limits,
            alarm_limits,
        ))
    } else if initial.is_instance_of::<PyList>() || initial.is_instance_of::<PyBytes>() {
        // Array records carry no scalar metadata; reject rather than
        // silently discard the options.
        if units.is_some()
            || prec.is_some()
            || desc.is_some()
            || adel.is_some()
            || mdel.is_some()
            || drive_limits.is_some()
            || alarm_limits.is_some()
        {
            return Err(PyTypeError::new_err(
                "metadata options (units/prec/desc/adel/mdel/drive_limits/alarm_limits) \
                 are not supported for array PVs",
            ));
        }
        let arr = py_to_scalar_array(initial)?;
        let code = crate::convert::scalar_array_type_code(&arr);
        PvKind::Array(PvArray::waveform(name, arr), code)
    } else if initial.is_instance_of::<PyFloat>() {
        PvKind::F64(apply_opts(
            Pv::ao(name, initial.extract::<f64>()?),
            units,
            prec,
            desc,
            adel,
            mdel,
            drive_limits,
            alarm_limits,
        ))
    } else if initial.is_instance_of::<PyString>() {
        PvKind::Str(apply_opts(
            Pv::string_out(name, initial.extract::<String>()?),
            units,
            prec,
            desc,
            adel,
            mdel,
            drive_limits,
            alarm_limits,
        ))
    } else {
        return Err(PyTypeError::new_err(format!(
            "cannot infer PV type from initial value of type {}",
            initial.get_type().name()?
        )));
    };
    Ok(PyPv { kind })
}

/// Generic scalar record covering all twelve NTScalar wire value types.
///
/// `type` is required (no inference): "boolean", "byte", "short", "int",
/// "long", "ubyte", "ushort", "uint", "ulong", "float", "double", "string"
/// (aliases like "u16"/"f32" accepted). `writable=False` serves the
/// read-only (input) flavor, `True` the writable (output) flavor. The
/// initial value is strictly coerced: out-of-range raises OverflowError,
/// wrong kinds raise TypeError.
#[pyfunction]
#[pyo3(signature = (name, initial, *, r#type, writable=false, units=None, prec=None,
                    desc=None, adel=None, mdel=None, drive_limits=None, alarm_limits=None))]
#[allow(clippy::too_many_arguments)]
pub fn scalar(
    name: String,
    initial: &Bound<'_, PyAny>,
    r#type: String,
    writable: bool,
    units: Option<String>,
    prec: Option<i32>,
    desc: Option<String>,
    adel: Option<f64>,
    mdel: Option<f64>,
    drive_limits: Option<(f64, f64)>,
    alarm_limits: Option<(f64, f64, f64, f64)>,
) -> PyResult<PyPv> {
    let code = parse_scalar_type(&r#type)?;
    let sv = py_to_scalar_typed(initial, code)?;
    let handle = if writable {
        Pv::<ScalarValue>::scalar_out(name, sv)
    } else {
        Pv::<ScalarValue>::scalar_in(name, sv)
    };
    let handle = apply_opts(
        handle,
        units,
        prec,
        desc,
        adel,
        mdel,
        drive_limits,
        alarm_limits,
    );
    Ok(PyPv {
        kind: PvKind::Typed(handle, code),
    })
}
