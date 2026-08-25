# GPUI upstream audit

Audited on 2026-08-21 against:

- Argon's pinned fork: [`ucb-substrate/zed@88651f66`](https://github.com/ucb-substrate/zed/commit/88651f66a37f5447a607a13325b3a6be24b2135a)
- Upstream: [`zed-industries/zed@f36aec82`](https://github.com/zed-industries/zed/commit/f36aec822be697df9049fed020b593147c93b4cf)

## Recommendation

Keep the current fork for now. GPUI is sufficient for Argon's current desktop
UI, and there is no framework-level reason to migrate away from it. Current
upstream is not a drop-in replacement, however: Argon depends on one feature
that upstream still does not provide, and eight months of upstream API changes
need a focused compatibility update.

The long-term target should be upstream GPUI plus either an upstreamed
per-edge-border patch or an Argon-side renderer for individual rectangle
edges. Until one of those exists, rebasing the small fork is lower risk than
dropping it.

## Fork-only functionality still required

The fork contains three Argon commits:

1. [`7c01abaa`](https://github.com/ucb-substrate/zed/commit/7c01abaafdb7c8c4826012547836a7c2b45c9a84)
   changes `PaintQuad` from one `BorderStyle` to `Edges<BorderStyle>`. Argon
   uses this to render only the unconstrained edges of a rectangle as dashed.
   Those same edges are the ones exposed as solution-space exploration drag
   handles, so this is user-visible editing state rather than decoration.
2. [`bbfcd861`](https://github.com/ucb-substrate/zed/commit/bbfcd8619fbae95c3c35d3ef6f46f893f3a15b5f)
   corrects the per-edge dashed-border calculation in the Metal shader.
3. [`88651f66`](https://github.com/ucb-substrate/zed/commit/88651f66a37f5447a607a13325b3a6be24b2135a)
   implements the per-edge representation in WGSL and HLSL and fixes the
   required GPU-struct alignment for Linux and Windows.

Upstream still exposes a single `PaintQuad.border_style: BorderStyle`. All
three fork commits therefore remain necessary to preserve the current visual
behavior on macOS, Linux, and Windows. No other fork-only functionality was
found.

An alternative that removes the fork is to paint four narrow edge quads in
Argon, assigning each a solid or dashed style. That would need visual testing
around rounded corners, clipping, scaling, and dash continuity, but it keeps
the customization out of GPUI internals. Upstreaming the existing generalized
patch is preferable if GPUI maintainers accept the API.

## Current upstream compatibility

A clean `cargo check -p argone` against the exact upstream commit reached
Argon's code and reported 31 compile errors. Only two errors are caused by the
missing fork feature; the other 29 are ordinary API drift concentrated in a
few migrations:

| Area | Errors | Required change |
| --- | ---: | --- |
| Blocking RPC calls | 12 | Replace removed `BackgroundExecutor::block*` use with an async RPC boundary; avoid blocking GPUI's UI thread. |
| Focus calls | 8 | Pass the app context to `Window::focus`. |
| Menus | 4 | Initialize the new `Menu.disabled` field. |
| Entity updates | 2 | Remove obsolete `.unwrap()` calls now that `Entity::update` returns `()`. |
| Text painting | 2 | Pass the new alignment and optional wrap-width arguments. |
| Per-edge borders | 2 | Rebase/upstream the fork patch or render the four edges in Argon. |
| Application startup | 1 | Add `gpui_platform` and construct the platform application through it. |

Upstream currently declares Rust 1.97.1, while this checkout uses Rust 1.91.1.
That toolchain upgrade is a prerequisite for adopting the audited upstream
commit; it is not a fork feature.

## Suggested upstreaming sequence

1. Pin an exact upstream commit and update the Rust toolchain.
2. Apply the mechanical startup, menu, focus, text-painting, and entity-update
   changes.
3. Convert the synchronous language-server wrapper to async request tasks with
   explicit timeout and UI update handling. This is the highest-risk part and
   should receive interaction tests.
4. Rebase the three border commits, then validate Metal, WGSL, and HLSL shader
   layouts on the supported desktop platforms.
5. Submit the per-edge-border support upstream. Once accepted, switch Argon to
   upstream directly and retire the fork.

Expect roughly three to six focused engineering days to reach a compiling,
functionally equivalent upstream build, and up to one to two weeks if the RPC
refactor and all three desktop rendering paths are exercised and hardened.
