//! Fuzzes the `credentials-config.json` decoder (decryption + structure) and the mapping of its
//! result against a fixed data-sources file: must never panic. Random inputs rarely pass the
//! PKCS#7 padding check; tests/fuzz_decoders.rs also fuzzes validly encrypted plaintexts.
#![no_main]
#![forbid(unsafe_code)]

use libfuzzer_sys::fuzz_target;

const DATA_SOURCES: &[u8] = br#"{"connections":{"c1":{"provider":"postgresql","driver":"postgres-jdbc","name":"c1","configuration":{"host":"db.example.test","handlers":{"ssh_tunnel":{"type":"TUNNEL","enabled":true,"properties":{"host":"bastion.example.test","authType":"PUBLIC_KEY","keyPath":"/home/nine_user/k"}}}}}}}"#;

fuzz_target!(|data: &[u8]| {
    let creds = ninesql_dbeaver::decrypt_credentials(data);
    if let Ok(ds) = ninesql_dbeaver::parse_data_sources(DATA_SOURCES) {
        let _ = ninesql_dbeaver::mapping::map_sources("General", &[ds], Some(creds));
    }
});
