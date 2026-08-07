//! Python client wrappers — sync-only for phase 1.

use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use spvirit_client::pva_client::PvaClient;
use spvirit_client::search::{build_auto_broadcast_targets, discover_servers};

use crate::convert::{decoded_to_py, py_to_json};
use crate::errors::to_py_err;
use crate::runtime::RUNTIME;

// ─── GetResult ───────────────────────────────────────────────────────────────

/// Result of a get operation: `.pv_name`, `.value` (decoded Python value),
/// `.raw_pva` (raw PVA frame bytes), `.raw_pvd` (raw pvData body bytes).
#[pyclass(name = "GetResult")]
pub struct PyGetResult {
    /// Name of the PV this result was read from.
    #[pyo3(get)]
    pub pv_name: String,
    value: PyObject,
    /// Raw PVA frame bytes of the get response.
    #[pyo3(get)]
    pub raw_pva: Vec<u8>,
    /// Raw pvData body bytes of the get response.
    #[pyo3(get)]
    pub raw_pvd: Vec<u8>,
}

impl PyGetResult {
    pub(crate) fn new(
        pv_name: String,
        value: PyObject,
        raw_pva: Vec<u8>,
        raw_pvd: Vec<u8>,
    ) -> Self {
        Self {
            pv_name,
            value,
            raw_pva,
            raw_pvd,
        }
    }
}

#[pymethods]
impl PyGetResult {
    /// Decoded Python value (usually a dict mirroring the NT structure).
    #[getter]
    fn value(&self, py: Python<'_>) -> PyObject {
        self.value.clone_ref(py)
    }

    fn __repr__(&self) -> String {
        format!("GetResult(pv_name={:?})", self.pv_name)
    }
}

// ─── DiscoveredServer ────────────────────────────────────────────────────────

/// A server found by `discover_servers()`: `.guid` (12-byte bytes) and
/// `.tcp_addr` (`"ip:port"`).
#[pyclass(name = "DiscoveredServer")]
#[derive(Clone)]
pub struct PyDiscoveredServer {
    /// Server GUID (12 bytes).
    #[pyo3(get)]
    pub guid: Vec<u8>,
    /// Server TCP address as `"ip:port"`.
    #[pyo3(get)]
    pub tcp_addr: String,
}

#[pymethods]
impl PyDiscoveredServer {
    fn __repr__(&self) -> String {
        format!("DiscoveredServer(tcp_addr={:?})", self.tcp_addr)
    }
}

// ─── ClientBuilder ───────────────────────────────────────────────────────────

/// Builder for `Client`. Defaults: TCP 5075, UDP 5076, timeout 5.0 s,
/// broadcast search enabled.
#[pyclass(name = "ClientBuilder")]
pub struct PyClientBuilder {
    udp_port: u16,
    tcp_port: u16,
    timeout_secs: f64,
    no_broadcast: bool,
    name_servers: Vec<String>,
    authnz_user: Option<String>,
    authnz_host: Option<String>,
    server_addr: Option<String>,
    search_addr: Option<String>,
    bind_addr: Option<String>,
    debug: bool,
}

#[pymethods]
impl PyClientBuilder {
    #[new]
    fn new() -> Self {
        Self {
            udp_port: 5076,
            tcp_port: 5075,
            timeout_secs: 5.0,
            no_broadcast: false,
            name_servers: Vec::new(),
            authnz_user: None,
            authnz_host: None,
            server_addr: None,
            search_addr: None,
            bind_addr: None,
            debug: false,
        }
    }

    /// Set the default TCP port used when connecting to servers.
    fn port(mut slf: PyRefMut<'_, Self>, port: u16) -> PyRefMut<'_, Self> {
        slf.tcp_port = port;
        slf
    }

    /// Set the UDP port used for broadcast search.
    fn udp_port(mut slf: PyRefMut<'_, Self>, port: u16) -> PyRefMut<'_, Self> {
        slf.udp_port = port;
        slf
    }

    /// Set the operation timeout in seconds.
    fn timeout(mut slf: PyRefMut<'_, Self>, secs: f64) -> PyRefMut<'_, Self> {
        slf.timeout_secs = secs;
        slf
    }

    /// Disable UDP broadcast search when `enabled` is True (use name servers
    /// or a fixed server address instead).
    fn no_broadcast(mut slf: PyRefMut<'_, Self>, enabled: bool) -> PyRefMut<'_, Self> {
        slf.no_broadcast = enabled;
        slf
    }

    /// Add a name server `"ip:port"` address to query for PV names.
    /// Raises ValueError on an invalid address.
    fn name_server(mut slf: PyRefMut<'_, Self>, addr: String) -> PyResult<PyRefMut<'_, Self>> {
        let _: SocketAddr = addr.parse().map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid address: {e}"))
        })?;
        slf.name_servers.push(addr);
        Ok(slf)
    }

    /// Set the user name reported during connection authentication.
    fn authnz_user(mut slf: PyRefMut<'_, Self>, user: String) -> PyRefMut<'_, Self> {
        slf.authnz_user = Some(user);
        slf
    }

    /// Set the host name reported during connection authentication.
    fn authnz_host(mut slf: PyRefMut<'_, Self>, host: String) -> PyRefMut<'_, Self> {
        slf.authnz_host = Some(host);
        slf
    }

    /// Connect directly to the server at `"ip:port"`, skipping search.
    /// Raises ValueError on an invalid address.
    fn server_addr(mut slf: PyRefMut<'_, Self>, addr: String) -> PyResult<PyRefMut<'_, Self>> {
        let _: SocketAddr = addr.parse().map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid address: {e}"))
        })?;
        slf.server_addr = Some(addr);
        Ok(slf)
    }

    /// Set the IP address search requests are sent to (validated at
    /// `build()`; ValueError on a bad IP).
    fn search_addr(mut slf: PyRefMut<'_, Self>, addr: String) -> PyRefMut<'_, Self> {
        slf.search_addr = Some(addr);
        slf
    }

    /// Set the local IP address to bind the search socket to (validated at
    /// `build()`; ValueError on a bad IP).
    fn bind_addr(mut slf: PyRefMut<'_, Self>, addr: String) -> PyRefMut<'_, Self> {
        slf.bind_addr = Some(addr);
        slf
    }

    /// Enable verbose protocol debug logging.
    fn debug(mut slf: PyRefMut<'_, Self>, enabled: bool) -> PyRefMut<'_, Self> {
        slf.debug = enabled;
        slf
    }

    /// Build and return a configured `Client`. Raises ValueError if any
    /// stored address string fails to parse.
    fn build(&self) -> PyResult<PyClient> {
        let mut b = PvaClient::builder()
            .port(self.tcp_port)
            .udp_port(self.udp_port)
            .timeout(Duration::from_secs_f64(self.timeout_secs));
        if self.no_broadcast {
            b = b.no_broadcast();
        }
        for ns in &self.name_servers {
            let addr: SocketAddr = ns.parse().map_err(|e| {
                pyo3::exceptions::PyValueError::new_err(format!("invalid address: {e}"))
            })?;
            b = b.name_server(addr);
        }
        if let Some(ref user) = self.authnz_user {
            b = b.authnz_user(user);
        }
        if let Some(ref host) = self.authnz_host {
            b = b.authnz_host(host);
        }
        if let Some(ref addr) = self.server_addr {
            let sa: SocketAddr = addr.parse().map_err(|e| {
                pyo3::exceptions::PyValueError::new_err(format!("invalid address: {e}"))
            })?;
            b = b.server_addr(sa);
        }
        if let Some(ref addr) = self.search_addr {
            let ip: std::net::IpAddr = addr
                .parse()
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
            b = b.search_addr(ip);
        }
        if let Some(ref addr) = self.bind_addr {
            let ip: std::net::IpAddr = addr
                .parse()
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))?;
            b = b.bind_addr(ip);
        }
        if self.debug {
            b = b.debug();
        }
        Ok(PyClient { inner: b.build() })
    }

    fn __repr__(&self) -> String {
        format!(
            "<spvirit.ClientBuilder (tcp={}, udp={}, timeout={}s)>",
            self.tcp_port, self.udp_port, self.timeout_secs
        )
    }
}

// ─── Client ──────────────────────────────────────────────────────────────────

/// High-level PVAccess client: get/put/monitor/subscribe/info/pvlist.
/// `Client()` uses broadcast-search defaults; `Client.builder()` configures.
/// Operations raise the SpviritError hierarchy.
#[pyclass(name = "Client")]
pub struct PyClient {
    inner: PvaClient,
}

#[pymethods]
impl PyClient {
    #[new]
    fn new() -> Self {
        Self {
            inner: PvaClient::builder().build(),
        }
    }

    /// Create a builder for fine-grained configuration.
    #[staticmethod]
    fn builder() -> PyClientBuilder {
        PyClientBuilder::new()
    }

    /// Fetch the current value of a PV (blocking).
    ///
    /// If `fields` is provided (a list of dotted paths or a single string,
    /// e.g. `["value", "alarm.severity"]`), the pvRequest restricts the
    /// returned structure to those paths.
    #[pyo3(signature = (pv_name, fields=None))]
    fn get(
        &self,
        py: Python<'_>,
        pv_name: String,
        fields: Option<PyObject>,
    ) -> PyResult<PyGetResult> {
        let client = self.inner.clone();
        let fields = crate::channel::normalize_fields(py, fields)?;
        let result = crate::runtime::block_on_py(py, async {
            if fields.is_empty() {
                client.pvget(&pv_name).await
            } else {
                let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
                client.pvget_fields(&pv_name, &refs).await
            }
        })
        .map_err(to_py_err)?;
        let value = decoded_to_py(py, &result.value);
        Ok(PyGetResult {
            pv_name: result.pv_name,
            value,
            raw_pva: result.raw_pva,
            raw_pvd: result.raw_pvd,
        })
    }

    /// Write a value to a PV (blocking).
    ///
    /// `fields` selects which pvRequest fields are targeted (a list of
    /// dotted paths or a single string). Defaults to `["value"]` when
    /// omitted.
    #[pyo3(signature = (pv_name, value, fields=None))]
    fn put(
        &self,
        py: Python<'_>,
        pv_name: String,
        value: PyObject,
        fields: Option<PyObject>,
    ) -> PyResult<()> {
        let json_val = py_to_json(value.bind(py))?;
        let client = self.inner.clone();
        let fields = crate::channel::normalize_fields(py, fields)?;
        crate::runtime::block_on_py(py, async {
            if fields.is_empty() {
                client.pvput(&pv_name, json_val).await
            } else {
                let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
                client.pvput_fields(&pv_name, json_val, &refs).await
            }
        })
        .map_err(to_py_err)
    }

    /// Subscribe to a PV and call `callback(value_dict)` for each update.
    ///
    /// Blocks (GIL released between updates) until the callback returns
    /// `False` or raises — a raised exception stops the monitor and
    /// propagates to the caller. `fields` restricts the subscription to the
    /// given dotted paths. For a non-blocking variant see `subscribe`.
    #[pyo3(signature = (pv_name, callback, fields=None))]
    fn monitor(
        &self,
        py: Python<'_>,
        pv_name: String,
        callback: PyObject,
        fields: Option<PyObject>,
    ) -> PyResult<()> {
        let client = self.inner.clone();
        let fields = crate::channel::normalize_fields(py, fields)?;
        let mut cb_err: Option<PyErr> = None;
        // The GIL is released for the wait; the callback reacquires it per
        // update via with_gil.
        let result = crate::runtime::block_on_py(py, {
            let cb_err = &mut cb_err;
            async move {
                let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
                client
                    .pvmonitor_fields(&pv_name, &refs, |update| {
                        let keep_going = Python::with_gil(|py| {
                            let py_val = decoded_to_py(py, &update.value);
                            match callback.call1(py, (py_val,)) {
                                Ok(ret) => {
                                    // If callback returns False, stop
                                    ret.extract::<bool>(py).unwrap_or(true)
                                }
                                Err(e) => {
                                    *cb_err = Some(e);
                                    false
                                }
                            }
                        });
                        if keep_going {
                            ControlFlow::Continue(())
                        } else {
                            ControlFlow::Break(())
                        }
                    })
                    .await
            }
        });
        if let Some(e) = cb_err {
            return Err(e);
        }
        result.map_err(to_py_err)
    }

    /// Retrieve introspection (field description) for a PV.
    fn info(&self, py: Python<'_>, pv_name: String) -> PyResult<PyObject> {
        let client = self.inner.clone();
        let desc = crate::runtime::block_on_py(py, client.pvinfo(&pv_name)).map_err(to_py_err)?;
        // Return as a dict: {struct_id, fields: [{name, field_type}, ...]}
        let dict = PyDict::new(py);
        dict.set_item("struct_id", &desc.struct_id)?;
        let fields: Vec<PyObject> = desc
            .fields
            .iter()
            .map(|f| {
                let fd = PyDict::new(py);
                fd.set_item("name", &f.name).expect("set");
                fd.set_item("field_type", format!("{:?}", f.field_type))
                    .expect("set");
                fd.into_any().unbind()
            })
            .collect();
        dict.set_item("fields", PyList::new(py, &fields)?)?;
        Ok(dict.into_any().unbind())
    }

    /// List PV names from a specific server.
    fn pvlist(&self, py: Python<'_>, server_addr: String) -> PyResult<Vec<String>> {
        let addr: SocketAddr = server_addr.parse().map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid address: {e}"))
        })?;
        let client = self.inner.clone();
        crate::runtime::block_on_py(py, client.pvlist(addr)).map_err(to_py_err)
    }

    /// Subscribe to a PV without blocking; returns a `Subscription` handle.
    ///
    /// `callback(value)` runs on a background runtime thread for each update,
    /// sequentially per subscription. Returning `False` from the callback
    /// unsubscribes, matching `monitor`; raising also unsubscribes and stores
    /// the message in `subscription.error`. Call `subscription.close()` to
    /// stop promptly — it works even while the PV is quiet. If the
    /// subscription ends on a network/protocol error, `subscription.error`
    /// holds the message and `is_active` becomes `False`.
    #[pyo3(signature = (pv_name, callback, fields=None))]
    fn subscribe(
        &self,
        py: Python<'_>,
        pv_name: String,
        callback: PyObject,
        fields: Option<PyObject>,
    ) -> PyResult<PySubscription> {
        let client = self.inner.clone();
        let fields = crate::channel::normalize_fields(py, fields)?;
        let state = Arc::new(SubscriptionState {
            active: AtomicBool::new(true),
            error: Mutex::new(None),
        });
        let task_state = state.clone();
        let task_pv = pv_name.clone();
        let handle = RUNTIME.spawn(async move {
            let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
            let result = client
                .pvmonitor_fields(&task_pv, &refs, |update| {
                    let keep_going = Python::with_gil(|py| {
                        let py_val = decoded_to_py(py, &update.value);
                        match callback.call1(py, (py_val,)) {
                            Ok(ret) => ret.extract::<bool>(py).unwrap_or(true),
                            Err(e) => {
                                // No caller to raise into: record the failure
                                // on the subscription and stop.
                                *task_state.error.lock().unwrap() = Some(e.to_string());
                                false
                            }
                        }
                    });
                    if keep_going {
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(())
                    }
                })
                .await;
            if let Err(e) = result {
                *task_state.error.lock().unwrap() = Some(e.to_string());
            }
            task_state.active.store(false, Ordering::SeqCst);
        });
        Ok(PySubscription {
            pv_name,
            state,
            handle: Mutex::new(Some(handle)),
        })
    }

    fn __repr__(&self) -> &'static str {
        "<spvirit.Client>"
    }
}

// ─── Subscription ────────────────────────────────────────────────────────────

struct SubscriptionState {
    active: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Handle to a non-blocking monitor started with `Client.subscribe`.
#[pyclass(name = "Subscription")]
pub struct PySubscription {
    pv_name: String,
    state: Arc<SubscriptionState>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl PySubscription {
    fn abort_task(&self) {
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.abort();
        }
    }
}

#[pymethods]
impl PySubscription {
    /// Name of the PV this subscription watches.
    #[getter]
    fn pv_name(&self) -> &str {
        &self.pv_name
    }

    /// True while updates are still being delivered.
    #[getter]
    fn is_active(&self) -> bool {
        self.state.active.load(Ordering::SeqCst)
    }

    /// Error message if the subscription ended on a failure, else None.
    #[getter]
    fn error(&self) -> Option<String> {
        self.state.error.lock().unwrap().clone()
    }

    /// Stop the subscription. Idempotent; returns immediately and works
    /// even while the PV is quiet.
    fn close(&self) {
        self.abort_task();
        self.state.active.store(false, Ordering::SeqCst);
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __exit__(
        &self,
        _exc_type: Option<PyObject>,
        _exc: Option<PyObject>,
        _tb: Option<PyObject>,
    ) -> bool {
        self.close();
        false
    }

    fn __repr__(&self) -> String {
        let status = if self.is_active() { "active" } else { "closed" };
        format!("<spvirit.Subscription '{}' ({status})>", self.pv_name)
    }
}

impl Drop for PySubscription {
    // A subscription nobody holds a handle to is uncontrollable; stop it
    // rather than leak a detached task.
    fn drop(&mut self) {
        self.abort_task();
    }
}

// ─── discover_servers ────────────────────────────────────────────────────────

/// Discover PVA servers on the network via UDP broadcast search.
///
/// `udp_port` (default 5076) is the search port, `timeout` (default 2.0 s)
/// how long to collect responses, `debug` enables verbose logging.
/// Returns a list of `DiscoveredServer`.
#[pyfunction]
#[pyo3(signature = (udp_port=5076, timeout=2.0, debug=false))]
pub fn py_discover_servers(
    py: Python<'_>,
    udp_port: u16,
    timeout: f64,
    debug: bool,
) -> PyResult<Vec<PyDiscoveredServer>> {
    let targets = build_auto_broadcast_targets();
    let dur = Duration::from_secs_f64(timeout);
    let servers = crate::runtime::block_on_py(py, discover_servers(udp_port, dur, &targets, debug))
        .map_err(to_py_err)?;
    Ok(servers
        .into_iter()
        .map(|s| PyDiscoveredServer {
            guid: s.guid.to_vec(),
            tcp_addr: s.tcp_addr.to_string(),
        })
        .collect())
}
