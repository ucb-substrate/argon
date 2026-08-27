# Diff-Based Incremental Compilation and Explicit Saving

This document preserves the original implementation plan from the development
session. A fresh session should treat it as the source of intent, inspect the
current code before changing it, and continue from the handoff notes at the end.

## Summary

Introduce a process-local incremental compiler that consumes arbitrary editor
diffs, reuses semantic analysis and compiled cells, and compiles unsaved Neovim
buffers after a short debounce.

GUI operations will modify Neovim buffers without writing them to disk. Changes
are persisted only through `cmd-s` in the GUI or normal Neovim save commands.

## Key Changes

### Stateful compiler and semantic diffing

- Add an `IncrementalCompiler` owning versioned source snapshots, parsed and
  typed modules, dependency graphs, semantic identities, and execution caches.
- Support arbitrary byte-oriented text edits plus full-text synchronization as
  a fallback.
- Reparse only changed files, initially reparsing the complete file rather than
  incrementally updating parser tokens.
- Compute canonical AST fingerprints that include semantic structure but
  exclude spans, comments, whitespace, and transient IDs.
- Match declarations by module path, kind, and name, then recursively match
  unchanged nodes using fingerprint-aware sequence matching. Treat ambiguous
  matches conservatively as changes.
- Replace traversal-order global symbol IDs with stable IDs derived from or
  interned against semantic declaration identities.
- Maintain stable source-origin IDs separately from current spans. Reused
  diagnostics and compiled objects resolve their origins against the latest
  source snapshot, preventing stale locations after whitespace or
  preceding-text edits.
- Reload the workspace graph when module declarations, dependencies, manifests,
  GDS inputs, or LYP inputs change.

### Static and dynamic result reuse

- Split static analysis into declaration/module units and record resolved
  dependencies while type-checking.
- Maintain separate interface and body fingerprints:
  - Interface changes invalidate dependent static analysis.
  - Body changes invalidate dependent execution without unnecessarily
    re-typechecking callers.
- Re-run module DAG validation only when imports or module declarations change.
- Promote the existing per-run cell cache into the incremental session. Key
  entries by stable cell identity, exact arguments, scope-name semantics, and
  relevant external inputs.
- Record every cached cell's transitive runtime dependencies and their semantic
  revisions. Reuse an entry only when all observed functions, cells, GDS files,
  layer data, and configuration remain current.
- Store reusable compiled-cell artifacts with cache-local IDs and source
  origins. Remap IDs and materialize current spans when assembling public
  `CompiledData`.
- Reuse unaffected child and sibling cells even when an edited leaf and its
  callers must be recomputed.
- Cache diagnostics alongside their owning static or execution unit.
- Initially recompute an invalidated cell's solver state as a unit; incremental
  constraint insertion/removal remains a later optimization.
- Preserve existing one-shot compilation functions as wrappers around a
  temporary session.

### Unsaved buffers, GUI edits, and saving

- Make analyzer `Document` snapshots—not files on disk—the source of truth for
  every open file. Unopened files continue to come from disk.
- Feed every LSP `didChange` into the incremental compiler and compile the
  resulting unsaved revision.
- Change GUI source operations such as drawing, placement, dimensions,
  constraints, and SSE value updates to:
  1. Send a `WorkspaceEdit` to Neovim.
  2. Leave the affected buffer modified.
  3. Let the resulting `didChange` update and compile the in-memory snapshot.
  4. Return GUI updates without issuing a write.
- Remove `ForceSave` calls from `apply_source_edit`/`apply_source_changes`, remove
  the `custom/forceSave` request after migration, and update comments and errors
  that currently describe automatic persistence.
- Remove the implicit `write` commands from GUI-triggered undo and redo. They
  modify the Neovim buffer and compile in memory like every other edit.
- Add a global GUI `Save` action bound to `cmd-s` and exposed in the application
  menu.
- Route that action through a dedicated analyzer/Neovim `custom/save` request.
  Its Lua handler writes every modified, file-backed Argon buffer attached to
  that analyzer client, covering multi-file GUI operations without saving
  unrelated buffers or workspaces.
- Keep normal Neovim `:write`, `:update`, `:wall`, and configured save mappings
  unchanged. Their ordinary LSP `didSave` notification confirms persistence.
- Treat save as persistence, not as the source of compilation. A save flushes
  any pending debounce for the current revision but avoids recompilation when
  that revision is already current.
- Keep GUI-originated edits in Neovim's normal undo history. GUI undo/redo
  continue to delegate to Neovim and no longer alter save state implicitly.
- Show the current workspace path in the GUI title, with `Argon` first, and add
  a Vim-like unsaved marker next to the title without shifting the title when
  the marker appears.

### Scheduling and consistency

- Debounce typing-driven compilation by approximately 100–200 ms, with an
  analyzer setting to tune or disable it.
- Give source changes monotonically increasing revisions. Run compilation
  outside the async state lock and discard results older than the newest
  requested revision.
- Add cancellation checkpoints between parsing, declaration analysis, cell
  execution, and solving.
- Publish diagnostics and GUI results only for the newest completed request.
- Allow GUI edits to request an immediate compile after their corresponding
  `didChange`, so optimistic drag/placement previews settle promptly without
  requiring a save.
- Track pending analyzer-issued workspace edits until their matching
  `didChange` arrives. This prevents duplicate application and avoids falsely
  reporting the editor/GUI as out of sync while an accepted edit is propagating.
- Keep source revision inside source state. Identify a compile request by its
  source revision and `Option<String>` cell invocation.
- Use `is_latest_compile_request` for the explicit freshness policy. Do not use
  magic revision values or a vague `compile_generation` field.
- Keep compilation separate from presentation. The analyzer sends one
  `update_cell` RPC for compiled output and additionally sends `fit` when opening
  a cell. Compilation itself must not know whether the GUI is opening or
  refreshing a view.
- The GUI receives the source revision with a compilation snapshot so it can
  reject an in-flight pre-edit result while settling an optimistic drag. Treat
  this as an ordering/correlation token, not UI intent.

### Configuration and state ownership

- Put workspace-wide compiler inputs—including the root library, dependencies,
  LYP file, and GDS imports—in `WorkspaceConfig`.
- Use `WorkspaceConfig` throughout published analyzer state rather than storing
  `arc::Library` or separate LYP/GDS arguments.
- Keep application settings in `config.toml`, including compile debounce and GUI
  appearance settings.
- Support runtime overrides through one Neovim command that sets arbitrary TOML
  keys without changing the persistent file, plus commands to reload the Argon
  configuration and save the active configuration to a file.
- Keep immutable services such as the editor client outside mutable analyzer
  state.
- Separate editor/source state, compiler-worker state, published diagnostics/UI
  state, application configuration, and GUI lifecycle state.
- Let a dedicated compiler worker own `IncrementalCompiler`; analyzer events
  enqueue source updates and compile requests rather than holding the general
  analyzer state lock throughout compilation and RPC publication.
- Consolidate GUI lifecycle synchronization into one mutex:

  ```rust
  struct GuiState {
      connection: Option<GuiConnection>,
      process: Option<Child>,
      next_connection_id: u64,
  }
  ```

  Clone or take values while briefly holding the lock, then perform network RPCs
  and process termination after releasing it. Retain the connection ID to
  prevent a delayed failure from an old connection clearing a newer connection.
- Recover deliberately from poisoned synchronous locks rather than panicking.
- Clear a GUI connection only for transport/disconnection failures, not every
  RPC error or timeout.
- When a GUI action needs Neovim, focus Neovim before sending the editor command.
  Send `custom/focusEditor` as a notification so a Neovim error prompt cannot
  make the GUI wait for a request response before showing the editor.

### Cell invocation and compiler API

- Parse GUI and CLI cell selections as source-level invocations, splice them
  into a generated entry cell, and run ordinary name resolution, type checking,
  and expression evaluation. This permits arithmetic, calls, strings, enums,
  sequences, and other Argon expressions as arguments.
- Keep generated declarations outside editor-visible diagnostics and remap
  invocation diagnostics to the supplied invocation text.
- Use these compiler API names:
  - `compile`: static analysis followed by cell execution.
  - `execute_cell`: execute a typed AST with an already resolved cell path and
    evaluated `CellArg` values.
  - `execute_cell_invocation`: execute a source-level invocation that has been
    spliced and type-checked with the workspace.
- Require `WorkspaceConfig` directly in these APIs; do not retain redundant
  `_with_config` names or compatibility wrappers.

### GUI geometry behavior

- Format GUI-generated initial conditions, including rectangle drawing, with at
  most one decimal place, consistent with drag-generated values.
- Preserve optimistic drag geometry until a compilation based on the accepted
  source edit arrives. Reject stale snapshots and reconcile pending edited
  source spans/values to avoid snap-back or rectangle jumping.

## Test Plan

- Run edit sequences through both incremental and fresh one-shot compilation,
  asserting identical diagnostics and semantically equivalent `CompiledData`.
- Verify reuse for whitespace, comments, preceding-text insertion, isolated leaf
  changes, function bodies, signatures, enums, imports, module changes, GDS/LYP
  changes, syntax-error recovery, and ambiguous duplicated syntax.
- Verify runtime cache counters show unaffected cells being reused while changed
  cells and their dependency closure recompute.
- Test unsaved Neovim edits:
  - Diagnostics and GUI geometry update before saving.
  - Disk contents remain unchanged.
  - Saving does not produce a different compilation result.
- Test every GUI editing operation to ensure it changes the Neovim buffer, marks
  it modified, updates the GUI through incremental compilation, and does not
  write the file.
- Test GUI undo and redo for correct buffer contents, dirty state, and in-memory
  recompilation without disk writes.
- Test `cmd-s` with one and multiple modified Argon buffers, ensuring all
  relevant buffers are saved and unrelated buffers are untouched.
- Test normal Neovim save commands and confirm `didSave` clears persistence state
  without redundant compilation.
- Test rapid and overlapping editor/GUI edits for revision ordering, debounce
  cancellation, pending-workspace-edit reconciliation, and suppression of stale
  diagnostics or GUI results.
- Add incremental benchmarks reporting cold time, warm no-op time, isolated-leaf
  edit time, cache hits/misses, and retained cache memory.

## Assumptions and Defaults

- Compilation cache lifetime is limited to the analyzer process.
- Arbitrary LSP text edits are supported; GUI and Neovim commands do not require
  special compiler mutation APIs.
- Changed files are initially reparsed in full, while downstream work is
  selected through semantic diffing.
- Correctness takes priority over reuse; uncertain matching causes conservative
  invalidation.
- Neovim remains the owner of buffer dirty state, undo history, and disk
  persistence.
- GUI `cmd-s` saves all modified Argon buffers attached to the current analyzer
  workspace because one GUI operation may edit multiple files.
- Existing serialized compiler output remains compatible. Source-level compiler
  APIs do not need compatibility wrappers.

## Handoff Status — 2026-08-26

The latest completed work is on the `incremental-compilation` branch. At the
time this handoff was written, the worktree was clean at commit `dfee4fe`
(`update naming`) before this document was added.

Implemented and verified areas include:

- `IncrementalCompiler` and a dedicated analyzer compiler worker.
- Analyzer source revisions and exact compile-request freshness checks.
- Unsaved `Document` snapshots feeding compilation through LSP changes.
- Explicit save behavior, multi-buffer Argon save handling, and GUI dirty-state
  reporting.
- GUI title workspace path and unsaved marker behavior.
- Debounced typing compilation through `compile_after_debounce`, with immediate
  analyzer-originated edit compilation.
- Separation of `update_cell` and `fit` presentation RPCs.
- Consolidated GUI lifecycle state and selective disconnect handling.
- Runtime/persistent TOML configuration plumbing.
- `WorkspaceConfig` ownership of dependencies, LYP, and GDS inputs.
- Source-level expression cell invocation in the CLI, analyzer, preview flow,
  and incremental worker.
- One-decimal-place GUI-generated initial conditions and revision-based drag
  reconciliation.
- Public compiler API naming: `compile`, `execute_cell`, and
  `execute_cell_invocation`.

The merge with main was resolved and the following checks passed immediately
before this handoff:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --no-deps -- -D warnings
cargo test --release --workspace
cargo test --workspace
```

Do not assume that every ambitious cache item in the original plan is complete.
A fresh session should audit the implementation and tests particularly for:

- canonical semantic AST fingerprints and conservative node matching;
- stable declaration IDs and stable source-origin remapping;
- interface/body-specific dependency invalidation;
- transitive runtime dependency tracking and fine-grained child-cell reuse;
- cache-local compiled IDs and current-span materialization;
- cancellation checkpoints or true interruption during synchronous parse,
  execution, and solving;
- complete coverage of all semantic-diff cases and retained-cache memory in the
  benchmark.

Start by reading:

```text
crates/compiler/src/incremental.rs
crates/analyzer/src/compiler_worker.rs
crates/analyzer/src/lib.rs
crates/analyzer/src/rpc.rs
crates/gui/src/editor/mod.rs
crates/gui/src/editor/canvas.rs
```

Then compare the code and tests against every unchecked area above. Preserve
the established state ownership, explicit-saving behavior, `WorkspaceConfig`
API, and compilation/presentation separation while completing the deeper
semantic reuse work.
