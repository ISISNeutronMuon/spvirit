//! Python-defined dynamic [`Source`] support.
//!
//! Lets Python code implement a PVAccess *source* — the same abstraction
//! used internally by the built-in store — so Python users can publish
//! arbitrary PV names computed on the fly, proxy other systems, add access
//! control, and so on.
//!
//! # Python API
//!
//! A *source* is any Python object that implements the duck-typed methods
//! below.  All methods may be either plain functions or `async def`
//! coroutines — the adapter detects awaitables automatically.
//!
//! ```text
//! class MySource:
//!     def claim(self, name): ...      # -> PvInfo | dict | None
//!     def get(self, name): ...        # -> NtScalar | ... | None
//!     def put(self, name, value): ... # -> dict[str, NtPayload] | list | None
//!     def names(self): ...            # -> Iterable[str]
//!     def rpc(self, name, args): ...  # optional -> NtPayload
//!     def subscribe(self, name): ...  # optional (ignored; use notifier)
//!     def on_start(self, notifier): ...# optional: receive PyNotifier
//! ```
//!
//! Register via `ServerBuilder.add_source(label, order, source)` before
//! build, or `Server.add_source(label, order, source)` after build.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict, PyList, PyTuple};
use tokio::sync::mpsc;

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::{PvInfo, Source};
use spvirit_types::NtPayload;

use crate::convert::decoded_to_py;
use crate::convert::parse_type_code;
use crate::nt::{nt_payload_to_py, py_to_nt_payload};
use crate::runtime::RUNTIME;

// ─── Type-string parsing ─────────────────────────────────────────────────────

/// Parse a type string like `"double"`, `"int"`, `"string"`, `"double[]"`,
/// `"string[]"`, or `"any"` into a [`FieldType`].
fn parse_field_type(s: &str) -> PyResult<FieldType> {
    let trimmed = s.trim();
    // array?
    if let Some(base) = trimmed.strip_suffix("[]") {
        let base = base.trim();
        if base == "string" || base == "str" {
            return Ok(FieldType::StringArray);
        }
        if let Some(tc) = parse_type_code(base) {
            return Ok(FieldType::ScalarArray(tc));
        }
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "unknown array element type: {base:?}"
        )));
    }
    if trimmed == "string" || trimmed == "str" {
        return Ok(FieldType::String);
    }
    if trimmed == "any" || trimmed == "variant" {
        return Ok(FieldType::Variant);
    }
    if let Some(tc) = parse_type_code(trimmed) {
        return Ok(FieldType::Scalar(tc));
    }
    Err(pyo3::exceptions::PyValueError::new_err(format!(
        "unknown field type: {trimmed:?}"
    )))
}

/// Build a [`StructureDesc`] from a Python `dict[str, str]` fields map.
fn dict_to_structure_desc(
    struct_id: Option<String>,
    fields: &Bound<'_, PyDict>,
) -> PyResult<StructureDesc> {
    let mut desc_fields = Vec::with_capacity(fields.len());
    for (key, val) in fields.iter() {
        let name: String = key.extract()?;
        let type_str: String = val.extract()?;
        let field_type = parse_field_type(&type_str)?;
        desc_fields.push(FieldDesc { name, field_type });
    }
    Ok(StructureDesc {
        struct_id,
        fields: desc_fields,
    })
}

// ─── PyPvInfo ────────────────────────────────────────────────────────────────

/// Describes a PV claimed by a Python source.  Returned from `claim()`.
///
/// ```python
/// return spvirit.PvInfo.nt_scalar("double", writable=True)
/// return spvirit.PvInfo("epics:nt/NTScalar:1.0", {"value": "double"}, writable=True)
/// ```
#[pyclass(name = "PvInfo")]
#[derive(Clone)]
pub struct PyPvInfo {
    pub inner: PvInfo,
}

#[pymethods]
impl PyPvInfo {
    /// Build a PvInfo for a generic structure.
    ///
    /// `fields` is a `{field_name: type_str}` dict where type strings are
    /// like `"double"`, `"int"`, `"string"`, `"double[]"`, or `"any"`.
    #[new]
    #[pyo3(signature = (struct_id, fields, writable=false))]
    fn new(struct_id: String, fields: &Bound<'_, PyDict>, writable: bool) -> PyResult<Self> {
        let desc = dict_to_structure_desc(Some(struct_id), fields)?;
        Ok(Self {
            inner: PvInfo {
                descriptor: desc,
                writable,
            },
        })
    }

    /// Build a PvInfo for an `NTScalar` of the given scalar type.
    #[staticmethod]
    #[pyo3(signature = (type_str, writable=false))]
    fn nt_scalar(type_str: &str, writable: bool) -> PyResult<Self> {
        let field_type = parse_field_type(type_str)?;
        Ok(Self {
            inner: PvInfo {
                descriptor: StructureDesc {
                    struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
                    fields: vec![FieldDesc {
                        name: "value".to_string(),
                        field_type,
                    }],
                },
                writable,
            },
        })
    }

    /// Build a PvInfo for an `NTScalarArray` of the given element type
    /// (pass the element type, e.g. `"double"`, NOT `"double[]"`).
    #[staticmethod]
    #[pyo3(signature = (element_type, writable=false))]
    fn nt_scalar_array(element_type: &str, writable: bool) -> PyResult<Self> {
        let array_spec = format!("{element_type}[]");
        let field_type = parse_field_type(&array_spec)?;
        Ok(Self {
            inner: PvInfo {
                descriptor: StructureDesc {
                    struct_id: Some("epics:nt/NTScalarArray:1.0".to_string()),
                    fields: vec![FieldDesc {
                        name: "value".to_string(),
                        field_type,
                    }],
                },
                writable,
            },
        })
    }

    /// True if the PV accepts writes from clients.
    #[getter]
    fn writable(&self) -> bool {
        self.inner.writable
    }

    /// Structure type ID of the claimed PV, or None.
    #[getter]
    fn struct_id(&self) -> Option<String> {
        self.inner.descriptor.struct_id.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "PvInfo(struct_id={:?}, fields={}, writable={})",
            self.inner.descriptor.struct_id,
            self.inner.descriptor.fields.len(),
            self.inner.writable
        )
    }
}

/// Extract a `PvInfo` from a Python object (either `PyPvInfo` or a dict).
fn py_to_pv_info(obj: &Bound<'_, PyAny>) -> PyResult<PvInfo> {
    if let Ok(info) = obj.downcast::<PyPvInfo>() {
        return Ok(info.borrow().inner.clone());
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let struct_id: Option<String> = match dict.get_item("struct_id")? {
            Some(v) if !v.is_none() => Some(v.extract()?),
            _ => None,
        };
        let writable: bool = match dict.get_item("writable")? {
            Some(v) if !v.is_none() => v.extract()?,
            _ => false,
        };
        let fields_obj = dict.get_item("fields")?.ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("PvInfo dict missing 'fields'")
        })?;
        let fields = fields_obj.downcast::<PyDict>().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err("PvInfo 'fields' must be a dict")
        })?;
        let desc = dict_to_structure_desc(struct_id, fields)?;
        return Ok(PvInfo {
            descriptor: desc,
            writable,
        });
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "expected PvInfo instance or dict with 'struct_id'/'fields'/'writable'",
    ))
}

// ─── PyNotifier ──────────────────────────────────────────────────────────────

/// Handle for publishing monitor updates to subscribed PVAccess clients.
///
/// Passed to sources via `source.on_start(notifier)`.  Call `notify(name, nt)`
/// from any Python thread to push a new value to all clients subscribed via
/// monitor.
#[pyclass(name = "Notifier")]
#[derive(Clone)]
pub struct PyNotifier {
    registry: Arc<MonitorRegistry>,
}

impl PyNotifier {
    pub fn new(registry: Arc<MonitorRegistry>) -> Self {
        Self { registry }
    }
}

#[pymethods]
impl PyNotifier {
    /// Publish a monitor update for `pv_name` with the given NT payload.
    ///
    /// Safe to call from any Python thread, including from inside a
    /// source callback that is already running on the Tokio runtime.
    fn notify(&self, py: Python<'_>, pv_name: String, nt: &Bound<'_, PyAny>) -> PyResult<()> {
        let payload = py_to_nt_payload(nt)?;
        let registry = self.registry.clone();
        py.allow_threads(|| {
            // If we're already inside the runtime, fire-and-forget spawn;
            // otherwise use the shared runtime's block_on.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    registry.notify_monitors(&pv_name, &payload).await;
                });
            } else {
                RUNTIME.block_on(async move {
                    registry.notify_monitors(&pv_name, &payload).await;
                });
            }
        });
        Ok(())
    }

    fn __repr__(&self) -> String {
        "Notifier(<MonitorRegistry>)".to_string()
    }
}

// ─── PySourceAdapter — Source trait impl ─────────────────────────────────────

pub struct PySourceAdapter {
    obj: Arc<PyObject>,
}

impl PySourceAdapter {
    pub fn new(obj: PyObject) -> Self {
        Self { obj: Arc::new(obj) }
    }

    /// If the user's Python object has an `on_start(notifier)` method, call
    /// it. Returns `Err` if the method raises (or doesn't exist as a
    /// callable) — the caller decides what a raise means in its context:
    /// aborting startup (deferred hooks on the shared start_hooks list) or
    /// propagating as a normal Python exception (the immediate
    /// `Server.add_source` window). This must NOT swallow-and-log: a
    /// swallowed exception here would let startup silently proceed after a
    /// source's on_start failed, which is exactly the bug this method
    /// used to have.
    pub fn invoke_on_start(&self, notifier: PyNotifier) -> PyResult<()> {
        let obj = self.obj.clone();
        Python::with_gil(|py| {
            let b = obj.bind(py);
            if let Ok(method) = b.getattr("on_start") {
                let py_notifier = notifier.into_pyobject(py)?;
                method.call1((py_notifier,))?;
            }
            Ok(())
        })
    }
}

/// Get (or lazily create) the shared asyncio event loop running on a
/// dedicated background Python thread.
///
/// We can't call `asyncio.run(coro)` directly from a Tokio worker — the
/// nested `run_until_complete` deadlocks because the Tokio worker is
/// holding the GIL while the selector waits. Instead we run one long-lived
/// event loop on its own thread and submit coroutines via
/// `asyncio.run_coroutine_threadsafe`.
fn asyncio_loop(py: Python<'_>) -> PyResult<PyObject> {
    static LOOP: std::sync::OnceLock<PyObject> = std::sync::OnceLock::new();
    if let Some(l) = LOOP.get() {
        return Ok(l.clone_ref(py));
    }
    let asyncio = py.import("asyncio")?;
    // Policy-default loop construction (NOT `SelectorEventLoop()` — see the
    // module-load-time `threading` import in `lib.rs`'s `#[pymodule]` fn
    // for the real fix and full explanation of the Windows bug this used to
    // work around by accident). Using the policy default keeps
    // subprocess/pipe support on Windows `ProactorEventLoop` and respects a
    // user-installed policy (uvloop, etc.) instead of silently overriding
    // it.
    let loop_obj: PyObject = asyncio.getattr("new_event_loop")?.call0()?.unbind();
    let loop_for_thread = loop_obj.clone_ref(py);
    // Readiness handshake: don't publish the loop via LOOP.set() until we
    // know it is actually inside run_forever() and able to accept
    // run_coroutine_threadsafe submissions. Without this, a submission
    // racing loop startup can silently vanish (the coroutine object is
    // created but never scheduled) rather than raising -- exactly the
    // "coroutine was never awaited" symptom this bridge must not produce.
    let ready = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let ready_for_thread = ready.clone();
    std::thread::Builder::new()
        .name("spvirit-asyncio".into())
        .spawn(move || {
            Python::with_gil(|py| {
                let l = loop_for_thread.bind(py);
                // Flip the readiness flag from inside the loop itself via
                // call_soon_threadsafe, so "ready" means "run_forever has
                // started pumping callbacks", not just "the OS thread
                // started".
                let ready_cb = {
                    let ready_for_cb = ready_for_thread.clone();
                    PyCFunction::new_closure(py, None, None, move |_args, _kwargs| {
                        let (lock, cvar) = &*ready_for_cb;
                        *lock.lock().unwrap() = true;
                        cvar.notify_all();
                        Ok::<(), PyErr>(())
                    })
                };
                match ready_cb.and_then(|cb| l.call_method1("call_soon_threadsafe", (cb,))) {
                    Ok(_) => {}
                    Err(e) => tracing::error!("failed to arm asyncio loop readiness signal: {e}"),
                }
                // run_forever releases the GIL while blocking in the selector.
                if let Err(e) = l.call_method0("run_forever") {
                    tracing::error!("asyncio loop exited: {}", e);
                }
            });
        })
        .expect("spawn asyncio thread");
    // Block (releasing the GIL) until the loop confirms it is running, or
    // give up after a bounded wait rather than risk a silent race forever.
    py.allow_threads(|| {
        let (lock, cvar) = &*ready;
        let guard = lock.lock().unwrap();
        let (guard, timeout_result) = cvar
            .wait_timeout_while(guard, std::time::Duration::from_secs(5), |ready| !*ready)
            .unwrap();
        if timeout_result.timed_out() {
            panic!("asyncio bridge loop did not signal readiness within 5s; it may have failed to start");
        }
        drop(guard);
    });
    let out = loop_obj.clone_ref(py);
    let _ = LOOP.set(loop_obj);
    Ok(out)
}

/// Call a Python method that may be sync or async; if the return value is a
/// coroutine, submit it to the shared asyncio loop and block on the result.
pub(crate) async fn call_py_await(
    obj: Arc<PyObject>,
    method: &'static str,
    build_args: impl for<'py> FnOnce(Python<'py>) -> PyResult<Bound<'py, PyTuple>> + Send,
) -> PyResult<PyObject> {
    // Phase 1: under the GIL, invoke the method. If sync, return the value.
    //          If async, schedule the coroutine on the shared asyncio loop
    //          and hand back the concurrent.futures.Future.
    enum Outcome {
        Value(PyObject),
        Future(PyObject),
    }
    let outcome: Outcome = Python::with_gil(|py| -> PyResult<Outcome> {
        let args = build_args(py)?;
        let ret = obj.call_method1(py, method, args)?;
        let bound = ret.bind(py);
        let is_awaitable = bound.hasattr("__await__").unwrap_or(false);
        if !is_awaitable {
            return Ok(Outcome::Value(ret));
        }
        let loop_obj = asyncio_loop(py)?;
        let asyncio = py.import("asyncio")?;
        let fut = asyncio
            .getattr("run_coroutine_threadsafe")?
            .call1((bound, loop_obj.bind(py)))?;
        Ok(Outcome::Future(fut.unbind()))
    })?;
    match outcome {
        Outcome::Value(v) => Ok(v),
        Outcome::Future(fut) => {
            // Phase 2: block on .result(). `result()` uses a threading
            // Condition that releases the GIL while waiting, letting the
            // asyncio thread acquire it to run the coroutine.
            Python::with_gil(|py| -> PyResult<PyObject> { Ok(fut.call_method0(py, "result")?) })
        }
    }
}

fn log_err(method: &str, e: impl std::fmt::Display) {
    tracing::error!("PySource.{}: {}", method, e);
}

impl Source for PySourceAdapter {
    fn claim<'a>(
        &'a self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + 'a>> {
        let obj = self.obj.clone();
        let name = name.to_string();
        Box::pin(async move {
            let ret = match call_py_await(obj, "claim", move |py| {
                PyTuple::new(py, &[name.into_pyobject(py)?.into_any()])
            })
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    log_err("claim", e);
                    return None;
                }
            };
            Python::with_gil(|py| {
                let b = ret.bind(py);
                if b.is_none() {
                    return None;
                }
                match py_to_pv_info(b) {
                    Ok(info) => Some(info),
                    Err(e) => {
                        log_err("claim", e);
                        None
                    }
                }
            })
        })
    }

    fn get<'a>(
        &'a self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + 'a>> {
        let obj = self.obj.clone();
        let name = name.to_string();
        Box::pin(async move {
            let ret = match call_py_await(obj, "get", move |py| {
                PyTuple::new(py, &[name.into_pyobject(py)?.into_any()])
            })
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    log_err("get", e);
                    return None;
                }
            };
            Python::with_gil(|py| {
                let b = ret.bind(py);
                if b.is_none() {
                    return None;
                }
                match py_to_nt_payload(b) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        log_err("get", e);
                        None
                    }
                }
            })
        })
    }

    fn put<'a>(
        &'a self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + 'a>> {
        let obj = self.obj.clone();
        let name = name.to_string();
        let value = value.clone();
        Box::pin(async move {
            // Build Python args under the GIL, converting the DecodedValue.
            let name_for_call = name.clone();
            let ret = call_py_await(obj, "put", move |py| {
                let v = decoded_to_py(py, &value);
                PyTuple::new(
                    py,
                    &[
                        name_for_call.into_pyobject(py)?.into_any(),
                        v.into_bound(py),
                    ],
                )
            })
            .await
            .map_err(|e| format!("{e}"))?;

            // Parse the return value.  Accept:
            //   None                                -> no propagation
            //   NtPayload wrapper                   -> [(name, payload)]
            //   dict[str, NtPayload]                -> each entry
            //   list[tuple[str, NtPayload]]         -> each entry
            Python::with_gil(|py| -> Result<Vec<(String, NtPayload)>, String> {
                let b = ret.bind(py);
                if b.is_none() {
                    return Ok(Vec::new());
                }
                // Try NT payload directly.
                if let Ok(p) = py_to_nt_payload(b) {
                    return Ok(vec![(name.clone(), p)]);
                }
                // dict?
                if let Ok(d) = b.downcast::<PyDict>() {
                    let mut out = Vec::with_capacity(d.len());
                    for (k, v) in d.iter() {
                        let key: String = k.extract().map_err(|e| format!("put dict key: {e}"))?;
                        let payload = py_to_nt_payload(&v)
                            .map_err(|e| format!("put dict value for '{key}': {e}"))?;
                        out.push((key, payload));
                    }
                    return Ok(out);
                }
                // iterable of (name, payload)?
                if let Ok(list) = b.downcast::<PyList>() {
                    let mut out = Vec::with_capacity(list.len());
                    for item in list.iter() {
                        let t = item
                            .downcast::<PyTuple>()
                            .map_err(|_| "put list item must be (name, payload)".to_string())?;
                        if t.len() != 2 {
                            return Err("put list tuple must have 2 elements".to_string());
                        }
                        let key: String = t
                            .get_item(0)
                            .and_then(|x| x.extract())
                            .map_err(|e| format!("{e}"))?;
                        let payload = t
                            .get_item(1)
                            .map_err(|e| format!("{e}"))
                            .and_then(|x| py_to_nt_payload(&x).map_err(|e| format!("{e}")))?;
                        out.push((key, payload));
                    }
                    return Ok(out);
                }
                Err(format!(
                    "put() must return None, NtPayload, dict, or list of tuples; got {}",
                    b.get_type()
                        .name()
                        .map(|n| n.to_string())
                        .unwrap_or_default()
                ))
            })
        })
    }

    fn subscribe<'a>(
        &'a self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + 'a>> {
        // Python sources publish monitor updates via `notifier.notify()` — no
        // Source-level subscribe channel needed.
        Box::pin(async { None })
    }

    fn rpc<'a>(
        &'a self,
        name: &str,
        args: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<NtPayload, String>> + Send + 'a>> {
        let obj = self.obj.clone();
        let name = name.to_string();
        let args = args.clone();
        Box::pin(async move {
            // If the Python object doesn't define rpc, fall back to an error.
            let has_rpc = Python::with_gil(|py| obj.bind(py).hasattr("rpc").unwrap_or(false));
            if !has_rpc {
                return Err("RPC not supported".to_string());
            }
            let ret = call_py_await(obj, "rpc", move |py| {
                let args_py = decoded_to_py(py, &args);
                PyTuple::new(
                    py,
                    &[name.into_pyobject(py)?.into_any(), args_py.into_bound(py)],
                )
            })
            .await
            .map_err(|e| format!("{e}"))?;
            Python::with_gil(|py| -> Result<NtPayload, String> {
                py_to_nt_payload(ret.bind(py)).map_err(|e| format!("{e}"))
            })
        })
    }

    fn names<'a>(&'a self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
        let obj = self.obj.clone();
        Box::pin(async move {
            // `names` may not be defined (we accept that too).
            let has = Python::with_gil(|py| obj.bind(py).hasattr("names").unwrap_or(false));
            if !has {
                return Vec::new();
            }
            let ret = match call_py_await(obj, "names", |py| Ok(PyTuple::empty(py))).await {
                Ok(r) => r,
                Err(e) => {
                    log_err("names", e);
                    return Vec::new();
                }
            };
            Python::with_gil(|py| {
                let b = ret.bind(py);
                if b.is_none() {
                    return Vec::new();
                }
                match b.extract::<Vec<String>>() {
                    Ok(v) => v,
                    Err(e) => {
                        log_err("names", e);
                        Vec::new()
                    }
                }
            })
        })
    }
}

// ─── Module registration ─────────────────────────────────────────────────────

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPvInfo>()?;
    m.add_class::<PyNotifier>()?;
    Ok(())
}

// Re-export `nt_payload_to_py` for tests/external callers — keeps it used.
#[allow(dead_code)]
pub(crate) fn _ensure_used(py: Python<'_>, p: NtPayload) -> PyObject {
    nt_payload_to_py(py, p)
}
