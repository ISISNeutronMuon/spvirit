//! spvirit — Python bindings for PVAccess client and server.

use pyo3::prelude::*;

mod convert;
mod errors;
mod runtime;

pub mod channel;
pub mod client;
pub mod codec;
pub mod discovery;
pub mod monitor_update;
pub mod nt;
pub mod packet;
pub mod pv;
pub mod server;
pub mod source;

#[pymodule]
fn spvirit(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Install the async bridge onto our shared Tokio runtime.
    runtime::init_async_runtime();

    // Eagerly import `threading` on the thread that is importing this
    // extension module. `import spvirit` always happens on the embedding
    // process's real main thread (it's a plain Python-level import), so
    // this guarantees `threading._main_thread` gets bound to the correct
    // OS thread's identity at module-load time.
    //
    // Why this matters: `threading.py` sets its module-level `_main_thread`
    // singleton to whichever thread is executing when `threading` is FIRST
    // imported into the process -- not necessarily the process's actual
    // main OS thread. Our async bridge (`source::asyncio_loop`) lazily
    // constructs an asyncio event loop the first time an `async def`
    // source/hook/event-handler callback runs, which can easily be on a
    // Tokio worker thread rather than this one. If THAT import is what
    // first pulls in `threading` (transitively, via `import asyncio`), the
    // worker thread gets mistaken for "the main thread" from then on. On
    // Windows this is directly observable: `asyncio.new_event_loop()`
    // builds a `ProactorEventLoop`, whose constructor calls
    // `signal.set_wakeup_fd()` iff `threading.current_thread() is
    // threading.main_thread()` -- and with `_main_thread` mis-bound to a
    // worker thread, that comparison spuriously succeeds on the worker
    // thread and raises `ValueError: set_wakeup_fd only works in main
    // thread of the main interpreter`, while succeeding (wrongly) on the
    // real main thread. Importing `threading` here, unconditionally and
    // before any background thread can touch Python, pins `_main_thread`
    // to the correct identity for the lifetime of the process.
    m.py().import("threading")?;

    // Error types
    errors::register(m)?;

    // Client classes
    m.add_class::<client::PyClient>()?;
    m.add_class::<client::PyClientBuilder>()?;
    m.add_class::<client::PyGetResult>()?;
    m.add_class::<client::PyDiscoveredServer>()?;
    m.add_class::<client::PySubscription>()?;

    // Server classes
    m.add_class::<server::PyServerBuilder>()?;
    m.add_class::<server::PyServer>()?;
    m.add_class::<server::PyStore>()?;
    m.add_class::<server::PyEventDecorator>()?;

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
    m.add_function(wrap_pyfunction!(pv::scalar, m)?)?;

    // Module-level functions
    m.add_function(wrap_pyfunction!(client::py_discover_servers, m)?)?;

    // Submodule: spvirit.codec — standalone encode/decode helpers.
    codec::register(m)?;

    // Submodule: spvirit.lowlevel — persistent channel & primitives.
    channel::register(m)?;

    Ok(())
}
