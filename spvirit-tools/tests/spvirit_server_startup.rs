//! The one `spserver` test that must NEVER skip.
//!
//! Every other `TestServer`-based test in this crate turns a startup failure
//! into `eprintln!("Skipping test, ...")` + `return`, which reports green. That
//! is fine for tests whose subject is a protocol detail, but it means a
//! regression that stops the binary booting is invisible to the whole suite.
//! This test hard-fails instead. `cargo test -p spvirit-tools` builds the
//! package's `[[bin]]` targets before running its integration tests, so
//! `target/<profile>/spserver` is guaranteed to exist by the time this runs.

mod protocol;

use protocol::frame_harness::TestServer;

#[test]
fn spserver_binary_starts_and_accepts_tcp() {
    let server = TestServer::spawn().expect("spserver must start and accept TCP within 2s");
    assert!(server.tcp_port > 0, "spserver must report a bound TCP port");
}

#[test]
fn spserver_binary_starts_in_every_pvlist_mode() {
    for mode in ["off", "discover", "list"] {
        let server = TestServer::spawn_with_args(&["--pvlist-mode", mode])
            .unwrap_or_else(|msg| panic!("spserver must start with --pvlist-mode {mode}: {msg}"));
        assert!(server.tcp_port > 0);
    }
}
