//! Typed PV handles for Python — wraps `spvirit_server::pv::Pv<T>`.

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
