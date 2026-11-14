# ADR-0019: TLS CA-file semantics

- **Status**: Accepted (maintainer, 2026-11-14). Clarifies [ADR-0011](0011-tls-and-ssh-defaults.md) Decision §1–§2 (trust anchors of the per-connection CA file and of `verify-ca` / `verify-full`). Every other point of ADR-0011 stands.
- **Date**: 2026-11-14

## Context
ADR-0011 §1 says TLS uses "the platform trust store plus an optional per-connection CA file". It does not say whether the CA file is added to the platform store or replaces it, nor what `verify-ca` trusts when no CA file is set.

The difference matters for `verify-ca`, which checks the certificate chain but not the host name. With the platform store as trust anchor, any server holding a publicly trusted certificate, for any host name, passes `verify-ca`. That mode then gives no protection against a man-in-the-middle with a certificate from a public CA (I5).

`ninesql-driver-pgwire` (P1-B, wave 2) follows libpq's `sslrootcert` semantics: a CA file is the only trust anchor, and `verify-ca` refuses to connect without one ([docs/08](../08-engine-capabilities.md#pgwire-postgresql-p1-b), `crates/drivers/ninesql-driver-pgwire/src/tls.rs`). The rustdoc of `TlsConfig::ca_file` in `ninesql-driver-api` still says "trusted in addition to the platform trust store", which contradicts the driver. The wave 2 integration review (L3) and a P1-B follow-up asked for the rule to be settled before more drivers are written.

## Decision
Every driver that offers TLS applies the following rules to `TlsConfig::ca_file` and `TlsMode`.

1. **A CA file is the only trust anchor.** When a connection sets a CA file, the driver trusts only the certificates in that file. It does not add the platform trust store. This matches libpq's `sslrootcert`.
2. **`verify-ca` requires a CA file.** Without one, the connect fails with a TLS error, and the message points the user to `verify-full` for servers certified by the system trust store. There is no fallback to the platform store.
3. **`verify-full` without a CA file uses the platform trust store** (the system roots loaded through `rustls-native-certs`, ADR-0011 §1). If that store is empty or cannot be loaded, the connect fails with a TLS error. With a CA file, rule 1 applies.
4. **Fail closed on a bad CA file.** An unreadable file, a file with no PEM certificate or an invalid certificate is a TLS error. The driver never ignores the file and falls back to the platform store.
5. **`require` and `disable` do not use the CA file.** `require` encrypts without verifying the certificate (ADR-0011 §2, flagged in the UI).
6. **Tests.** Each driver runs `conformance::assert_verify_full_rejects_untrusted_certificate` against its `dev/` TLS service, and has a unit test that a CA file replaces the platform store and that `verify-ca` without a CA file fails.

The rustdoc of `TlsConfig::ca_file` in `ninesql-driver-api` is aligned with this ADR in the next reviewed driver-API PR (frozen API, `security-reviewer` review). Until then, this ADR is the reference.

## Consequences
- `verify-ca` keeps a meaning: the server is certified by a CA the user chose, not by any public CA.
- Users with a private CA must set the CA file; users of a publicly certified server use `verify-full` without one. A CA file that holds only the private CA cannot also validate a public certificate: users who need both put both in the file.
- A connection saved or imported with `verify-ca` and no CA file cannot connect. Declaring this limit in `Capabilities` and checking it at save and import time is tracked in the ROADMAP (wave 2 integration review, M2). The P1-C dialog and the DBeaver import (P2-C) must report it rather than let the connect fail later.
- pgwire already complies. The mysqlwire driver and every later driver must follow the same rules; their docs/08 sections state it.
- ROADMAP follow-ups "P1-B / driver API" and "Wave 2 integration review (L1, L3)" carry the rustdoc alignment.

## Rejected alternatives
- **CA file added to the platform store** (what the current rustdoc says): `verify-ca` would accept any publicly certified server, and a private-CA connection would also trust every public CA. It contradicts libpq, which users of PostgreSQL tools expect.
- **`verify-ca` without a CA file falls back to the platform store**: same weakness as above, silently.
- **Per-driver choice**: the same connection setting would mean different things on different engines, which the UI cannot present honestly (I8).
