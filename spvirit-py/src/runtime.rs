//! Shared Tokio runtime and async bridge helpers.
//!
//! A single multi-thread runtime is lazily created and shared across the
//! entire Python process.  Sync wrappers use `RUNTIME.block_on(...)` inside
//! `py.allow_threads()`; async wrappers use `future_into_py` backed by
//! `pyo3_async_runtimes`.

use std::future::Future;
use std::sync::LazyLock;

use pyo3::prelude::*;
use tokio::runtime::Runtime;

pub static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime for spvirit-py")
});

/// Initialize the `pyo3-async-runtimes` bridge with our shared RUNTIME.
/// Called once from the module init.  Sync `RUNTIME.block_on(...)` and
/// async `future_into_py` then share the same thread pool.
pub fn init_async_runtime() {
    // `&*RUNTIME` has `'static` lifetime because RUNTIME itself is `static`.
    let _ = pyo3_async_runtimes::tokio::init_with_runtime(&*RUNTIME);
}

/// Block on a future, releasing the GIL for the duration.  Use from sync
/// methods.
///
/// Handle-aware: if we're already executing inside a `RUNTIME`-driven task
/// (e.g. a `Pv::on_put` validator invoked while a client PUT is being
/// processed on a runtime worker thread, and that validator's Python
/// callback turns around and calls `pv.set()`/`pv.get()` on some other PV),
/// a plain `RUNTIME.block_on(fut)` would panic — you cannot block_on inside
/// a runtime. In that case we use `tokio::task::block_in_place`, which lets
/// the current worker thread block while the runtime moves other tasks to
/// different workers (mirrors the re-entrancy handling in
/// `source::PyNotifier::notify`). Outside the runtime (the common case —
/// called straight from a Python thread), we just `RUNTIME.block_on` as
/// before.
pub fn block_on_py<F, T>(py: Python<'_>, fut: F) -> T
where
    F: Future<Output = T> + Send,
    T: Send,
{
    py.allow_threads(|| {
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| RUNTIME.block_on(fut))
        } else {
            RUNTIME.block_on(fut)
        }
    })
}

/// Convert a Rust future into a Python awaitable (asyncio Future).
pub fn future_into_py<F, T>(py: Python<'_>, fut: F) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    pyo3_async_runtimes::tokio::future_into_py(py, fut)
}
