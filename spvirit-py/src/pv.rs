//! Typed PV handles for Python — wraps `spvirit_server::pv::Pv<T>`.

use std::sync::Mutex;

use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyKeyError, PyRuntimeError, PyTypeError};
use pyo3::prelude::*;

use spvirit_server::pv::{AnyPv, Pv, PvError};

use crate::runtime::block_on_py;

pub(crate) fn pv_err(e: PvError) -> PyErr {
    match e {
        PvError::Unbound => PyRuntimeError::new_err(e.to_string()),
        PvError::NotFound(_) => PyKeyError::new_err(e.to_string()),
        PvError::TypeMismatch { .. } => PyTypeError::new_err(e.to_string()),
        PvError::PutRejected(_) => PyRuntimeError::new_err(e.to_string()),
    }
}

#[derive(Clone)]
pub(crate) enum PvKind {
    F64(Pv<f64>),
    Bool(Pv<bool>),
    I32(Pv<i32>),
    Str(Pv<String>),
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
        }
    }
}

#[pymethods]
impl PyPv {
    /// PV name.
    fn name(&self) -> &str {
        match &self.kind {
            PvKind::F64(p) => p.name(),
            PvKind::Bool(p) => p.name(),
            PvKind::I32(p) => p.name(),
            PvKind::Str(p) => p.name(),
        }
    }

    fn __repr__(&self) -> String {
        let ty = match &self.kind {
            PvKind::F64(_) => "float",
            PvKind::Bool(_) => "bool",
            PvKind::I32(_) => "int",
            PvKind::Str(_) => "str",
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
        }
        Ok(callback)
    }

    /// Periodic scan: `pv.scan(period, fn)` or `@pv.scan(period=0.1)`.
    ///
    /// `fn(pv)` returns the new value, or `None` to leave the PV at its
    /// current value (e.g. the callback called `pv.set(...)`/other PVs
    /// itself). Must be attached BEFORE the PV is served (`Server(...)`);
    /// attaching afterwards is a silent no-op (core logs a warning).
    #[pyo3(signature = (period, callback=None))]
    fn scan(&self, py: Python<'_>, period: f64, callback: Option<PyObject>) -> PyResult<PyObject> {
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

pub(crate) enum PutVal {
    F64(f64),
    Bool(bool),
    I32(i32),
    Str(String),
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
            let list = pyo3::types::PyList::new(py, vals).ok();
            match list.and_then(|l| cb.call1(py, (l,)).ok()) {
                Some(ret) => ret.extract::<f64>(py).unwrap_or(0.0),
                None => 0.0,
            }
        })
    });
    Ok(PyPv {
        kind: PvKind::F64(out),
    })
}

/// Build a typed PV, inferring the record type from `initial`'s Python type:
/// `bool` -> `bo`, `float` -> `ao`, `str` -> `string_out`. Plain `int` raises
/// `TypeError` (longin/longout aren't implemented yet — use a float). Note
/// `bool` is checked before `int` since `isinstance(True, int)` is `True` in
/// Python.
#[pyfunction]
#[pyo3(signature = (name, initial, *, units=None, prec=None, desc=None,
                    adel=None, mdel=None, drive_limits=None, alarm_limits=None))]
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
) -> PyResult<PyPv> {
    use pyo3::types::{PyBool, PyFloat, PyInt, PyString};
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
        return Err(PyTypeError::new_err(
            "integer records need longin/longout — not yet implemented; use a float",
        ));
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
