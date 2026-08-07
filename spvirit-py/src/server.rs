//! Python server wrappers — sync-only for phase 1.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use pyo3::prelude::*;

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
        let b = b.source(label, order, as_dyn);

        // Register the source's on_start on the shared hook list, so it
        // interleaves with builder-registered on_start hooks in true
        // registration order (the spec's "one list" rule). The notifier
        // does not exist yet at add_source time; the hook reads it lazily
        // from the cell that `build()` fills.
        let cell = slf.notifier_cell.clone();
        slf.builder = Some(b.on_start(move |_store| {
            let adapter = adapter.clone();
            let cell = cell.clone();
            Box::pin(async move {
                if let Some(notifier) = cell.get() {
                    adapter.invoke_on_start(notifier.clone());
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
        Ok(PyServer {
            server: Some(server),
            store: Some(store),
            notifier: Some(notifier),
            post_build_sources: sources,
        })
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
            sb = sb.source(label, order, as_dyn);

            // Same deferred hook as PyServerBuilder::add_source: fires at
            // server start, not here, and interleaves on the shared list.
            let cell = notifier_cell.clone();
            sb = sb.on_start(move |_store| {
                let adapter = adapter.clone();
                let cell = cell.clone();
                Box::pin(async move {
                    if let Some(notifier) = cell.get() {
                        adapter.invoke_on_start(notifier.clone());
                    }
                })
            });
        }
        let mut server = py.allow_threads(|| RUNTIME.block_on(sb.build()));
        let store = server.store().clone();
        let registry = server.monitor_registry();
        let notifier = PyNotifier::new(registry);
        let _ = notifier_cell.set(notifier.clone());
        Ok(PyServer {
            server: Some(server),
            store: Some(store),
            notifier: Some(notifier),
            post_build_sources: python_sources,
        })
    }

    /// Return a fresh `ServerBuilder` (equivalent to `ServerBuilder()`).
    #[staticmethod]
    fn builder() -> PyServerBuilder {
        PyServerBuilder::new()
    }

    /// Start serving on a background thread (returns immediately).
    fn start(&mut self) -> PyResult<()> {
        self.start_background().map(|_| ())
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
    /// `on_start(notifier)` (if defined) is invoked immediately: there is no
    /// startup left to join once the server has been built (or is already
    /// running), so this is the one path where the hook fires synchronously
    /// rather than through the shared startup list.
    fn add_source(&mut self, label: String, order: i32, source: PyObject) -> PyResult<()> {
        let adapter = Arc::new(PySourceAdapter::new(source));
        if let Some(notifier) = self.notifier.clone() {
            adapter.invoke_on_start(notifier);
        }
        let as_dyn: Arc<dyn spvirit_server::pvstore::Source> = adapter.clone();
        // If the server has already started (`run`/`start_background` took
        // it), there is no live registry left to join — the source's PVs
        // simply won't be routable. Still fire on_start above; that part
        // has no prerequisite on the server field.
        if let Some(server) = self.server.as_mut() {
            server.add_source(label.clone(), order, as_dyn);
        }
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
    fn start_background(&mut self) -> PyResult<PyStore> {
        let server = self
            .server
            .take()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?
            .clone();

        std::thread::spawn(move || {
            if let Err(e) = RUNTIME.block_on(server.run()) {
                tracing::error!("background server error: {e}");
            }
        });

        Ok(PyStore { inner: store })
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
