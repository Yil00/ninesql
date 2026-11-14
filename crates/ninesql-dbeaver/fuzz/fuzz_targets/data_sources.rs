//! Fuzzes the `data-sources*.json` decoder and the mapping: must never panic.
#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(ds) = ninesql_dbeaver::parse_data_sources(data) {
        let _ = ninesql_dbeaver::mapping::map_sources("General", &[ds], None);
    }
});
