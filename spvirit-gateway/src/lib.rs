//! spvirit-gateway — a p4p-compatible PVAccess gateway.
pub mod bridge;
pub mod cache;
pub mod config;
pub mod convert;
pub mod loopguard;
pub mod proxy;
pub mod upstream;
#[cfg(test)]
mod smoke {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
