//! spvirit-gateway — a p4p-compatible PVAccess gateway.
pub mod config;
pub mod loopguard;
pub mod upstream;
#[cfg(test)]
mod smoke {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
