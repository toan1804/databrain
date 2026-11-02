//! Any name, quoted as an identifier for every database type.
#![no_main]

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::invariants::check_quoting;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|name: &str| {
    for kind in ConnectorKind::ALL {
        check_quoting(name, kind);
    }
});
