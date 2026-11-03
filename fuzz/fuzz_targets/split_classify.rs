//! Any text, split and classified as every database type.
#![no_main]

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::invariants::check_split_classify;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    // The first byte picks the database type, so each input exercises one
    // dialect deeply instead of every dialect shallowly.
    let Some((&k, rest)) = text.as_bytes().split_first() else { return };
    let kind = ConnectorKind::ALL[k as usize % ConnectorKind::ALL.len()];
    if let Ok(rest) = std::str::from_utf8(rest) {
        check_split_classify(rest, kind);
    }
});
