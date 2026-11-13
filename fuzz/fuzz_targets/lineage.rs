//! Any text, traced as every SQL database type (PII masking reads this graph).
#![no_main]

use databrain_connector_core::ConnectorKind;
use databrain_lineage::invariants::check_lineage;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first byte picks the database type, as in `split_classify`.
    let Some((&k, rest)) = data.split_first() else { return };
    let kind = ConnectorKind::ALL[k as usize % ConnectorKind::ALL.len()];
    if !kind.traits().language.is_sql() {
        return;
    }
    if let Ok(text) = std::str::from_utf8(rest) {
        check_lineage(text, kind);
    }
});
