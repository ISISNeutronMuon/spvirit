//! Reusable PVA channel exposed to Python.
//!
//! A `Channel` holds a single established TCP connection to a PVA server
//! for a single PV name and lets callers perform repeated `get` / `put` /
//! `monitor` / `introspect` operations over it.  Provides both sync and
//! async (`_async`) variants.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use pyo3::prelude::*;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use spvirit_client::client::{
    ChannelConn, encode_get_request, encode_monitor_request, encode_put_request, ensure_status_ok,
    establish_channel,
};
use spvirit_client::pva_client::decode_init_introspection;
use spvirit_client::transport::{read_packet, read_until};
use spvirit_client::types::{PvGetError, PvGetOptions};
use spvirit_codec::epics_decode::{DecodeMode, PvaPacket, PvaPacketCommand};
use spvirit_codec::spvd_decode::PvdDecoder;
use spvirit_codec::spvd_encode::encode_pv_request;
use spvirit_codec::spvirit_encode::encode_control_message;

use crate::codec::PyStructureDesc;
use crate::convert::{decoded_to_py, py_to_json};
use crate::errors::to_py_err;
use crate::runtime::{block_on_py, future_into_py};

const PVA_VERSION: u8 = 2;
const QOS_INIT: u8 = 0x08;

/// Build the pvRequest body for a channel operation.  An empty `fields`
/// slice yields the canonical "all fields" pvRequest bytes; otherwise we
/// delegate to [`encode_pv_request`] (which supports dotted nested paths).
fn build_pv_request(fields: &[String], is_be: bool) -> Vec<u8> {
    if fields.is_empty() {
        vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00]
    } else {
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        encode_pv_request(&refs, is_be)
    }
}

/// Normalise a user-supplied `fields` argument into a `Vec<String>`.
///
/// Accepts `None` -> empty, a single string (treated as a one-entry list;
/// this preserves backwards compatibility with the previous `field: str`
/// kwarg), or an iterable of strings.
pub(crate) fn normalize_fields(py: Python<'_>, fields: Option<PyObject>) -> PyResult<Vec<String>> {
    let Some(obj) = fields else {
        return Ok(Vec::new());
    };
    let bound = obj.bind(py);
    if let Ok(s) = bound.extract::<String>() {
        return Ok(vec![s]);
    }
    bound.extract::<Vec<String>>()
}

/// Shared mutable state for a `Channel`.  Option-ized so `close()` can
/// drop the TCP stream while leaving the Python handle alive.
struct ChannelState {
    conn: Option<ChannelConn>,
    pv_name: String,
    timeout: Duration,
    next_ioid: u32,
}

impl ChannelState {
    fn alloc_ioid(&mut self) -> u32 {
        let v = self.next_ioid;
        self.next_ioid = self.next_ioid.wrapping_add(1).max(1);
        v
    }

    fn conn_mut(&mut self) -> Result<&mut ChannelConn, PvGetError> {
        self.conn
            .as_mut()
            .ok_or_else(|| PvGetError::Protocol("channel is closed".to_string()))
    }
}

/// Persistent TCP channel to one PV on one server. Create with
/// `Channel.connect()`; supports repeated get/put/monitor/introspect plus
/// raw-frame access, and can be used as a context manager.
#[pyclass(name = "Channel", module = "spvirit.lowlevel")]
pub struct PyChannel {
    state: Arc<Mutex<ChannelState>>,
}

fn parse_addr(addr: &str) -> PyResult<SocketAddr> {
    addr.parse()
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid address: {e}")))
}

async fn do_connect(
    pv_name: String,
    server_addr: SocketAddr,
    timeout: Duration,
) -> Result<ChannelState, PvGetError> {
    let mut opts = PvGetOptions::new(pv_name.clone());
    opts.timeout = timeout;
    opts.server_addr = Some(server_addr);
    let conn = establish_channel(server_addr, &opts).await?;
    Ok(ChannelState {
        conn: Some(conn),
        pv_name,
        timeout,
        next_ioid: 1,
    })
}

async fn run_get(
    state: Arc<Mutex<ChannelState>>,
    fields: Vec<String>,
) -> Result<
    (
        String,
        spvirit_codec::spvd_decode::DecodedValue,
        Vec<u8>,
        Vec<u8>,
    ),
    PvGetError,
> {
    let mut guard = state.lock().await;
    let timeout = guard.timeout;
    let ioid = guard.alloc_ioid();
    let pv_name = guard.pv_name.clone();
    let conn = guard.conn_mut()?;
    let is_be = conn.is_be;
    let version = conn.version;
    let sid = conn.sid;

    let pv_request = if fields.is_empty() {
        vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00]
    } else {
        build_pv_request(&fields, is_be)
    };

    let get_init = encode_get_request(sid, ioid, QOS_INIT, &pv_request, version, is_be);
    conn.stream.write_all(&get_init).await?;

    let init_resp = read_until(&mut conn.stream, timeout, &mut conn.reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 10 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
    })
    .await?;
    let desc = decode_init_introspection(&init_resp, "GET")?;

    let get_data = encode_get_request(sid, ioid, 0x00, &[], version, is_be);
    conn.stream.write_all(&get_data).await?;

    let data_resp = read_until(&mut conn.stream, timeout, &mut conn.reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 10 && op.ioid == ioid && op.subcmd == 0x00)
    })
    .await?;
    let mut pkt = PvaPacket::new(&data_resp);
    let cmd = pkt
        .decode_payload()
        .ok_or_else(|| PvGetError::Protocol("get data decode failed".to_string()))?;
    match cmd {
        PvaPacketCommand::Op(mut op) => {
            // A decode failure leaves decoded_value None, reported below.
            let _ = op.decode_with_field_desc(&desc, is_be, DecodeMode::Strict);
            let value = op
                .decoded_value
                .ok_or_else(|| PvGetError::Decode("no decoded value".to_string()))?;
            Ok((pv_name, value, data_resp, op.body))
        }
        _ => Err(PvGetError::Protocol(
            "unexpected get data response".to_string(),
        )),
    }
}

async fn run_put(
    state: Arc<Mutex<ChannelState>>,
    json_val: serde_json::Value,
    fields: Vec<String>,
) -> Result<(), PvGetError> {
    let mut guard = state.lock().await;
    let timeout = guard.timeout;
    let ioid = guard.alloc_ioid();
    let conn = guard.conn_mut()?;
    let is_be = conn.is_be;
    let sid = conn.sid;

    let pv_request = build_pv_request(&fields, is_be);
    let init = encode_put_request(sid, ioid, QOS_INIT, &pv_request, PVA_VERSION, is_be);
    conn.stream.write_all(&init).await?;

    let init_resp = read_until(&mut conn.stream, timeout, &mut conn.reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
    })
    .await?;
    let desc = decode_init_introspection(&init_resp, "PUT")?;

    let payload = spvirit_client::put_encode::encode_put_payload(&desc, &json_val, is_be)
        .map_err(|e| PvGetError::Protocol(format!("put encode: {e}")))?;
    let req = encode_put_request(sid, ioid, 0x00, &payload, PVA_VERSION, is_be);
    conn.stream.write_all(&req).await?;

    let resp = read_until(&mut conn.stream, timeout, &mut conn.reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.ioid == ioid && op.subcmd == 0x00)
    })
    .await?;
    ensure_status_ok(&resp, is_be, "PUT")?;
    Ok(())
}

async fn run_introspect(
    state: Arc<Mutex<ChannelState>>,
) -> Result<spvirit_codec::spvd_decode::StructureDesc, PvGetError> {
    // Cheapest way: do GET INIT only and return the introspection.
    let mut guard = state.lock().await;
    let timeout = guard.timeout;
    let ioid = guard.alloc_ioid();
    let conn = guard.conn_mut()?;
    let is_be = conn.is_be;
    let version = conn.version;
    let sid = conn.sid;

    let pv_request = vec![0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];
    let init = encode_get_request(sid, ioid, QOS_INIT, &pv_request, version, is_be);
    conn.stream.write_all(&init).await?;
    let init_resp = read_until(&mut conn.stream, timeout, &mut conn.reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 10 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
    })
    .await?;
    decode_init_introspection(&init_resp, "GET")
}

/// Monitor loop running against the persistent channel.  Invokes
/// `callback(decoded_value_py)` inside the GIL; stops when the callback
/// returns a falsy value or raises.
async fn run_monitor(
    state: Arc<Mutex<ChannelState>>,
    callback: PyObject,
    fields: Vec<String>,
    cb_err: &mut Option<PyErr>,
) -> Result<(), PvGetError> {
    let mut guard = state.lock().await;
    let timeout = guard.timeout;
    let ioid = guard.alloc_ioid();
    let conn = guard.conn_mut()?;
    let is_be = conn.is_be;
    let sid = conn.sid;

    let decoder = PvdDecoder::new(is_be);
    let pv_request = build_pv_request(&fields, is_be);
    let init = encode_monitor_request(sid, ioid, QOS_INIT, &pv_request, PVA_VERSION, is_be);
    conn.stream.write_all(&init).await?;

    let init_resp = read_until(&mut conn.stream, timeout, &mut conn.reassembler, |cmd| {
        matches!(cmd, PvaPacketCommand::Op(op) if op.command == 13 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
    })
    .await?;
    let field_desc = decode_init_introspection(&init_resp, "MONITOR")?;

    let start = encode_monitor_request(sid, ioid, 0x44, &[], PVA_VERSION, is_be);
    conn.stream.write_all(&start).await?;

    let mut echo_interval = tokio::time::interval(Duration::from_secs(10));
    echo_interval.tick().await; // skip immediate tick
    let mut echo_token: u32 = 1;

    loop {
        tokio::select! {
            _ = echo_interval.tick() => {
                let msg = encode_control_message(false, is_be, PVA_VERSION, 3, echo_token);
                echo_token = echo_token.wrapping_add(1);
                let _ = conn.stream.write_all(&msg).await;
            }
            res = read_packet(&mut conn.stream, timeout, &mut conn.reassembler) => {
                let bytes = match res {
                    Ok(b) => b,
                    Err(PvGetError::Timeout(_)) => continue,
                    Err(e) => return Err(e),
                };
                let mut pkt = PvaPacket::new(&bytes);
                if let Some(PvaPacketCommand::Op(op)) = pkt.decode_payload() {
                    if op.command == 13 && op.ioid == ioid && op.subcmd == 0x00 {
                        let payload = &bytes[8..];
                        let pos = 5;
                        if let Ok((decoded, _)) =
                            decoder.decode_structure_with_bitset(&payload[pos..], &field_desc)
                        {
                            let keep_going = Python::with_gil(|py| {
                                let v = decoded_to_py(py, &decoded);
                                match callback.call1(py, (v,)) {
                                    Ok(ret) => ret.extract::<bool>(py).unwrap_or(true),
                                    Err(e) => {
                                        *cb_err = Some(e);
                                        false
                                    }
                                }
                            });
                            if !keep_going {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}

#[pymethods]
impl PyChannel {
    /// Connect to `server_addr` and establish a channel for `pv_name`.
    #[staticmethod]
    #[pyo3(signature = (pv_name, server_addr, timeout=5.0))]
    fn connect(
        py: Python<'_>,
        pv_name: String,
        server_addr: String,
        timeout: f64,
    ) -> PyResult<PyChannel> {
        let sa = parse_addr(&server_addr)?;
        let dur = Duration::from_secs_f64(timeout);
        let state = block_on_py(py, do_connect(pv_name, sa, dur)).map_err(to_py_err)?;
        Ok(PyChannel {
            state: Arc::new(Mutex::new(state)),
        })
    }

    /// Async variant of [`connect`].
    #[staticmethod]
    #[pyo3(signature = (pv_name, server_addr, timeout=5.0))]
    fn connect_async<'py>(
        py: Python<'py>,
        pv_name: String,
        server_addr: String,
        timeout: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sa = parse_addr(&server_addr)?;
        let dur = Duration::from_secs_f64(timeout);
        future_into_py(py, async move {
            let state = do_connect(pv_name, sa, dur).await.map_err(to_py_err)?;
            Ok(PyChannel {
                state: Arc::new(Mutex::new(state)),
            })
        })
    }

    /// Name of the PV this channel is bound to.
    #[getter]
    fn pv_name(&self, py: Python<'_>) -> String {
        py.allow_threads(|| {
            let guard = self.state.blocking_lock();
            guard.pv_name.clone()
        })
    }

    /// True while the underlying TCP connection is open.
    #[getter]
    fn is_open(&self, py: Python<'_>) -> bool {
        py.allow_threads(|| {
            let guard = self.state.blocking_lock();
            guard.conn.is_some()
        })
    }

    /// Server address as `"ip:port"`, or None when closed.
    #[getter]
    fn server_addr(&self, py: Python<'_>) -> Option<String> {
        py.allow_threads(|| {
            let guard = self.state.blocking_lock();
            guard.conn.as_ref().map(|c| c.server_addr.to_string())
        })
    }

    /// Server-assigned channel ID, or None when closed.
    #[getter]
    fn sid(&self, py: Python<'_>) -> Option<u32> {
        py.allow_threads(|| {
            let guard = self.state.blocking_lock();
            guard.conn.as_ref().map(|c| c.sid)
        })
    }

    /// Fetch the current value (blocking). Returns a `GetResult` whose
    /// `.value` is a dict mirroring the NT structure.
    #[pyo3(signature = (fields=None))]
    fn get(
        &self,
        py: Python<'_>,
        fields: Option<PyObject>,
    ) -> PyResult<crate::client::PyGetResult> {
        let state = self.state.clone();
        let fields = normalize_fields(py, fields)?;
        let (pv_name, value, raw_pva, raw_pvd) =
            block_on_py(py, run_get(state, fields)).map_err(to_py_err)?;
        let py_val = decoded_to_py(py, &value);
        Ok(crate::client::PyGetResult::new(
            pv_name, py_val, raw_pva, raw_pvd,
        ))
    }

    /// Async variant of `get`; returns an awaitable resolving to a `GetResult`.
    #[pyo3(signature = (fields=None))]
    fn get_async<'py>(
        &self,
        py: Python<'py>,
        fields: Option<PyObject>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let state = self.state.clone();
        let fields = normalize_fields(py, fields)?;
        future_into_py(py, async move {
            let (pv_name, value, raw_pva, raw_pvd) =
                run_get(state, fields).await.map_err(to_py_err)?;
            Python::with_gil(|py| {
                let py_val = decoded_to_py(py, &value);
                Ok(Py::new(
                    py,
                    crate::client::PyGetResult::new(pv_name, py_val, raw_pva, raw_pvd),
                )?)
            })
        })
    }

    /// Write a value (blocking).
    ///
    /// `fields` selects which pvRequest fields are targeted. Defaults to
    /// `["value"]` when omitted. A single string is still accepted for
    /// backwards compatibility with earlier versions of this binding.
    #[pyo3(signature = (value, fields=None))]
    fn put(&self, py: Python<'_>, value: PyObject, fields: Option<PyObject>) -> PyResult<()> {
        let state = self.state.clone();
        let json = py_to_json(value.bind(py))?;
        let fields = normalize_fields(py, fields)?;
        block_on_py(py, run_put(state, json, fields)).map_err(to_py_err)
    }

    /// Async variant of `put`; returns an awaitable.
    #[pyo3(signature = (value, fields=None))]
    fn put_async<'py>(
        &self,
        py: Python<'py>,
        value: PyObject,
        fields: Option<PyObject>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let state = self.state.clone();
        let json = py_to_json(value.bind(py))?;
        let fields = normalize_fields(py, fields)?;
        future_into_py(py, async move {
            run_put(state, json, fields).await.map_err(to_py_err)?;
            Ok(())
        })
    }

    /// Retrieve the PV's structure description (INIT exchange only).
    fn introspect(&self, py: Python<'_>) -> PyResult<PyStructureDesc> {
        let state = self.state.clone();
        let desc = block_on_py(py, run_introspect(state)).map_err(to_py_err)?;
        Ok(PyStructureDesc::from_inner(desc))
    }

    /// Async variant of `introspect`; returns an awaitable.
    fn introspect_async<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let state = self.state.clone();
        future_into_py(py, async move {
            let desc = run_introspect(state).await.map_err(to_py_err)?;
            Python::with_gil(|py| Py::new(py, PyStructureDesc::from_inner(desc)))
        })
    }

    /// Subscribe and block (GIL released between updates) until
    /// `callback(value)` returns False or raises — a raised exception stops
    /// the monitor and propagates to the caller.
    ///
    /// `fields` restricts the subscription to the given dotted paths.
    #[pyo3(signature = (callback, fields=None))]
    fn monitor(
        &self,
        py: Python<'_>,
        callback: PyObject,
        fields: Option<PyObject>,
    ) -> PyResult<()> {
        let state = self.state.clone();
        let fields = normalize_fields(py, fields)?;
        let mut cb_err: Option<PyErr> = None;
        let result = py.allow_threads(|| {
            crate::runtime::RUNTIME.block_on(run_monitor(state, callback, fields, &mut cb_err))
        });
        if let Some(e) = cb_err {
            return Err(e);
        }
        result.map_err(to_py_err)
    }

    /// Close the underlying TCP stream.  Subsequent operations raise
    /// ProtocolError.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| {
            let mut guard = self.state.blocking_lock();
            guard.conn.take();
        });
        Ok(())
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc=None, _tb=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: Option<PyObject>,
        _exc: Option<PyObject>,
        _tb: Option<PyObject>,
    ) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        py.allow_threads(|| {
            let guard = self.state.blocking_lock();
            let addr = guard
                .conn
                .as_ref()
                .map(|c| c.server_addr.to_string())
                .unwrap_or_else(|| "<closed>".to_string());
            format!("Channel(pv_name={:?}, server={})", guard.pv_name, addr)
        })
    }

    /// Read the next PVA frame off the wire and return a `Packet`.
    #[pyo3(signature = (timeout=None))]
    fn read_packet(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<crate::packet::PyPacket> {
        let state = self.state.clone();
        let override_timeout = timeout.map(Duration::from_secs_f64);
        let bytes = block_on_py(py, async move {
            let mut guard = state.lock().await;
            let t = override_timeout.unwrap_or(guard.timeout);
            let conn = guard.conn_mut()?;
            read_packet(&mut conn.stream, t, &mut conn.reassembler).await
        })
        .map_err(to_py_err)?;
        Ok(crate::packet::PyPacket::from_bytes(bytes))
    }

    /// Async variant of `read_packet`; returns an awaitable.
    #[pyo3(signature = (timeout=None))]
    fn read_packet_async<'py>(
        &self,
        py: Python<'py>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let state = self.state.clone();
        let override_timeout = timeout.map(Duration::from_secs_f64);
        future_into_py(py, async move {
            let bytes = {
                let mut guard = state.lock().await;
                let t = override_timeout.unwrap_or(guard.timeout);
                let conn = guard.conn_mut().map_err(to_py_err)?;
                read_packet(&mut conn.stream, t, &mut conn.reassembler)
                    .await
                    .map_err(to_py_err)?
            };
            Python::with_gil(|py| Py::new(py, crate::packet::PyPacket::from_bytes(bytes)))
        })
    }

    /// Read frames until `predicate(packet)` returns truthy, then return
    /// that packet.  `predicate` may be any Python callable.
    #[pyo3(signature = (predicate, timeout=None, max_frames=None))]
    fn read_until(
        &self,
        py: Python<'_>,
        predicate: PyObject,
        timeout: Option<f64>,
        max_frames: Option<usize>,
    ) -> PyResult<crate::packet::PyPacket> {
        let state = self.state.clone();
        let override_timeout = timeout.map(Duration::from_secs_f64);
        let max = max_frames.unwrap_or(usize::MAX);
        let mut seen = 0usize;
        loop {
            if seen >= max {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "read_until: max_frames reached",
                ));
            }
            seen += 1;
            let bytes = {
                let state = state.clone();
                block_on_py(py, async move {
                    let mut guard = state.lock().await;
                    let t = override_timeout.unwrap_or(guard.timeout);
                    let conn = guard.conn_mut()?;
                    read_packet(&mut conn.stream, t, &mut conn.reassembler).await
                })
                .map_err(to_py_err)?
            };
            let pkt = crate::packet::PyPacket::from_bytes(bytes);
            let (matched, pkt_back) = Python::with_gil(|py| -> PyResult<(bool, _)> {
                let pkt_obj = Py::new(py, pkt)?;
                let result = predicate.call1(py, (pkt_obj.clone_ref(py),))?;
                let matched = result.extract::<bool>(py).unwrap_or(false);
                Ok((matched, pkt_obj))
            })?;
            if matched {
                // Move the inner PyPacket back out.
                let pkt: crate::packet::PyPacket = Python::with_gil(|py| -> PyResult<_> {
                    let bound = pkt_back.bind(py);
                    let data: Vec<u8> = bound.borrow().raw().to_vec();
                    Ok(crate::packet::PyPacket::from_bytes(data))
                })?;
                return Ok(pkt);
            }
        }
    }
}

/// Register the `spvirit.lowlevel` submodule and add `Channel`.
pub fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = parent.py();
    let m = PyModule::new(py, "lowlevel")?;
    m.add_class::<PyChannel>()?;
    m.add_class::<crate::packet::PyPacket>()?;
    crate::discovery::register(&m)?;
    parent.add_submodule(&m)?;
    py.import("sys")?
        .getattr("modules")?
        .set_item("spvirit.lowlevel", &m)?;
    Ok(())
}
