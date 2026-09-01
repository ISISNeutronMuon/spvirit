//! The `examples/` directory is what a `Source` author copies. Under the
//! parking convention (`pvstore.rs`'s `Source::subscribe` doc) a *pumped*
//! source's stream closing MEANS "this PV is dead" and makes the server send
//! DESTROY_CHANNEL to every subscriber — so an example must never drop a
//! `Sender` merely because its channel is `Full`.
//!
//! `senders.retain(|tx| tx.try_send(v).is_ok())` does exactly that: it treats
//! backpressure as death. This test pins the examples against a regression to
//! that one-liner, which is the shape two of them shipped with.

use std::fs;
use std::path::Path;

#[test]
fn no_example_drops_a_sender_on_a_merely_full_channel() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let entries = fs::read_dir(&dir).expect("examples/ must be readable");

    let mut offenders = Vec::new();
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("example must be readable");
        for (i, line) in text.lines().enumerate() {
            // `try_send(..).is_ok()` collapses `Full` and `Closed` into one
            // "failed" verdict; every caller that then prunes on `false`
            // drops a live receiver's sender.
            if line.contains("try_send") && line.contains("is_ok") {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.file_name().unwrap().to_string_lossy(),
                    i + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "example(s) collapse `TrySendError::Full` into `Closed` and so drop a \
         sender under ordinary backpressure. For a pumped source that reads as \
         the death of the PV and sends DESTROY_CHANNEL to every subscriber. \
         Match on `TrySendError` instead: keep `Full`, prune only `Closed`.\n  \
         {}",
        offenders.join("\n  ")
    );
}
