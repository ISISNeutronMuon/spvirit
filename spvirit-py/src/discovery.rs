//! Discovery primitives — UDP search / server discovery / pvlist.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use pyo3::prelude::*;
use pyo3::types::PyList;

use spvirit_client::pvlist::{PvListSource, pvlist_with_fallback};
use spvirit_client::search::{
    SearchTarget, build_auto_broadcast_targets, build_search_targets, discover_servers,
    parse_addr_list, search_pv, search_pv_tcp,
};
use spvirit_client::types::PvOptions;

use crate::errors::to_py_err;
use crate::runtime::{block_on_py, future_into_py};

fn parse_ip(s: &str) -> PyResult<IpAddr> {
    s.parse::<IpAddr>()
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid IP: {e}")))
}

/// Parse an `EPICS_PVA_ADDR_LIST`-style string into a list of IP strings.
#[pyfunction(name = "parse_addr_list")]
fn parse_addr_list_py(s: &str) -> Vec<String> {
    parse_addr_list(s)
        .into_iter()
        .map(|ip| ip.to_string())
        .collect()
}

/// Return the auto-detected broadcast targets as list of
/// `{"target": str, "bind": str}` dicts.
#[pyfunction]
fn auto_broadcast_targets(py: Python<'_>) -> PyResult<PyObject> {
    let targets = build_auto_broadcast_targets();
    let list = PyList::empty(py);
    for t in &targets {
        let d = pyo3::types::PyDict::new(py);
        d.set_item("target", t.target.to_string())?;
        d.set_item("bind", t.bind.to_string())?;
        list.append(d)?;
    }
    Ok(list.into())
}

/// Assemble search targets the same way the default client does, honoring
/// `EPICS_PVA_ADDR_LIST`/`EPICS_PVA_AUTO_ADDR_LIST`.
#[pyfunction]
#[pyo3(signature = (search_addr=None, bind_addr=None))]
fn default_search_targets(
    py: Python<'_>,
    search_addr: Option<String>,
    bind_addr: Option<String>,
) -> PyResult<PyObject> {
    let sa = search_addr.as_deref().map(parse_ip).transpose()?;
    let ba = bind_addr.as_deref().map(parse_ip).transpose()?;
    let targets = build_search_targets(sa, ba);
    let list = PyList::empty(py);
    for t in &targets {
        let d = pyo3::types::PyDict::new(py);
        d.set_item("target", t.target.to_string())?;
        d.set_item("bind", t.bind.to_string())?;
        list.append(d)?;
    }
    Ok(list.into())
}

/// A user-supplied search target: either a `(target_ip, bind_ip)` tuple or a
/// `{"target": ..., "bind": ...}` dict as produced by
/// `default_search_targets()` / `auto_broadcast_targets()`.
#[derive(FromPyObject)]
pub(crate) enum TargetSpec {
    Pair(String, String),
    Map(std::collections::HashMap<String, String>),
}

fn to_search_targets(items: Vec<TargetSpec>) -> PyResult<Vec<SearchTarget>> {
    items
        .into_iter()
        .map(|spec| {
            let (t, b) = match spec {
                TargetSpec::Pair(t, b) => (t, b),
                TargetSpec::Map(m) => {
                    let t = m.get("target").cloned().ok_or_else(|| {
                        pyo3::exceptions::PyValueError::new_err(
                            "target dict must have 'target' and 'bind' keys",
                        )
                    })?;
                    let b = m.get("bind").cloned().ok_or_else(|| {
                        pyo3::exceptions::PyValueError::new_err(
                            "target dict must have 'target' and 'bind' keys",
                        )
                    })?;
                    (t, b)
                }
            };
            Ok(SearchTarget {
                target: parse_ip(&t)?,
                bind: parse_ip(&b)?,
            })
        })
        .collect()
}

fn resolve_targets(targets: Option<Vec<TargetSpec>>) -> PyResult<Vec<SearchTarget>> {
    match targets {
        Some(items) => to_search_targets(items),
        None => Ok(build_search_targets(None, None)),
    }
}

/// Broadcast a search and return the server address `"ip:port"` for
/// `pv_name`, or raise on timeout.
#[pyfunction(name = "search_pv")]
#[pyo3(signature = (pv_name, udp_port=5076, timeout=3.0, targets=None, debug=false))]
fn search_pv_udp(
    py: Python<'_>,
    pv_name: String,
    udp_port: u16,
    timeout: f64,
    targets: Option<Vec<TargetSpec>>,
    debug: bool,
) -> PyResult<String> {
    let tgts = resolve_targets(targets)?;
    let dur = Duration::from_secs_f64(timeout);
    let (addr, _guid) = block_on_py(py, async move {
        search_pv(&pv_name, udp_port, dur, &tgts, debug).await
    })
    .map_err(to_py_err)?;
    Ok(addr.to_string())
}

/// Async variant of `search_pv`; returns an awaitable with the same result.
#[pyfunction(name = "search_pv_async")]
#[pyo3(signature = (pv_name, udp_port=5076, timeout=3.0, targets=None, debug=false))]
fn search_pv_udp_async<'py>(
    py: Python<'py>,
    pv_name: String,
    udp_port: u16,
    timeout: f64,
    targets: Option<Vec<TargetSpec>>,
    debug: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let tgts = resolve_targets(targets)?;
    let dur = Duration::from_secs_f64(timeout);
    future_into_py(py, async move {
        let (addr, _guid) = search_pv(&pv_name, udp_port, dur, &tgts, debug)
            .await
            .map_err(to_py_err)?;
        Ok(addr.to_string())
    })
}

/// Search for a PV via TCP against the provided name-servers.
#[pyfunction(name = "search_pv_tcp")]
#[pyo3(signature = (pv_name, name_server, timeout=3.0, debug=false))]
fn search_pv_tcp_py(
    py: Python<'_>,
    pv_name: String,
    name_server: String,
    timeout: f64,
    debug: bool,
) -> PyResult<String> {
    let ns: SocketAddr = name_server.parse().map_err(|e| {
        pyo3::exceptions::PyValueError::new_err(format!("invalid name server {name_server}: {e}"))
    })?;
    let dur = Duration::from_secs_f64(timeout);
    let (addr, _guid) = block_on_py(
        py,
        async move { search_pv_tcp(&pv_name, ns, dur, debug).await },
    )
    .map_err(to_py_err)?;
    Ok(addr.to_string())
}

/// Discover reachable PVA servers on the local network.  Returns a list
/// of `{"guid": hex, "addr": "ip:port"}` dicts.
#[pyfunction(name = "discover_servers")]
#[pyo3(signature = (udp_port=5076, timeout=1.0, targets=None, debug=false))]
fn discover_servers_py(
    py: Python<'_>,
    udp_port: u16,
    timeout: f64,
    targets: Option<Vec<TargetSpec>>,
    debug: bool,
) -> PyResult<PyObject> {
    let tgts = resolve_targets(targets)?;
    let dur = Duration::from_secs_f64(timeout);
    let servers = block_on_py(py, async move {
        discover_servers(udp_port, dur, &tgts, debug).await
    })
    .map_err(to_py_err)?;
    let list = PyList::empty(py);
    for s in &servers {
        let d = pyo3::types::PyDict::new(py);
        d.set_item("guid", hex_encode(&s.guid))?;
        d.set_item("addr", s.tcp_addr.to_string())?;
        list.append(d)?;
    }
    Ok(list.into())
}

/// Async variant of `discover_servers`; returns an awaitable with the same
/// result.
#[pyfunction(name = "discover_servers_async")]
#[pyo3(signature = (udp_port=5076, timeout=1.0, targets=None, debug=false))]
fn discover_servers_async<'py>(
    py: Python<'py>,
    udp_port: u16,
    timeout: f64,
    targets: Option<Vec<TargetSpec>>,
    debug: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let tgts = resolve_targets(targets)?;
    let dur = Duration::from_secs_f64(timeout);
    future_into_py(py, async move {
        let servers = discover_servers(udp_port, dur, &tgts, debug)
            .await
            .map_err(to_py_err)?;
        let out: PyObject = Python::with_gil(|py| -> PyResult<PyObject> {
            let list = PyList::empty(py);
            for s in &servers {
                let d = pyo3::types::PyDict::new(py);
                d.set_item("guid", hex_encode(&s.guid))?;
                d.set_item("addr", s.tcp_addr.to_string())?;
                list.append(d)?;
            }
            Ok(list.unbind().into())
        })?;
        Ok(out)
    })
}

fn source_name(s: PvListSource) -> &'static str {
    match s {
        PvListSource::PvList => "pvlist",
        PvListSource::GetField => "getfield",
        PvListSource::ServerRpc => "server_rpc",
        PvListSource::ServerGet => "server_get",
    }
}

/// List all PVs on `server_addr`.  Returns a tuple
/// `(names: List[str], source: str)`.
#[pyfunction(name = "pvlist")]
#[pyo3(signature = (server_addr, timeout=5.0))]
fn pvlist_py(py: Python<'_>, server_addr: String, timeout: f64) -> PyResult<(Vec<String>, String)> {
    let sa: SocketAddr = server_addr
        .parse()
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid addr: {e}")))?;
    let mut opts = PvOptions::new(String::new());
    opts.timeout = Duration::from_secs_f64(timeout);
    let (names, src) =
        block_on_py(py, async move { pvlist_with_fallback(&opts, sa).await }).map_err(to_py_err)?;
    Ok((names, source_name(src).to_string()))
}

/// Async variant of `pvlist`; returns an awaitable with the same result.
#[pyfunction(name = "pvlist_async")]
#[pyo3(signature = (server_addr, timeout=5.0))]
fn pvlist_async<'py>(
    py: Python<'py>,
    server_addr: String,
    timeout: f64,
) -> PyResult<Bound<'py, PyAny>> {
    let sa: SocketAddr = server_addr
        .parse()
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("invalid addr: {e}")))?;
    let mut opts = PvOptions::new(String::new());
    opts.timeout = Duration::from_secs_f64(timeout);
    future_into_py(py, async move {
        let (names, src) = pvlist_with_fallback(&opts, sa).await.map_err(to_py_err)?;
        Ok((names, source_name(src).to_string()))
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Attach discovery functions to the `spvirit.lowlevel` submodule.
pub fn register(lowlevel: &Bound<'_, PyModule>) -> PyResult<()> {
    lowlevel.add_function(wrap_pyfunction!(parse_addr_list_py, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(auto_broadcast_targets, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(default_search_targets, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(search_pv_udp, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(search_pv_udp_async, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(search_pv_tcp_py, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(discover_servers_py, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(discover_servers_async, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(pvlist_py, lowlevel)?)?;
    lowlevel.add_function(wrap_pyfunction!(pvlist_async, lowlevel)?)?;
    Ok(())
}
