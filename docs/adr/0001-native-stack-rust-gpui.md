# ADR-0001: Native stack – Rust and GPUI

- **Status**: Accepted. Partially superseded by [ADR-0013](0013-gpui-source-and-supply-chain.md) for Decision §2 (GPUI and `gpui-component` now come from crates.io through `gpui-kit` / `gpui-pre`, under supply-chain controls).
- **Date**: 2026-11-14

## Context
NineSQL exists because the JVM / Eclipse RCP stack of DBeaver is heavy on Linux. The rewrite must be native, fast to start, fluid with large result sets, and correct on Wayland and HiDPI screens. It must stay Apache 2.0 (I7).

GPUI is the GPU-accelerated UI framework of the Zed editor. It is Apache 2.0, renders through Vulkan on Linux and Metal on macOS, and supports Wayland and X11. It is also young: its API changes with Zed's development, its documentation is thin, and as of late 2026 the crates.io releases lag well behind Zed's `main`, so most users depend on a git revision. `gpui-component` (Longbridge, Apache 2.0) provides a widget set (inputs, lists, virtual tables, docks, a code editor with tree-sitter highlighting) built on GPUI; its releases must match the GPUI lineage they were built against.

Zed's own `editor`, `language`, `project` and `workspace` crates are **GPL-3.0**: linking them would make NineSQL GPL.

## Decision
1. **Language**: Rust stable, edition 2024, `#![forbid(unsafe_code)]` in our crates.
2. **UI**: GPUI, pinned to **one git revision** of `zed-industries/zed` in the workspace `Cargo.toml`, together with the `gpui-component` revision known to build against it. Bumping the pair is a dedicated PR (`chore(ui): bump gpui`) with the GPUI spike checks re-run (P0-A).
3. If a crates.io release of GPUI and a matching `gpui-component` release become current enough, P0-A or a later task may switch to them; that switch does not need a new ADR.
4. **Forbidden**: Zed's GPL crates, any webview (Tauri, wry, WebKitGTK), any JVM. Enforced by `deny.toml`.
5. **SQL editor**: built on `gpui-component`'s editor / input with `tree-sitter` grammars (Apache / MIT). If it cannot meet the editor requirements of P3, the fallback is our own `ninesql-editor` crate on GPUI primitives, never Zed's `editor`.
6. **Primary target**: Linux (Debian, Ubuntu) on Wayland and X11. macOS builds are kept compiling because they make UI iteration possible for contributors on Macs, but they are not a supported release target in v1.

## Consequences
- A GPUI regression or API break can block the UI: the pinned revision and a small spike app (`crates/ninesql-ui/examples/spike.rs`) catch it before a bump lands.
- Contributors need Vulkan-capable drivers on Linux (Mesa is enough); headless CI builds the UI but does not render it. UI rendering tests are limited to view-model tests with `TestAppContext`.
- Accessibility (screen readers) depends on GPUI's progress; we track it as a known limitation and keep full keyboard navigation in the meantime.
- Build times are long on a cold cache (GPUI pulls a large dependency graph): CI caches `target/`, and AGENTS.md tells agents to wrap builds in `timeout`.

## Rejected alternatives
- **Tauri / Electron**: webview-based, against the "native and lean" goal; WebKitGTK quality varies across distributions.
- **egui**: immediate mode, excellent for tools, but weaker for large text editing, rich tables and desktop conventions (menus, IME, accessibility).
- **Iced**: retained and Elm-like, credible, but no mature virtualized table or code editor comparable to `gpui-component`.
- **Slint**: good declarative UI, but its royalty-free license is not Apache 2.0 for all uses and the DSL adds a second language.
- **GTK4 (gtk-rs)**: native on GNOME, but heavier to theme, slower for very large grids, and KDE users get a foreign look.
- **Embedding Zed's editor**: GPL-3.0, incompatible with I7.
