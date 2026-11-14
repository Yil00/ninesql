# ADR-0016: SSH – RSA keys disabled until russh moves to rsa ≥ 0.10, and non-strict host-key policies

- **Status**: Accepted (maintainer, 2026-11-14). Amends [ADR-0011](0011-tls-and-ssh-defaults.md) Decision §5 and §7 (RSA private keys and RSA host keys), §6 (host-key policies other than the default), and clarifies §4 (TLS mode of imported DBeaver connections). Every other point of ADR-0011 stands.
- **Date**: 2026-11-14

## Context
[ADR-0011](0011-tls-and-ssh-defaults.md) chose `russh` for SSH tunnels (§5), required host keys to be checked, with unknown hosts confirmed by the user and changed keys blocked (§6), and allowed RSA keys with SHA-2 signatures (§7). It also said imported DBeaver connections keep their effective TLS mode (§4). P1-D work on `ninesql-ssh` and the P2-A security review found:

- **RSA.** In russh 0.65.1, RSA support sits behind the `rsa` Cargo feature, which pins `rsa =0.10.0-rc.18`. That version is flagged by RUSTSEC-2023-0071 (Marvin attack, timing side channel; patched in `rsa` ≥ 0.10.0). The project does not add new advisory ignores to `deny.toml`. The Marvin attack concerns private-key operations (decryption and signing); verifying a host key signature alone would not be exposed. The feature cannot be enabled for host keys only.
- **Host-key policies.** The frozen driver API ([ADR-0004](0004-driver-api-runtime-and-results.md)) already has `HostKeyVerification::Disabled` next to `Strict`, and users migrating from DBeaver expect a "trust new hosts automatically" mode. ADR-0011 §6 only describes the strict behavior. The `ninesql-core` connection model (`SshHop`) states that there is deliberately no option to skip host-key verification.
- **Crypto provider.** russh needs either `ring` or `aws-lc-rs` as its crypto backend.
- **DBeaver TLS import.** The P2-A security review asked what "keep their effective TLS mode" means when a DBeaver connection has no TLS settings at all.

## Decision
### 1. RSA support stays off
1. The russh `rsa` feature is **not enabled**. No ignore for RUSTSEC-2023-0071 is added, even though host-key verification alone would not be exposed.
2. Resulting behavior, which the UI and documentation must state plainly:
   - RSA private key files (`id_rsa`, any OpenSSH RSA key) are refused with a clear error that suggests an Ed25519 or ECDSA key, or `ssh-agent`.
   - RSA keys held by `ssh-agent` still work: the agent signs, NineSQL never handles the private key. Only SHA-2 signatures (`rsa-sha2-256`, `rsa-sha2-512`) are accepted.
   - Servers that offer only RSA host keys are unreachable. Negotiation fails with "no common algorithm"; this fails safe.
   - A host known only under an `ssh-rsa` key in `~/.ssh/known_hosts` is treated as **unknown**, and the key types it is known under are reported (`other_key_types`). Such a host always needs explicit user confirmation, under every policy except `Disabled`; it is never auto-accepted, not even under `AcceptNew`.
   - Imported DBeaver connections that use an RSA key file (`id_rsa`) need a different key type or `ssh-agent`; the import report should flag them.
3. **Revisit** when a russh release depends on `rsa` ≥ 0.10.0 (tracked in the [ROADMAP](../ROADMAP.md#phase-1-follow-ups)). Enabling RSA then is its own PR with a `security-reviewer` review.

### 2. Host-key policies, per hop
Each SSH hop (jump hosts and the last hop) carries one of three policies:

| Policy | Unknown host | Host known under another key type only | Changed key | `@revoked` key |
|---|---|---|---|---|
| `Strict` (default) | Explicit user confirmation, showing the SHA-256 fingerprint | Confirmation, with `other_key_types` shown | Hard error | Hard error |
| `AcceptNew` | Key recorded in NineSQL's `known_hosts` without asking; known keys still verified | Confirmation, with `other_key_types` shown | Hard error | Hard error |
| `Disabled` | Not verified | Not verified | Not verified | Refused |

1. `Strict` is the default for every hop, as ADR-0011 §6 requires. A changed key keeps ADR-0011 §6's behavior: no "connect anyway" in the same dialog.
2. `AcceptNew` and `Disabled` are explicit, per-connection, per-hop choices. They are always surfaced as security warnings: a badge on the connection and its tabs, and a warning text in the connection dialog (I5).
3. The UI shows `other_key_types` ("this host is known under another key type") and every entry of `Tunnel::security_warnings()`, including fingerprints that `AcceptNew` recorded automatically.
4. Exposure: the `ninesql-core` connection model gains a per-hop policy field in P1-D core work, replacing the "no option to skip host key verification" rule of `SshHop`. Until that lands, nothing exposes `AcceptNew` or `Disabled` to the user.
5. Driver API: `TunnelInfo` currently has no `AcceptNew` variant, so a tunnel that used `AcceptNew` on any hop is reported as `HostKeyVerification::Disabled`. This over-warns and never under-warns. Adding a `HostKeyVerification::AcceptNew` variant is a follow-up that changes the frozen API and needs its own `security-reviewer`-reviewed PR.

### 3. Crypto provider
russh uses the **`ring`** backend. `ring` is already in the lockfile and pulls no OpenSSL, consistent with [ADR-0002](0002-licensing-and-dependency-policy.md) and ADR-0011.

### 4. Clarification of ADR-0011 §4 (DBeaver import TLS)
"Imported DBeaver connections keep their effective TLS mode" applies to TLS settings the DBeaver connection had **explicitly**. When it has none, the NineSQL default (`verify-full`) applies, and the import report says so for that connection.

## Consequences
- No advisory ignore for SSH; `cargo deny check advisories` stays clean.
- Users with only RSA key files, or servers with only RSA host keys, cannot connect over SSH until RSA is re-enabled. The workaround for keys is `ssh-agent` or a new Ed25519 key; there is none for RSA-only servers. The connection error and the DBeaver import report must say so.
- `AcceptNew` weakens the first connection to a host (trust on first use without asking) and `Disabled` removes MITM protection for that hop. Both stay visible at all times; neither is ever a default, and neither is set by the DBeaver import.
- Tests to add in `ninesql-ssh`: an RSA key file is refused; an agent-held RSA key signs with SHA-2; a host known only under `ssh-rsa` is reported as unknown with `other_key_types` and is not auto-accepted under `AcceptNew`; a changed or `@revoked` key blocks under `Strict` and `AcceptNew`; `@revoked` is refused under `Disabled`.
- Follow-ups (ROADMAP): RSA support once russh depends on `rsa` ≥ 0.10.0; the `HostKeyVerification::AcceptNew` variant in the driver API; the per-hop policy field in `ninesql-core` (P1-D).

## Rejected alternatives
- **Enable russh's `rsa` feature and ignore RUSTSEC-2023-0071 under a scoped ADR**: the exposure for host-key verification is low, but the feature also enables RSA private-key signing, which is the vulnerable operation, and the project does not add new advisory ignores.
- **Drop `AcceptNew` and `Disabled` entirely**: keeps ADR-0011 §6 untouched, but pushes users with many hosts or throwaway environments to other tools, and `Disabled` already exists in the frozen driver API. Explicit, flagged opt-ins are consistent with I5.
- **`aws-lc-rs` as the crypto backend**: adds a large C build to the graph for no gain over `ring`, which is already in the lockfile.
