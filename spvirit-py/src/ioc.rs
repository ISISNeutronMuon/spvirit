//! Tier 3 from Python: `spvirit.ioc.*` record constructors, `spvirit.Ioc`,
//! and the live record handles a built `Ioc` hands back.
//!
//! Field names here are verbatim EPICS — `EGU`, `HIHI`, `HHSV` — and not
//! tier 2's `units=` / `drive_limits=`. That is not a style choice: tier-3
//! fields have to round-trip to `.db` text *and* surface as `.FIELD` PVs, so
//! `rec["EGU"]` and `EGU=` must be the same token. Renaming would break the
//! 1:1 mapping that is the tier's entire value.

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyString};
use spvirit_ioc::RecordSpec;
use spvirit_ioc::model::Kind;

/// Tier 2 spellings that would otherwise be silently accepted as unmodelled
/// fields, producing a record whose units nothing reads.
const TIER_TWO_SPELLINGS: [(&str, &str); 4] = [
    ("UNITS", "EGU"),
    ("DRIVE_LIMITS", "DRVL/DRVH"),
    ("PRECISION", "PREC"),
    ("DESCRIPTION", "DESC"),
];

/// Render a Python value the way a `.db` file writes it.
fn render(value: &Bound<'_, PyAny>) -> PyResult<String> {
    // bool before int: in Python bool *is* an int, and PINI=True must become
    // YES, not 1.
    if let Ok(b) = value.downcast::<PyBool>() {
        return Ok(if b.is_true() { "YES".into() } else { "NO".into() });
    }
    if value.is_instance_of::<PyString>() {
        return value.extract::<String>();
    }
    if value.downcast::<PyInt>().is_ok() {
        return Ok(value.extract::<i64>()?.to_string());
    }
    if value.downcast::<PyFloat>().is_ok() {
        let v: f64 = value.extract()?;
        return Ok(if v.fract() == 0.0 && v.is_finite() && v.abs() < 1e15 {
            format!("{}", v as i64)
        } else {
            format!("{v}")
        });
    }
    Err(pyo3::exceptions::PyTypeError::new_err(format!(
        "field values must be str, int, float or bool, got {}",
        value.get_type().name()?
    )))
}

fn spec_from_kwargs(
    kind: Kind,
    name: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<RecordSpec> {
    let mut spec = RecordSpec::new(kind, name);
    let Some(kwargs) = kwargs else {
        return Ok(spec);
    };
    for (key, value) in kwargs.iter() {
        let key: String = key.extract::<String>()?.to_ascii_uppercase();
        if let Some((_, correct)) = TIER_TWO_SPELLINGS.iter().find(|(k, _)| *k == key) {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "'{key}' is tier 2's spelling; tier 3 uses verbatim EPICS field \
                 names — write {correct} instead. The tiers spell fields \
                 differently on purpose: a tier-3 field has to round-trip to .db \
                 text and surface as a .FIELD PV, so the name cannot be changed."
            )));
        }
        spec = spec.field(&key, render(&value)?);
    }
    Ok(spec)
}

/// A record described but not yet built.
///
/// Becomes a live handle once passed to [`PyIoc`]: `Ioc(records=[...])` binds
/// every spec it is given, after which `get()`/`set()` and `["FIELD"]` work.
/// Before that the handle methods raise `Unbound`, because there is no engine
/// to talk to.
///
/// It wraps the Rust `spvirit_ioc::RecordSpec` directly and inherits its
/// pending→bound state machine (Ruling 6): `RecordSpec` is `Arc`-shared, so a
/// clone handed to `Ioc` and this handle share one binding slot, and there is
/// no Python-side "already built" flag to keep in sync. `to_db_record` reads
/// the fields behind the slot, so `fields()` works before and after build.
#[pyclass(name = "RecordSpec", module = "spvirit.ioc")]
pub struct PyRecordSpec {
    /// The Rust handle. Cheap to clone (an `Arc`); every clone shares the one
    /// binding slot `IocSource::from_records` fills.
    pub(crate) inner: RecordSpec,
    /// Cached for `__repr__` and error messages without locking the handle.
    pub(crate) name: String,
}

impl PyRecordSpec {
    fn wrap(spec: RecordSpec) -> PyRecordSpec {
        let name = spec.name().to_string();
        PyRecordSpec { inner: spec, name }
    }
}

#[pymethods]
impl PyRecordSpec {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// The field map as it will be lowered, for inspection and tests. Works
    /// before and after the spec is built, because the fields live behind the
    /// binding slot rather than being consumed at build.
    fn fields(&self, py: Python<'_>) -> PyResult<PyObject> {
        let raw = self.inner.to_db_record();
        let d = PyDict::new(py);
        for (k, v) in raw.fields {
            d.set_item(k, v)?;
        }
        Ok(d.into())
    }

    fn __repr__(&self) -> String {
        format!("<spvirit.ioc.RecordSpec '{}'>", self.name)
    }
}

macro_rules! ctor {
    ($($f:ident => $k:expr),* $(,)?) => {$(
        #[pyfunction]
        #[pyo3(signature = (name, **fields))]
        pub fn $f(name: &str, fields: Option<&Bound<'_, PyDict>>) -> PyResult<PyRecordSpec> {
            Ok(PyRecordSpec::wrap(spec_from_kwargs($k, name, fields)?))
        }
    )*};
}

ctor! {
    ai => Kind::Ai,
    ao => Kind::Ao,
    bi => Kind::Bi,
    bo => Kind::Bo,
    longin => Kind::LongIn,
    longout => Kind::LongOut,
}

/// Build the `spvirit.ioc` submodule.
pub fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = parent.py();
    let m = PyModule::new(py, "ioc")?;
    m.add_class::<PyRecordSpec>()?;
    m.add_function(wrap_pyfunction!(ai, &m)?)?;
    m.add_function(wrap_pyfunction!(ao, &m)?)?;
    m.add_function(wrap_pyfunction!(bi, &m)?)?;
    m.add_function(wrap_pyfunction!(bo, &m)?)?;
    m.add_function(wrap_pyfunction!(longin, &m)?)?;
    m.add_function(wrap_pyfunction!(longout, &m)?)?;
    parent.add_submodule(&m)?;
    // A submodule added this way is reachable as an attribute but not
    // importable as `from spvirit.ioc import ao` until it is in sys.modules.
    py.import("sys")?
        .getattr("modules")?
        .set_item("spvirit.ioc", &m)?;
    Ok(())
}
