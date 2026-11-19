# ADR-0028: Allow the FreeType License for the bundled FreeType, and record the icon licenses

- **Status**: Accepted (maintainer, 2026-11-19). Amends [ADR-0002](0002-licensing-and-dependency-policy.md) Decision §2 (license allow-list) and §5 (bundled C libraries): the FreeType License (FTL) is allowed for the FreeType copy that `freetype-sys` compiles and links statically. It also records the icon licenses (Lucide: ISC; Feather: MIT) of the assets embedded through `gpui-kit-assets`. Every other point of ADR-0002 stands.
- **Date**: 2026-11-19

## Context
[ADR-0002](0002-licensing-and-dependency-policy.md) §2 lists the licenses allowed for code linked into the shipped binary, and §5 allows C libraries bundled through a crate when their license passes §2, with their notices in `NOTICE` and the `.deb` copyright files. Anything else needs a new ADR.

The P6-E end-of-MVP security review (2026-11-19, finding M1) found two pieces of third-party code in the release binary whose notices were not shipped:

- **FreeType 2.13.2**, compiled from source and linked statically by `freetype-sys` 0.20.1, which `zed-font-kit` 0.14.1-zed pulls in for `gpui-pre-wgpu` (GPUI's text rendering on Linux). `freetype-sys` builds its bundled copy because the FreeType of the Ubuntu 22.04 builder (the glibc floor of the [ADR-0010 P6-A amendment](0010-packaging-and-distribution.md#amendment-p6-a-packaging-2026-11-19) §8) is older than the version the crate requires. FreeType is offered under a choice of the FreeType License (FTL) or GPL-2.0; NineSQL takes it under the FTL. The FTL is a permissive BSD-style license, compatible with distributing NineSQL under Apache-2.0, whose one material condition is a credit: the documentation of a product that uses FreeType must say so ("Portions of this software are copyright © The FreeType Project (www.freetype.org). All rights reserved.").
- **Lucide icons (ISC) and Feather icons (MIT)**, embedded as SVG assets by `gpui-kit-assets` 0.9.1. Both licenses are already in the ADR-0002 §2 allow-list, but the crate declares only its own license, so the icons' copyright notices were not in the third-party license file.

`cargo deny` cannot see either: `freetype-sys` declares `MIT` (the license of its Rust code), and the license of the icon files is not in `gpui-kit-assets`'s metadata. The license file that `cargo about` generates for the `.deb` is built from the same metadata, so it missed them too.

The maintainer's choice on 2026-11-19 was to keep the static FreeType and credit it, rather than link the system FreeType.

## Decision
1. The **FreeType License (FTL)** is allowed for the FreeType copy that `freetype-sys` compiles and links statically into the binary. NineSQL uses FreeType under the FTL, never under its GPL-2.0 option (I7).
2. The FTL credit, the FreeType copyright notice and the Lucide (ISC) and Feather (MIT) copyright and permission notices are shipped with every package: in `NOTICE` and in the files installed under `/usr/share/doc/ninesql/` (and the Flatpak's license directory). The fix is the P6-E M1 follow-up PR (`core-engineer`, `NOTICE` and `packaging/`).
3. **Enforcement.** Because `cargo deny` reads `freetype-sys` as MIT, `deny.toml` is not changed and cannot enforce this decision. The notices check that `make deb` runs (added by the M1 fix) fails the build when one of these notices is missing from the package. A change of the font stack (a new crate bundling a C library, a `freetype-sys` or `gpui-kit-assets` update that changes what it bundles) is checked by hand against this ADR in the GPUI bump PR ([ADR-0013](0013-gpui-source-and-supply-chain.md)).
4. **Tracking.** The bundled FreeType is listed in the CycloneDX SBOM as its own component, with its version, so a FreeType advisory can be matched against a release (P6-E L4, same follow-up PR).
5. Any other license, or FreeType under any other terms, still needs a new ADR, as ADR-0002 §2 says.

## Consequences
- The release binary carries its own FreeType, which **gets no Debian or Ubuntu security update**: a FreeType vulnerability is fixed for NineSQL users only by a NineSQL release with an updated `freetype-sys` (or a system FreeType once the builder floor allows it). It is tracked as a [Phase 6 follow-up](../ROADMAP.md#phase-6-follow-ups) and through the SBOM.
- License compliance for bundled C code and embedded assets now depends on a check outside `cargo deny`. Contributors who add a dependency that bundles C code or assets must look past the crate's declared license (ADR-0002 §5 already asked for the notices; this ADR makes the check part of `make deb`).
- Lucide and Feather need no ADR of their own: ISC and MIT were already allowed. They are recorded here because their notices were missing for the same reason as FreeType's.

## Rejected alternatives
- **Link the system FreeType** (`libfreetype6` in `Depends`): it would get distro security updates, but the Ubuntu 22.04 builder's FreeType is below `freetype-sys`'s floor, so it would mean raising the builder floor (dropping Ubuntu 22.04 and Debian 12, [docs/10](../10-supported-platforms.md)) or patching the crate ([ADR-0018](0018-patched-third-party-crates.md)); not done before v0.1.0. It can be reconsidered when the floor moves.
- **Add `FTL` to `[licenses] allow` in `deny.toml`**: does nothing, since no crate declares FTL; it would suggest a check that does not exist.
- **Replace GPUI's font stack**: out of proportion for a permissive license whose only condition is a credit.
