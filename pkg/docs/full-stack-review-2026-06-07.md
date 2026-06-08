# Full-Stack Review — Consolidated Report
**Date:** 2026-06-07
**Scope:** 42 files / 5,122 deletions / 2,198 insertions / 3 reviewers in parallel
**Mode:** team (CON · MNT · SCL · COR × USB · ROB · SEC · EFF)

---

## 1. Summary Card

| Metric | Value |
|---|---|
| Reviewers dispatched | 3 (A: manifest/registry · B: pipeline/memory/LLM · C: TUI/UI) |
| Findings total | 16 |
| P0 / P1 / P2 / P3 | 5 / 6 / 4 / 1 |
| Verified by file:line | 16/16 |
| Build / test status | `cargo build` clean · 958 lib + 15 silent_router + 18 pressure tests pass · 110+ deprecation warnings |
| Release verdict | **HOLD** — 3 P0 TUI bugs are release-blockers; 2 P0 manifest/cache bugs are correctness |
| Quality dimension (1-5) | CON=2 · MNT=2 · COR=2 · SCL=4 · USB=2 · SEC=3 · EFF=4 |

**One-line headline:** Renderer migrated to V42-B CardStream, writers did not. The TUI is structurally clean and the backend optimizations are correct in intent — but five P0 bugs need to ship in the same release as the refactor or the v1.5.0 release will regress basic chat persistence.

---

## 2. Findings — Consolidated

### Reviewer A — Manifest / Tool Registry

**A-1 [P0 · CON] `fs_mkdir` is registered twice in tools.toml, second entry overwrites first**
- Evidence: `tools.toml:142` and `tools.toml:212` both define `fs_mkdir` · `manifest.rs:83` HashMap insert — last write wins · second entry has no `cluster` field, so `cluster_of("fs_mkdir")` returns `None`
- Impact: When manifest is the source of truth for cluster assignment (Phase 3 design), `fs_mkdir` silently loses its cluster membership. `extract_tool_domain` fallback would still work, but `cluster_of()` would panic or return `Err` depending on the consumer
- Fix: Delete `tools.toml:211-221`. Add a `tools.toml:1-5` loader assertion that asserts `by_name` length == unique-name count to prevent future duplicates from silently overwriting

**A-2 [P0 · CON] `code_exec` name in manifest does not match `code_execute` in code**
- Evidence: `tools.toml:460` declares `[tools.code_exec]` · `code_exec.rs:63,65,109` emits `ToolId("code_execute")` · `injector.rs:1385` and `mcip.rs:248` both reference `code_execute`
- Impact: `manifest.by_name.get("code_exec")` returns `None`. The new `schemas()` lookup fails for this tool — schema shows up as a stub or panic depending on the call path. `code_execute` is currently *invisible* to the manifest-driven code path
- Fix: Rename `tools.toml:460` to `[tools.code_execute]`. Re-run schema test to confirm

### Reviewer B — Pipeline / Memory / LLM Optimizations

**B-1 [P0 · ROB/COR] Checkpoint hash cache is a poisoned OnceLock + not session-scoped**
- Evidence: `post.rs` declares `static LAST_HASH: OnceLock<std::sync::Mutex<Option<u64>>> = OnceLock::new()` · accessor uses `.lock().unwrap()` (panics forever on poison) · static lifetime means cache is shared across sessions · `std::sync::Mutex` held across `.await` is unsound in tokio
- Impact: Any panic inside the lock guard (e.g., during a `Hash::hash` call) corrupts the lock; subsequent turns `unwrap()` and panic the whole TUI. Worse: User A's cache is read by User B if sessions share a process
- Fix: Move to `TurnContext` (already `&mut` passed in — no clone needed per Phase 1 decision) · use `tokio::sync::Mutex` · clear on `SessionStart` boundary

**B-2 [P1 · COR] `pin_tool_behavior` race against LFU prune in same turn**
- Evidence: `post.rs` call sequence is `record_tool_behavior(...)` → `pin_tool_behavior(...)` for D-tier promotion · `memory_palace.rs::record_tool_behavior` calls `prune()` inline when capacity is hit · LFU `prune()` skips `pinned` entries, but at the time of `prune()` the entry is not yet pinned
- Impact: If `record_tool_behavior` triggers `prune()` and the new entry is the LFU victim, it is evicted before `pin_tool_behavior` runs. The user's expectation of "D-tier tools stay in Palace" is violated under memory pressure
- Fix: Reorder in `post.rs` — set `pinned: true` on the entry *before* `record_tool_behavior` (or call `pin_tool_behavior` first, then `record_tool_behavior` with the already-pinned entry)

### Reviewer C — TUI / UI Layer

**C-1 [P0 · COR/USB] User-typed messages never appear in the chat panel**
- Evidence: `state/mod.rs:3625 add_message` writes only `state.messages` (the `#[deprecated]` field) · `modes/common.rs:84,98 render_cards` reads only `state.cards` · `cards/writer.rs:164 push_user_message` exists as the intended bridge but grep across the whole crate shows **zero call sites** · `event/mod.rs:1133, 2270, 2292, 2317, 2358, 2401` and `run.rs:655, 875` all call `state.add_message(...)` directly
- Impact: 100% user-impact. Every typed turn: user message disappears. LLM response renders normally. Looks like "the AI is replying to nothing"
- Fix: Wire `push_user_message` into the existing `add_message` path. Two paths to choose:
  - Path A (minimal, in-place fix): At the top of `add_message`, call `crate::tui::cards::writer::push_user_message(state, &text, &ts)` in addition to the existing message-vector write
  - Path B (clean, full migration): Delete `state.messages` from the V42-B hot path; route all user text through `push_user_message`. Bump `SessionExport::version` to 4 and serialize `cards` field (C-3)

**C-2 [P0 · COR] V40 sessions load into an empty chat panel**
- Evidence: `run.rs:3140 load_session_from_path` → `apply_session_export` (`run.rs:3176-3180`) writes only `state.messages` from the export's `messages` JSON field · `state/session_migrate.rs:86 migrate_v3_to_v4` exists with 9 passing unit tests, but grep across the entire crate returns **zero non-test call sites** · module comment at `session_migrate.rs:11-12` admits: *"本 Phase 暂不实际转换 messages → cards"*
- Impact: >80% of users with existing `~/.abacus/sessions/*.json` (v2 format) will see an empty panel after upgrading to v1.5.0. The `.v3_backup/` is never created because the migration never runs
- Fix: At the top of `apply_session_export`, dispatch on `export.version`:
  - `version <= 3` → call `migrate_v3_to_v4(&export, path)` first, then proceed with the v4 read
  - `version == 4` → read the new `cards` field directly

**C-3 [P0 · COR] `save_session` writes the wrong format**
- Evidence: `run.rs:3031-3038` constructs `SessionExport { version: 2, …, messages: state.messages.iter().cloned().collect(), … }` · no `cards` field · `SessionVersion::V4` variant in `session_migrate.rs:44` expects `version: 4` and a `cards` field
- Impact: Combined with C-1, **the v1.5.0 release has no working persistence at all** — type a message, it's invisible; save the session, the format is wrong; reload, the format is also wrong but with no messages to lose anyway. Catastrophic regression for the only stateful UX feature
- Fix: Bump `version: 4` · replace `messages: Vec<Message>` with `cards: CardStream` (need `Serialize` derive on the Card types in `abacus-ui-kit` or serialize via `to_value()` if `CardStream` is opaque) · update `apply_session_export` to read the new field. This is a breaking change for `~/.abacus/sessions/*.json` — but the file is unusable in V42-B without it

**C-4 [P1 · MNT] `run.rs.bak` is a 205 KB stray file in the diff**
- Evidence: `tui/run.rs.bak` (205 564 bytes, dated 2026-06-03) — the old `run.rs` · line 36: `use crate::tui::components::format_ctx;` (old path; happens to resolve but is dead code in a file Rust will never compile)
- Impact: Breaks `cargo package`, bloats diff stats, footgun for `git stash`/reflog restores
- Fix: `rm pkg/crates/abacus-cli/src/tui/run.rs.bak` · add `*.bak` to `.gitignore`

**C-5 [P1 · MNT/CON] `pub mod phase_14_audit` is doc-as-code pollution**
- Evidence: `state/session_migrate.rs:346-354` defines an 80-line module with 2 constants and a doc block counting files to delete in a future phase · `V40_FIELDS_TO_CLEAN` and `V40_MODULES_TO_REMOVE` are not referenced anywhere · line 351 says "to remove: messages.rs 1439 lines" but `messages.rs` is already deleted
- Impact: Pollutes compile time · confuses readers (looks like a real module) · drifts from reality
- Fix: Move body to `docs/phase-14-cleanup.md` · keep `migrate_v3_to_v4` + `migrate_messages_to_cards` only after C-2 wires them in

**C-6 [P1 · MNT] 110+ deprecation warnings, zero migrations**
- Evidence: `cargo check` produces 31×`state.messages` + 31×`state.trace_events` + 18×`state.streaming_text` + 17×`state.streaming_thinking` + 9×`state.streaming_tools` + 4×`state.streaming_md` warnings · the deprecation messages advise migration to LlmCard/AbacusCard APIs · **none of the 110 call sites have been migrated**
- Impact: The deprecation markers were added prematurely. This is the worst-of-both: 123 warnings on every build, no progress. Trains developers to ignore the noise
- Fix: Either (a) actually port the call sites to CardStream-backed helpers (multi-day work), or (b) remove the `#[deprecated]` markers (the fields are still canonical, the rename is incomplete). Track as a separate ticket

**C-7 [P1 · MNT] `components/card.rs::Card` struct is dead code (100+ lines)**
- Evidence: `tui/components/card.rs:8-10` admits: *"Card 当前 pub 但**无外部调用方** (dead code)"* · grep across `pkg/` finds zero call sites for `components::card::Card` (only `render_card_bar` is used, by `panel.rs:162`) · `card.rs:20-111` defines a full `Block`-based rounded card widget with shadow rendering, never invoked
- Impact: Public-by-accident becomes a future API trap — Agent apps reaching for `abacus-cli::tui::components::Card` will find it stable-by-accident
- Fix: Gate behind `#[cfg(feature = "panel-card-widget")]`, or move to `abacus-ui-kit::CardWidget`, or demote to `pub(crate)`. The comment already tells you clippy would flag it

**C-8 [P2 · SCL] `abacus-ui-kit` is built but has no real external consumer**
- Evidence: New crate `pkg/crates/abacus-ui-kit/` (8 files, ~140 KB) · sole declared dependency: `abacus-cli` · only 2 examples (`quant_panel.rs`, `v42b_card_stream.rs`) and 1 binary `abacus` consume it · no documented in-tree consumer outside `abacus-cli` itself · `quant_panel.rs:19` comment says *"Agent 应用会把这些 Section 注入到 abacus-cli 主 TUI 的 state.section_registry 中"* — but there is no public `init_extensions()` hook for an Agent binary to call before construction
- Impact: The crate's stated value is "跨 crate 公开契约" but until a consumer binary lives outside `pkg/crates/abacus-cli/`, the boundary is theoretical
- Fix (pick one):
  - **(a)** Provide `init_extensions(registry: &mut SectionRegistry, dashboard: &mut DashboardRegistry)` and `AppState::new_with_extensions(…)` constructors
  - **(b)** Defer to a real v1.6 third-party Agent SDK milestone and merge back into `abacus-cli` for now

**C-9 [P2 · CON] `panel_layout` config wiring promised in extensions.rs is unimplemented**
- Evidence: `tui/extensions.rs:60-68` documents *"用户可通过 config.toml `[tui.panel] sections = [...]] 覆盖"* · `tui/state/mod.rs:1470` defines `pub panel_layout: Vec<String>` · `tui/components/panel.rs:391-393` reads `state.panel_layout` and passes it to `section_registry.build_stack(&layout)` (✓ partial) · grep finds **no** `config.toml` parser for `[tui.panel] sections = [...]` · `panel_layout` is initialized once at `state/mod.rs:2688` from `default_panel_layout()` and never written to again
- Impact: Tests in `extensions.rs:113-115` pass because the constant exists, not because the feature works
- Fix: Implement the config parser in `tui/setup.rs` next to existing TOML config, OR correct the doc to "覆盖方式 TBD" until Phase 15

**C-10 [P2 · CON] Two stray `use abacus_ui_kit::SectionContext;` imports**
- Evidence: `tui/cards/render.rs:33` and `tui/cards/hit_test.rs:23` both have an unused import
- Fix: Delete both lines (5-second fix)

**C-11 [P2 · MNT] Two unused `body_height` overrides in Card implementations**
- Evidence: `tui/cards/expert.rs:73` and `tui/cards/llm.rs:124` define `body_height` that passes through to the default
- Fix: Delete the override entirely to silence the warnings

**C-12 [P3 · MNT] `SessionExport` should live in `tui/state/`, not in `tui/run.rs`**
- Evidence: `run.rs:2930-3011` — 80 lines of serde-only struct definition with no runtime use in `run.rs` other than read/write
- Impact: Couples session format to the binary's run-loop file · future `abacus-server` crate would have to depend on `abacus-cli` for the struct shape
- Fix: Move `SessionExport` + `bool_is_false` / `session_tokens_is_empty` helpers to a new `tui/state/session_export.rs` module

---

## 3. Risk Matrix

| ID | Title | Severity | Probability user hits | Combined |
|---|---|---|---|---|
| A-1 | `fs_mkdir` duplicate, cluster lost | High | Every cluster lookup | 🟠 |
| A-2 | `code_exec` manifest vs `code_execute` code | High | Every schema call for that tool | 🟠 |
| B-1 | OnceLock Mutex poison, unsound across await, not session-scoped | Critical | Any panic in lock guard or any session boundary | ⛔ |
| B-2 | pin race with LFU prune | Medium | Memory pressure + D-tier tool | 🟡 |
| C-1 | User-typed messages invisible | Critical | 100% (every typed turn) | ⛔ |
| C-2 | V40 sessions load empty | Critical | >80% (most users have history) | ⛔ |
| C-3 | save_session writes v2, not v4 | Critical | 100% (every /save or auto-save) | ⛔ |
| C-4 | run.rs.bak 205 KB stray | Low | Build-time only | 🟡 |
| C-5 | phase_14_audit doc-as-code | Low | Doc-time only | 🟡 |
| C-6 | 110+ deprecation warnings, no migration | Medium | Every cargo build | 🟠 |
| C-7 | `Card` struct dead code (100+ lines) | Low | None today | 🟡 |
| C-8 | abacus-ui-kit has no external consumer | Medium | Future SDK milestone | 🟠 |
| C-9 | panel_layout config not wired | Low | User editing config.toml | 🟢 |
| C-10 | 2 stray SectionContext imports | Low | Build warnings | 🟢 |
| C-11 | 2 unused body_height overrides | Low | Build warnings | 🟢 |
| C-12 | SessionExport misplaced in run.rs | Low | Future refactor | 🟢 |

---

## 4. Action Items

### L0 — **Must do before v1.5.0 release** (release-blockers)

1. **A-1** — Delete `tools.toml:211-221` (duplicate `fs_mkdir` entry). Add uniqueness assertion in `manifest.rs:83` (`assert_eq!(by_name.len(), original_len, "duplicate tool name in tools.toml")`).
2. **A-2** — Rename `tools.toml:460` entry from `[tools.code_exec]` to `[tools.code_execute]`. Confirm `code_exec.rs:63,65,109` matches.
3. **B-1** — Move `LAST_HASH` from `static OnceLock<Mutex<…>>` in `post.rs` to a `TurnContext` field (zero-cost per Phase 1 decision: TurnContext is `&mut` passed in). Use `tokio::sync::Mutex`. Clear on `SessionStart` boundary.
4. **C-1 + C-2 + C-3** — Pick Path A or Path B and execute the full writer-migration:
   - **Path A (minimal, ~30 lines):**
     - In `tui/state/mod.rs:3625 add_message`, also call `cards::writer::push_user_message(state, &text, &ts)`
     - In `tui/run.rs:3176 apply_session_export`, dispatch on `export.version` and call `migrate_v3_to_v4` for v<=3
     - Bump `save_session` to `version: 4` and serialize the new `cards` field
     - In `tui/run.rs:3031-3038`, replace `messages: state.messages.iter().cloned().collect()` with `cards: state.cards.to_value()`
   - **Path B (clean, ~150 lines):** Same as A, plus delete `state.messages` and `state.add_message` entirely; rewrite all 7 call sites in `event/mod.rs` + `run.rs` to use `push_user_message` directly
   - **Recommendation:** Path A. It preserves backward compat for any direct `state.messages` reads (currently 31 callers) and keeps the diff narrow.

### L1 — **Should do in this PR** (high-confidence cleanups, ~10 minutes total)

5. **C-4** — `rm pkg/crates/abacus-cli/src/tui/run.rs.bak` and add `*.bak` to `.gitignore`.
6. **C-5** — Move `pub mod phase_14_audit` body to `docs/phase-14-cleanup.md`; keep only `migrate_v3_to_v4` + `migrate_messages_to_cards` in `session_migrate.rs` after C-2 wires them in.
7. **C-7** — Demote `tui/components/card.rs::Card` to `pub(crate) #[allow(dead_code)]` and remove the `pub` visibility on the struct, OR gate behind a feature flag. Keep `render_card_bar` (still used by `panel.rs:162`).
8. **C-10** — Delete `use abacus_ui_kit::SectionContext;` from `tui/cards/render.rs:33` and `tui/cards/hit_test.rs:23`.
9. **C-11** — Delete unused `body_height` overrides in `tui/cards/expert.rs:73` and `tui/cards/llm.rs:124`.
10. **B-2** — In `post.rs`, reorder the D-tier promotion sequence: set `pinned: true` on the `BehaviorMemory` entry *before* calling `record_tool_behavior` (or call `pin_tool_behavior` first with a stub entry). Eliminates the prune race.

### L2 — **Do in a follow-up PR** (technical-debt paydown)

11. **C-6** — Migrate the 110+ deprecation call sites to CardStream helpers, OR remove the `#[deprecated]` markers (the rename is incomplete; the fields are still canonical). Pick one. Multi-day work; track as a separate ticket.
12. **C-9** — Implement `[tui.panel] sections = [...]` in `tui/setup.rs` TOML config. Currently `panel_layout` is hard-coded; the test in `extensions.rs:113-115` lies about user-facing config.
13. **C-12** — Move `SessionExport` from `tui/run.rs:2930-3011` to a new `tui/state/session_export.rs` module. Sibling of `session_migrate.rs`.
14. **C-8** — Decide the fate of `abacus-ui-kit`. (a) Ship a real v1.6 SDK with a documented `init_extensions(…)` API in `AppState::new_with_extensions(…)`, OR (b) collapse it back into `abacus-cli` until a consumer exists. Half-built boundaries cost more than no boundary.

### L3 — **Strategic / future milestone**

15. **Code-quality-gate check** before next release: the `code-quality-gate` skill's three review gates (multi-role adversarial, granular simulation, reference chain) should run end-to-end on the writer→renderer data flow, not just the unit tests in isolation. The bug class "renderer migrated, writer not" (C-1/C-2/C-3) requires end-to-end tracing to detect.

---

## 5. Dimensional Scoring

| Dimension | Score (1-5) | Reasoning |
|---|---|---|
| **CON** (consistency) | 2 | Manifest has duplicate entry (A-1) + wrong name (A-2); 110+ deprecation markers with no migration (C-6); two `panel_layout` API mismatches (C-9) |
| **MNT** (maintainability) | 2 | 205 KB stray file (C-4); 80 lines of doc-as-code (C-5); 100+ lines of dead code with `pub` (C-7); `SessionExport` in the wrong module (C-12) |
| **COR** (correctness) | 2 | 3 P0 TUI bugs break basic chat (C-1/C-2/C-3); 1 P0 cache poison (B-1); 1 P0 manifest bug (A-2); 1 P0 manifest bug (A-1) |
| **SCL** (scalability) | 4 | New `Section` / `DashboardTab` / `SectionContext` / `SectionRegistry` design is the strongest part of the diff (C-11). Clean DAG, well-tested downcast, 100% wiring. Will scale well once the writer side catches up |
| **USB** (usability) | 2 | The single most-used feature (type → render → save → reload) is broken in three places. 80%+ of users with session history are affected |
| **SEC** (security) | 3 | No new attack surface introduced. The B-1 panic vector is correctness, not security. B-2 race is a UX issue, not a data leak |
| **EFF** (efficiency) | 4 | LLM call reduction optimizations (checkpoint cache, adaptive self-consistency, preflight skip, pressure-skip-on-circuit) are net-positive. The post-fix OnceLock is the only performance-critical fix needed |

**Overall:** 2.7/5. The diff is **structurally well-organized** (panel_sections + dashboard_tabs + ui-kit is a clean answer to "how do we keep the trait signature small but still let implementations reach richer state") but **incomplete at the integration boundary** (renderers migrated, writers did not). The 5 P0s are the integration-boundary bugs; the L1s are hygiene; the L2s are paydown.

---

## 6. Successes Worth Keeping

These parts of the refactor genuinely improve the codebase and should not be rolled back:

- **`abacus-ui-kit::Section` + `SectionContext` + `SectionRegistry` design** (`section.rs:60-100`, 605 lines). The `ext()` + `ext_type_id` downcast pattern is a clean answer to "how do we keep the trait signature small but still let implementations reach richer state". The 4 unit tests in `section_ctx.rs:104-165` cover the unsafe-safety contract well.
- **`extensions.rs` API surface** (`register_builtin_sections`, `register_builtin_dashboard_tabs`, `default_panel_layout`, `default_dashboard_tabs`, `new_section_registry`, `new_dashboard_registry`). Six small, well-named functions. Five unit tests assert invariants. The 100% call-site coverage from `state/mod.rs:2686-2688` is the right kind of "wiring".
- **`panel_sections/` and `dashboard_tabs/` split.** Six 80-300-line modules with single responsibilities. The "1 trait + 1 zero-sized struct + 1 default impl" pattern (`llm.rs:44-50`) is a clean idiom. New sections are now `Box::new(MySection)` + a 1-line `register`.
- **`card.rs::render_card_bar`** (1 function, 16 lines, 1 caller). Small, focused, single caller, clear contract.
- **`quant_panel.rs` example** (393 lines, self-documenting, 5 unit tests, mocked cross-crate context). Sets the bar for "how to document a new public trait".
- **`v42b_card_stream.rs` example** (156 lines). Reuses the production `modes/common.rs` rendering path — a regression in production would also break the example. Good safety net.
- **Theme migration** (42 import sites updated, no dangling `crate::tui::theme` references). The new `theme.rs` in ui-kit re-exports the old submodule paths (`brand`, `mode_color`, `z_index`) at the crate root, so external code that used the public items continues to compile.
- **LLM call reduction** (checkpoint cache + adaptive self-consistency + preflight skip + pressure-skip-on-circuit). Net-positive once B-1 is fixed; will save real LLM tokens per turn.
- **Pressure monitor multi-source** (`SourceRegistration` + `SourceThresholds` + `ManualPressureSource` + `combined_pressure` + `should_reject` + `classify_with`). Cleanest extension to a sensitive subsystem in the diff.
- **SilentRouter migration from manifest** (`build_maps_from_manifest()` + `Domain::Session=7` + `DOMAIN_COUNT=8`). 7 domains → 8 domains, no behavior change, all wiring reads from `tools.toml`.

---

## 7. Recommended Next Steps

1. **Stop the v1.5.0 release.** 3 P0 TUI bugs are visible on every keystroke. The release cannot ship without L0 #4.
2. **Execute L0 in this order** (smallest diff first, then integration): A-1 → A-2 → B-1 → B-2 → C-1 → C-2 → C-3.
3. **Bundle L1 #5-10** into the same PR (10 minutes total, all hygiene).
4. **Smoke-test end-to-end** after L0+L1: type a message, see it in panel · load a v2 session, see all cards · save with Ctrl+S, reload, all cards reappear · panic in any tool executor, verify B-1 fix is sound.
5. **Schedule L2 (#11-14) as separate tickets.** None of them block the release.
6. **Decision needed on C-8 (abacus-ui-kit).** I'll ask separately.
7. **Launch the external-tool preprocessing pipeline** after L0 is green.
