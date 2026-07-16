//! spvirit — Python bindings for PVAccess client and server.

use pyo3::prelude::*;

mod convert;
mod errors;
mod runtime;

pub mod channel;
pub mod client;
pub mod codec;
pub mod discovery;
pub mod nt;
pub mod packet;
pub mod pv;
pub mod server;
pub mod source;

#[pymodule]
fn spvirit(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Install the async bridge onto our shared Tokio runtime.
    runtime::init_async_runtime();

    // Error types
    errors::register(m)?;

    // Client classes
    m.add_class::<client::PyClient>()?;
    m.add_class::<client::PyClientBuilder>()?;
    m.add_class::<client::PyGetResult>()?;
    m.add_class::<client::PyMonitorEvent>()?;
    m.add_class::<client::PyDiscoveredServer>()?;
    m.add_class::<client::PySubscription>()?;

    // Server classes
    m.add_class::<server::PyServerBuilder>()?;
    m.add_class::<server::PyServer>()?;
    m.add_class::<server::PyStore>()?;

    // Dynamic-source classes
    source::register(m)?;

    // NT classes
    m.add_class::<nt::PyAlarm>()?;
    m.add_class::<nt::PyTimeStamp>()?;
    m.add_class::<nt::PyDisplay>()?;
    m.add_class::<nt::PyControl>()?;
    m.add_class::<nt::PyNtScalar>()?;
    m.add_class::<nt::PyNtScalarArray>()?;
    m.add_class::<nt::PyNtTable>()?;
    m.add_class::<nt::PyNtNdArray>()?;

    // Typed PV handles
    m.add_class::<pv::PyPv>()?;
    m.add_function(wrap_pyfunction!(pv::ai, m)?)?;
    m.add_function(wrap_pyfunction!(pv::ao, m)?)?;
    m.add_function(wrap_pyfunction!(pv::bi, m)?)?;
    m.add_function(wrap_pyfunction!(pv::bo, m)?)?;
    m.add_function(wrap_pyfunction!(pv::string_in, m)?)?;
    m.add_function(wrap_pyfunction!(pv::string_out, m)?)?;
    m.add_function(wrap_pyfunction!(pv::longin, m)?)?;
    m.add_function(wrap_pyfunction!(pv::longout, m)?)?;
    m.add_function(wrap_pyfunction!(pv::mbbi, m)?)?;
    m.add_function(wrap_pyfunction!(pv::mbbo, m)?)?;
    m.add_function(wrap_pyfunction!(pv::waveform, m)?)?;
    m.add_function(wrap_pyfunction!(pv::aai, m)?)?;
    m.add_function(wrap_pyfunction!(pv::aao, m)?)?;
    m.add_function(wrap_pyfunction!(pv::calc, m)?)?;
    m.add_function(wrap_pyfunction!(pv::pv, m)?)?;

    // Module-level functions
    m.add_function(wrap_pyfunction!(client::py_discover_servers, m)?)?;

    // Submodule: spvirit.codec — standalone encode/decode helpers.
    codec::register(m)?;

    // Submodule: spvirit.lowlevel — persistent channel & primitives.
    channel::register(m)?;

    Ok(())
}
