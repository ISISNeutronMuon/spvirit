//! Python server wrappers — sync-only for phase 1.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use pyo3::prelude::*;

use spvirit_codec::spvd_decode::DecodedValue;
use spvirit_server::SimplePvStore;
use spvirit_server::pva_server::PvaServer;
use spvirit_types::{ScalarArrayValue, ScalarValue};

use crate::convert::{decoded_to_py, py_to_scalar, py_to_scalar_array, scalar_to_py};
use crate::nt::{nt_payload_to_py, py_to_nt_payload};
use crate::runtime::RUNTIME;
use crate::source::{PyNotifier, PySourceAdapter};

// ─── ServerBuilder ───────────────────────────────────────────────────────────

#[pyclass(name = "ServerBuilder")]
pub struct PyServerBuilder {
    builder: Option<spvirit_server::PvaServerBuilder>,
    /// Python sources to wire up on build (label, order, adapter).
    python_sources: Vec<(String, i32, Arc<PySourceAdapter>)>,
}

#[pymethods]
impl PyServerBuilder {
    #[new]
    fn new() -> Self {
        Self {
            builder: Some(PvaServer::builder()),
            python_sources: Vec::new(),
        }
    }

    fn ai(mut slf: PyRefMut<'_, Self>, name: String, initial: f64) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.ai(name, initial));
        slf
    }

    fn ao(mut slf: PyRefMut<'_, Self>, name: String, initial: f64) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.ao(name, initial));
        slf
    }

    fn bi(mut slf: PyRefMut<'_, Self>, name: String, initial: bool) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.bi(name, initial));
        slf
    }

    fn bo(mut slf: PyRefMut<'_, Self>, name: String, initial: bool) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.bo(name, initial));
        slf
    }

    fn string_in(mut slf: PyRefMut<'_, Self>, name: String, initial: String) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.string_in(name, initial));
        slf
    }

    fn string_out(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        initial: String,
    ) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.string_out(name, initial));
        slf
    }

    fn waveform<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array(data)?;
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.waveform(name, arr));
        Ok(slf)
    }

    fn aai<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array(data)?;
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.aai(name, arr));
        Ok(slf)
    }

    fn aao<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array(data)?;
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.aao(name, arr));
        Ok(slf)
    }

    #[pyo3(signature = (name, data, indx=0, nelm=None))]
    fn sub_array<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
        indx: usize,
        nelm: Option<usize>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array(data)?;
        let n = nelm.unwrap_or(arr.len());
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.sub_array(name, arr, indx, n));
        Ok(slf)
    }

    fn nt_table<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        columns: &Bound<'py, PyAny>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let dict = columns.downcast::<pyo3::types::PyDict>().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err("columns must be a dict of {name: list}")
        })?;
        let mut cols: Vec<(String, ScalarArrayValue)> = Vec::new();
        for (key, val) in dict.iter() {
            let col_name: String = key.extract()?;
            let col_data = py_to_scalar_array(&val)?;
            cols.push((col_name, col_data));
        }
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.nt_table(name, cols));
        Ok(slf)
    }

    fn nt_ndarray<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        data: &Bound<'py, PyAny>,
        dims: Vec<(i32, i32)>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let arr = py_to_scalar_array(data)?;
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.nt_ndarray(name, arr, dims));
        Ok(slf)
    }

    fn mbbi(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        choices: Vec<String>,
        initial: i32,
    ) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.mbbi(name, choices, initial));
        slf
    }

    fn mbbo(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        choices: Vec<String>,
        initial: i32,
    ) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.mbbo(name, choices, initial));
        slf
    }

    fn generic<'py>(
        mut slf: PyRefMut<'py, Self>,
        name: String,
        struct_id: String,
        fields: &Bound<'py, pyo3::types::PyDict>,
    ) -> PyResult<PyRefMut<'py, Self>> {
        let mut field_vec: Vec<(String, spvirit_types::PvValue)> = Vec::new();
        for (key, val) in fields.iter() {
            let field_name: String = key.extract()?;
            let pv_val = py_to_pv_value(&val)?;
            field_vec.push((field_name, pv_val));
        }
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.generic(name, struct_id, field_vec));
        Ok(slf)
    }

    fn db_file(mut slf: PyRefMut<'_, Self>, path: String) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.db_file(path));
        slf
    }

    fn db_string(mut slf: PyRefMut<'_, Self>, content: String) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.db_string(&content));
        slf
    }

    fn on_put(mut slf: PyRefMut<'_, Self>, name: String, callback: PyObject) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
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
        slf
    }

    fn scan(
        mut slf: PyRefMut<'_, Self>,
        name: String,
        period_secs: f64,
        callback: PyObject,
    ) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        let dur = Duration::from_secs_f64(period_secs);
        slf.builder = Some(b.scan(name, dur, move |pv_name: &str| {
            Python::with_gil(|py| match callback.call1(py, (pv_name,)) {
                Ok(ret) => py_to_scalar(ret.bind(py)).unwrap_or(ScalarValue::F64(0.0)),
                Err(e) => {
                    tracing::error!("scan callback error: {e}");
                    ScalarValue::F64(0.0)
                }
            })
        }));
        slf
    }

    fn port(mut slf: PyRefMut<'_, Self>, port: u16) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.port(port));
        slf
    }

    fn udp_port(mut slf: PyRefMut<'_, Self>, port: u16) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.udp_port(port));
        slf
    }

    fn listen_ip(mut slf: PyRefMut<'_, Self>, ip: String) -> PyResult<PyRefMut<'_, Self>> {
        let ip_addr: IpAddr = ip
            .parse()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.listen_ip(ip_addr));
        Ok(slf)
    }

    fn advertise_ip(mut slf: PyRefMut<'_, Self>, ip: String) -> PyResult<PyRefMut<'_, Self>> {
        let ip_addr: IpAddr = ip
            .parse()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.advertise_ip(ip_addr));
        Ok(slf)
    }

    fn compute_alarms(mut slf: PyRefMut<'_, Self>, enabled: bool) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.compute_alarms(enabled));
        slf
    }

    fn beacon_period(mut slf: PyRefMut<'_, Self>, secs: u64) -> PyRefMut<'_, Self> {
        let b = slf.builder.take().expect("builder consumed");
        slf.builder = Some(b.beacon_period(secs));
        slf
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
    ) -> PyRefMut<'_, Self> {
        let adapter = Arc::new(PySourceAdapter::new(source));
        slf.python_sources
            .push((label.clone(), order, adapter.clone()));
        let b = slf.builder.take().expect("builder consumed");
        // Cast to Arc<dyn Source> via Arc<PySourceAdapter>.
        let as_dyn: Arc<dyn spvirit_server::pvstore::Source> = adapter;
        slf.builder = Some(b.source(label, order, as_dyn));
        slf
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
        // Invoke `on_start(notifier)` on every Python source that defines it.
        for (_, _, adapter) in &sources {
            adapter.invoke_on_start(notifier.clone());
        }
        Ok(PyServer {
            server: Some(server),
            store: Some(store),
            notifier: Some(notifier),
            post_build_sources: sources,
        })
    }
}

// ─── Server ──────────────────────────────────────────────────────────────────

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
                        port=None, udp_port=None, listen_ip=None, compute_alarms=None))]
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
        compute_alarms: Option<bool>,
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
        if let Some(c) = compute_alarms {
            sb = sb.compute_alarms(c);
        }
        let mut python_sources: Vec<(String, i32, Arc<PySourceAdapter>)> = Vec::new();
        for (label, order, obj) in sources.unwrap_or_default() {
            let adapter = Arc::new(PySourceAdapter::new(obj));
            python_sources.push((label.clone(), order, adapter.clone()));
            let as_dyn: Arc<dyn spvirit_server::pvstore::Source> = adapter;
            sb = sb.source(label, order, as_dyn);
        }
        let mut server = py.allow_threads(|| RUNTIME.block_on(sb.build()));
        let store = server.store().clone();
        let registry = server.monitor_registry();
        let notifier = PyNotifier::new(registry);
        for (_, _, adapter) in &python_sources {
            adapter.invoke_on_start(notifier.clone());
        }
        Ok(PyServer {
            server: Some(server),
            store: Some(store),
            notifier: Some(notifier),
            post_build_sources: python_sources,
        })
    }

    /// Start serving on a background thread (returns immediately).
    fn start(&mut self) -> PyResult<()> {
        self.start_background().map(|_| ())
    }

    /// Mint a typed handle to any served record (handle-built or .db-loaded).
    fn pv(&self, py: Python<'_>, name: String) -> PyResult<crate::pv::PyPv> {
        use crate::pv::{PvKind, PyPv, pv_err};
        use spvirit_types::ScalarValue;
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?;
        let store = server.store().clone();
        let sniff = py.allow_threads(|| RUNTIME.block_on(store.get_value(&name)));
        let kind = match sniff {
            None => {
                return Err(pyo3::exceptions::PyKeyError::new_err(format!(
                    "PV '{name}' not found"
                )));
            }
            Some(ScalarValue::F64(_)) | Some(ScalarValue::F32(_)) => {
                let h = py
                    .allow_threads(|| RUNTIME.block_on(server.pv::<f64>(&name)))
                    .map_err(pv_err)?;
                PvKind::F64(h)
            }
            Some(ScalarValue::Bool(_)) => {
                let h = py
                    .allow_threads(|| RUNTIME.block_on(server.pv::<bool>(&name)))
                    .map_err(pv_err)?;
                PvKind::Bool(h)
            }
            Some(ScalarValue::I8(_)) | Some(ScalarValue::I16(_)) | Some(ScalarValue::I32(_)) => {
                let h = py
                    .allow_threads(|| RUNTIME.block_on(server.pv::<i32>(&name)))
                    .map_err(pv_err)?;
                PvKind::I32(h)
            }
            Some(ScalarValue::Str(_)) => {
                let h = py
                    .allow_threads(|| RUNTIME.block_on(server.pv::<String>(&name)))
                    .map_err(pv_err)?;
                PvKind::Str(h)
            }
            Some(other) => {
                return Err(pyo3::exceptions::PyKeyError::new_err(format!(
                    "PV '{name}' has unsupported value type {other:?} for typed handles"
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
    /// `on_start(notifier)` (if defined) is invoked immediately.
    fn add_source(&mut self, label: String, order: i32, source: PyObject) -> PyResult<()> {
        let server = self
            .server
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("server already consumed"))?;
        let adapter = Arc::new(PySourceAdapter::new(source));
        if let Some(notifier) = self.notifier.clone() {
            adapter.invoke_on_start(notifier);
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

#[pyclass(name = "Store")]
pub struct PyStore {
    inner: Arc<SimplePvStore>,
}

#[pymethods]
impl PyStore {
    /// Get the current scalar value of a PV (returns None if not found).
    fn get_value(&self, py: Python<'_>, name: String) -> PyResult<PyObject> {
        let store = self.inner.clone();
        let val = py.allow_threads(|| RUNTIME.block_on(store.get_value(&name)));
        Ok(match val {
            Some(v) => scalar_to_py(py, &v),
            None => py.None(),
        })
    }

    /// Get the full NT payload for a PV (returns NtScalar, NtScalarArray, etc.).
    fn get_nt(&self, py: Python<'_>, name: String) -> PyResult<PyObject> {
        let store = self.inner.clone();
        let val = py.allow_threads(|| RUNTIME.block_on(store.get_nt(&name)));
        Ok(match val {
            Some(payload) => nt_payload_to_py(py, payload),
            None => py.None(),
        })
    }

    /// Set a scalar value on a PV. Returns True if the PV exists.
    fn set_value(&self, py: Python<'_>, name: String, value: &Bound<'_, PyAny>) -> PyResult<bool> {
        let sv = py_to_scalar(value)?;
        let store = self.inner.clone();
        Ok(py.allow_threads(|| RUNTIME.block_on(store.set_value(&name, sv))))
    }

    /// Set an array value on a PV. Returns True if the PV exists.
    fn set_array_value(
        &self,
        py: Python<'_>,
        name: String,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        let arr = py_to_scalar_array(value)?;
        let store = self.inner.clone();
        Ok(py.allow_threads(|| RUNTIME.block_on(store.set_array_value(&name, arr))))
    }

    /// Write a full NT payload (NtScalar, NtScalarArray, etc.) to a PV.
    /// Returns True if the PV exists.
    fn put_nt(&self, py: Python<'_>, name: String, nt: &Bound<'_, PyAny>) -> PyResult<bool> {
        let payload = py_to_nt_payload(nt)?;
        let store = self.inner.clone();
        Ok(py.allow_threads(|| RUNTIME.block_on(store.put_nt(&name, payload))))
    }

    /// List all PV names in the store.
    fn pv_names(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        let store = self.inner.clone();
        Ok(py.allow_threads(|| RUNTIME.block_on(store.pv_names())))
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
