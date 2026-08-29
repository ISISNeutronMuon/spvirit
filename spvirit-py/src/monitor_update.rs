//! Python view of a decoded monitor update.
//!
//! Monitor callbacks and `Channel.monitor` used to hand out the decoded value
//! directly. They now hand out a [`PyMonitorUpdate`], which carries the same
//! value plus the changed and overrun bitsets resolved to dotted field paths.

use pyo3::prelude::*;

use spvirit_client::MonitorUpdate;

use crate::convert::decoded_to_py;

/// A single monitor update: the value plus the changed and overrun bitsets.
#[pyclass(name = "MonitorUpdate", module = "spvirit.lowlevel", frozen)]
pub struct PyMonitorUpdate {
    value: PyObject,
    changed: Vec<String>,
    overrun: Vec<String>,
}

impl PyMonitorUpdate {
    /// Build the Python view of a decoded update. Requires the GIL because
    /// the value is converted eagerly.
    pub fn from_update(py: Python<'_>, update: &MonitorUpdate) -> Self {
        Self {
            value: decoded_to_py(py, &update.value),
            changed: update.changed_paths(),
            overrun: update.overrun_paths(),
        }
    }
}

/// Invoke a Python monitor callback with one decoded update.
///
/// Acquires the GIL, builds the [`PyMonitorUpdate`] view, and calls the
/// callback. Returns `Ok(keep_going)` — where a callback that returns a
/// falsey value (or a non-bool) is treated as `true`/continue except when it
/// explicitly returns `False` — or `Err(e)` if the callback raised. The
/// error is returned rather than sunk here because the three call sites
/// dispose of it differently: the blocking `monitor` re-raises it, the
/// low-level channel stores the `PyErr`, and the background `subscribe`
/// stringifies it into a `Mutex` (it cannot hold a `PyErr` across the task
/// boundary). Keeping the dispatch here and the disposal at the call site
/// preserves each site's exact behavior.
pub(crate) fn dispatch_monitor_update(
    callback: &PyObject,
    update: &MonitorUpdate,
) -> Result<bool, PyErr> {
    Python::with_gil(|py| {
        let py_val = PyMonitorUpdate::from_update(py, update);
        let ret = callback.call1(py, (py_val,))?;
        Ok(ret.extract::<bool>(py).unwrap_or(true))
    })
}

#[pymethods]
impl PyMonitorUpdate {
    /// The decoded value, in the same form the monitor callback used to yield.
    #[getter]
    fn value(&self, py: Python<'_>) -> PyObject {
        self.value.clone_ref(py)
    }

    /// Dotted paths of the fields marked changed in this update.
    #[getter]
    fn changed(&self) -> Vec<String> {
        self.changed.clone()
    }

    /// Dotted paths of the fields the server dropped intermediate updates for.
    #[getter]
    fn overrun(&self) -> Vec<String> {
        self.overrun.clone()
    }

    /// True when any field reports an overrun.
    #[getter]
    fn has_overrun(&self) -> bool {
        !self.overrun.is_empty()
    }

    fn __repr__(&self) -> String {
        format!(
            "MonitorUpdate(changed={:?}, overrun={:?})",
            self.changed, self.overrun
        )
    }
}
