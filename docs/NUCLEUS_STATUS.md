# Nucleus Status

Code-grounded snapshot of the "Nucleus" passive intent-observation feature. Every
claim below was verified by reading the current source or running the current test
suite this session — see the Discrepancies section for anywhere a prior-notes
hypothesis didn't hold up.

## Architecture summary

- The app binary crate is `crates/nucleus` (renamed from `crates/zed` by a separate,
  unrelated rebrand commit). Its entry point is `crates/nucleus/src/main.rs`.
- The actual intent-classification logic lives in a **separate crate**,
  `crates/nucleus_intent` (`lib` path `src/nucleus.rs`) — not in the `nucleus` crate
  itself, despite the similar name. This split exists because `crates/nucleus` was
  already taken by the app-binary rename; see Discrepancies.
- `crates/nucleus` (the binary) does **not** depend on `nucleus_intent` directly.
  It depends only on `crates/engine_panel`, which depends on `nucleus_intent` and
  is the only consumer of it anywhere in the workspace (confirmed via
  `grep -rl nucleus_intent --include=Cargo.toml .`).
- `nucleus_intent` has no `pub fn init` and no `init` call in `main.rs`/`zed.rs`.
  Instead, `crates/nucleus/src/zed.rs`'s `initialize_panels` unconditionally calls
  `EnginePanel::load` for every workspace window at startup (alongside
  ProjectPanel, TerminalPanel, GitPanel, etc. — `zed.rs:775-809`), and
  `EnginePanel::new` (`engine_panel.rs:44-82`) is what constructs the
  `NucleusEngine` entity. So the classifier **is** a real always-on passive
  observer for every workspace, just wired in through the panel's construction
  path rather than its own top-level `init` — being added to the dock does not
  imply the panel is visually open, but the engine entity, its subscriptions, and
  its two background timers (`_decay_task`, `_nudge_task`) start regardless.
- `engine_panel::init(cx)` (called from `main.rs:744`) only registers the
  `ToggleFocus`/`Toggle` actions — it does not construct anything itself.

## Implemented intents

`DeveloperIntent` (`nucleus.rs:87-105`), 11 variants:

| Intent | Status | What drives its score |
|---|---|---|
| `ConsultingAgent` | Real — hard gate | `SessionState::agent_active` (agent panel has an active thread *and* keyboard focus). Evaluated first in `classify`; short-circuits everything else, sets its own probability to 1.0 and every other intent's to 0.0. Not part of the weighted blend. |
| `Debugging` | Real — weighted | Failed test/task runs, open error diagnostics, saves-with-no-file-switch ("iterating on a fix"), small edits, a 30-300s pause (decaying), cursor-at-diagnostic-location, terminal focus. See Classifier structure below. |
| `Implementing` | Real — weighted | Clean saves with no open errors, ≥2 file switches, large edits (+ sustained-burst bonus), passing test/task runs, pause < 10s, editor focus. |
| `Idle` | Real — but no behavioral rules of its own | Not scored from any signal directly; computed as leftover mass (`1 - debugging - implementing`, floored at 0) **or** a pause-driven floor (`idle_activity_floor`, ramping 0→1.0 as the pause approaches 300s), whichever is higher. Exists specifically so normalization doesn't degenerate into a hard 1.0/0.0 split when only one of Debugging/Implementing has any signal. |
| `Refactoring` | Stub | Always 0.0 (`classify`'s match arm: `_ => 0.0`). No signal references this variant anywhere in `nucleus.rs` outside the enum/label definitions. |
| `Exploring` | Stub | Same — always 0.0, no scoring code. |
| `Reviewing` | Stub | Same — always 0.0, no scoring code. |
| `Testing` | Stub | Same — always 0.0. A doc comment (`nucleus.rs:877`) explicitly notes terminal focus doesn't distinguish Debugging from Testing "since Testing isn't implemented yet." |
| `Documenting` | Stub | Same — always 0.0, no scoring code. |
| `Configuring` | Stub | Same — always 0.0, no scoring code. |
| `Planning` | Stub | Same — always 0.0, no scoring code. |

`feedback_toast.rs:24-29`'s `CORRECTABLE_INTENTS` list independently confirms this:
it only offers `Debugging`, `Implementing`, `Idle`, `ConsultingAgent` as feedback
choices, with a comment explaining the other 7 are excluded because they're
"always scored 0, never actually predicted."

## Classifier structure

`classify(state: &SessionState, last_edit_magnitude: Option<usize>) -> IntentPrediction`
(`nucleus.rs:981-1172`):

**Gate (runs first, short-circuits):**
1. `state.agent_active` → returns `ConsultingAgent` at confidence 1.0 immediately,
   skipping all weighted scoring below entirely.

**Weighted signals** (each intent accumulates independently, then clamped to `[0,1]`):

Debugging:
| Signal | Weight |
|---|---|
| `failed_test_runs > 0` | `0.35 + 0.03 × (min(failed_test_runs,5) - 1)` |
| `diagnostics.errors > 0` | `0.15 + 0.02 × (min(errors,5) - 1)` |
| 0 file switches AND ≥2 saves | `0.20` |
| last edit magnitude < 10 chars | `0.10` |
| pause in `[30s, 300s]` | `0.15 × (1 - position)`, linearly decaying across the range (`pause_investigating_weight`) |
| `cursor_at_diagnostic` | `0.30` |
| `focused_pane == Terminal` | `0.08` |

Implementing:
| Signal | Weight |
|---|---|
| 0 diagnostics errors AND ≥1 save | `0.25` |
| ≥2 file switches | `0.06 × min(file_switches, 5)` |
| last edit magnitude ≥ 40 chars | `0.25`, plus `0.15 × (min(large_edits,4) - 1)` if ≥2 recurring large edits within a 30s burst window |
| passing test/task runs (>0, 0 failed) | `0.15` |
| pause < 10s | `0.10` |
| `focused_pane == Editor` | `0.08` |

**Normalization**: `idle_score = max(1 - debugging_score - implementing_score, 0, idle_activity_floor(pause_seconds))`;
`total = debugging_score + implementing_score + idle_score`; each intent's final
probability is `score / total`. The winner is whichever of the three has the
highest probability (ties favor Debugging, then Implementing, over Idle — see the
`>=` comparisons at `nucleus.rs:1138-1146`).

**Part C signals** (`focused_pane`, `cursor_at_diagnostic`) are folded into the
weighted scoring above as small priors/correlations, not additional gates —
explicitly documented as a deliberate distinction from the `agent_active` gate
(`nucleus.rs:965-980`).

## SessionState fields: fed into scoring vs. tracked-only

| Field | Fed into `classify`? | Notes |
|---|---|---|
| `recent_actions.{test_runs,failed_test_runs,saves,file_switches,large_edits}` | Yes | Direct signal inputs (see tables above). |
| `diagnostics.{errors,warnings}` | Partially | `errors` feeds both Debugging and Implementing signals; `warnings` is tracked (`DiagnosticsSummary`) and displayed in the panel (`engine_panel.rs:317`) but never read inside `classify`. |
| `pause_seconds` | Yes | Both the Debugging pause-investigating weight and the Idle floor. |
| `agent_active` | Yes | The hard gate. |
| `focused_pane` | Yes | Small Debugging/Implementing priors. |
| `cursor_at_diagnostic` | Yes | Debugging signal. |
| `current_symbol` | **No** | Computed every `refresh()` (`current_symbol`, from editor breadcrumbs) and displayed in the panel's Session State row, but `classify` never reads it — confirmed by its absence from `classify`'s body. |
| `diff_summary` | **No** | Built as a display string on every buffer edit (`handle_buffer_edited`) and shown in the panel, but not passed to or used by `classify` — only the numeric `last_edit_magnitude` (a separate field on `NucleusEngine`, not part of `SessionState`) feeds scoring. |
| `active_files` | **No** | Tracked (rolling list of up to 5 recently-touched files) and displayed, not read by `classify`. |

## Task/test-run detection

`poll_task_terminals` (`nucleus.rs:598-662`) polls `TerminalPanel::panes()` every
`PRUNE_INTERVAL` (10s) for task terminals (`terminal.task()`), tracking each
terminal's `TaskStatus` transition to `Completed`/`Failed` by `EntityId` so reruns
in a reused terminal are each counted once. Polling is used deliberately instead of
reacting to `workspace::Event::ItemAdded`, because task terminals never fire that
event (documented at `nucleus.rs:589-597`, confirmed against `terminal_panel.rs`'s
internal pane ownership).

**Known conflation** (unchanged from prior notes, confirmed still true in code):
- Every completed/failed task terminal — regardless of task label — increments the
  same `RecentActions::test_runs`/`failed_test_runs` counters. There is no
  label-based filtering distinguishing "Run tests" from "Build" or "Lint" tasks;
  `poll_task_terminals` records an outcome for *any* task terminal completion.
- Commands typed directly into a plain (non-task) shell terminal are invisible —
  stated explicitly in the panel's own UI copy: *"Task runs are detected via
  Zed's task runner only (task::Spawn) — commands typed directly into a plain
  shell terminal aren't seen."* (`engine_panel.rs:325-326`).

## Logging schema

Written to `~/.nucleus/logs/YYYY-MM-DD.jsonl` (`logging.rs:225-227`, via
`paths::home_dir()` — deliberately outside Zed's own data/log dirs). Async,
best-effort, buffered (flushes every 5s or once 50 lines queue up); never writes
source code text, only paths/symbol names/counts.

Three line types exist (`logging.rs`'s internal `LogLine` enum, `#[serde(tag =
"type")]`):

**`intent_prediction`**
```
{ "type": "intent_prediction", "timestamp": <rfc3339>,
  "prediction": { "prediction_id": <uuid>, "probabilities": [[intent, f32], ...],
                   "intent": <DeveloperIntent>, "confidence": f32, "evidence": [String] },
  "session_state": { ...full SessionState, see table above... } }
```
Only logged on a "meaningful change" — selected intent changed, or confidence
moved by more than `0.1` (`CONFIDENCE_LOG_THRESHOLD`) — not on every classifier
tick (`is_meaningful_prediction_change`, `nucleus.rs:816-825`).

**`raw_event`** — one of `Edit`/`Save`/`FileSwitch`/`SelectionChanged`/
`TaskStarted`/`TaskCompleted`/`TaskFailed` (internally tagged `event`, flattened
into the outer object):
```
{ "type": "raw_event", "timestamp": <rfc3339>, "event": "edit",
  "file": <path|null>, "symbol": <string|null>, "inserted_chars": usize, "deleted_chars": usize }
```
(shape varies per variant; see `RawEvent` in `logging.rs:40-65`).

**`feedback`**
```
{ "type": "feedback", "timestamp": <rfc3339>,
  "prediction_id": <uuid>, "correct": bool, "actual_intent": <DeveloperIntent|null> }
```
Written by `FeedbackNudgeToast` in response to the periodic nudge (every 12
minutes, skipped while Idle or ConsultingAgent, never re-asks the same
`prediction_id` twice).

`prediction_id` (`PredictionId`, a `uuid::Uuid` newtype) exists and is the
correlation key linking a `feedback` line back to the `intent_prediction` it's
responding to — confirmed present in both `IntentPrediction` and `Feedback`
structs, and exercised end-to-end by `test_feedback_log_round_trips`.

## Test coverage

`cargo test -p nucleus_intent --lib`: **15 passed; 0 failed; 0 ignored** (run this
session; first attempt hit a transient build error in an unrelated crate — see
Discrepancies — a clean retry passed all 15 in 0.02s test time).

15 `#[test]`/`#[gpui::test]` functions, all in `nucleus_intent/src/nucleus.rs`'s
`mod tests` (no other test files in the crate):

| Test | Guards against |
|---|---|
| `test_pause_alone_lands_on_idle_at_low_debugging_confidence` | A lone ~35s pause with no other signal must not resolve to confident Debugging (regression for a real logged false positive). |
| `test_multiple_failed_test_runs_is_confident_debugging` | 3 failed test runs + pause + open error should be meaningfully confident Debugging with evidence naming the failed runs. |
| `test_continuous_editing_resolves_to_implementing` | Large edit + clean save + no pause resolves to Implementing. |
| `test_long_pause_resolves_to_idle` | A 305s pause (just past the 300s investigating window) resolves to Idle. |
| `test_single_failed_test_run_alone_is_not_confident_debugging` | One failed run alone shouldn't push Debugging confidence high (contrast with the multi-run case). |
| `test_zero_signal_fresh_session_resolves_sanely` | A brand-new session with zero activity resolves confidently to Idle, not a spurious near-tie. |
| `test_stale_signal_with_long_pause_resolves_to_idle` | Regression: a stale failed-test signal still inside the rolling window, combined with a 250s pause, must decay to Idle (old flat pause weight bug). |
| `test_sustained_edit_burst_resolves_to_implementing` | Regression: a real sustained burst of large edits (from actual logged data) must resolve to Implementing, not Idle (old single-magnitude-only bug). |
| `test_probabilities_can_genuinely_blend` | A co-occurring Debugging signal + Implementing signal produces genuine 3-way probability spread, not a degenerate 1.0/0.0 split. |
| `test_logger_writes_real_jsonl_lines` | Real JSONL lines actually land on disk at `~/.nucleus/logs/`, every line has `type` + `timestamp`, including the size-triggered immediate-flush path. |
| `test_reads_back_real_logged_data` | The read path (`list_log_dates`/`read_log_file`/`parse_log_line`) round-trips real on-disk data, including the flattened `raw_event` shape. |
| `test_agent_active_gate_overrides_debugging_signals` | The `ConsultingAgent` gate actually short-circuits strongly-Debugging-shaped signals rather than blending with them. |
| `test_pane_focus_alone_is_insufficient_to_drive_classification` | Terminal/editor focus alone, with no other signal, doesn't drive a confident classification (tie-breaker, not a driver). |
| `test_diagnostic_location_correlation_shifts_score_meaningfully` | Cursor-at-diagnostic-location scores meaningfully higher Debugging than the same error present but cursor elsewhere. |
| `test_feedback_log_round_trips` | A written `feedback` line round-trips back with the same `prediction_id` and fields via the real read path. |

Three tests (`test_logger_writes_real_jsonl_lines`, `test_reads_back_real_logged_data`,
`test_feedback_log_round_trips`) deliberately write to and read the real
`~/.nucleus/logs/` directory rather than a mock, serialized behind a shared
`std::sync::Mutex` to avoid interleaved writes corrupting the file under
`cargo test`'s default parallelism.

## Debug/log-viewer UI (`engine_panel`)

Confirmed by reading `engine_panel.rs` and `log_view.rs` directly:

- A left/right-dock panel (`Panel` impl, default left, `Sparkle` icon), loaded
  automatically for every workspace at startup (see Architecture summary).
- **Overview tab**: current prediction (label + confidence % + progress bar +
  evidence bullet list), and a "Session state" block showing active files,
  current symbol, pause seconds, test/task run counts (total + failed), saves,
  file switches, error/warning counts, and the last diff summary string — plus a
  static caption explaining task detection is `task::Spawn`-only.
- **Logs tab**: Live/History mode toggle, All/Predictions/Raw-events filter,
  click-to-expand rows showing pretty-printed JSON. Live mode subscribes directly
  to `NucleusEvent` (never reads its own log file back). History mode lists
  available dates (from `list_log_dates`) and loads one day at a time via
  `read_log_file`, capped at `MAX_HISTORY_LINES` (500), on a background task.
- Live tail is capped at 200 in-memory entries (`log_view.rs`'s `MAX_LIVE_LINES`),
  independent of the on-disk log's own (absent) retention limit.
- `FeedbackNudgeRequested` events are handled separately from the log-row path —
  they trigger `FeedbackNudgeToast` via `workspace.toggle_status_toast`, not a log
  row.
- Renders no source code or diff content anywhere — `log_entry_to_pretty_json`'s
  own doc comment states this explicitly, and none of the logged types carry it.

## Known limitations

- 7 of 11 `DeveloperIntent` variants are unscored stubs — see the Implemented
  intents table. (`feedback_toast.rs:21-23`'s comment on `CORRECTABLE_INTENTS`
  makes this explicit from the UI side too.)
- Task/test-run detection cannot distinguish task types — any completed/failed
  task terminal counts the same, regardless of label (`nucleus.rs:598-662`, no
  label filtering in `poll_task_terminals`).
- Commands run in a plain (non-task) shell terminal are invisible to the engine —
  stated directly in the panel's own UI copy (`engine_panel.rs:325-326`).
- `cursor_at_diagnostic` is a point-in-time check with no dwell tracking — a
  single-tick correlation, not "has the cursor lingered near this error"
  (`nucleus.rs:728-732`'s doc comment explicitly frames this as an accepted
  first pass, not unimplemented dwell logic that's merely deferred).
- Window-scoped signals (`agent_active`, `focused_pane`, `cursor_at_diagnostic`)
  only refresh on the `PRUNE_INTERVAL` tick (10s), so they can be up to 10s stale
  even though every other signal updates instantly on its triggering event
  (`nucleus.rs:664-669`'s doc comment).
- No retention policy for `~/.nucleus/logs/*.jsonl` — files accumulate
  indefinitely; explicitly out of scope per `logging.rs:1-7`'s module doc
  comment ("No retention policy or SQLite migration here").
- No OSC 133 support and no dedicated "terminal watcher" component exist
  anywhere in the codebase — `grep` for both across all of `crates/` returned
  zero matches. Task/terminal observation is entirely the polling approach
  described above; there is nothing to report a "status" on beyond that absence.
- All weights (`WEIGHT_*` constants, `nucleus.rs:848-887`) and window sizes
  (`RECENT_WINDOW`, `EDIT_BURST_WINDOW`, `PAUSE_INVESTIGATING_MIN/MAX_SECS`,
  `FEEDBACK_NUDGE_INTERVAL`, `DIAGNOSTIC_LOCATION_LINE_WINDOW`) are explicitly
  documented as first-pass guesses, "not tuned against real usage yet."

## Discrepancies found

- **Crate identity**: the session prompt and prior notes both refer to auditing
  "the `nucleus` crate" for `DeveloperIntent`/`classify`/`SessionState`. That code
  does not live in `crates/nucleus` — it lives in `crates/nucleus_intent`.
  `crates/nucleus` is the renamed app-binary crate (formerly `crates/zed`, per an
  unrelated concurrent rebrand), and does not contain any classifier logic itself.
- **`crates/zed/src/main.rs` no longer exists.** The rebrand mentioned above moved
  it to `crates/nucleus/src/main.rs`. The session prompt's step 1 instruction to
  walk `crates/zed/src/main.rs` was followed against the current path instead.
- **No `nucleus_intent::init(cx)` exists**, and `nucleus_intent` is not a direct
  dependency of the `nucleus` binary crate at all — it's only reachable through
  `engine_panel`. Initial inspection of `main.rs`/`zed.rs`'s top-level `init` call
  list could easily (and wrongly) suggest the classifier isn't wired in; it is,
  but through `initialize_panels`'s `EnginePanel::load` call, not a conventional
  `init`. Worth calling out since this is exactly the kind of place a
  restated-from-memory summary would get it wrong.
- **Test run flakiness, unrelated to `nucleus_intent`**: the first `cargo test -p
  nucleus_intent --lib` attempt failed with 57 compile errors in the unrelated
  `agent` crate (`crates/agent/src/thread.rs`, `web_search_tool.rs` — missing
  enum variants/types that clearly exist in source). A clean retry compiled and
  passed with no code changes in between, matching the same incremental-cache
  hard-linking flakiness this session already saw once with `encoding_selector`
  on this external volume (`target/debug/incremental` hard-link failures logged
  as warnings on every build). Not a real defect in `agent` or `nucleus_intent`.
- No other discrepancies were found against the specific claims this session
  could check (intent list and stub/real status, gate-then-weighted classifier
  shape, `SessionState` fields read vs. tracked-only, JSONL schema and line
  types, `prediction_id` presence, test count/purpose, panel contents, and the
  OSC 133/terminal-watcher absence) — prior notes on those points held up against
  the code as read this session.
