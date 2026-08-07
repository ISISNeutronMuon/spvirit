//! Python exception hierarchy mirroring `PvGetError`.
//!
//! ```text
//! SpviritError(Exception)
//! ├── TimeoutError        (also subclasses builtin TimeoutError)
//! ├── SearchError
//! ├── ProtocolError
//! ├── DecodeError
//! ├── IoError             (also subclasses builtin OSError)
//! └── PutRejectedError
//! ```

use pyo3::create_exception;
use pyo3::prelude::*;
use pyo3::sync::GILOnceCell;
use pyo3::types::{PyDict, PyTuple, PyType};
use spvirit_client::PvGetError;

create_exception!(
    spvirit,
    SpviritError,
    pyo3::exceptions::PyException,
    "Base class for all spvirit errors."
);
create_exception!(
    spvirit,
    SearchError,
    SpviritError,
    "The PV could not be located."
);
create_exception!(
    spvirit,
    ProtocolError,
    SpviritError,
    "Unexpected or invalid PVAccess protocol state."
);
create_exception!(spvirit, DecodeError, SpviritError, "Malformed wire data.");
create_exception!(
    spvirit,
    PutRejectedError,
    SpviritError,
    "A put was rejected by the PV's on_put validator."
);

// TimeoutError and IoError also subclass the corresponding builtins
// (TimeoutError / OSError) so idiomatic `except TimeoutError:` and
// `except OSError:` catch them. `create_exception!` only supports a single
// base, so these two are created dynamically with `type()` at module init.
static TIMEOUT_ERROR: GILOnceCell<Py<PyType>> = GILOnceCell::new();
static IO_ERROR: GILOnceCell<Py<PyType>> = GILOnceCell::new();

fn new_dual_exception(
    py: Python<'_>,
    name: &str,
    extra_base: &Bound<'_, PyType>,
    doc: &str,
) -> PyResult<Py<PyType>> {
    let bases = PyTuple::new(py, [&py.get_type::<SpviritError>(), extra_base])?;
    let dict = PyDict::new(py);
    dict.set_item("__doc__", doc)?;
    dict.set_item("__module__", "spvirit")?;
    let ty = py
        .get_type::<PyType>()
        .call1((name, bases, dict))?
        .downcast_into::<PyType>()?;
    Ok(ty.unbind())
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("SpviritError", py.get_type::<SpviritError>())?;
    m.add("SearchError", py.get_type::<SearchError>())?;
    m.add("ProtocolError", py.get_type::<ProtocolError>())?;
    m.add("DecodeError", py.get_type::<DecodeError>())?;
    m.add("PutRejectedError", py.get_type::<PutRejectedError>())?;

    let timeout = TIMEOUT_ERROR.get_or_try_init(py, || {
        new_dual_exception(
            py,
            "TimeoutError",
            &py.get_type::<pyo3::exceptions::PyTimeoutError>(),
            "Operation or search timed out.",
        )
    })?;
    m.add("TimeoutError", timeout.bind(py))?;

    let io = IO_ERROR.get_or_try_init(py, || {
        new_dual_exception(
            py,
            "IoError",
            &py.get_type::<pyo3::exceptions::PyOSError>(),
            "Socket or OS-level failure.",
        )
    })?;
    m.add("IoError", io.bind(py))?;
    Ok(())
}

fn timeout_err(msg: String) -> PyErr {
    Python::with_gil(|py| match TIMEOUT_ERROR.get(py) {
        Some(ty) => PyErr::from_type(ty.bind(py).clone(), msg),
        None => SpviritError::new_err(msg),
    })
}

fn io_err(msg: String) -> PyErr {
    Python::with_gil(|py| match IO_ERROR.get(py) {
        Some(ty) => PyErr::from_type(ty.bind(py).clone(), msg),
        None => SpviritError::new_err(msg),
    })
}

/// Convert a `PvGetError` into the matching Python exception.
pub fn to_py_err(e: PvGetError) -> PyErr {
    match e {
        PvGetError::Io(e) => io_err(e.to_string()),
        PvGetError::Timeout(ctx) => timeout_err(ctx.to_string()),
        PvGetError::Search(ctx) => SearchError::new_err(ctx.to_string()),
        PvGetError::Protocol(ctx) => ProtocolError::new_err(ctx),
        PvGetError::Decode(ctx) => DecodeError::new_err(ctx),
        PvGetError::Codec(e) => DecodeError::new_err(e.to_string()),
    }
}

/// Convert a raw `std::io::Error` into the matching Python exception.
#[allow(dead_code)]
pub fn io_to_py_err(e: std::io::Error) -> PyErr {
    io_err(e.to_string())
}

/// Convert a codec / decode string error to `DecodeError`.
pub fn decode_msg_to_py_err(msg: impl Into<String>) -> PyErr {
    DecodeError::new_err(msg.into())
}

/// Convert a protocol string error to `ProtocolError`.
pub fn protocol_msg_to_py_err(msg: impl Into<String>) -> PyErr {
    ProtocolError::new_err(msg.into())
}
