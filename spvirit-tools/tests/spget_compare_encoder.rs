//! `spget_compare` used to compare captures against its own copy of the
//! CONNECTION_VALIDATION encoder. These tests pin the two ways that copy was
//! wrong, against the library encoder the tool now calls.

use spvirit_codec::spvirit_encode::{encode_authnz_user_host, encode_client_connection_validation};

/// The old local `encode_authnz_blob` (`spvirit_get_compare.rs:218-235`),
/// copied byte-for-byte, for contrast. Index 18 is the user length prefix:
/// 5 type-id bytes, then 0x02, 0x04, b"user", 0x60, 0x04, b"host", 0x60.
fn legacy_authnz_blob(user: &str, host: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xFD, 0x01, 0x00, 0x80, 0x00]);
    out.push(0x02);
    out.push(0x04);
    out.extend_from_slice(b"user");
    out.push(0x60);
    out.push(0x04);
    out.extend_from_slice(b"host");
    out.push(0x60);
    let user_bytes = user.as_bytes();
    let host_bytes = host.as_bytes();
    out.push(user_bytes.len() as u8);
    out.extend_from_slice(user_bytes);
    out.push(host_bytes.len() as u8);
    out.extend_from_slice(host_bytes);
    out
}

#[test]
fn the_library_authnz_blob_respects_frame_endianness() {
    // Compare the AUTHNZ sub-blob directly, not the whole frame: every field
    // of the surrounding CONNECTION_VALIDATION already differs between LE and
    // BE, so a whole-frame `assert_ne!` would pass even if the type id were
    // hardcoded. This asserts the exact byte the local copy got wrong.
    let le = encode_authnz_user_host("u", "h", false);
    let be = encode_authnz_user_host("u", "h", true);
    assert_eq!(&le[0..5], &[0xFD, 0x01, 0x00, 0x80, 0x00]);
    assert_eq!(&be[0..5], &[0xFD, 0x00, 0x01, 0x80, 0x00]);

    let legacy = legacy_authnz_blob("u", "h");
    assert_eq!(
        &legacy[0..5],
        &le[0..5],
        "the local copy matched the LE frame..."
    );
    assert_ne!(
        &legacy[0..5],
        &be[0..5],
        "...and silently mis-diffed every big-endian capture"
    );
}

#[test]
fn a_long_identity_is_not_truncated_to_a_single_length_byte() {
    let long_user = "u".repeat(300);
    let legacy = legacy_authnz_blob(&long_user, "h");
    // 5 type-id bytes + 0x02 + 0x04 + "user" + 0x60 + 0x04 + "host" + 0x60
    // = 18 bytes, so index 18 is the user length prefix.
    assert_eq!(legacy[18], 44, "300 as u8 wraps to 44 — a corrupt frame");

    let real = encode_client_connection_validation(0x4000, 0, 0, "ca", &long_user, "h", 2, false);
    assert!(
        real.windows(300).any(|w| w == long_user.as_bytes()),
        "the library encoder must carry the full identity"
    );
    assert!(
        real.len() > legacy.len(),
        "a proper PVA Size uses more than one byte for 300"
    );
}
