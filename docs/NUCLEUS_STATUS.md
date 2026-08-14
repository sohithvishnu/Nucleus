# Nucleus Status

Code-grounded snapshot of the "Nucleus" passive intent-observation feature, refreshed
this session. Every claim below was verified by reading the current source or running
the current test suite this session — see the **Discrepancies** section for anywhere
the prior version of this doc (or a prior session's own report) didn't hold up, most
notably the log-viewer clipboard item.

## Architecture summary

- The app binary crate is `crates/nucleus` (renamed from `crates/zed` by a separate
  rebrand commit). Its entry point is `crates/nucleus/src/main.rs`.
- The actual intent-classification logic lives in a **separate crate**,
  `crates/nucleus_intent` (`lib` path `src/nucleus.rs`) — not in the `nucleus` crate
  itself, despite the similar name.
- `crates/nucleus` (the binary) does **not** depend on `nucleus_intent` directly.
  It depends only on `crates/engine_panel`, which depends on `nucleus_intent` and
  is the only consumer of it anywhere in the workspace (re-confirmed this session via
  `grep -rl nucleus_intent --include=Cargo.toml .`).
- `nucleus_intent` has no `pub fn init` and no `init` call in `main.rs`/`zed.rs`.
  Instead, `crates/nucleus/src/zed.rs`'s `initialize_panels` unconditionally calls
  `EnginePanel::load` for every workspace window at startup, and `EnginePanel::new`
  constructs the `NucleusEngine` entity — confirmed still true (`zed.rs:778`,
  `zed.rs:803`). The classifier **is** a real always-on passive observer for every
  workspace, wired in through the panel's construction path, not a top-level `init`.
- `engine_panel::init(cx)` only registers the `ToggleFocus`/`Toggle` actions — it does
  not construct anything itself.

## Implemented intents

`DeveloperIntent` (`nucleus.rs:164-183`), still 11 variants — **but the real/stub
split has changed substantially since the original audit.** A classifier-expansion
session landed Parts A-D on top of the original Debugging/Implementing/Idle split:

| Intent | Status | What drives its score |
|---|---|---|
| `ConsultingAgent` | Real — hard gate | `SessionState::agent_active` (agent panel has an active thread *and* keyboard focus). Evaluated first in `classify`; short-circuits everything else. Unchanged since original audit. |
| `Debugging` | Real — weighted | Unchanged signal set: failed test/task runs, open error diagnostics, saves-with-no-file-switch, small edits, 30-300s decaying pause, cursor-at-diagnostic-location, terminal focus. |
| `Implementing` | Real — weighted | Original signals (clean saves, ≥2 file switches, large edits + burst bonus, passing runs, pause < 10s, editor focus) **plus a new edit-density signal** (see below). |
| `Testing` | **Real — newly scored** (was a stub) | `state.test_command_running` / `test_command_recently_passed`, driven entirely by `terminal_watcher`'s plain-terminal command categorization (Phase 4b-1) — a `test`-category command currently in flight, or one that finished with exit code 0 within `ACTIVITY_BURST_WINDOW` (30s). Structurally independent of `RecentActions::failed_test_runs` (which stays task-runner-only), so a failed task-runner test still contributes to Debugging exactly as before, and Testing can never dilute or be diluted by it. |
| `Exploring` | **Real — newly scored** (was a stub) | Decayed `navigation_events` (selection-changed events) at or above threshold, gated on decayed `edit_events` being low/absent in the same window, plus a small "no long pause" bonus. |
| `Configuring` | **Real — newly scored** (was a stub) | File-identity driven: decayed `edit_events > 0.0` *and* the current file (`active_files.first()`) matches `is_recognized_config_file` (extension list, exact-name list, or `.env`-family dotfiles). |
| `Idle` | Real — no behavioral rules of its own | Unchanged shape: leftover mass across all five weighted categories now (`1 - debugging - implementing - testing - exploring - configuring`, floored at 0), **or** the pause-driven `idle_activity_floor` ramp, whichever is higher. |
| `Refactoring` | Stub | Always 0.0. |
| `Reviewing` | Stub | Always 0.0. |
| `Documenting` | Stub | Always 0.0. |
| `Planning` | Stub | Always 0.0. |

`feedback_toast.rs`'s `CORRECTABLE_INTENTS` list was updated in the same session and
now independently confirms this split: **7** entries (`Debugging`, `Implementing`,
`Testing`, `Exploring`, `Configuring`, `Idle`, `ConsultingAgent`) — up from 4 in the
original audit, with a comment explaining the remaining 4 stubs are excluded because
they're "always scored 0, never actually predicted."

## Classifier structure

`classify(state: &SessionState, last_edit_magnitude: Option<usize>) -> IntentPrediction`
(`nucleus.rs:1526-1862`, grown substantially from the original `981-1172`):

**Gate (runs first, short-circuits):** unchanged — `state.agent_active` → `ConsultingAgent` at confidence 1.0, skipping all weighted scoring.

**Weighted signals**, now six categories instead of three:

Debugging (original signals + terminal-engagement session's rule 1):
| Signal | Weight |
|---|---|
| `failed_test_runs > 0` | `0.35 + 0.03 × (min(failed_test_runs,5) - 1)` |
| `diagnostics.errors > 0` | `0.15 + 0.02 × (min(errors,5) - 1)` |
| 0 file switches AND ≥2 saves | `0.20` |
| last edit magnitude < 10 chars | `0.10` |
| pause in `[30s, 300s]` | `0.15 × (1 - position)`, linearly decaying |
| `cursor_at_diagnostic` | `0.30` |
| **focused terminal's last command failed (any category)** | `0.20 + 0.06 × (min(terminal_activity_events, 8.0) - 2.0)`, gated on `terminal_activity_events ≥ 2.0` |

`focused_pane == Terminal`'s old flat `0.08` is **gone** — see "Terminal-engagement
session" below.

Implementing (original signals only — no new terminal-engagement signal was added
here; see below for why):
| Signal | Weight |
|---|---|
| 0 diagnostics errors AND ≥1 save | `0.25` |
| ≥2 file switches | `0.06 × min(file_switches, 5)` |
| last edit magnitude ≥ 40 chars | `0.25`, plus `0.15 × (min(large_edits,4) - 1)` if ≥2 recurring large edits within a 30s burst window |
| decayed `edit_events` ≥ 2.0 | `0.30 + 0.06 × (min(edit_events, 8.0) - 2.0)` |
| passing test/task runs (>0, 0 failed) | `0.15` |
| pause < 10s | `0.10` |

`focused_pane == Editor`'s old flat `0.08` is also gone, same reason.

Testing (original signals + terminal-engagement session's rule 2):
| Signal | Weight |
|---|---|
| `test_command_running` | `0.55` |
| `test_command_recently_passed` | `0.45` |
| **focused terminal's last command was `Test`-category (pass or fail)** | `0.20 + 0.06 × (min(terminal_activity_events, 8.0) - 2.0)`, same gate as Debugging's rule above |

Exploring (original signal + terminal-engagement session's rule 3):
| Signal | Weight |
|---|---|
| decayed `navigation_events` ≥ 2.0 AND decayed `edit_events` ≤ 1.5 | `0.45 + 0.05 × (min(navigation_events, 8.0) - 2.0)` |
| pause < 10s | `0.10` |
| **focused terminal's last command succeeded and wasn't a test** | `0.25 + 0.06 × (min(terminal_activity_events, 8.0) - 2.0)`, same gate as above |

Configuring (unchanged):
| Signal | Weight |
|---|---|
| decayed `edit_events > 0.0` AND active file is a recognized config file | `0.55` (flat, not scaled) |

**Activity-density decay (new, replaces an interim hard-rolling-window design):**
`edit_events`/`navigation_events` are **not** raw counts — each contributing event's
weight decays exponentially, `exp(-age_seconds / 15.0)` (`DENSITY_DECAY_TIME_
CONSTANT_SECS`), summed via a pure helper `decayed_density`. Chosen (per the code's
own doc comment) specifically to avoid the cliff a hard rolling-window boundary
creates (an edit at t-29s counting fully, t-31s counting zero). `classify` itself
never computes wall-clock decay — `NucleusEngine::refresh` computes the already-
decayed number once per tick and hands it in, keeping `classify` a pure, deterministic
function of its `SessionState` argument.

**Normalization**: extended from the original 3-way sum to all six:
`idle_score = max(1 - debugging - implementing - testing - exploring - configuring, 0,
idle_activity_floor(pause_seconds))`; `total` sums all six; each intent's probability
is `score / total`. Winner selection iterates a fixed priority order — **Testing >
Debugging > Configuring > Implementing > Exploring > Idle** — replacing the current
best only on a strict win, which reproduces the original three-way tie-break exactly
("ties favor Debugging, then Implementing, over Idle") while giving the three new
categories a documented slot (see the priority-order comment at `nucleus.rs:1786+`
for the reasoning behind that specific order).

**`cursor_at_diagnostic`** remains a small correlation signal folded into Debugging's
weighted scoring, not a gate — unchanged. **`focused_pane`** is no longer a weighted
signal at all as of the terminal-engagement session — see below.

## Terminal-engagement session

Motivated by a real logged prediction: `Idle 92% — terminal currently focused (not
enough alone to indicate active work)`. Correctly downweighted alone, but the real
problem was `focused_pane` being the *only* thing the classifier knew about terminal
activity — no equivalent of the editor's edit-density/navigation-density existed for
terminal engagement (selecting/scrolling output), so actively reading a stack trace
in the terminal could register as nothing.

**Part A — new signal**: `RecentActions::terminal_activity_events`, a third decayed
density (same `decayed_density`/`DENSITY_DECAY_TIME_CONSTANT_SECS` machinery as
`edit_events`/`navigation_events`), fed by `terminal::Event::SelectionsChanged` on
every plain terminal (`NucleusEngine` now holds a `terminal_subscriptions:
HashMap<EntityId, Subscription>`, subscribed once per terminal in
`poll_plain_terminals`, pruned alongside `terminal_watcher`'s own per-terminal state).
Deliberately covers selection only, not pure scrolling — no low-noise dedicated event
exists for scroll-without-select in this terminal stack, and reusing `Event::Wakeup`
would be far too noisy (fires for reasons unrelated to engagement, like output
arriving or cursor blink).

New `SessionState` field `focused_terminal_last_command: Option<LastCommandOutcome>`
(`LastCommandOutcome { category: CommandCategory, exit_code: i32 }`, new in
`terminal_watcher.rs`) — the *focused* terminal's most recently completed command,
tracked via a new `TerminalCommandWatcher::last_completed` map, populated in
`scan_lines` and exposed via `last_completed_command(terminal_id)`.
`NucleusEngine::compute_focused_pane` now also returns the focused terminal's
`EntityId` (when applicable) so `prune_and_refresh` can look this up.

`terminal_engagement(state)` gates `focused_terminal_last_command` behind
`terminal_activity_events ≥ MIN_TERMINAL_ACTIVITY_FOR_SIGNAL` (2.0, same threshold
shape as the other two density signals) and routes the `Some` case three ways —
see the updated Debugging/Testing/Exploring tables above. Rules 1 ("failed, any
category" → Debugging) and 2 ("Test-category, regardless of pass/fail" → Testing)
are deliberately **not** mutually exclusive: a failed test satisfies both at once and
scores both categories, the same way task-runner and plain-terminal test signals
were already allowed to coexist without one diluting the other. Rule 4 (no completed
command yet) contributes nothing — not even to Idle's floor, since the selection
activity that would trigger this already resets `pause_seconds` independently, same
as every other activity signal in this file.

**Part B — pane focus demoted to a tiebreaker**: `WEIGHT_TERMINAL_FOCUS`/
`WEIGHT_EDITOR_FOCUS` (the old flat `+0.08` contributions) are gone entirely.
`apply_focus_tiebreaker` runs once, after the normal priority-order winner selection,
and only ever swaps the winner for the focused pane's associated intent
(`Debugging` for `Terminal`, `Implementing` for `Editor`) when that intent is the
*true* runner-up and within `FOCUS_TIE_MARGIN` (0.05, five percentage points) of the
winner. A swap updates both entries in `probabilities` (not just the headline
`intent`/`confidence`), which is required for `classify`'s own contract (selected
intent's confidence must equal its own `probabilities` entry, and be the argmax) to
keep holding after the swap — confirmed by the `classify_never_violates_its_own_contract`
proptest, which caught two real bugs during this session before landing (a
tie-break-order mismatch between a naive re-sort and `classify`'s own documented
priority order, and an under-normalized confidence value after a swap).

## SessionState fields: fed into scoring vs. tracked-only

| Field | Fed into `classify`? | Notes |
|---|---|---|
| `recent_actions.{test_runs,failed_test_runs,saves,file_switches,large_edits}` | Yes | Unchanged direct signal inputs. |
| `recent_actions.{edit_events,navigation_events,terminal_activity_events}` | **Yes** | `f32` decayed densities. `terminal_activity_events` is new this session, feeding Debugging/Testing/Exploring via `terminal_engagement`. |
| `focused_terminal_last_command` | **Yes — new** | The focused terminal's most recently completed command (category + exit code); `None` gates the terminal-engagement signal off entirely. |
| `diagnostics.{errors,warnings}` | Partially | Unchanged: `errors` feeds Debugging/Implementing; `warnings` still tracked/displayed only, never read inside `classify`. |
| `pause_seconds` | Yes | Unchanged: Debugging's pause weight, Idle's floor, and now also Implementing's/Exploring's "continuous activity" bonuses. |
| `agent_active` | Yes | The hard gate, unchanged. |
| `focused_pane` | **Changed** | No longer a weighted score input — a context selector (gates `focused_terminal_last_command`'s computation) and, via `apply_focus_tiebreaker`, a tiebreaker only. |
| `cursor_at_diagnostic` | Yes | Unchanged Debugging signal. |
| `test_command_running` / `test_command_recently_passed` | **Yes — new** | Testing's only inputs. |
| `active_files` | **Yes — changed.** The original audit said "No" (tracked/displayed only). That's now wrong: `active_files.first()` is read by Configuring's file-identity check. |
| `current_symbol` | No | Unchanged: computed and displayed, never read inside `classify`. |
| `diff_summary` | No | Unchanged: built and displayed, not passed to `classify` (only the numeric `last_edit_magnitude` feeds scoring). |

## Task/test-run detection

`poll_task_terminals` (`nucleus.rs:747-` — re-read in full this session, logic
byte-for-byte unchanged from the original audit) still polls `TerminalPanel::panes()`
every `PRUNE_INTERVAL` (10s) for task terminals, tracking `TaskStatus` transitions by
`EntityId`.

**Known conflation (unchanged, still true):**
- Every completed/failed task terminal, regardless of label, still increments the
  same `RecentActions::test_runs`/`failed_test_runs` counters — no label-based
  filtering was added by the classifier-expansion session, despite that session
  adding a *separate*, differently-scoped `Testing` intent. Confirmed this session:
  `poll_task_terminals`'s body has no `categorize_command`/label-matching call
  anywhere in it.
- Commands typed directly into a plain (non-task) shell terminal are **now
  partially visible** — this is the one real change here. Phase 4b-1's
  terminal-watcher (see below) detects and categorizes plain-terminal commands, and
  Part B of the classifier-expansion session wired *test*-category plain-terminal
  commands specifically into the new `Testing` intent. The panel's own UI caption
  (`engine_panel.rs`) still says task detection is `task::Spawn`-only — that caption
  is now about the **existing Debugging/Implementing signals** specifically (still
  true, those remain task-runner-only), not about the app's terminal observation as a
  whole anymore. Worth a caption update in a future UI session; not done here (audit
  only).

## Terminal watcher (Phase 4b-1) — confirmed in detail this session

Fully exists (it didn't at the time of the original audit — see Discrepancies) and
now includes a shell-injection bugfix on top of the original Phase 4b-1 landing:

- **Injection mechanism**: `hook_install_command` (`terminal_watcher.rs:217-230`)
  builds a **single physical line** — `. '<path>' 2>/dev/null` (POSIX) or
  `source '<path>' 2>/dev/null; rm -f '<path>'; clear` (fish) — sourcing a hook
  script written once to a temp file, *not* a raw multi-line PTY write. Confirmed by
  reading the function directly.
- **Readiness gate**: confirmed present — `TerminalCommandWatcher::mark_observed`
  (`terminal_watcher.rs:432-434`) returns `true` only the first time a terminal is
  seen needing injection; callers wait for a second poll tick before actually
  injecting, giving the shell's own startup (rc files, `conda init`, etc.) a full
  `PRUNE_INTERVAL` to finish first.
- **Redaction patterns**, read directly from `REDACTION_PATTERNS`
  (`terminal_watcher.rs:348-371`) — exactly 5, unchanged from what was originally
  requested: `--token=...`, `--password=...`, `Authorization: Bearer <token>`,
  AWS-style access key IDs (`AKIA[0-9A-Z]{16}`), and a generic
  `key=`/`secret=`/`pass=` assignment pattern (8+ char opaque value).
- **Command categorization**: `CommandCategory` (`terminal_watcher.rs:268-275`) —
  `Test`, `Build`, `Git`, `Package`, `Lint`, `Other`, via `categorize_command`. This
  is the exact data source Part B's `Testing` intent reads (`has_pending_command_of_
  category`, added this classifier-expansion session).
- Also present, not in the original audit's scope but confirmed while reading this
  file: a fix for a bash-specific `DEBUG`-trap edge case (the trap, once armed,
  applies to *every subsequent simple command in the same script*, so cleanup/
  `PROMPT_COMMAND` setup had to be reordered to run before it arms) and a
  skip-next-end-marker guard so hook installation itself doesn't emit a spurious
  end-of-command marker.

## The sqlez migration fix — confirmed

`is_already_applied_add_column` (`crates/sqlez/src/migrations.rs:28-37`) exists
exactly as scoped: takes the migration text and the resulting `anyhow::Error`,
normalizes whitespace (guards against `sqlformat` splitting `ADD`/`COLUMN` across
lines) and returns `true` only when the statement is an `ADD COLUMN` *and* the error
chain contains `"duplicate column name"` — i.e. "this exact column already exists,"
never a broader class of migration failure. Used in `migrate()`'s loop to log a
warning and treat the step as already-applied instead of hard-failing. `sqlez`'s test
suite: **21 passed, 0 failed** this session (includes
`migration_tolerates_column_that_already_exists` and a companion negative test
confirming unrelated `ADD COLUMN` failures still fail loudly).

## Log-viewer clipboard copy — confirmed present and correctly wired

**This is the item the prompt specifically flagged as unconfirmed. It exists, and it
works as originally scoped:**

- **Per-line copy**: every rendered log row (`render_log_row`, `engine_panel.rs:~577`)
  has a `ui::CopyButton` next to its expand chevron, built from
  `format_log_entry_for_clipboard(entry)` for that specific row.
- **Copy-all-visible**: one `ui::CopyButton` in the log tab's filter row
  (`engine_panel.rs:~384-429`), built from the *exact same* `self.log_view.
  visible_lines()` call the rows below it are rendered from — confirmed this
  respects the active Live/History mode and All/Predictions/Raw-events filter by
  construction, not by a separate, potentially-drifting code path. Disabled when
  nothing is visible.
- **Clipboard API**: `ui::CopyButton` (an existing, pre-built component also used
  elsewhere, e.g. `repl/outputs.rs`) — wraps `cx.write_to_clipboard(ClipboardItem::
  new_string(...))` internally, the same clipboard-writing pattern used throughout
  this codebase.
- **Copy feedback**: also built into `ui::CopyButton` — its icon swaps to a checkmark
  and its tooltip to "Copied!" for 2 seconds on click. No custom toast code was
  needed or added.
- **Format**: `format_log_entry_for_clipboard` (`engine_panel.rs:785`) reuses
  `summarize_log_entry`'s existing badge/summary computation — the same values
  already rendered on screen — assembling `[HH:MM:SS] badge: summary`, so copy-paste
  reads the same as the panel.

No `notifications`/`StatusToast` dependency was added to `engine_panel`'s
`Cargo.toml` for this — confirmed via `grep`, only `ui.workspace = true` (already
present) was needed.

## Logging schema

Unchanged from the original audit — re-confirmed by reading `logging.rs` directly.
Written to `~/.nucleus/logs/YYYY-MM-DD.jsonl`, async, best-effort, buffered. Same
three `LogEntry`/`LogLine` variants (`intent_prediction`, `raw_event`, `feedback`),
same `RawEvent` variant set including the Phase 4b-1 `TerminalCommandStarted`/
`TerminalCommandFinished` additions. `prediction_id` correlation unchanged.

## Test coverage

`cargo test -p nucleus_intent --lib`: **93 passed; 0 failed; 0 ignored** (run this
session — up from 84 immediately before the terminal-engagement session's 9 new
tests landed; the doc's previous "78" snapshot predates even that). Breakdown by
module:

| Module | Count | What it covers |
|---|---|---|
| `tests` | 41 | `classify()` scoring (original regression suite + classifier-expansion additions + terminal-engagement session's 9 new tests: the three routing rules, the no-completed-command/below-threshold cases, the failed-test double-count, and three tiebreaker tests — exact tie, clear-leader-never-overridden, margin boundary), plus the real-file logger/feedback round-trip tests. |
| `terminal_watcher::tests` | 32 | Hook script content, install-command shape, redaction, categorization, marker parsing, injection-state bookkeeping, the bash `DEBUG`-trap fix, `has_pending_command_of_category`. |
| `terminal_watcher::stress_tests` | 11 | `proptest`-based fuzzing of redaction/categorization/marker-parsing/hook-install-command against arbitrary input. |
| `classify_stress_tests` | 3 | `proptest`-based fuzzing of `classify()` itself — including `edit_events`/`navigation_events`/`terminal_activity_events` (all three, since this session) generated as *full-range `f32`*, deliberately including NaN/Infinity/negative values a real engine could never produce, plus `focused_terminal_last_command` fuzzed independently (`None`/`Some` with arbitrary category+exit code) — to stress-test that every use of those fields degrades safely. Also re-run this session at `PROPTEST_CASES=5000` (25× the default) with no failures, on top of the default-count run. |

`cargo test -p sqlez --lib`: **21 passed, 0 failed** (unchanged, not touched this
session).

Regression scenarios explicitly re-verified this session under the *new* tiebreaker
model (not just re-run — checked their actual numeric output and, for the
pane-focus-alone scenario specifically, re-confirmed the *reasoning* still holds, not
just the pass/fail): pause-alone still resolves to Idle at low Debugging confidence,
sustained-edit-burst still resolves to Implementing, stale-signal-long-pause still
resolves to Idle, diagnostic-location-correlation still shows a meaningful (>0.1)
jump, pane-focus-alone still stays under 0.2 confidence for both Debugging and
Implementing (now via a *stronger* mechanism than before — no flat contribution
fires at all, and the tiebreaker's own margin check, `1.0 - 0.0 = 1.0 > 0.05`,
confirms it can't spuriously promote either category off a bare 0 vs. 1.0 gap), and
the agent-gate override still zeroes every other probability. None of their numeric
values shifted from before this session's work landed — every new/changed field
still defaults to `0.0`/`None`/`false` for every pre-existing test scenario, and the
two old flat pane-focus weights they never depended on are simply gone.

## Debug/log-viewer UI (`engine_panel`)

Confirmed by reading `engine_panel.rs` and `log_view.rs` directly this session:

- Unchanged panel shell: left/right-dock, `Sparkle` icon, loaded automatically at
  startup.
- **Overview tab**: unchanged — current prediction, Session state block, static
  task-detection caption.
- **Logs tab**: unchanged core mechanics (Live/History toggle, All/Predictions/Raw-
  events filter, click-to-expand pretty-printed JSON, live tail capped at 200
  in-memory entries, history capped at 500 via `read_log_file`) **plus the new
  clipboard copy affordances** described above.
- Renders no source code or diff content anywhere — unchanged, still explicit in
  `log_entry_to_pretty_json`'s doc comment.

## Known limitations

- 4 of 11 `DeveloperIntent` variants are unscored stubs now, not 7 — Refactoring,
  Reviewing, Documenting, Planning. (Was 7 of 11 at the original audit; Testing/
  Exploring/Configuring moved from stub to real this session's predecessor.)
- Task/test-run detection still cannot distinguish task types for the *task-runner*
  path — any completed/failed task terminal counts the same regardless of label. The
  new `Testing` intent works around this for *plain-terminal* commands specifically
  (via categorization), not by fixing the underlying task-runner conflation.
- `cursor_at_diagnostic` is still a point-in-time check with no dwell tracking —
  unchanged, still an accepted first pass per its own doc comment.
- Window-scoped signals (`agent_active`, `focused_pane`, `cursor_at_diagnostic`, and
  now `focused_terminal_last_command`) still only refresh on the `PRUNE_INTERVAL`
  tick (10s) — unchanged, extended to the new field for the same reason
  (`compute_focused_pane` needs a `Window`).
- `terminal_activity_events` only tracks *selection* activity, not pure scrolling —
  no low-noise dedicated event exists for scroll-without-select in this terminal
  stack, and `Event::Wakeup` is too noisy to reuse (fires for reasons unrelated to
  engagement). A real limitation if a user reads terminal output purely by scroll
  wheel without ever dragging to select — not addressed this session.
- No retention policy for `~/.nucleus/logs/*.jsonl` — unchanged, still explicitly out
  of scope per `logging.rs`'s module doc comment.
- **No OSC 133 support** — still true; re-confirmed via `grep -rn osc_dispatch`
  across all of `crates/` (the one hit is `terminal_watcher.rs`'s own doc comment
  *explaining* why it doesn't use OSC 133, not an implementation of it). Unlike the
  original audit, though, a terminal-watcher component now very much *does* exist
  (Phase 4b-1) — see the original audit's own claim about this in Discrepancies
  below.
- All weight constants are still explicitly documented as first-pass guesses, "not
  tuned against real usage yet" — now including the new Part A-D weights, the
  15-second density-decay time constant, the terminal-engagement weights
  (`WEIGHT_TERMINAL_ENGAGEMENT_{FAILED,TEST,EXPLORING}`, all reasoned relative to
  existing weights but not tuned against real usage), and `FOCUS_TIE_MARGIN` (0.05).
- A classifier-expansion session (Part E) cross-checked the taxonomy and idle-floor
  curve against WakaTime/ActivityWatch and left `docs/PHASE6_INTERRUPTION_NOTES.md`
  as a landing pad for future interruption-policy work — validation only, nothing
  built. `docs/PHASE6_INTERRUPTION_NOTES.md` exists; confirmed via `ls docs/`.
- The Overview tab's static caption ("Task runs are detected via Zed's task runner
  only... commands typed directly into a plain shell terminal aren't seen") is now
  stale in its literal wording — plain-terminal commands *are* seen for the new
  `Testing` intent (just not for the original Debugging/Implementing task-run
  counters). Not fixed this session (audit only, no implementation changes).

## Discrepancies found (this session)

- **Log-viewer clipboard feature: confirmed to exist and work as scoped.** This was
  the one genuinely unconfirmed item going into this audit (a session was prompted
  to build it, no completion report was ever received). Read directly: it's fully
  present (`CopyButton` per-row and copy-all-visible in `engine_panel.rs`), correctly
  respects the active filter (derives from the same `visible_lines()` the rows
  render from), uses the real clipboard API (`ui::CopyButton` → `cx.write_to_
  clipboard`), and gives copy feedback (the component's own built-in checkmark/
  "Copied!" state). No gaps found.
- **`active_files` now IS fed into `classify`**, contradicting the original audit's
  explicit "No — tracked, not read by classify" claim for that field. This changed
  because of the new `Configuring` intent (`active_files.first()` is the file-identity
  check), not because of any error in the original audit at the time it was written —
  it was correct then, the code changed since.
- **The original audit's own "No terminal watcher exists" claim (in its Known
  limitations) is now false** — Phase 4b-1 landed after that audit was written. This
  isn't a new discrepancy so much as a reminder of exactly the risk this refresh
  session exists to catch: a "current snapshot" doc goes stale fast in an actively-
  worked codebase, and a session reading it without re-verifying would have
  confidently repeated a now-wrong claim.
- **Classifier-expansion work (edit/navigation-density, Testing/Exploring/
  Configuring, decay weighting) has landed**, fully, in the working tree — confirmed
  by reading `classify()` in full and running the test suite, not inferred. This was
  explicitly flagged as possibly-not-landed-yet in this session's prompt; it has.
- **`feedback_toast.rs`'s `CORRECTABLE_INTENTS`** grew from 4 to 7 entries in the
  same session that added the three new real intents — a good sign of that session
  keeping a directly-coupled piece of code in sync, confirmed by reading it directly
  rather than assuming.
- **Test count grew from 15 (original audit, classify-only) to 78** across the whole
  `nucleus_intent` crate (32 classify/logging + 32 terminal-watcher + 14 proptest) —
  not a discrepancy, just noting the comparison basis changed since the original
  audit only counted `classify`-related tests.
- Two stray macOS AppleDouble sidecar files (`._feedback_toast.rs`, `._migrations.rs`,
  `._PHASE6_INTERRUPTION_NOTES.md`) are present as untracked files in `git status` —
  artifacts of this external volume's ExFAT filesystem lacking extended-attribute
  support, not anything checked into source control or referenced by any build.
  Harmless, not investigated further (out of this audit's scope — no implementation
  code involved).
