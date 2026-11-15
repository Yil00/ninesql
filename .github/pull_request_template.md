## Summary
<!-- What this PR does, in two or three sentences. Title in Conventional Commits format: feat(pgwire): … -->

## ROADMAP task and features
<!-- e.g. P1-B · F-CON-05, F-CON-06 -->

## Task report
- **Done:**
- **Not done (and why):**
- **Decisions that would warrant an ADR:**
- **Files from other owners to modify:**
- **Test path used:** <!-- local / cloud sandbox with Docker / CI only -->

## Invariants
- [ ] This PR touches secrets, the network (TLS / SSH), the driver API, read-only enforcement, DBeaver files or packaging → invariants I1–I8 checked and `security-reviewer` review done
- [ ] Otherwise: not applicable

## Checklist
- [ ] fmt, clippy, tests and `cargo deny` green
- [ ] Capability matrix / feature parity inventory updated if relevant
- [ ] Documentation and ROADMAP up to date
- [ ] Tutorials in `docs/tutorials/` updated if user-visible behavior or commands changed; a new user-facing feature has a tutorial section
- [ ] Screenshots attached for visible UI changes (local run)
- [ ] DCO-signed commits (`git commit -s`)
