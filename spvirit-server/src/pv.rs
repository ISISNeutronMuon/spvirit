//! Typed PV handles — the ergonomic front door to [`SimplePvStore`].
//!
//! A [`Pv<T>`] is created *pending* (it owns a record template plus attached
//! callbacks) and becomes *bound* to a store when passed to
//! `PvaServer::serve(...)`. Handles are cheap clones; all clones observe and
//! drive the same record.

use spvirit_types::ScalarValue;

/// Errors from typed PV handle operations.
#[derive(Debug, Clone, PartialEq)]
pub enum PvError {
    /// The handle has not been bound to a server/store yet.
    Unbound,
    /// No record with this name exists in the store.
    NotFound(String),
    /// The record's value type does not match the handle's `T`.
    TypeMismatch {
        expected: &'static str,
        actual: String,
    },
    /// A PUT was rejected by an `on_put` callback.
    PutRejected(String),
}

impl std::fmt::Display for PvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PvError::Unbound => write!(f, "PV handle is not bound to a server yet"),
            PvError::NotFound(n) => write!(f, "PV '{n}' not found"),
            PvError::TypeMismatch { expected, actual } => {
                write!(
                    f,
                    "PV value type mismatch: expected {expected}, record holds {actual}"
                )
            }
            PvError::PutRejected(msg) => write!(f, "PUT rejected: {msg}"),
        }
    }
}

impl std::error::Error for PvError {}

/// Scalar types that can back a typed [`Pv<T>`] handle.
pub trait PvScalar: Sized + Send + Sync + 'static {
    /// Human-readable type name, used in [`PvError::TypeMismatch`].
    const TYPE_NAME: &'static str;
    fn into_scalar(self) -> ScalarValue;
    fn from_scalar(v: ScalarValue) -> Option<Self>;
}

impl PvScalar for f64 {
    const TYPE_NAME: &'static str = "f64";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::F64(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::F64(x) => Some(x),
            ScalarValue::F32(x) => Some(x as f64),
            _ => None,
        }
    }
}

impl PvScalar for bool {
    const TYPE_NAME: &'static str = "bool";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::Bool(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::Bool(b) => Some(b),
            _ => None,
        }
    }
}

impl PvScalar for i32 {
    const TYPE_NAME: &'static str = "i32";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::I32(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::I32(x) => Some(x),
            ScalarValue::I16(x) => Some(x as i32),
            ScalarValue::I8(x) => Some(x as i32),
            _ => None,
        }
    }
}

impl PvScalar for String {
    const TYPE_NAME: &'static str = "String";
    fn into_scalar(self) -> ScalarValue {
        ScalarValue::Str(self)
    }
    fn from_scalar(v: ScalarValue) -> Option<Self> {
        match v {
            ScalarValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spvirit_types::ScalarValue;

    #[test]
    fn pvscalar_roundtrip_f64() {
        assert_eq!(f64::from_scalar(ScalarValue::F64(1.5)), Some(1.5));
        assert!(matches!(1.5f64.into_scalar(), ScalarValue::F64(x) if x == 1.5));
    }

    #[test]
    fn pvscalar_f64_accepts_f32_widening() {
        assert_eq!(f64::from_scalar(ScalarValue::F32(2.0)), Some(2.0));
    }

    #[test]
    fn pvscalar_rejects_wrong_variant() {
        assert_eq!(f64::from_scalar(ScalarValue::Str("x".into())), None);
        assert_eq!(bool::from_scalar(ScalarValue::F64(1.0)), None);
        assert_eq!(i32::from_scalar(ScalarValue::Str("1".into())), None);
        assert_eq!(String::from_scalar(ScalarValue::Bool(true)), None);
    }

    #[test]
    fn pverror_display() {
        let e = PvError::TypeMismatch {
            expected: "f64",
            actual: "Str".into(),
        };
        assert!(e.to_string().contains("f64"));
        assert!(PvError::Unbound.to_string().contains("not bound"));
    }
}
