//! Python server wrappers — sync-only for phase 1.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use pyo3::prelude::*;
use pyo3::types::PyTuple;

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::SimplePvStore;
use spvirit_server::pva_server::PvaServer;
use spvirit_types::{NtPayload, ScalarArrayValue, ScalarValue};

use crate::convert::{
    coerce_scalar_array_value, coerce_scalar_value, decoded_to_py, parse_scalar_type, py_to_scalar,
    py_to_scalar_array, py_to_scalar_array_maybe_typed, py_to_scalar_array_typed,
    py_to_scalar_typed, scalar_array_type_code, scalar_to_py, scalar_value_type_code,
};
use crate::nt::{nt_payload_to_py, py_to_nt_payload};
use crate::runtime::{RUNTIME, block_on_py};
use crate::source::{PyNotifier, PySourceAdapter};

// ─── ServerBuilder ───────────────────────────────────────────────────────────

/// Fluent builder for a PVAccess server. Chain record definitions and
/// configuration, then call `build()`. Single-use: any method called after
/// `build()` raises RuntimeError.
#[pyclass(name = "ServerBuilder")]
pub struct PyServerBuilder {
    builder: Option<spvirit_server::PvaServerBuilder>,
    /// Python sources to wire up on build (label, order, adapter).
    python_sources: Vec<(String, i32, Arc<PySourceAdapter>)>,
    /// Filled during `build()`; read by deferred source `on_start` hooks when
    /// they fire at server start.
    notifier_cell: Arc<std::sync::OnceLock<PyNotifier>>,
}

/// Take the inner builder, raising `RuntimeError` if `build()` already ran.
fn take_builder(
    slf: &mut PyRefMut<'_, PyServerBuilder>,
) -> PyResult<spvirit_server::PvaServerBuilder> {
    slf.builder.take().ok_or_else(|| {
        pyo3::exceptions::PyRuntimeError::new_err(
            "ServerBuilder already consumed by build(); create a new builder",
        )
    })
}

#[pymethods]
impl PyServerBuilder {
    #[new]
    fn new() -> Self {
        Self {
            builder: Some(PvaServer::builder()),
            python_sources: Vec::new(),
            notifier_cell: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Add an `ai` (analog input) NTScalar double record — read-only over the wire.
    fn ai(mut slf: PyRefMut<'_, Self>, name: String, initial: f64) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.ai(name, initial));
        Ok(slf)
    }

    /// Add an `ao` (analog output) NTScalar double record — writable over the wire.
    fn ao(mut slf: PyRefMut<'_, Self>, name: String, initial: f64) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.ao(name, initial));
        Ok(slf)
    }

    /// Add a `bi` (binary input) NTScalar boolean record — read-only over the wire.
    fn bi(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        initial: bool,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.bi(name, initial));
        Ok(slf)
    }

    /// Add a `bo` (binary output) NTScalar boolean record — writable over the wire.
    fn bo(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        initial: bool,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.bo(name, initial));
        Ok(slf)
    }

    /// Add a `stringin` NTScalar string record — read-only over the wire.
    fn string_in(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        initial: String,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.string_in(name, initial));
        Ok(slf)
    }

    /// Add a `stringout` NTScalar string record — writable over the wire.
    fn string_out(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        initial: String,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.string_out(name, initial));
        Ok(slf)
    }

    /// Add a `waveform` NTScalarArray record — writable over the wire.
    /// `type=` selects the element type explicitly.
    #[pyo3(signature = (name, data, *, r#type=None))]
    fn waveform<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
        r#type: Option<String>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.waveform(name, arr));
        Ok(slf)
    }

    /// Add an `aai` (analog array input) NTScalarArray record — read-only over the wire.
    /// `type=` selects the element type explicitly.
    #[pyo3(signature = (name, data, *, r#type=None))]
    fn aai<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
        r#type: Option<String>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.aai(name, arr));
        Ok(slf)
    }

    /// Add an `aao` (analog array output) NTScalarArray record — writable over the wire.
    /// `type=` selects the element type explicitly.
    #[pyo3(signature = (name, data, *, r#type=None))]
    fn aao<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
        r#type: Option<String>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.aao(name, arr));
        Ok(slf)
    }

    /// Add a `subArray` record serving a `nelm`-element window of `data`
    /// starting at `indx` (defaults to the full array). `type=` selects the
    /// element type explicitly.
    #[pyo3(signature = (name, data, indx=0, nelm=None, *, r#type=None))]
    fn sub_array<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
        indx: usize,
        nelm: Option<usize>,
        r#type: Option<String>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
        let n = nelm.unwrap_or(arr.len());
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.sub_array(name, arr, indx, n));
        Ok(slf)
    }

    /// Add an NTTable record from a `{column_name: list}` dict of columns.
    /// `types=` selects per-column element types explicitly.
    #[pyo3(signature = (name, columns, *, types=None))]
    fn nt_table<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        columns: &Bound<'py, PyAny>,
        types: Option<&Bound<'py, pyo3::types::PyDict>>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let dict = columns.downcast::<pyo3::types::PyDict>().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err("columns must be a dict of {name: list}")
        })?;
        let mut cols: Vec<(String, ScalarArrayValue)> = Vec::new();
        for (key, val) in dict.iter() {
            let col_name: String = key.extract()?;
            let ty: Option<String> = match types {
                Some(d) => match d.get_item(&col_name)? {
                    Some(t) => Some(t.extract()?),
                    None => None,
                },
                None => None,
            };
            let col_data = py_to_scalar_array_maybe_typed(&val, ty.as_deref())?;
            cols.push((col_name, col_data));
        }
        if let Some(d) = types {
            let valid: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
            for key in d.keys().iter() {
                let key: String = key.extract()?;
                if !valid.contains(&key.as_str()) {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "types: unknown column {key:?} (valid columns: {valid:?})"
                    )));
                }
            }
        }
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.nt_table(name, cols));
        Ok(slf)
    }

    /// Add an NTNDArray record from flat array data and `(size, full_size)`
    /// dimension pairs. `type=` selects the element type explicitly.
    #[pyo3(signature = (name, data, dims, *, r#type=None))]
    fn nt_ndarray<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
        dims: Vec<(i32, i32)>,
        r#type: Option<String>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array_maybe_typed(data, r#type.as_deref())?;
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.nt_ndarray(name, arr, dims));
        Ok(slf)
    }

    /// Add an `mbbi` (multi-bit binary input) NTEnum record — read-only over
    /// the wire. `initial` is the choice index.
    fn mbbi(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        choices: Vec<String>,
        initial: i32,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.mbbi(name, choices, initial));
        Ok(slf)
    }

    /// Add an `mbbo` (multi-bit binary output) NTEnum record — writable over
    /// the wire. `initial` is the choice index.
    fn mbbo(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        choices: Vec<String>,
        initial: i32,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.mbbo(name, choices, initial));
        Ok(slf)
    }

    /// Add a generic structure record with the given struct ID and a
    /// `{field_name: value}` dict (scalars or lists). `types=` selects
    /// per-field types explicitly (e.g. `"short"`, `"short[]"`).
    #[pyo3(signature = (name, struct_id, fields, *, types=None))]
    fn generic<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        struct_id: String,
        fields: &Bound<'py, pyo3::types::PyDict>,
        types: Option<&Bound<'py, pyo3::types::PyDict>>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let mut field_vec: Vec<(String, spvirit_types::PvValue)> = Vec::new();
        for (key, val) in fields.iter() {
            let field_name: String = key.extract()?;
            let ty: Option<String> = match types {
                Some(d) => match d.get_item(&field_name)? {
                    Some(t) => Some(t.extract()?),
                    None => None,
                },
                None => None,
            };
            let pv_val = match ty {
                Some(t) => py_to_pv_value_typed(&val, &t)?,
                None => py_to_pv_value(&val)?,
            };
            field_vec.push((field_name, pv_val));
        }
        if let Some(d) = types {
            let valid: Vec<&str> = field_vec.iter().map(|(n, _)| n.as_str()).collect();
            for key in d.keys().iter() {
                let key: String = key.extract()?;
                if !valid.contains(&key.as_str()) {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "types: unknown field {key:?} (valid fields: {valid:?})"
                    )));
                }
            }
        }
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.generic(name, struct_id, field_vec));
        Ok(slf)
    }

    /// Load record definitions from an EPICS `.db` file at `path`.
    fn db_file(mut slf: PyRefMut<'_, Self>, path: String) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.db_file(path));
        Ok(slf)
    }

    /// Load record definitions from EPICS `.db` text given as a string.
    fn db_string(mut slf: PyRefMut<'_, Self>, content: String) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.db_string(&content));
        Ok(slf)
    }

    fn on_put(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        callback: PyObject,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(
            b.on_put(name, move |pv_name: &str, decoded: &DecodedValue| {
                Python::with_gil(|py| {
                    let py_val = decoded_to_py(py, decoded);
                    if let Err(e) = callback.call1(py, (pv_name, py_val)) {
                        tracing::error!("on_put callback error: {e}");
                    }
                });
            }),
        );
        Ok(slf)
    }

    /// Register a startup hook: `@builder.on_start`.
    ///
    /// The callback is invoked as `callback(store)` once at server start,
    /// before scan tasks spawn and before the listener accepts. May be `def`
    /// or `async def`. It shares one ordered list with sources' `on_start`
    /// hooks (registered via `add_source`) — all fire in true registration
    /// order, builder hooks and source hooks interleaved. Raising aborts
    /// startup, naming the hook. Returns the callback unchanged so it works
    /// as a decorator.
    fn on_start(mut slf: PyRefMut<'_, Self>, callback: PyObject) -> PyResult<PyObject> {
        let cb = Arc::new(callback.clone_ref(slf.py()));
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.on_start(move |store| {
            let cb = cb.clone();
            Box::pin(async move {
                let py_store = PyStore { inner: store };
                let result = crate::source::call_py_await(cb, "__call__", move |py| {
                    PyTuple::new(py, &[py_store.into_pyobject(py)?.into_any()])
                })
                .await;
                if let Err(e) = result {
                    // Propagate as a panic so run_start_hooks aborts startup,
                    // naming the hook (see run_start_hooks' error message).
                    panic!("on_start hook raised: {e}");
                }
            })
        }));
        Ok(callback)
    }

    /// Register an event handler: `@builder.on_event("NAME")`.
    ///
    /// The callback is invoked as `callback(store, event)` on the dispatcher
    /// after `post_event`. May be `def` or `async def`. Raising is logged and
    /// does not stop the dispatcher. Returns a decorator.
    fn on_event(slf: PyRefMut<'_, Self>, event: String) -> PyResult<PyObject> {
        let py = slf.py();
        let builder_obj: PyObject = slf.into_pyobject(py)?.into_any().unbind();
        let decorator = PyEventDecorator {
            builder: builder_obj,
            event,
        };
        Ok(decorator.into_pyobject(py)?.into_any().unbind())
    }

    /// Internal: attach an already-resolved handler. Called by the decorator
    /// returned from `on_event`.
    fn _add_event_handler(
        mut slf: PyRefMut<'_, Self>,
        event: String,
        callback: PyObject,
    ) -> PyResult<()> {
        let cb = Arc::new(callback);
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.on_event(event, move |store, ev| {
            let cb = cb.clone();
            Box::pin(async move {
                let py_store = PyStore { inner: store };
                let result = crate::source::call_py_await(cb, "__call__", move |py| {
                    PyTuple::new(
                        py,
                        &[
                            py_store.into_pyobject(py)?.into_any(),
                            ev.into_pyobject(py)?.into_any(),
                        ],
                    )
                })
                .await;
                if let Err(e) = result {
                    tracing::error!("event handler raised: {e}");
                }
            })
        }));
        Ok(())
    }

    fn scan(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        period: f64,
        callback: PyObject,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        let dur = Duration::from_secs_f64(period);
        slf.builder = Some(b.scan(name, dur, move |pv_name: &str| {
            Python::with_gil(|py| match callback.call1(py, (pv_name,)) {
                Ok(ret) => py_to_scalar(ret.bind(py)).unwrap_or(ScalarValue::F64(0.0)),
                Err(e) => {
                    tracing::error!("scan callback error: {e}");
                    ScalarValue::F64(0.0)
                }
            })
        }));
        Ok(slf)
    }

    /// Set the TCP port the server listens on.
    fn port(mut slf: PyRefMut<'_, Self>, port: u16) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.port(port));
        Ok(slf)
    }

    /// Set the UDP port used for search requests and beacons.
    fn udp_port(mut slf: PyRefMut<'_, Self>, port: u16) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.udp_port(port));
        Ok(slf)
    }

    /// Set the IP address to bind listeners to. Raises ValueError on an
    /// invalid IP string.
    fn listen_ip(mut slf: PyRefMut<'_, Self>, ip: String) -> PyResult<PyRefMut<'_, Self>> {
        let ip_addr: IpAddr = ip
            .parse()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.listen_ip(ip_addr));
        Ok(slf)
    }

    /// Set the IP address advertised to clients in search responses and
    /// beacons. Raises ValueError on an invalid IP string.
    fn advertise_ip(mut slf: PyRefMut<'_, Self>, ip: String) -> PyResult<PyRefMut<'_, Self>> {
        let ip_addr: IpAddr = ip
            .parse()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.advertise_ip(ip_addr));
        Ok(slf)
    }

    /// Enable or disable automatic alarm computation from record limits.
    fn compute_alarms(mut slf: PyRefMut<'_, Self>, enabled: bool) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.compute_alarms(enabled));
        Ok(slf)
    }

    /// Set the UDP beacon period in seconds (float, rounded to whole
    /// seconds, minimum 1).
    fn beacon_period(mut slf: PyRefMut<'_, Self>, secs: f64) -> PyResult<PyRefMut<'_, Self>> {
        let b = take_builder(&mut slf)?;
        slf.builder = Some(b.beacon_period(secs.round().max(1.0) as u64));
        Ok(slf)
    }

    fn __repr__(&self) -> &'static str {
        if self.builder.is_some() {
            "<spvirit.ServerBuilder>"
        } else {
            "<spvirit.ServerBuilder (consumed)>"
        }
    }

    /// Register a Python-defined [`Source`].
    ///
    /// `source` is any Python object implementing `claim`, `get`, `put`,
    /// `names`, and (optionally) `rpc` / `on_start`.  See the
    /// `demo_source_*.py` examples for patterns.
    ///
    /// Lower `order` values are tried first during PV name resolution;
    /// the built-in record store is always at order 0.
    fn add_source(
        mut slf: PyRefMut<'_, Self>,
        label: String,
        order: i32,
        source: PyObject,
    ) -> PyResult<PyRefMut<'_, Self>> {
        let adapter = Arc::new(PySourceAdapter::new(source));
        slf.python_sources
            .push((label.clone(), order, adapter.clone()));
        let b = take_builder(&mut slf)?;
        // Cast to Arc<dyn Source> via Arc<PySourceAdapter>.
        let as_dyn: Arc<dyn spvirit_server::pvstore::Source> = adapter.clone();
        let label_for_hook = label.clone();
        let b = b.source(label, order, as_dyn);

        // Only sources that opt in get a `.FIELD` tier — otherwise every
        // Python source would start claiming dotted names it cannot answer.
        let has_fields =
            Python::with_gil(|py| adapter.obj.bind(py).hasattr("fields").unwrap_or(false));
        let b = if has_fields {
            let provider: Arc<dyn spvirit_server::field_provider::RecordFieldProvider> =
                adapter.clone();
            let field_source: Arc<dyn spvirit_server::pvstore::Source> =
                Arc::new(spvirit_server::record_fields::RecordFieldSource::new(provider));
            b.source(format!("{label_for_hook}-fields"), order + 10, field_source)
        } else {
            b
        };

        // Register the source's on_start on the shared hook list, so it
        // interleaves with builder-registered on_start hooks in true
        // registration order (the spec's "one list" rule). The notifier
        // does not exist yet at add_source time; the hook reads it lazily
        // from the cell that `build()` fills.
        let cell = slf.notifier_cell.clone();
        slf.builder = Some(b.on_start(move |_store| {
            let adapter = adapter.clone();
            let cell = cell.clone();
            let label = label_for_hook.clone();
            Box::pin(async move {
                if let Some(notifier) = cell.get() {
                    if let Err(e) = adapter.invoke_on_start(notifier.clone()) {
                        // Propagate as a panic so run_start_hooks aborts
                        // startup, naming the hook — the same rule that
                        // holds for a raising @builder.on_start. A raising
                        // source on_start is not a lesser citizen: letting
                        // startup silently proceed after it failed was the
                        // exact bug this hook exists to fix.
                        panic!("on_start hook for source '{label}' raised: {e}");
                    }
                } else {
                    // Should be unreachable: `build()` always fills the cell
                    // before any hook can fire. If it ever happens, the
                    // source's on_start silently never runs, so log loudly.
                    tracing::error!(
                        "on_start hook for source '{label}' fired before the notifier \
                         cell was filled; on_start was NOT invoked"
                    );
                }
            })
        }));
        Ok(slf)
    }

    /// Build and return a `Server` that can be started.
    fn build(&mut self) -> PyResult<PyServer> {
        let b = self
            .builder
            .take()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("builder already consumed"))?;
        let mut server = b.build();
        let store = server.store().clone();
        // Pre-create the monitor registry so Python sources can notify
        // PVAccess monitor subscribers before .run() starts.
        let registry = server.monitor_registry();
        let notifier = PyNotifier::new(registry);
        let sources = std::mem::take(&mut self.python_sources);
        // Hand the notifier to the deferred source on_start hooks registered
        // in add_source. They fire at server start (via run_start_hooks),
        // not here.
        let _ = self.notifier_cell.set(notifier.clone());
        let events = server.events().clone();
        Ok(PyServer {
            server: Some(server),
            store: Some(store),
            notifier: Some(notifier),
            post_build_sources: sources,
            events,
        })
    }
}

/// Returned by `builder.on_event("NAME")` — calling it with a function
/// registers that function and returns it unchanged, so it works as
/// `@builder.on_event("NAME")`.
#[pyclass]
pub struct PyEventDecorator {
    builder: PyObject,
    event: String,
}

#[pymethods]
impl PyEventDecorator {
    fn __call__(&self, py: Python<'_>, callback: PyObject) -> PyResult<PyObject> {
        self.builder.call_method1(
            py,
            "_add_event_handler",
            (self.event.clone(), callback.clone_ref(py)),
        )?;
        Ok(callback)
    }
}

// ─── Server ──────────────────────────────────────────────────────────────────

/// A PVAccess server. Construct with `Server(pvs=..., ...)` or
/// `ServerBuilder.build()`; start with `start()`, `run()`, or
/// `start_background()`.
#[pyclass(name = "Server")]
pub struct PyServer {
    server: Option<PvaServer>,
    store: Option<Arc<SimplePvStore>>,
    /// Notifier handed to each Python source so it can publish monitor updates.
    notifier: Option<PyNotifier>,
    /// Adapters for all Python sources registered on this server — kept alive
    /// so they outlive `run()`.
    #[allow(dead_code)]
    post_build_sources: Vec<(String, i32, Arc<PySourceAdapter>)>,
    /// Kept independently of `server` (which `run()`/`start_background()`
    /// consume) so `post_event`/`drain_events` keep working once the
    /// server is running.
    events: Arc<spvirit_server::events::Events>,
}

#[pymethods]
impl PyServer {
    /// Build a server from typed PV handles (`spvirit.ai(...)` etc.).
    ///
    /// `pvs` — list of `Pv` handles; `sources` — list of `(label, order, obj)`
    /// tuples of Python `Source` objects; remaining kwargs mirror
    /// `ServerBuilder` configuration.
    #[new]
    #[pyo3(signature = (*, pvs=None, db_file=None, db_string=None, sources=None,
                        port=None, udp_port=None, listen_ip=None, advertise_ip=None,
                        compute_alarms=None, beacon_period=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        pvs: Option<Vec<crate::pv::PyPv>>,
        db_file: Option<String>,
        db_string: Option<String>,
        sources: Option<Vec<(String, i32, PyObject)>>,
        port: Option<u16>,
        udp_port: Option<u16>,
        listen_ip: Option<String>,
        advertise_ip: Option<String>,
        compute_alarms: Option<bool>,
        beacon_period: Option<f64>,
    ) -> PyResult<Self> {
        let handles: Vec<spvirit_server::pv::AnyPv> =
            pvs.unwrap_or_default().iter().map(|p| p.any()).collect();
        let mut sb = PvaServer::serve(handles);
        if let Some(p) = db_file {
            sb = sb.db_file(p);
        }
        if let Some(s) = db_string {
            sb = sb.db_string(&s);
        }
        if let Some(p) = port {
            sb = sb.port(p);
        }
        if let Some(p) = udp_port {
            sb = sb.udp_port(p);
        }
        if let Some(ip) = listen_ip {
            let addr: IpAddr = ip
                .parse()
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
            sb = sb.listen_ip(addr);
        }
        if let Some(ip) = advertise_ip {
            let addr: IpAddr = ip
                .parse()
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
            sb = sb.advertise_ip(addr);
        }
        if let Some(c) = compute_alarms {
            sb = sb.compute_alarms(c);
        }
        if let Some(secs) = beacon_period {
            sb = sb.beacon_period(secs.round().max(1.0) as u64);
        }
        let notifier_cell: Arc<std::sync::OnceLock<PyNotifier>> =
            Arc::new(std::sync::OnceLock::new());
        let mut python_sources: Vec<(String, i32, Arc<PySourceAdapter>)> = Vec::new();
        for (label, order, obj) in sources.unwrap_or_default() {
            let adapter = Arc::new(PySourceAdapter::new(obj));
            python_sources.push((label.clone(), order, adapter.clone()));
            let as_dyn: Arc<dyn spvirit_server::pvstore::Source> = adapter.clone();
            let label_for_hook = label.clone();
            sb = sb.source(label, order, as_dyn);

            // Only sources that opt in get a `.FIELD` tier — otherwise every
            // Python source would start claiming dotted names it cannot answer.
            let has_fields =
                Python::with_gil(|py| adapter.obj.bind(py).hasattr("fields").unwrap_or(false));
            if has_fields {
                let provider: Arc<dyn spvirit_server::field_provider::RecordFieldProvider> =
                    adapter.clone();
                let field_source: Arc<dyn spvirit_server::pvstore::Source> = Arc::new(
                    spvirit_server::record_fields::RecordFieldSource::new(provider),
                );
                sb = sb.source(format!("{label_for_hook}-fields"), order + 10, field_source);
            }

            // Same deferred hook as PyServerBuilder::add_source: fires at
            // server start, not here, and interleaves on the shared list.
            let cell = notifier_cell.clone();
            sb = sb.on_start(move |_store| {
                let adapter = adapter.clone();
                let cell = cell.clone();
                let label = label_for_hook.clone();
                Box::pin(async move {
                    if let Some(notifier) = cell.get() {
                        if let Err(e) = adapter.invoke_on_start(notifier.clone()) {
                            panic!("on_start hook for source '{label}' raised: {e}");
                        }
                    } else {
                        // Should be unreachable: filled right after
                        // `sb.build()` below, before any hook can fire.
                        tracing::error!(
                            "on_start hook for source '{label}' fired before the notifier \
                             cell was filled; on_start was NOT invoked"
                        );
                    }
                })
            });
        }
        let mut server = py.allow_threads(|| RUNTIME.block_on(sb.build()));
        let store = server.store().clone();
        let registry = server.monitor_registry();
        let notifier = PyNotifier::new(registry);
        let _ = notifier_cell.set(notifier.clone());
        let events = server.events().clone();
        Ok(PyServer {
            server: Some(server),
            store: Some(store),
            notifier: Some(notifier),
            post_build_sources: python_sources,
            events,
        })
    }

    /// Return a fresh `ServerBuilder` (equivalent to `ServerBuilder()`).
    #[staticmethod]
    fn builder() -> PyServerBuilder {
        PyServerBuilder::new()
    }

    /// Start serving on a background thread (returns immediately).
    fn start(&mut self, py: Python<'_>) -> PyResult<()> {
        self.start_background(py).map(|_| ())
    }

    fn __repr__(&self) -> &'static str {
        if self.server.is_some() {
            "<spvirit.Server>"
        } else {
            "<spvirit.Server (running)>"
        }
    }

    /// Mint a typed handle to any served record (handle-built or .db-loaded).
    fn pv(&self, py: Python<'_>, name: String) -> PyResult<crate::pv::PyPv> {
        use crate::pv::{PvKind, PyPv, pv_err};
        use spvirit_types::{NtPayload, ScalarValue};
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?;
        let store = server.store().clone();
        let sniff = block_on_py(py, store.get_nt(&name));
        let kind = match sniff {
            None => {
                return Err(pyo3::exceptions::PyKeyError::new_err(format!(
                    "PV '{name}' not found"
                )));
            }
            Some(NtPayload::Scalar(nt)) => match nt.value {
                ScalarValue::F64(_) | ScalarValue::F32(_) => {
                    let h = block_on_py(py, server.pv::<f64>(&name)).map_err(pv_err)?;
                    PvKind::F64(h)
                }
                ScalarValue::Bool(_) => {
                    let h = block_on_py(py, server.pv::<bool>(&name)).map_err(pv_err)?;
                    PvKind::Bool(h)
                }
                ScalarValue::I8(_) | ScalarValue::I16(_) | ScalarValue::I32(_) => {
                    let h = block_on_py(py, server.pv::<i32>(&name)).map_err(pv_err)?;
                    PvKind::I32(h)
                }
                ScalarValue::Str(_) => {
                    let h = block_on_py(py, server.pv::<String>(&name)).map_err(pv_err)?;
                    PvKind::Str(h)
                }
                // long / ubyte / ushort / uint / ulong — dynamically typed handle.
                other => {
                    let code = crate::convert::scalar_value_type_code(&other);
                    let h = block_on_py(py, server.pv::<ScalarValue>(&name)).map_err(pv_err)?;
                    PvKind::Typed(h, code)
                }
            },
            Some(NtPayload::Enum(_)) => {
                let h = block_on_py(py, server.pv::<i32>(&name)).map_err(pv_err)?;
                PvKind::I32(h)
            }
            Some(NtPayload::ScalarArray(_)) => {
                let h = block_on_py(py, server.array_pv(&name)).map_err(pv_err)?;
                PvKind::Array(h)
            }
            Some(other) => {
                return Err(pyo3::exceptions::PyKeyError::new_err(format!(
                    "PV '{name}' has unsupported payload {other:?} for typed handles"
                )));
            }
        };
        Ok(PyPv { kind })
    }

    /// Get a handle to the PV store for runtime get/set.
    fn store(&self) -> PyResult<PyStore> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?
            .clone();
        Ok(PyStore { inner: store })
    }

    /// Return the monitor notifier for publishing updates from Python code.
    fn notifier(&self) -> PyResult<PyNotifier> {
        self.notifier
            .clone()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))
    }

    /// Register an additional Python source after build.  The source's
    /// `on_start(notifier)` (if defined) is invoked immediately, synchronously,
    /// rather than through the shared startup list.
    ///
    /// Only valid in the window between `build()` and `run()`/
    /// `start_background()`: `PvaServer::run()` builds its `SourceRegistry`
    /// internally and never hands it back, so once the server is running
    /// there is no live registry left for a late source to join. Calling
    /// this after the server has started raises `RuntimeError` — silently
    /// accepting the source and firing its `on_start` while leaving its PVs
    /// permanently unroutable would be a worse failure mode than an error.
    ///
    /// Note: a source added in this pre-`run()` window has its `on_start`
    /// fired **immediately**, synchronously, here — not queued onto the
    /// shared `on_start` hook list. That means it can run *before* hooks
    /// registered earlier via `@builder.on_start`/`ServeBuilder::on_start`,
    /// which violates "registration order, one list" for this narrow
    /// window. This is architecturally forced: `PvaServer::start_hooks` is
    /// private with no API to push onto it after `build()`. Prefer
    /// `ServerBuilder.add_source(...)` (before `build()`) when hook
    /// ordering relative to other `on_start` hooks matters.
    ///
    /// If the source's `on_start` raises here, there is no startup in
    /// flight to abort — this call happens synchronously in the gap between
    /// `build()` and `run()`/`start_background()`. The exception simply
    /// propagates as a normal Python exception out of this `add_source()`
    /// call; the source is not added to the registry and its `on_start` is
    /// not retried.
    fn add_source(&mut self, label: String, order: i32, source: PyObject) -> PyResult<()> {
        let server = self
            .server
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?;
        let adapter = Arc::new(PySourceAdapter::new(source));
        if let Some(notifier) = self.notifier.clone() {
            adapter.invoke_on_start(notifier)?;
        }
        let as_dyn: Arc<dyn spvirit_server::pvstore::Source> = adapter.clone();
        server.add_source(label.clone(), order, as_dyn);
        self.post_build_sources.push((label, order, adapter));
        Ok(())
    }

    /// Run the server (blocking). This does not return until the server stops.
    fn run(&mut self, py: Python<'_>) -> PyResult<()> {
        let server = self
            .server
            .take()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?;
        py.allow_threads(|| {
            RUNTIME
                .block_on(server.run())
                .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
        })
    }

    /// Start the server in a background thread and return the store handle.
    ///
    /// Runs every `on_start` hook synchronously before returning, so a
    /// raising hook surfaces here as a `RuntimeError` naming the hook,
    /// rather than only being logged from the background thread after this
    /// call has already returned successfully.
    fn start_background(&mut self, py: Python<'_>) -> PyResult<PyStore> {
        let server = self
            .server
            .take()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?
            .clone();

        block_on_py(py, server.run_start_hooks())
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;

        // Start the event dispatcher here rather than leaving it to
        // `serve_after_start_hooks` on the background thread: otherwise
        // `post_event()` / `drain_events()` immediately after this call race
        // the thread getting that far. `start_dispatcher` is idempotent, and
        // needs a runtime context because it spawns.
        {
            let _guard = RUNTIME.enter();
            server.events().start_dispatcher(store.clone());
        }

        std::thread::spawn(move || {
            if let Err(e) = RUNTIME.block_on(server.serve_after_start_hooks()) {
                tracing::error!("background server error: {e}");
            }
        });

        Ok(PyStore { inner: store })
    }

    /// Post a named event.
    ///
    /// Inline sinks (if any) are awaited to completion before this returns;
    /// handlers registered via `@builder.on_event(...)` are queued on the
    /// dispatcher. When this returns, records on that event have processed
    /// and handlers are queued — not necessarily run. Never assume a handler
    /// has finished by the time `post_event` returns; use `drain_events()`
    /// in tests if you need that.
    ///
    /// Stays synchronous by blocking the calling Python thread on the shared
    /// multi-threaded runtime, with the GIL released for the duration
    /// (`block_on_py`) — so the guarantee is genuinely upheld here, and a
    /// sink that needs the GIL cannot deadlock against the poster.
    fn post_event(&self, py: Python<'_>, name: String) -> PyResult<()> {
        let events = self.events.clone();
        block_on_py(py, async move { events.post(&name).await });
        Ok(())
    }

    /// Block until every queued event handler has finished running.
    ///
    /// Test-only: production code should not need to know when handlers
    /// have finished, only that `post_event` has queued them.
    fn drain_events(&self, py: Python<'_>) -> PyResult<()> {
        let events = self.events.clone();
        block_on_py(py, async move { events.drain().await });
        Ok(())
    }
}

// ─── Store ───────────────────────────────────────────────────────────────────

/// Name-keyed runtime access to the server's record store: get/set scalar,
/// array, and full NT values.
#[pyclass(name = "Store")]
pub struct PyStore {
    inner: Arc<SimplePvStore>,
}

#[pymethods]
impl PyStore {
    /// Get the current scalar value of a PV (returns None if not found).
    fn get_value(&self, py: Python<'_>, name: String) -> PyResult<PyObject> {
        let store = self.inner.clone();
        let val = block_on_py(py, store.get_value(&name));
        Ok(match val {
            Some(v) => scalar_to_py(py, &v),
            None => py.None(),
        })
    }

    /// Get the full NT payload for a PV (returns NtScalar, NtScalarArray, etc.).
    fn get_nt(&self, py: Python<'_>, name: String) -> PyResult<PyObject> {
        let store = self.inner.clone();
        let val = block_on_py(py, store.get_nt(&name));
        Ok(match val {
            Some(payload) => nt_payload_to_py(py, payload),
            None => py.None(),
        })
    }

    /// Set a scalar value on a PV, strictly coerced to the record's current
    /// wire type (the record is the authority — writes never retype it).
    /// Returns True if the PV exists.
    fn set_value(&self, py: Python<'_>, name: String, value: &Bound<'_, PyAny>) -> PyResult<bool> {
        let store = self.inner.clone();
        let sv = match block_on_py(py, store.get_value(&name)) {
            Some(current) => py_to_scalar_typed(value, scalar_value_type_code(&current))?,
            None => return Ok(false),
        };
        Ok(block_on_py(py, store.set_value(&name, sv)))
    }

    /// Set an array value on a PV, strictly coerced to the record's current
    /// element type. Returns True if the PV exists.
    fn set_array_value(
        &self,
        py: Python<'_>,
        name: String,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let store = self.inner.clone();
        let code = match block_on_py(py, store.get_nt(&name)) {
            Some(NtPayload::ScalarArray(nt)) => Some(scalar_array_type_code(&nt.value)),
            Some(NtPayload::NdArray(nt)) => Some(scalar_array_type_code(&nt.value)),
            Some(_) => None, // non-array record: keep inference, core decides
            None => return Ok(false),
        };
        let arr = match code {
            Some(c) => py_to_scalar_array_typed(value, c)?,
            None => py_to_scalar_array(value)?,
        };
        Ok(block_on_py(py, store.set_array_value(&name, arr)))
    }

    /// Write a full NT payload (NtScalar, NtScalarArray, etc.) to a PV. The
    /// payload's value is strictly coerced to the record's current wire type.
    /// Returns True if the PV exists.
    fn put_nt(&self, py: Python<'_>, name: String, nt: &Bound<'_, PyAny>) -> PyResult<bool> {
        let payload = py_to_nt_payload(nt)?;
        let store = self.inner.clone();
        let payload = match (block_on_py(py, store.get_nt(&name)), payload) {
            (None, _) => return Ok(false),
            (Some(NtPayload::Scalar(current)), NtPayload::Scalar(mut new)) => {
                new.value = coerce_scalar_value(new.value, scalar_value_type_code(&current.value))?;
                NtPayload::Scalar(new)
            }
            (Some(NtPayload::ScalarArray(current)), NtPayload::ScalarArray(mut new)) => {
                new.value =
                    coerce_scalar_array_value(new.value, scalar_array_type_code(&current.value))?;
                NtPayload::ScalarArray(new)
            }
            (_, p) => p, // table/ndarray/enum/generic or kind mismatch: core decides
        };
        Ok(block_on_py(py, store.put_nt(&name, payload)))
    }

    /// List all PV names in the store.
    fn pv_names(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let store = self.inner.clone();
        Ok(block_on_py(py, store.pv_names()))
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let store = self.inner.clone();
        let n = block_on_py(py, store.pv_names()).len();
        format!("<spvirit.Store ({n} PVs)>")
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Convert a Python value to a [`PvValue`].
///
/// Scalars (bool, int, float, str) become `PvValue::Scalar`.
/// Lists become `PvValue::ScalarArray`.
fn py_to_pv_value(obj: &Bound<'_, PyAny>) -> PyResult<spvirit_types::PvValue> {
    if let Ok(list) = obj.downcast::<pyo3::types::PyList>() {
        let arr = py_to_scalar_array(list.as_any())?;
        Ok(spvirit_types::PvValue::ScalarArray(arr))
    } else {
        let sv = py_to_scalar(obj)?;
        Ok(spvirit_types::PvValue::Scalar(sv))
    }
}

/// Typed variant of `py_to_pv_value`: `"short"` coerces a scalar,
/// `"short[]"` coerces an array.
fn py_to_pv_value_typed(obj: &Bound<'_, PyAny>, ty: &str) -> PyResult<spvirit_types::PvValue> {
    let t = ty.trim();
    if let Some(base) = t.strip_suffix("[]") {
        let code = parse_scalar_type(base)?;
        Ok(spvirit_types::PvValue::ScalarArray(py_to_scalar_array_typed(obj, code)?))
    } else {
        let code = parse_scalar_type(t)?;
        Ok(spvirit_types::PvValue::Scalar(py_to_scalar_typed(obj, code)?))
    }
}
