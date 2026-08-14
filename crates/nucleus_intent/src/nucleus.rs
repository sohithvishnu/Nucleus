//! Phase 4: passive observer.
//!
//! Collects real editor/project/terminal events into a rolling [`SessionState`],
//! then runs a rule-based classifier over that state to guess the developer's
//! current [`DeveloperIntent`]. Produces no suggestions and persists nothing.
//!
//! The `DeveloperIntent` variants and the scoring rules below are authored
//! directly from the phase-4 spec (not ported from an external design doc),
//! and are expected to be retuned once real disagreement data comes in.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use agent_ui::AgentPanel;
use collections::{HashMap, HashSet, VecDeque};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyWindowHandle, App, Context, Entity, EntityId, EventEmitter, Focusable, Subscription, Task,
    WeakEntity, Window,
};
use language::{Buffer, BufferEvent};
use project::Project;
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::TerminalPanel;
use workspace::Workspace;
use workspace::item::ItemHandle;

mod feedback_toast;
mod logging;
mod terminal_watcher;
mod terminal_watcher_settings;

pub use feedback_toast::FeedbackNudgeToast;
pub use logging::{
    Feedback, LogEntry, MAX_HISTORY_LINES, NucleusLogger, RawEvent, list_log_dates, log_dir,
    parse_log_line, read_log_file,
};
pub use terminal_watcher::{CommandCategory, categorize_command};
pub use terminal_watcher_settings::NucleusTerminalWatcherSettings;

/// How far back "recent" activity counters look before decaying away.
///
/// Provisional: this is a guess, not tuned against real usage yet.
const RECENT_WINDOW: Duration = Duration::from_secs(5 * 60);

/// How often stale activity is pruned from the rolling window, and how often
/// the terminal dock is polled for newly-completed task terminals (see
/// [`NucleusEngine::poll_task_terminals`] for why polling rather than an
/// item-added event is used). Worst-case task-completion detection latency
/// is one interval; provisional, not tuned against real usage yet.
const PRUNE_INTERVAL: Duration = Duration::from_secs(10);

/// Number of most-recently-touched files retained for display.
const MAX_ACTIVE_FILES: usize = 5;

/// How large a single edit must be (inserted + deleted chars) to count as
/// "large" — shared between the burst tracking below and `classify`'s own
/// large-edit signal so both agree on one definition.
const LARGE_EDIT_THRESHOLD: usize = 40;

/// How far back to look for *recent, burst-like* activity — deliberately
/// much shorter than `RECENT_WINDOW`: this is specifically about detecting
/// something happening live right now, not "sometime in the last five
/// minutes." Scoped to recurring large edits (`RecentActions::large_edits`)
/// and `SessionState::test_command_recently_passed` (Part B) — both still a
/// plain hard-cutoff rolling window, unlike the two Part A density signals
/// below, which deliberately replace a hard cutoff with continuous decay
/// (see `DENSITY_DECAY_TIME_CONSTANT_SECS`) specifically to avoid the
/// artificial cliff a window boundary creates. Kept as a hard window here
/// because neither of *these* two signals has that cliff problem in
/// practice: a large-edit burst and a passing test are each already
/// individually meaningful events (not "many small things add up"), so
/// there's no thin, easily-misjudged case at the boundary the way there is
/// for "was there enough recent typing" — see `classify`'s doc comment for
/// the fuller reasoning.
const ACTIVITY_BURST_WINDOW: Duration = Duration::from_secs(30);

/// Part A (classifier-expansion, decay revision): the time constant for
/// the exponential recency decay backing the edit-density and
/// navigation-density signals, replacing this session's earlier
/// hard-rolling-window design. Inspired by Eclipse Mylyn's
/// Degree-of-Interest model — each interaction contributes decaying
/// "interest" rather than mattering fully inside a window and not at all
/// outside it, avoiding the artificial cliff a hard cutoff creates (an
/// edit at t-29s counting fully, one at t-31s counting zero, even though
/// nothing meaningfully changed between them). A single event's
/// contribution is `exp(-age_seconds / DENSITY_DECAY_TIME_CONSTANT_SECS)`:
/// 1.0 the instant it happens, ~0.37 (1/e) after 15s, ~0.14 after 30s,
/// ~0.007 after 75s (see `DENSITY_RETENTION_WINDOW`). 15s was chosen to
/// land in the same "tens of seconds" scale the rest of this file already
/// uses for burst-like signals (`ACTIVITY_BURST_WINDOW` = 30s = 2 time
/// constants) — dense continuous typing (an edit every 1-3s, the
/// motivating log's own cadence) accumulates a decayed sum in the
/// high-single-digits within this timescale, a similar order of magnitude
/// to the old hard-count design's cap, without the cliff.
const DENSITY_DECAY_TIME_CONSTANT_SECS: f32 = 15.0;

/// How long a timestamp is kept in memory at all before being pruned,
/// purely for bookkeeping hygiene — *not* the scoring boundary (scoring is
/// the continuous decay above, which is why this can be generous: five
/// time constants, past which a single event's contribution is ~0.007,
/// negligible against the thresholds below regardless of whether it's kept
/// or dropped).
const DENSITY_RETENTION_WINDOW: Duration = Duration::from_secs(75);

/// Sums each `timestamps` entry's exponentially-decayed contribution as of
/// `now` — the actual Part A recency-and-frequency weighting. Called from
/// `NucleusEngine::refresh` (which has a real wall-clock `now`) to produce
/// the plain `f32` that goes into `RecentActions::{edit_events,
/// navigation_events}`; `classify` itself only ever sees that already-computed
/// number, never wall-clock time, so it stays a pure, deterministic function
/// of its `SessionState` argument (see `classify_is_deterministic_for_identical_input`).
/// Callers are expected to have already pruned `timestamps` to
/// `DENSITY_RETENTION_WINDOW` (this function doesn't re-check that itself —
/// it would just contribute a negligible amount for anything beyond it
/// anyway).
fn decayed_density(timestamps: &VecDeque<Instant>, now: Instant) -> f32 {
    timestamps
        .iter()
        .map(|at| {
            let age_secs = now.saturating_duration_since(*at).as_secs_f32();
            (-age_secs / DENSITY_DECAY_TIME_CONSTANT_SECS).exp()
        })
        .sum()
}

/// How often the feedback nudge (Part A) is allowed to fire. Deliberately
/// low-frequency and mid-range within the requested 10-15 minute window: an
/// interruption tool whose own tooling interrupts often would undermine the
/// thing it's trying to reduce. 12 minutes doesn't line up with any other
/// timer here (`PRUNE_INTERVAL`, `ACTIVITY_BURST_WINDOW`), which is deliberate —
/// it should read as independent of session activity, not synchronized to it.
const FEEDBACK_NUDGE_INTERVAL: Duration = Duration::from_secs(12 * 60);

/// How many lines away from a known active error the cursor can be and
/// still count as "at" it, for the diagnostic-location correlation signal
/// (Part C). ±2 is a deliberately tight window — wide enough to tolerate the
/// cursor sitting one line above/below the exact error token (e.g. on a
/// function signature when the error is reported on its closing brace)
/// without being so wide it fires for "somewhere in the same neighborhood."
const DIAGNOSTIC_LOCATION_LINE_WINDOW: u32 = 2;

/// Minimum confidence delta (on top of an unchanged selected intent) that
/// counts as a "meaningful shift" worth logging an `intent_prediction` line
/// for. Chosen arbitrarily as "more than a rounding-error-sized move"; not
/// tuned against real usage yet.
const CONFIDENCE_LOG_THRESHOLD: f32 = 0.1;

/// Cross-checked (classifier-expansion session, Part E — validation only,
/// no variants added) against WakaTime's heartbeat category taxonomy
/// (coding, building, indexing, debugging, browsing, running tests, writing
/// tests, manual testing, writing docs, communicating, code reviewing,
/// notes, researching, learning, designing). Findings, in full, are in that
/// session's report — briefly: `Debugging` matches cleanly; `Exploring`
/// matches WakaTime's "browsing" in *name* but not fully in *signal*
/// (WakaTime's is file-tree navigation, this crate's is in-buffer
/// selection/cursor movement — see `classify`'s Part C doc comment);
/// `Testing` matches WakaTime's "running tests" specifically, not its
/// separate "writing tests"/"manual testing"; WakaTime's "building" and
/// "indexing" have no equivalent variant here at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum DeveloperIntent {
    Debugging,
    Implementing,
    Refactoring,
    Exploring,
    Reviewing,
    Testing,
    Documenting,
    Configuring,
    Planning,
    #[default]
    Idle,
    /// The user is actively interacting with Zed's own built-in agent panel
    /// — ground truth (a directly observable focus state), not inference,
    /// so `classify` gates on it ahead of and independent from the
    /// Debugging/Implementing/Idle weighted scoring. See `classify`'s doc
    /// comment.
    ConsultingAgent,
}

impl DeveloperIntent {
    pub const ALL: [DeveloperIntent; 11] = [
        Self::Debugging,
        Self::Implementing,
        Self::Refactoring,
        Self::Exploring,
        Self::Reviewing,
        Self::Testing,
        Self::Documenting,
        Self::Configuring,
        Self::Planning,
        Self::Idle,
        Self::ConsultingAgent,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Debugging => "Debugging",
            Self::Implementing => "Implementing",
            Self::Refactoring => "Refactoring",
            Self::Exploring => "Exploring",
            Self::Reviewing => "Reviewing",
            Self::Testing => "Testing",
            Self::Documenting => "Documenting",
            Self::Configuring => "Configuring",
            Self::Planning => "Planning",
            Self::Idle => "Idle",
            Self::ConsultingAgent => "Consulting Agent",
        }
    }
}

/// No `Eq` (unlike most other plain-data structs in this file): the two
/// decayed-density fields below are `f32`, which can't implement `Eq`
/// (no total ordering — `NaN`). `PartialEq` (used by tests via `assert_eq!`)
/// is unaffected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RecentActions {
    pub test_runs: u32,
    pub failed_test_runs: u32,
    pub saves: u32,
    pub file_switches: u32,
    /// Large edits (>= [`LARGE_EDIT_THRESHOLD`] chars) within the last
    /// [`ACTIVITY_BURST_WINDOW`] — deliberately a much shorter, separate
    /// window than the other fields above (which use the full
    /// `RECENT_WINDOW`): this one exists specifically to tell "a burst of
    /// large edits is happening right now" apart from "one happened
    /// sometime in the last five minutes." `#[serde(default)]` so
    /// historical log lines written before this field existed still parse.
    #[serde(default)]
    pub large_edits: u32,
    /// Activity-density signal (classifier-expansion session), Part A #1:
    /// recency-and-frequency-decayed sum of edit events of *any* size (see
    /// [`decayed_density`]/[`DENSITY_DECAY_TIME_CONSTANT_SECS`]) — a
    /// continuous relative of `large_edits` above (every large edit
    /// contributes here too, alongside every small one), not a superset in
    /// the discrete-counting sense anymore. Exists because a normal editing
    /// session is mostly small ~1-char edits punctuated by occasional
    /// larger ones; `large_edits` alone can't see a dense run of small
    /// edits as activity, which was the root cause of a real logged
    /// false-Idle bug (continuous editing, no pause, still classified Idle
    /// because no single edit ever cleared the "large" threshold). See
    /// `classify`'s doc comment.
    #[serde(default)]
    pub edit_events: f32,
    /// Activity-density signal, Part A #2: recency-and-frequency-decayed
    /// sum of selection-changed (navigation) events. Feeds
    /// `DeveloperIntent::Exploring` (Part C) — dense navigation with
    /// little/no editing is a distinct real pattern (browsing code, jumping
    /// between definitions) from both Implementing and Idle.
    #[serde(default)]
    pub navigation_events: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsSummary {
    pub errors: usize,
    pub warnings: usize,
}

/// Which kind of view currently holds keyboard focus, as tracked by GPUI's
/// focus system (`entity.focus_handle(cx).contains_focused(window, cx)`).
/// Agent-panel focus is deliberately not a variant here — that's Part B's
/// hard `ConsultingAgent` gate (`SessionState::agent_active`), not a soft
/// prior, so adding it here too would be a redundant second signal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FocusedPane {
    Editor,
    Terminal,
    #[default]
    Other,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub active_files: Vec<PathBuf>,
    pub recent_actions: RecentActions,
    pub diagnostics: DiagnosticsSummary,
    pub current_symbol: Option<String>,
    pub pause_seconds: u64,
    pub diff_summary: Option<String>,
    /// Part B: whether the agent panel currently has an active thread *and*
    /// keyboard focus — a hard, ground-truth gate in `classify`, not a
    /// weighted signal. See `classify`'s doc comment.
    #[serde(default)]
    pub agent_active: bool,
    /// Part C: mild prior toward Debugging (terminal) or Implementing
    /// (editor) — see `classify`.
    #[serde(default)]
    pub focused_pane: FocusedPane,
    /// Part C: whether the cursor is currently at or within
    /// [`DIAGNOSTIC_LOCATION_LINE_WINDOW`] lines of a line with a known
    /// active error — a sharper Debugging signal than `diagnostics.errors`
    /// alone (which only says an error exists somewhere in the file).
    #[serde(default)]
    pub cursor_at_diagnostic: bool,
    /// Classifier-expansion session, Part B: whether a `test`-category
    /// plain-terminal command (`terminal_watcher::CommandCategory::Test`,
    /// Phase 4b-1) is currently in flight. Driven entirely by the
    /// terminal-watcher's plain-terminal command tracking — deliberately
    /// separate from `RecentActions::{test_runs,failed_test_runs}` (which
    /// come from Zed's task runner only) so this can't dilute or be diluted
    /// by that existing, unrelated data source. See `classify`'s doc
    /// comment for the boundary with `DeveloperIntent::Debugging`.
    #[serde(default)]
    pub test_command_running: bool,
    /// Classifier-expansion session, Part B: whether a `test`-category
    /// plain-terminal command finished *successfully* (exit code 0) within
    /// [`ACTIVITY_BURST_WINDOW`] — a passing run someone just watched
    /// complete, not a stale one from minutes ago.
    #[serde(default)]
    pub test_command_recently_passed: bool,
}

/// Stable per-prediction identifier so a feedback response (Part A) can
/// reference exactly which `intent_prediction` log line it's about, even
/// though predictions aren't logged on every classifier tick (only on
/// meaningful change — see `NucleusEngine::is_meaningful_prediction_change`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PredictionId(uuid::Uuid);

impl PredictionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for PredictionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentPrediction {
    pub prediction_id: PredictionId,
    /// Probability per intent, in [`DeveloperIntent::ALL`] order.
    pub probabilities: Vec<(DeveloperIntent, f32)>,
    pub intent: DeveloperIntent,
    pub confidence: f32,
    pub evidence: Vec<String>,
}

/// Everything [`NucleusEngine`] hands to [`NucleusLogger`] for persisting,
/// emitted at the same call sites *in addition to* (not instead of) the
/// logger call — so a live log viewer can subscribe to the same in-memory
/// data the logger writes, without reading its own output back off disk.
#[derive(Debug, Clone)]
pub enum NucleusEvent {
    RawEvent(RawEvent),
    IntentPrediction {
        prediction: IntentPrediction,
        session_state: SessionState,
    },
    /// Part A: the periodic feedback nudge fired — a UI layer (`EnginePanel`)
    /// reacts by showing [`FeedbackNudgeToast`]. Carries just enough to
    /// build that toast; `NucleusEngine` itself renders nothing (see the
    /// module doc comment: "produces no suggestions").
    FeedbackNudgeRequested {
        prediction_id: PredictionId,
        intent: DeveloperIntent,
    },
}

/// Passive observer entity: subscribes to real workspace/project/editor/
/// terminal events, aggregates them into [`SessionState`], and classifies
/// [`DeveloperIntent`] from that state. Also emits [`NucleusEvent`] for
/// every line it logs, so observers wanting a live tail can `cx.subscribe`
/// instead of tailing the log file; for plain "state changed, re-render"
/// observers `cx.observe` and reading [`Self::session_state`] /
/// [`Self::prediction`] is still the simpler option.
pub struct NucleusEngine {
    session_state: SessionState,
    prediction: IntentPrediction,

    active_editor: Option<Entity<Editor>>,
    active_buffer_snapshot: Option<text::BufferSnapshot>,
    active_file_path: Option<PathBuf>,
    /// Size (inserted + deleted chars) of the most recent buffer edit, used by
    /// the classifier; kept separate from `diff_summary`'s display string so
    /// we never have to re-parse formatted text back into numbers.
    last_edit_magnitude: Option<usize>,

    recent_files: Vec<PathBuf>,
    save_timestamps: VecDeque<Instant>,
    file_switch_timestamps: VecDeque<Instant>,
    test_run_outcomes: VecDeque<(Instant, bool)>,
    /// Backs `RecentActions::large_edits` — see `ACTIVITY_BURST_WINDOW`.
    large_edit_timestamps: VecDeque<Instant>,
    /// Backs `RecentActions::edit_events` (Part A #1) — raw timestamps of
    /// every edit, regardless of size, unlike `large_edit_timestamps` above
    /// which only tracks edits >= `LARGE_EDIT_THRESHOLD`. Pruned to
    /// `DENSITY_RETENTION_WINDOW` (not `ACTIVITY_BURST_WINDOW` — this feeds
    /// `decayed_density`, not a hard count), and only ever turned into the
    /// actual decayed number at `refresh` time.
    edit_timestamps: VecDeque<Instant>,
    /// Backs `RecentActions::navigation_events` (Part A #2) — same
    /// decay-at-`refresh`-time treatment as `edit_timestamps` above.
    selection_timestamps: VecDeque<Instant>,
    /// Backs `SessionState::test_command_recently_passed` (Part B) —
    /// timestamps of `test`-category plain-terminal commands that finished
    /// with exit code 0. Whether one is *currently* running is queried
    /// directly from `terminal_watcher` instead (see
    /// `TerminalCommandWatcher::has_pending_command_of_category`), so this
    /// only needs to track the "just passed" recency case.
    test_pass_timestamps: VecDeque<Instant>,
    last_activity_at: Instant,

    workspace: WeakEntity<Workspace>,
    /// Most recently observed `TaskStatus` per task-terminal, keyed by the
    /// terminal's `EntityId`. Used to detect the `Running` -> `Completed`
    /// transition rather than just "is this terminal currently completed" —
    /// terminals are commonly reused across reruns (`task::Rerun` defaults to
    /// `use_new_terminal: false`), so a plain "have we ever seen this
    /// terminal" set would only ever count the first run in a reused tab.
    last_seen_task_status: HashMap<EntityId, terminal::TaskStatus>,
    /// Phase 4b-1: shell-hook injection + marker-driven command tracking for
    /// plain (non-task) terminals — see `terminal_watcher`. Markers are
    /// written to a per-terminal file (not the terminal's own stdout), so
    /// detection is polled alongside injection in `poll_plain_terminals`
    /// rather than reactive to a GPUI event — no terminal subscription is
    /// needed here at all.
    terminal_watcher: terminal_watcher::TerminalCommandWatcher,

    logger: NucleusLogger,
    /// The last `IntentPrediction` actually written to the log, compared
    /// against on every `refresh()` so we only log on a meaningful change
    /// (selected intent changes, or confidence moves by more than
    /// `CONFIDENCE_LOG_THRESHOLD`) rather than on every classifier tick.
    last_logged_prediction: Option<IntentPrediction>,

    /// Captured at construction time (see `new`'s doc comment on why a
    /// window is guaranteed to exist then) so the periodic timers below can
    /// re-enter it later for the window-scoped focus checks
    /// (`contains_focused`) that Parts B/C need — `NucleusEngine`'s own
    /// methods otherwise only ever get `Context<Self>`, never `Window`.
    window_handle: AnyWindowHandle,
    /// Part A: which prediction the last feedback nudge asked about, so an
    /// unchanged prediction doesn't get re-prompted every tick.
    last_nudge_prediction_id: Option<PredictionId>,

    _subscriptions: Vec<Subscription>,
    _project_subscription: Option<Subscription>,
    _active_item_subscriptions: Vec<Subscription>,
    _decay_task: Task<()>,
    _nudge_task: Task<()>,
}

impl EventEmitter<NucleusEvent> for NucleusEngine {}

impl NucleusEngine {
    /// `window` is required (not optional/deferred) because `new` always
    /// runs synchronously inside `Workspace::update_in` when the panel that
    /// owns this engine is constructed (see `engine_panel::EnginePanel::new`
    /// and `EnginePanel::load`) — a window is guaranteed to exist at this
    /// exact point, so its handle is captured once here rather than
    /// threading `&mut Window` through every method that might eventually
    /// need it.
    pub fn new(workspace: Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let weak_workspace = workspace.downgrade();
        let logger = NucleusLogger::new(cx.background_executor().clone());
        let workspace_subscription = cx.subscribe(&workspace, Self::handle_workspace_event);
        let window_handle = window.window_handle();

        let decay_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PRUNE_INTERVAL).await;
                let Ok(window_handle) = this.read_with(cx, |this, _| this.window_handle) else {
                    return;
                };
                let result = window_handle.update(cx, |_, window, cx| {
                    this.update(cx, |this, cx| this.prune_and_refresh(window, cx))
                });
                if !matches!(result, Ok(Ok(()))) {
                    return;
                }
            }
        });

        let nudge_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(FEEDBACK_NUDGE_INTERVAL)
                    .await;
                if this
                    .update(cx, |this, cx| this.maybe_request_feedback_nudge(cx))
                    .is_err()
                {
                    return;
                }
            }
        });

        // `new` runs synchronously inside Workspace's own `update_in` call that
        // constructs this panel, so `workspace` is still leased here — reading
        // it (to get `project` or the active item) would double-lease and
        // panic. Defer that initial read to the next turn, once the update
        // that's constructing us has returned.
        cx.spawn({
            let workspace = workspace.clone();
            async move |this, cx| {
                this.update(cx, |this, cx| {
                    this.bind_project(&workspace, cx);
                    this.sync_active_item(&workspace, cx);
                    this.refresh(cx);
                })
                .ok();
            }
        })
        .detach();

        Self {
            session_state: SessionState::default(),
            prediction: IntentPrediction::default(),
            active_editor: None,
            active_buffer_snapshot: None,
            active_file_path: None,
            last_edit_magnitude: None,
            recent_files: Vec::new(),
            save_timestamps: VecDeque::new(),
            file_switch_timestamps: VecDeque::new(),
            test_run_outcomes: VecDeque::new(),
            large_edit_timestamps: VecDeque::new(),
            edit_timestamps: VecDeque::new(),
            selection_timestamps: VecDeque::new(),
            test_pass_timestamps: VecDeque::new(),
            last_activity_at: Instant::now(),
            workspace: weak_workspace,
            last_seen_task_status: HashMap::default(),
            terminal_watcher: terminal_watcher::TerminalCommandWatcher::new(),
            logger,
            last_logged_prediction: None,
            window_handle,
            last_nudge_prediction_id: None,
            _subscriptions: vec![workspace_subscription],
            _project_subscription: None,
            _active_item_subscriptions: Vec::new(),
            _decay_task: decay_task,
            _nudge_task: nudge_task,
        }
    }

    fn bind_project(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let project = workspace.read(cx).project().clone();
        self._project_subscription = Some(cx.subscribe(&project, Self::handle_project_event));
    }

    pub fn session_state(&self) -> &SessionState {
        &self.session_state
    }

    pub fn prediction(&self) -> &IntentPrediction {
        &self.prediction
    }

    /// Cheap to clone (see [`NucleusLogger`]'s doc comment) — lets a UI
    /// layer reacting to [`NucleusEvent::FeedbackNudgeRequested`] (e.g.
    /// [`FeedbackNudgeToast`]) log a feedback response directly without
    /// routing it back through this engine.
    pub fn logger(&self) -> NucleusLogger {
        self.logger.clone()
    }

    fn handle_workspace_event(
        this: &mut Self,
        workspace: Entity<Workspace>,
        event: &workspace::Event,
        cx: &mut Context<Self>,
    ) {
        if let workspace::Event::ActiveItemChanged = event {
            this.sync_active_item(&workspace, cx);
            this.refresh(cx);
        }
    }

    fn handle_project_event(
        this: &mut Self,
        project: Entity<Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        if let project::Event::DiagnosticsUpdated { .. } = event {
            let summary = project.read(cx).diagnostic_summary(false, cx);
            this.session_state.diagnostics = DiagnosticsSummary {
                errors: summary.error_count,
                warnings: summary.warning_count,
            };
            this.last_activity_at = Instant::now();
            this.refresh(cx);
        }
    }

    fn sync_active_item(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let active_item = workspace.read(cx).active_item(cx);
        let editor = active_item
            .as_ref()
            .and_then(|item| item.act_as::<Editor>(cx));

        let new_file_path = editor.as_ref().and_then(|editor| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .and_then(|buffer| buffer.read(cx).file().map(|file| file.full_path(cx)))
        });

        if new_file_path.is_some() && new_file_path != self.active_file_path {
            self.record_file_switch(new_file_path.clone().unwrap(), cx);
        }
        self.active_file_path = new_file_path;

        self._active_item_subscriptions.clear();
        self.active_editor = None;
        self.active_buffer_snapshot = None;

        if let Some(editor) = editor {
            self._active_item_subscriptions.push(cx.subscribe(
                &editor,
                |this, _editor, event, cx| {
                    if let EditorEvent::SelectionsChanged { local: true } = event {
                        this.record_raw_event(
                            RawEvent::SelectionChanged {
                                file: this.active_file_path.clone(),
                            },
                            cx,
                        );
                        this.selection_timestamps.push_back(Instant::now());
                        this.last_activity_at = Instant::now();
                        this.refresh(cx);
                    }
                },
            ));

            if let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() {
                self.active_buffer_snapshot = Some(buffer.read(cx).text_snapshot());
                self._active_item_subscriptions.push(cx.subscribe(
                    &buffer,
                    |this, buffer, event, cx| match event {
                        BufferEvent::Edited { .. } => this.handle_buffer_edited(buffer, cx),
                        BufferEvent::Saved => this.handle_buffer_saved(cx),
                        _ => {}
                    },
                ));
            }

            self.active_editor = Some(editor);
        }
    }

    fn handle_buffer_edited(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let new_snapshot = buffer.read(cx).text_snapshot();
        let Some(old_snapshot) = self.active_buffer_snapshot.replace(new_snapshot.clone()) else {
            return;
        };
        if new_snapshot.version == old_snapshot.version {
            return;
        }

        let mut edit_count = 0usize;
        let mut inserted = 0usize;
        let mut deleted = 0usize;
        for edit in new_snapshot.edits_since::<usize>(&old_snapshot.version) {
            edit_count += 1;
            deleted += edit.old.end - edit.old.start;
            inserted += edit.new.end - edit.new.start;
        }
        if edit_count == 0 {
            return;
        }

        let file_label = self
            .active_file_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "buffer".to_string());
        let magnitude = inserted + deleted;
        self.last_edit_magnitude = Some(magnitude);
        self.session_state.diff_summary = Some(format!(
            "{edit_count} edit(s), +{inserted}/-{deleted} chars in {file_label}"
        ));
        self.edit_timestamps.push_back(Instant::now());
        if magnitude >= LARGE_EDIT_THRESHOLD {
            self.large_edit_timestamps.push_back(Instant::now());
        }

        let symbol = self.current_symbol(cx);
        self.record_raw_event(
            RawEvent::Edit {
                file: self.active_file_path.clone(),
                symbol,
                inserted_chars: inserted,
                deleted_chars: deleted,
            },
            cx,
        );

        self.last_activity_at = Instant::now();
        self.refresh(cx);
    }

    fn handle_buffer_saved(&mut self, cx: &mut Context<Self>) {
        self.save_timestamps.push_back(Instant::now());
        self.record_raw_event(
            RawEvent::Save {
                file: self.active_file_path.clone(),
            },
            cx,
        );
        self.last_activity_at = Instant::now();
        self.refresh(cx);
    }

    fn record_file_switch(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.file_switch_timestamps.push_back(Instant::now());
        self.record_raw_event(RawEvent::FileSwitch { file: path.clone() }, cx);
        self.recent_files.retain(|existing| existing != &path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(MAX_ACTIVE_FILES);
    }

    /// Writes to the logger and emits [`NucleusEvent::RawEvent`] with the
    /// same value, so a live log viewer sees exactly what got persisted
    /// without reading it back off disk.
    fn record_raw_event(&mut self, event: RawEvent, cx: &mut Context<Self>) {
        self.logger.log_raw_event(&event);
        cx.emit(NucleusEvent::RawEvent(event));
    }

    /// Scans the terminal dock's panes for task terminals that have finished,
    /// recording each one's outcome exactly once.
    ///
    /// This polls rather than reacting to `workspace::Event::ItemAdded`
    /// because task terminals never actually fire that event: `TerminalPanel`
    /// owns its panes directly and subscribes to them with its own
    /// `handle_pane_event`, so `pane::Event::AddItem` is forwarded into
    /// `TerminalPanel`'s internal state, not into `Workspace`'s event stream.
    /// (Confirmed by reading `terminal_panel.rs`'s `add_terminal_task`, which
    /// adds the terminal view straight to the panel's own pane.) Polling
    /// `TerminalPanel::panes()` sidesteps that entirely and also covers
    /// terminals reused across reruns and terminals in split panes.
    fn poll_task_terminals(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            return;
        };

        // Collected during the scan and only recorded (logger + NucleusEvent)
        // afterwards, once `cx` is no longer borrowed by `terminal_panel`/
        // `pane` — `record_raw_event` needs `cx` mutably, which the scan
        // loop below can't offer while iterating panes borrowed through it.
        let mut newly_observed = Vec::new();
        let mut events_to_record = Vec::new();
        for pane in terminal_panel.read(cx).panes() {
            for terminal_view in pane.read(cx).items_of_type::<TerminalView>() {
                let terminal = terminal_view.read(cx).terminal();
                let Some(task) = terminal.read(cx).task() else {
                    continue;
                };
                let status = task.status;
                let label = task.spawned_task.label.clone();
                let entity_id = terminal.entity_id();
                let previous_status = self.last_seen_task_status.insert(entity_id, status);

                if previous_status.is_none() {
                    events_to_record.push(RawEvent::TaskStarted {
                        label: Some(label.clone()),
                    });
                }

                if let terminal::TaskStatus::Completed { success } = status
                    && !matches!(
                        previous_status,
                        Some(terminal::TaskStatus::Completed { .. })
                    )
                {
                    let event = if success {
                        RawEvent::TaskCompleted {
                            label: Some(label.clone()),
                        }
                    } else {
                        RawEvent::TaskFailed {
                            label: Some(label.clone()),
                        }
                    };
                    events_to_record.push(event);
                    newly_observed.push(success);
                }
            }
        }

        for event in events_to_record {
            self.record_raw_event(event, cx);
        }

        if newly_observed.is_empty() {
            return;
        }
        let now = Instant::now();
        for success in newly_observed {
            self.test_run_outcomes.push_back((now, success));
        }
        self.last_activity_at = now;
    }

    /// Phase 4b-1: injects shell hooks into any plain (non-task) terminal
    /// that hasn't already been injected, then polls every live, injected
    /// plain terminal's marker file for newly-appended command start/finish
    /// markers (see `terminal_watcher::TerminalCommandWatcher::
    /// poll_marker_file`) and records any outcomes found. Runs on the same
    /// `PRUNE_INTERVAL` cadence as `poll_task_terminals` — detection latency
    /// here is now up to one interval (was near-instant, reactive to
    /// `Event::Wakeup`, before markers moved off the terminal's own stdout
    /// to a dedicated file — see `terminal_watcher`'s module doc comment for
    /// why), the same tradeoff already accepted for task-terminal detection.
    ///
    /// Gated on `terminal.task().is_none()` so this never observes the same
    /// terminals `poll_task_terminals` already covers. Not unit-tested
    /// directly: exercising this gate needs a real PTY-backed `Terminal`
    /// entity (there's no lightweight way to construct one outside
    /// `crates/terminal`'s own heavier test harness), which is out of
    /// proportion for this phase-4b-1 session — see
    /// `terminal_watcher::tests::test_injected_and_task_status_bookkeeping_are_independent`
    /// for what *is* tested instead: the two pieces of per-terminal state
    /// (`last_seen_task_status` here, `TerminalCommandWatcher`'s own state)
    /// are structurally separate collections, so even if this gate were
    /// somehow bypassed, plain- and task-terminal bookkeeping couldn't
    /// silently merge into one another.
    fn poll_plain_terminals(&mut self, cx: &mut Context<Self>) {
        if !NucleusTerminalWatcherSettings::get_global(cx).enabled {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            return;
        };

        let mut live_ids = HashSet::default();
        let mut to_inject: Vec<Entity<terminal::Terminal>> = Vec::new();
        for pane in terminal_panel.read(cx).panes() {
            for terminal_view in pane.read(cx).items_of_type::<TerminalView>() {
                let terminal = terminal_view.read(cx).terminal();
                if terminal.read(cx).task().is_some() {
                    // Already covered by poll_task_terminals — see this
                    // method's doc comment.
                    continue;
                }
                let entity_id = terminal.entity_id();
                live_ids.insert(entity_id);
                if !self.terminal_watcher.needs_injection(entity_id) {
                    continue;
                }
                // Wait one full poll tick after first noticing a terminal
                // before injecting into it, so the shell's own startup (rc
                // files, `conda init`, etc.) has time to finish — injecting
                // immediately can race that startup and land input while the
                // shell isn't ready to receive it.
                if self.terminal_watcher.mark_observed(entity_id) {
                    continue;
                }
                to_inject.push(terminal.clone());
            }
        }

        for terminal in to_inject {
            let entity_id = terminal.entity_id();
            let shell_kind = terminal.read(cx).shell_kind();
            let Some(script) = terminal_watcher::shell_hook_script(shell_kind) else {
                // Unsupported shell — mark injected anyway so we don't retry
                // every poll tick for a shell we can't hook.
                self.terminal_watcher.mark_injected(entity_id);
                continue;
            };

            let script_path =
                paths::temp_dir().join(format!("nucleus_hook_{}.sh", uuid::Uuid::new_v4()));
            if let Err(error) = std::fs::write(&script_path, script) {
                log::warn!("failed to write nucleus terminal hook script: {error}");
                // Leave injection state as "observed but not injected" so
                // this is retried on the next poll tick rather than skipped
                // forever over a likely-transient I/O error.
                continue;
            }

            let marker_path =
                paths::temp_dir().join(format!("nucleus_markers_{}.log", uuid::Uuid::new_v4()));
            self.terminal_watcher
                .set_marker_file(entity_id, marker_path.clone());

            let input =
                terminal_watcher::hook_install_command(shell_kind, &script_path, &marker_path);
            terminal.update(cx, |terminal, _cx| {
                terminal.write_program_input(input);
            });
            self.terminal_watcher.mark_injected(entity_id);
        }

        self.terminal_watcher.prune(&live_ids);
        self.poll_marker_files(&live_ids, cx);
    }

    /// Polls every currently-live plain terminal's marker file for
    /// newly-appended command start/finish markers and records any
    /// outcomes — the file-based replacement for the old
    /// `Event::Wakeup`-reactive `handle_terminal_wakeup`. Terminals that
    /// were never successfully injected (or whose shell isn't supported)
    /// simply have no marker file yet; `poll_marker_file` treats that as
    /// "nothing new," not an error.
    fn poll_marker_files(&mut self, live_ids: &HashSet<EntityId>, cx: &mut Context<Self>) {
        let mut any_outcome = false;
        for &entity_id in live_ids {
            for outcome in self.terminal_watcher.poll_marker_file(entity_id) {
                any_outcome = true;
                match outcome {
                    terminal_watcher::TerminalCommandOutcome::Started { command } => {
                        self.record_raw_event(
                            RawEvent::TerminalCommandStarted {
                                command: terminal_watcher::redact_command(&command),
                            },
                            cx,
                        );
                    }
                    terminal_watcher::TerminalCommandOutcome::Finished {
                        command,
                        exit_code,
                        duration,
                    } => {
                        // Categorize the pre-redaction command, matching
                        // terminal_watcher's own documented convention (a
                        // command name is never itself the secret-shaped
                        // part `redact_command` targets — see
                        // `categorize_command`'s doc comment).
                        if categorize_command(&command) == CommandCategory::Test
                            && exit_code == 0
                        {
                            self.test_pass_timestamps.push_back(Instant::now());
                        }
                        self.record_raw_event(
                            RawEvent::TerminalCommandFinished {
                                command: terminal_watcher::redact_command(&command),
                                exit_code,
                                duration_ms: duration.as_millis() as u64,
                            },
                            cx,
                        );
                    }
                }
            }
        }
        if any_outcome {
            self.last_activity_at = Instant::now();
        }
    }

    /// Runs every `PRUNE_INTERVAL`. Also where the window-scoped Part B/C
    /// signals (`contains_focused` needs a `Window`, which none of the
    /// event-triggered call sites for `refresh` below have) get recomputed
    /// — so `agent_active`/`focused_pane`/`cursor_at_diagnostic` are only as
    /// fresh as the last prune tick (up to `PRUNE_INTERVAL` stale), while
    /// everything else `refresh` reads stays instantly reactive.
    fn prune_and_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let cutoff = now.checked_sub(RECENT_WINDOW).unwrap_or(now);
        self.save_timestamps.retain(|at| *at >= cutoff);
        self.file_switch_timestamps.retain(|at| *at >= cutoff);
        self.test_run_outcomes.retain(|(at, _)| *at >= cutoff);
        let burst_cutoff = now.checked_sub(ACTIVITY_BURST_WINDOW).unwrap_or(now);
        self.large_edit_timestamps.retain(|at| *at >= burst_cutoff);
        self.test_pass_timestamps.retain(|at| *at >= burst_cutoff);
        // Density-decay signals (Part A) use a much more generous retention
        // cutoff than the hard-window signals above — see
        // `DENSITY_RETENTION_WINDOW`'s doc comment for why that's fine
        // (this is bookkeeping hygiene, not the scoring boundary).
        let density_cutoff = now.checked_sub(DENSITY_RETENTION_WINDOW).unwrap_or(now);
        self.edit_timestamps.retain(|at| *at >= density_cutoff);
        self.selection_timestamps.retain(|at| *at >= density_cutoff);
        self.poll_task_terminals(cx);
        self.poll_plain_terminals(cx);
        self.session_state.agent_active = self.compute_agent_active(window, cx);
        self.session_state.focused_pane = self.compute_focused_pane(window, cx);
        self.session_state.cursor_at_diagnostic = self.compute_cursor_at_diagnostic(cx);
        self.refresh(cx);
    }

    /// Part B: an agent thread must both exist (`active_thread_id`) *and*
    /// currently hold keyboard focus — an agent thread merely sitting open
    /// in the background (e.g. reviewing an old conversation while actually
    /// typing in the editor) should not gate out Debugging/Implementing.
    fn compute_agent_active(&self, window: &mut Window, cx: &mut App) -> bool {
        let Some(workspace) = self.workspace.upgrade() else {
            return false;
        };
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return false;
        };
        let panel = panel.read(cx);
        panel.active_thread_id(cx).is_some() && panel.focus_handle(cx).contains_focused(window, cx)
    }

    /// Part C: mild prior. Agent-panel focus isn't checked here — see
    /// `FocusedPane`'s doc comment for why that's `compute_agent_active`'s
    /// job alone, not a second weaker signal here too.
    fn compute_focused_pane(&self, window: &mut Window, cx: &mut App) -> FocusedPane {
        if let Some(editor) = self.active_editor.as_ref()
            && editor.focus_handle(cx).contains_focused(window, cx)
        {
            return FocusedPane::Editor;
        }

        let Some(workspace) = self.workspace.upgrade() else {
            return FocusedPane::Other;
        };
        let Some(terminal_panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            return FocusedPane::Other;
        };
        let has_focused_terminal = terminal_panel.read(cx).panes().iter().any(|pane| {
            pane.read(cx)
                .items_of_type::<TerminalView>()
                .any(|terminal_view| terminal_view.focus_handle(cx).contains_focused(window, cx))
        });
        if has_focused_terminal {
            FocusedPane::Terminal
        } else {
            FocusedPane::Other
        }
    }

    /// Part C: whether the cursor is currently within
    /// `DIAGNOSTIC_LOCATION_LINE_WINDOW` lines of an active error. Simple
    /// point-in-time correlation, no dwell tracking — see `classify`'s doc
    /// comment on why that's an acceptable first pass rather than
    /// unnecessary complexity.
    fn compute_cursor_at_diagnostic(&self, cx: &mut App) -> bool {
        let Some(editor) = self.active_editor.as_ref() else {
            return false;
        };
        editor.update(cx, |editor, cx| {
            let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
                return false;
            };
            let display_snapshot = editor.display_snapshot(cx);
            let cursor_row = editor
                .selections
                .newest::<text::Point>(&display_snapshot)
                .head()
                .row;
            let buffer_snapshot = buffer.read(cx).snapshot();
            let full_range = text::Point::zero()..buffer_snapshot.max_point();
            buffer_snapshot
                .diagnostics_in_range::<_, text::Point>(full_range, false)
                .filter(|entry| entry.diagnostic.severity == lsp::DiagnosticSeverity::ERROR)
                .any(|entry| cursor_row.abs_diff(entry.range.start.row) <= DIAGNOSTIC_LOCATION_LINE_WINDOW)
        })
    }

    /// Part A: fires the periodic feedback nudge — but only while there's a
    /// meaningful, ongoing prediction worth asking about. Skips `Idle`
    /// (nothing to confirm during a stretch already recognized as inactive)
    /// and `ConsultingAgent` (ground truth, not worth second-guessing —
    /// also matches the requirement that the nudge not fire during Part B's
    /// gate). Never re-asks about the same prediction twice.
    fn maybe_request_feedback_nudge(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.prediction.intent,
            DeveloperIntent::Idle | DeveloperIntent::ConsultingAgent
        ) {
            return;
        }
        if self.last_nudge_prediction_id == Some(self.prediction.prediction_id) {
            return;
        }
        self.last_nudge_prediction_id = Some(self.prediction.prediction_id);
        cx.emit(NucleusEvent::FeedbackNudgeRequested {
            prediction_id: self.prediction.prediction_id,
            intent: self.prediction.intent,
        });
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.session_state.active_files = self.recent_files.clone();
        self.session_state.pause_seconds = now
            .saturating_duration_since(self.last_activity_at)
            .as_secs();
        self.session_state.current_symbol = self.current_symbol(cx);
        self.session_state.recent_actions = RecentActions {
            saves: self.save_timestamps.len() as u32,
            file_switches: self.file_switch_timestamps.len() as u32,
            test_runs: self.test_run_outcomes.len() as u32,
            failed_test_runs: self
                .test_run_outcomes
                .iter()
                .filter(|(_, success)| !success)
                .count() as u32,
            large_edits: self.large_edit_timestamps.len() as u32,
            edit_events: decayed_density(&self.edit_timestamps, now),
            navigation_events: decayed_density(&self.selection_timestamps, now),
        };
        self.session_state.test_command_running = self
            .terminal_watcher
            .has_pending_command_of_category(CommandCategory::Test);
        self.session_state.test_command_recently_passed = !self.test_pass_timestamps.is_empty();

        let new_prediction = classify(&self.session_state, self.last_edit_magnitude);
        if self.is_meaningful_prediction_change(&new_prediction) {
            self.logger
                .log_intent_prediction(&new_prediction, &self.session_state);
            cx.emit(NucleusEvent::IntentPrediction {
                prediction: new_prediction.clone(),
                session_state: self.session_state.clone(),
            });
            self.last_logged_prediction = Some(new_prediction.clone());
        }
        self.prediction = new_prediction;

        cx.notify();
    }

    /// A prediction is worth logging when the selected intent changes, or
    /// when confidence moves by more than [`CONFIDENCE_LOG_THRESHOLD`] —
    /// not on every classifier tick, most of which reproduce the same
    /// conclusion from marginally different session state.
    fn is_meaningful_prediction_change(&self, new_prediction: &IntentPrediction) -> bool {
        match &self.last_logged_prediction {
            None => true,
            Some(last) => {
                last.intent != new_prediction.intent
                    || (last.confidence - new_prediction.confidence).abs()
                        > CONFIDENCE_LOG_THRESHOLD
            }
        }
    }

    fn current_symbol(&self, cx: &App) -> Option<String> {
        let editor = self.active_editor.as_ref()?;
        let (segments, _font) = editor.breadcrumbs(cx)?;
        let joined = segments
            .into_iter()
            .map(|segment| segment.text.to_string())
            .collect::<Vec<_>>()
            .join(" > ");
        (!joined.is_empty()).then_some(joined)
    }
}

// Per-signal weights for `classify` below. Each is a *bounded contribution*
// toward that intent's own score (not an unbounded point value) — no single
// signal, including a pause, can push an intent's score anywhere near 1.0 on
// its own. Values are a first pass sized to roughly the order of magnitude
// in the design doc (failed test runs ~0.35, diagnostic errors ~0.15 — that
// doc also scores a `debugger_active` signal we don't currently collect, so
// it's omitted rather than faked) and are NOT hand-tuned to make any
// specific logged case look right in isolation; expect these to move once
// more real disagreement data comes in.
const WEIGHT_FAILED_TEST_RUNS: f32 = 0.35;
const WEIGHT_FAILED_TEST_RUNS_PER_EXTRA: f32 = 0.03;
const WEIGHT_DIAGNOSTIC_ERRORS: f32 = 0.15;
const WEIGHT_DIAGNOSTIC_ERRORS_PER_EXTRA: f32 = 0.02;
const WEIGHT_ITERATING_SAME_FILE: f32 = 0.20;
const WEIGHT_SMALL_EDIT: f32 = 0.10;
/// Full weight at [`PAUSE_INVESTIGATING_MIN_SECS`], decaying linearly to 0 at
/// [`PAUSE_INVESTIGATING_MAX_SECS`] — see [`pause_investigating_weight`].
const WEIGHT_PAUSE_INVESTIGATING: f32 = 0.15;
const PAUSE_INVESTIGATING_MIN_SECS: u64 = 30;
const PAUSE_INVESTIGATING_MAX_SECS: u64 = 300;

const WEIGHT_CLEAN_SAVE: f32 = 0.25;
const WEIGHT_FILE_SWITCH_EACH: f32 = 0.06;
const WEIGHT_LARGE_EDIT: f32 = 0.25;
const WEIGHT_PASSING_TESTS: f32 = 0.15;
const WEIGHT_CONTINUOUS_ACTIVITY: f32 = 0.10;
/// Bonus per *additional* large edit beyond the first within
/// `ACTIVITY_BURST_WINDOW` — i.e. a lone large edit earns only
/// [`WEIGHT_LARGE_EDIT`], same as before this signal existed; a genuine
/// sustained burst (2nd, 3rd, ... recurrence) earns this on top, each time.
/// See [`classify`]'s doc comment for the bug this fixes.
const WEIGHT_EDIT_BURST_EACH_EXTRA: f32 = 0.15;
/// Caps how many recurring large edits keep adding burst bonus, same
/// diminishing-returns shape as the other `.min(N)`-capped counts below.
const MAX_BURST_EDITS_COUNTED: u32 = 4;

/// Part C, pane-focus prior: deliberately small — this is a tie-breaker, not
/// a driver. Terminal focus only distinguishes Debugging from Implementing/
/// Idle for now, since Testing isn't implemented yet (a terminal could just
/// as easily mean "running the app," not "running tests").
const WEIGHT_TERMINAL_FOCUS: f32 = 0.08;
/// Part C, pane-focus prior: same magnitude and same reasoning as
/// [`WEIGHT_TERMINAL_FOCUS`], mirrored for Implementing.
const WEIGHT_EDITOR_FOCUS: f32 = 0.08;
/// Part C, diagnostic-location correlation: deliberately larger than
/// [`WEIGHT_DIAGNOSTIC_ERRORS`] (0.15) — "editing exactly where the failure
/// pointed" is close to unambiguous, versus "there's an error somewhere in
/// this file."
const WEIGHT_CURSOR_AT_DIAGNOSTIC: f32 = 0.30;

// ---- Classifier-expansion session: Part A (activity-density), Part B
// (Testing), Part C (Exploring), Part D (Configuring). See `classify`'s doc
// comment for how these fit together and interact with the scoring above. ----

/// Part A #1 (edit-density): base contribution once the decayed sum
/// [`RecentActions::edit_events`] reaches [`MIN_EDIT_DENSITY_FOR_SIGNAL`] —
/// deliberately requiring more than a single fresh edit's own ~1.0
/// contribution, so a single small edit alone still doesn't count as
/// "active work" (that was the whole point of [`WEIGHT_SMALL_EDIT`] being
/// small on its own). Comparable in magnitude to [`WEIGHT_LARGE_EDIT`]
/// since both describe the same underlying thing (real, ongoing editing)
/// through a different lens (frequency vs. size) — see [`classify`]'s doc
/// comment for why both are allowed to contribute without either being
/// double-counted.
const WEIGHT_EDIT_DENSITY: f32 = 0.30;
/// Bonus per unit of decayed density beyond [`MIN_EDIT_DENSITY_FOR_SIGNAL`],
/// up to [`MAX_EDIT_DENSITY_COUNTED`] — same diminishing-returns shape as
/// [`WEIGHT_EDIT_BURST_EACH_EXTRA`], just continuous instead of per-integer-
/// count now that the underlying signal is a decayed sum, not a count.
const WEIGHT_EDIT_DENSITY_PER_EXTRA: f32 = 0.06;
/// A lone, just-happened edit contributes almost exactly 1.0 to the decayed
/// sum (`exp(-0/15) = 1.0`); requiring at least 2.0 means it takes at least
/// two edits close together in time (or several a bit further apart) to
/// clear this — never a single edit, no matter how fresh.
const MIN_EDIT_DENSITY_FOR_SIGNAL: f32 = 2.0;
/// Cap on how much decayed density keeps adding bonus — continuous typing
/// at the motivating log's own cadence (an edit every 1-3s) accumulates a
/// decayed sum in roughly this range within `DENSITY_DECAY_TIME_CONSTANT_SECS`-
/// scale timeframes; see `DENSITY_DECAY_TIME_CONSTANT_SECS`'s doc comment.
const MAX_EDIT_DENSITY_COUNTED: f32 = 8.0;

/// Part A #2 / Part C (navigation-density, feeding `DeveloperIntent::
/// Exploring`): base contribution once the decayed sum
/// [`RecentActions::navigation_events`] reaches
/// [`MIN_NAVIGATION_DENSITY_FOR_EXPLORING`] *and* edit activity in the same
/// window is low (see the `<= MAX_EDIT_DENSITY_FOR_LOW_EDIT_ACTIVITY` check
/// in `classify`) — dense navigation with real editing alongside it is just
/// Implementing, not Exploring, so this signal only fires for the
/// genuinely edit-sparse case.
const WEIGHT_NAVIGATION_DENSITY: f32 = 0.45;
/// Bonus per unit of decayed density beyond
/// [`MIN_NAVIGATION_DENSITY_FOR_EXPLORING`], up to
/// [`MAX_NAVIGATION_DENSITY_COUNTED`].
const WEIGHT_NAVIGATION_DENSITY_PER_EXTRA: f32 = 0.05;
/// Roughly matches the motivating log's own example (four consecutive
/// selection-changed events with zero edits, landing within a couple of
/// seconds of each other decays to a sum a little under that raw count —
/// see `classify`'s doc comment) — deliberately requiring more than one or
/// two isolated navigations.
const MIN_NAVIGATION_DENSITY_FOR_EXPLORING: f32 = 2.0;
const MAX_NAVIGATION_DENSITY_COUNTED: f32 = 8.0;
/// How much decayed edit-density is still consistent with "low or no edit
/// activity" for Exploring's purposes — a single incidental fresh edit
/// (contributing ~1.0) while mostly navigating shouldn't disqualify the
/// signal, but real, ongoing editing should.
const MAX_EDIT_DENSITY_FOR_LOW_EDIT_ACTIVITY: f32 = 1.5;
/// Exploring's own small "still engaged, just not editing" bonus — same
/// magnitude and reasoning as [`WEIGHT_CONTINUOUS_ACTIVITY`], mirrored here
/// since Exploring is a distinct "no long pause" state from Implementing.
const WEIGHT_EXPLORING_CONTINUOUS: f32 = 0.10;

/// Part B (Testing): a test command actually running right now is close to
/// unambiguous ground truth (the single largest weight in this file,
/// alongside [`WEIGHT_CURSOR_AT_DIAGNOSTIC`]'s "close to unambiguous"
/// framing) — see `classify`'s doc comment for the boundary with
/// `DeveloperIntent::Debugging`'s existing failed-test-run signal.
const WEIGHT_TEST_RUNNING: f32 = 0.55;
/// Lower than [`WEIGHT_TEST_RUNNING`]: a test that already finished is
/// clearly recent testing activity, but a little less "this is happening
/// right now" than one still in flight. Not dramatically lower, though —
/// `WEIGHT_CONTINUOUS_ACTIVITY`/`WEIGHT_EXPLORING_CONTINUOUS` both apply
/// unconditionally whenever `pause_seconds < 10` (regardless of *what*
/// activity, matching their own existing, independent reasoning), so this
/// needs enough margin to stay decisive against that baseline rather than
/// only against zero.
const WEIGHT_TEST_RECENTLY_PASSED: f32 = 0.45;

/// Part D (Configuring): a single flat weight, not a base-plus-per-extra
/// scale like the count-driven signals above — file identity is a yes/no
/// fact, not something that gets *more* true with repetition. Set high
/// because, like [`WEIGHT_CURSOR_AT_DIAGNOSTIC`] and [`WEIGHT_TEST_RUNNING`],
/// "actively editing a file recognized as configuration" is close to
/// unambiguous once it's true at all.
const WEIGHT_CONFIG_FILE_EDIT: f32 = 0.55;

/// File extensions recognized as configuration for
/// [`is_recognized_config_file`] — deliberately small and easy to extend
/// rather than exhaustive, per Part D's own scope.
const CONFIG_FILE_EXTENSIONS: &[&str] = &["json", "toml", "yaml", "yml", "ini"];
/// Exact file names (matched case-insensitively) recognized as
/// configuration even though their extension alone wouldn't be enough to
/// tell (or, for dotfiles like `.env`, have no conventional extension at
/// all — see [`is_recognized_config_file`]'s handling of the `.env` family).
const CONFIG_FILE_NAMES: &[&str] = &[
    "cargo.toml",
    "package.json",
    "tasks.json",
    "settings.json",
    "keymap.json",
    ".editorconfig",
];

/// Part D: whether `path`'s file name is recognized as a configuration
/// file — by exact name ([`CONFIG_FILE_NAMES`]), by extension
/// ([`CONFIG_FILE_EXTENSIONS`]), or as a `.env`-family dotfile (`.env`,
/// `.env.local`, `.env.production`, ...), which Rust's own `Path::extension`
/// can't recognize as having an "extension" at all (a leading-dot name with
/// no further dot, or one more dot, is treated as an extensionless hidden
/// file, not `name.ext`).
fn is_recognized_config_file(path: &std::path::Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_name_lower = file_name.to_ascii_lowercase();
    if CONFIG_FILE_NAMES.contains(&file_name_lower.as_str()) || file_name_lower.starts_with(".env")
    {
        return true;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| CONFIG_FILE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// A pause within the "might be investigating" window `[30s, 300s]`
/// contributes *less* the longer it runs: a pause that just crossed 30s
/// reads as "stopped to look at something," one approaching 300s reads as
/// "probably stepped away." A flat weight across the whole range (the prior
/// version of this function) couldn't tell those apart, which combined with
/// a stale-but-still-in-window signal (e.g. a failed test run from minutes
/// ago) to hold Debugging artificially high the entire time someone sat
/// idle — bug A in the session this rewrite is for.
fn pause_investigating_weight(pause_seconds: u64) -> f32 {
    if !(PAUSE_INVESTIGATING_MIN_SECS..=PAUSE_INVESTIGATING_MAX_SECS).contains(&pause_seconds) {
        return 0.0;
    }
    let span = (PAUSE_INVESTIGATING_MAX_SECS - PAUSE_INVESTIGATING_MIN_SECS) as f32;
    let position = (pause_seconds - PAUSE_INVESTIGATING_MIN_SECS) as f32 / span;
    WEIGHT_PAUSE_INVESTIGATING * (1.0 - position)
}

/// Idle's own recency-driven floor, independent of what Debugging/
/// Implementing scored: 0 right after activity, ramping to 1.0 once the
/// pause reaches [`PAUSE_INVESTIGATING_MAX_SECS`] (the same point past which
/// a pause no longer reads as active investigation either). This makes Idle
/// a real third state grounded in "how long has it actually been," not
/// purely leftover mass — see [`classify`]'s doc comment.
///
/// Cross-checked (classifier-expansion session, Part E — validation only,
/// curve unchanged this session) against two industry defaults: WakaTime
/// ends an activity duration after a 2-minute (120s) gap with no file
/// change; ActivityWatch's default AFK threshold is 3 minutes (180s) of no
/// keyboard/mouse input. Both are hard step functions at their threshold,
/// unlike this linear ramp to 300s — a deliberate difference (this file's
/// own history: a hard cutoff is exactly the "Bug A" cliff shape that
/// `pause_investigating_weight`/this function were introduced to avoid),
/// but it does mean this curve is still assigning meaningful non-Idle
/// weight (`idle_activity_floor(150) ≈ 0.5`) at a pause length both
/// reference tools would already have fully committed to "away." Worth a
/// possible follow-up (shortening `PAUSE_INVESTIGATING_MAX_SECS`, or just
/// the floor's own ramp length) — not changed this session, since it wasn't
/// this session's scope and the ramp shape itself already has a specific,
/// documented reason for existing.
fn idle_activity_floor(pause_seconds: u64) -> f32 {
    (pause_seconds as f32 / PAUSE_INVESTIGATING_MAX_SECS as f32).clamp(0.0, 1.0)
}

/// Weighted rule-based scoring for [`DeveloperIntent::Debugging`],
/// [`DeveloperIntent::Implementing`], [`DeveloperIntent::Testing`],
/// [`DeveloperIntent::Exploring`], and [`DeveloperIntent::Configuring`].
/// The remaining four (Refactoring, Reviewing, Documenting, Planning) are
/// still stubs (score 0). [`DeveloperIntent::Idle`] has no behavioral rules
/// of its own either but *is* a real state in the scoring math below (not
/// purely leftover mass) — per the phase-4 "start narrow" plan, later
/// extended by the classifier-expansion session (see "Parts A-D" below).
///
/// ## History (why this looks the way it does)
///
/// Each intent accumulates its own score independently by summing every
/// applicable signal's weight (bounded contributions in roughly `[0, 1]`,
/// not unbounded points), clamped to `[0, 1]`. An earlier version of this
/// function normalized by dividing each score by `debugging_score +
/// implementing_score` alone, which is degenerate whenever only one
/// category has any nonzero score (the common case): that always yields a
/// hard 1.0/0.0 split regardless of how weak the nonzero score is. Fixed by
/// giving Idle a real score and normalizing all three together.
///
/// That fix immediately surfaced two further bugs, both from the same root
/// cause — treating "how much is currently happening" as *only* derivable
/// from the two behavioral scores, with no independent sense of time or
/// recurrence:
///
/// - **Bug A**: a long-ish pause combined with a signal that's stale but
///   still sitting inside the rolling window (e.g. a failed test run from
///   four minutes ago) held Debugging artificially high for the entire
///   pause, because [`WEIGHT_PAUSE_INVESTIGATING`] applied at full strength
///   anywhere in `[30s, 300s]` regardless of *where* in that range — a 31s
///   pause and a 299s pause scored identically. Fixed by
///   [`pause_investigating_weight`] decaying across the range, and by
///   [`idle_activity_floor`] giving Idle its own pause-driven score instead
///   of relying solely on leftover mass to eventually catch up.
/// - **Bug B**: a real, sustained burst of large edits (confirmed from
///   `~/.nucleus/logs/2026-07-29.jsonl`, 12:09:02-12:09:22: repeated
///   200-254 char edits, no gaps) still resolved to Idle the entire time.
///   `last_edit_magnitude` only ever reflects the single most recent edit,
///   so a burst scored no differently from one isolated large edit — not a
///   staleness problem (the data was fresh every time), but a *failure to
///   let recurrence accumulate*. A single large edit's fixed
///   [`WEIGHT_LARGE_EDIT`] + [`WEIGHT_CONTINUOUS_ACTIVITY`] (0.35 combined)
///   simply isn't enough to outscore Idle's leftover mass whenever any
///   Debugging-flavored signal is present too (e.g. pre-existing diagnostics
///   errors, extremely common while mid-edit). Fixed with a new
///   `RecentActions::large_edits` count over a short `ACTIVITY_BURST_WINDOW`
///   (distinct from the 5-minute `RECENT_WINDOW`): the first large edit
///   earns only the existing flat weight (numerically identical to before),
///   but each *additional* recurrence within the burst window adds
///   [`WEIGHT_EDIT_BURST_EACH_EXTRA`] on top, letting sustained activity
///   build real confidence the way a single edit correctly should not.
///
/// ## Part B and Part C (feedback-telemetry session)
///
/// [`DeveloperIntent::ConsultingAgent`] (`state.agent_active`) is evaluated
/// *first* and short-circuits everything below when true: unlike every
/// other signal here, it isn't inferred from circumstantial evidence — it's
/// a direct, structural read of GPUI's own focus state (see
/// `NucleusEngine::compute_agent_active`). Blending it into the weighted
/// scoring the way Debugging/Implementing signals are blended would let a
/// stale, unrelated signal (e.g. a failed test run from minutes ago) dilute
/// ground truth, which defeats the point of it being ground truth.
///
/// `state.focused_pane` and `state.cursor_at_diagnostic` (Part C), by
/// contrast, *are* additional weighted contributions within the existing
/// Debugging/Implementing scoring below — deliberately small priors
/// ([`WEIGHT_TERMINAL_FOCUS`]/[`WEIGHT_EDITOR_FOCUS`]) or a sharper
/// correlation signal ([`WEIGHT_CURSOR_AT_DIAGNOSTIC`]), not gates.
///
/// ## Parts A-D (classifier-expansion session)
///
/// Motivating bug, confirmed from a real logged trace: continuous, gapless
/// editing (edits firing every 1-3s, no pause) still classified Idle
/// repeatedly. "Active work" was driven almost entirely by *individual*
/// edit size (`last_edit_magnitude` >= `LARGE_EDIT_THRESHOLD`), but a
/// normal editing session is mostly small ~1-char edits punctuated by
/// occasional larger ones — each small edit correctly scores little alone
/// (that was Bug B's own fix, above), but nothing captured that *many*
/// small edits with no gaps is itself real activity.
///
/// **Part A** adds two recency-and-frequency-*decayed* signals to
/// `RecentActions`, independent of each other and of `large_edits`:
/// `edit_events` (any-size edits — see `WEIGHT_EDIT_DENSITY`, this
/// session's actual fix) and `navigation_events` (selection-changed
/// events, feeding Part C). Deliberately *not* a hard rolling-window count
/// (an earlier revision of this session used one, over `ACTIVITY_BURST_
/// WINDOW`) — a hard window has an artificial cliff at its boundary (an
/// edit at t-29s counting fully, one at t-31s counting zero, despite
/// nothing meaningfully different about them), so instead each event
/// contributes an exponentially-decaying weight via `decayed_density`
/// (Mylyn Degree-of-Interest-inspired; see `DENSITY_DECAY_TIME_CONSTANT_
/// SECS`'s doc comment for the actual decay curve and reasoning), summed
/// across recent events. `edit_events` is a *continuous relative* of
/// `large_edits` (every large edit contributes to both, alongside every
/// small one), so a burst of large edits scores both
/// `WEIGHT_LARGE_EDIT`/`WEIGHT_EDIT_BURST_EACH_EXTRA` *and*
/// `WEIGHT_EDIT_DENSITY` — deliberately not treated as double-counting,
/// since frequency and size are genuinely different evidence for the same
/// underlying real activity, and a burst of purely *small* edits (no large
/// ones at all) still needs to be able to score on density alone.
/// `classify` itself never computes the decay — it only ever sees the
/// already-decayed `f32` `NucleusEngine::refresh` hands it, which is what
/// keeps `classify` a pure, deterministic function of its `SessionState`
/// argument rather than one that implicitly depends on wall-clock time.
///
/// **Part B** (`DeveloperIntent::Testing`) is driven entirely by
/// `state.test_command_running`/`test_command_recently_passed`, which come
/// from `terminal_watcher`'s plain-terminal command categorization (Phase
/// 4b-1) — structurally separate from `RecentActions::
/// {test_runs,failed_test_runs}`, which come from Zed's task runner only.
/// This separation is what keeps Testing from fighting
/// `WEIGHT_FAILED_TEST_RUNS` over the same evidence: a *failed* task-runner
/// test continues to contribute to Debugging exactly as before (Testing's
/// signals are both false/absent in that case, by construction, not by a
/// runtime check), while a plain-terminal test that's running or just
/// passed contributes to Testing, entirely independent of whatever the
/// task runner separately reports.
///
/// **Part C** (`DeveloperIntent::Exploring`) scores Part A's decayed
/// `navigation_events`, but only when decayed `edit_events` in the same
/// window is low/absent (`MAX_EDIT_DENSITY_FOR_LOW_EDIT_ACTIVITY`) — dense
/// navigation *with* real editing alongside it is just Implementing, not a
/// distinct browsing/reading pattern.
///
/// **Part D** (`DeveloperIntent::Configuring`) is file-identity driven, not
/// behavioral: fires when `edit_events > 0.0` (any recent edit activity —
/// reusing Part A's decayed signal rather than the never-expiring
/// `last_edit_magnitude`, so this naturally stops firing once editing
/// actually stops for a while, not just whenever a config file happens to
/// be open) *and* the currently-active file (`state.active_files.first()`)
/// is recognized as configuration (see `is_recognized_config_file`) — a
/// file merely being open isn't enough; it has to actually be getting
/// edited.
///
/// **Part E** (validation only, no code here): three prior-art cross-checks
/// against WakaTime/ActivityWatch/HCI-interruptibility research — see the
/// session report and `docs/PHASE6_INTERRUPTION_NOTES.md`. None of them
/// changed anything in this function; they're recorded there, not here.
///
/// ## Normalization, extended
///
/// `idle_score` and `total` now sum all six weighted categories (Debugging,
/// Implementing, Testing, Exploring, Configuring, and Idle's own score),
/// not just the original three — required so the three new categories'
/// probabilities are genuine fractions of the same whole rather than
/// existing outside the distribution entirely. Winner selection iterates a
/// fixed priority order and keeps the *first* strictly-non-losing category
/// on ties, reproducing the original three's exact tie-break behavior
/// ("ties favor Debugging, then Implementing, over Idle") while giving the
/// three new categories a deliberate slot each — see the priority-order
/// comment at the winner-selection code below for the specific ranking and
/// why. For every pre-existing test scenario (none of which set any of the
/// three new fields), `testing_score`/`exploring_score`/`configuring_score`
/// are all exactly `0.0`, so `total` and every existing probability are
/// numerically identical to before this section existed.
fn classify(state: &SessionState, last_edit_magnitude: Option<usize>) -> IntentPrediction {
    if state.agent_active {
        let probabilities = DeveloperIntent::ALL
            .into_iter()
            .map(|intent| {
                let probability = if intent == DeveloperIntent::ConsultingAgent {
                    1.0
                } else {
                    0.0
                };
                (intent, probability)
            })
            .collect();
        return IntentPrediction {
            prediction_id: PredictionId::new(),
            probabilities,
            intent: DeveloperIntent::ConsultingAgent,
            confidence: 1.0,
            evidence: vec!["Agent thread active".to_string()],
        };
    }

    let actions = &state.recent_actions;

    let mut debugging_score = 0.0f32;
    let mut debugging_evidence = Vec::new();
    let mut implementing_score = 0.0f32;
    let mut implementing_evidence = Vec::new();

    // --- Debugging signals ---
    if actions.failed_test_runs > 0 {
        debugging_score += WEIGHT_FAILED_TEST_RUNS
            + WEIGHT_FAILED_TEST_RUNS_PER_EXTRA * (actions.failed_test_runs.min(5) - 1) as f32;
        if actions.failed_test_runs == actions.test_runs && actions.test_runs > 1 {
            debugging_evidence.push(format!(
                "{} consecutive failed test/task run(s), no passes in the current window",
                actions.failed_test_runs
            ));
        } else {
            debugging_evidence.push(format!(
                "{} failed test/task run(s) in the last {} minutes",
                actions.failed_test_runs,
                RECENT_WINDOW.as_secs() / 60
            ));
        }
    }
    if state.diagnostics.errors > 0 {
        debugging_score += WEIGHT_DIAGNOSTIC_ERRORS
            + WEIGHT_DIAGNOSTIC_ERRORS_PER_EXTRA * (state.diagnostics.errors.min(5) - 1) as f32;
        debugging_evidence.push(format!(
            "{} error diagnostic(s) currently open",
            state.diagnostics.errors
        ));
    }
    if actions.file_switches == 0 && actions.saves >= 2 {
        debugging_score += WEIGHT_ITERATING_SAME_FILE;
        debugging_evidence.push(format!(
            "{} saves to the same file with no file switches (iterating on a fix)",
            actions.saves
        ));
    }
    if let Some(magnitude) = last_edit_magnitude
        && magnitude < 10
    {
        debugging_score += WEIGHT_SMALL_EDIT;
        debugging_evidence.push(format!(
            "small, localized edit (~{magnitude} chars changed)"
        ));
    }
    let pause_weight = pause_investigating_weight(state.pause_seconds);
    if pause_weight > 0.0 {
        debugging_score += pause_weight;
        debugging_evidence.push(format!(
            "{}s pause before resuming activity (investigating)",
            state.pause_seconds
        ));
    }
    if state.cursor_at_diagnostic {
        debugging_score += WEIGHT_CURSOR_AT_DIAGNOSTIC;
        debugging_evidence.push(format!(
            "cursor within {DIAGNOSTIC_LOCATION_LINE_WINDOW} line(s) of an active error"
        ));
    }
    if state.focused_pane == FocusedPane::Terminal {
        debugging_score += WEIGHT_TERMINAL_FOCUS;
        debugging_evidence.push("terminal currently focused".to_string());
    }
    debugging_score = debugging_score.min(1.0);

    // --- Implementing signals ---
    if state.diagnostics.errors == 0 && actions.saves >= 1 {
        implementing_score += WEIGHT_CLEAN_SAVE;
        implementing_evidence.push(format!(
            "{} clean save(s) with no open errors",
            actions.saves
        ));
    }
    if actions.file_switches >= 2 {
        implementing_score += WEIGHT_FILE_SWITCH_EACH * actions.file_switches.min(5) as f32;
        implementing_evidence.push(format!(
            "{} file switches in the last {} minutes (spreading work across files)",
            actions.file_switches,
            RECENT_WINDOW.as_secs() / 60
        ));
    }
    if let Some(magnitude) = last_edit_magnitude
        && magnitude >= LARGE_EDIT_THRESHOLD
    {
        implementing_score += WEIGHT_LARGE_EDIT;
        implementing_evidence.push(format!(
            "large edit (~{magnitude} chars changed) suggesting new code being written"
        ));
        let recurring = actions.large_edits.min(MAX_BURST_EDITS_COUNTED);
        if recurring >= 2 {
            implementing_score += WEIGHT_EDIT_BURST_EACH_EXTRA * (recurring - 1) as f32;
            implementing_evidence.push(format!(
                "{recurring} large edits in the last {}s (sustained burst)",
                ACTIVITY_BURST_WINDOW.as_secs()
            ));
        }
    }
    // Part A #1: edit-density — independent of `large_edits` above (which
    // only counts edits >= LARGE_EDIT_THRESHOLD). A dense, recent run of
    // small edits earns this on its own merits; a dense run of *large*
    // edits earns both this and the burst bonus above, since they're
    // genuinely different evidence (frequency vs. size) for the same real
    // activity, not a double-count of one fact under two names — see
    // `classify`'s doc comment. `edit_events` is already a recency-decayed
    // sum (not a raw count) by the time it reaches here.
    if actions.edit_events >= MIN_EDIT_DENSITY_FOR_SIGNAL {
        let counted = actions.edit_events.min(MAX_EDIT_DENSITY_COUNTED);
        implementing_score +=
            WEIGHT_EDIT_DENSITY + WEIGHT_EDIT_DENSITY_PER_EXTRA * (counted - MIN_EDIT_DENSITY_FOR_SIGNAL);
        implementing_evidence.push(format!(
            "frequent recent edits (recency-weighted density {:.1}, decaying over ~{:.0}s)",
            actions.edit_events, DENSITY_DECAY_TIME_CONSTANT_SECS
        ));
    }
    if actions.test_runs > 0 && actions.failed_test_runs == 0 {
        implementing_score += WEIGHT_PASSING_TESTS;
        implementing_evidence.push(format!("{} passing test/task run(s)", actions.test_runs));
    }
    if state.pause_seconds < 10 {
        implementing_score += WEIGHT_CONTINUOUS_ACTIVITY;
        implementing_evidence.push("continuous editing activity, no long pauses".to_string());
    }
    if state.focused_pane == FocusedPane::Editor {
        implementing_score += WEIGHT_EDITOR_FOCUS;
        implementing_evidence.push("editor currently focused".to_string());
    }
    implementing_score = implementing_score.min(1.0);

    // --- Testing signals (Part B) ---
    // Driven entirely by `terminal_watcher`'s plain-terminal command
    // categorization — structurally separate from `actions.failed_test_runs`
    // above (which comes from Zed's task runner only), so a currently-
    // running/just-passed test here can never dilute or be diluted by an
    // unrelated failed *task* elsewhere. See `classify`'s doc comment for
    // the full boundary reasoning.
    let mut testing_score = 0.0f32;
    let mut testing_evidence = Vec::new();
    if state.test_command_running {
        testing_score += WEIGHT_TEST_RUNNING;
        testing_evidence.push("a test command is currently running in the terminal".to_string());
    }
    if state.test_command_recently_passed {
        testing_score += WEIGHT_TEST_RECENTLY_PASSED;
        testing_evidence.push(format!(
            "a test command passed in the terminal within the last {}s",
            ACTIVITY_BURST_WINDOW.as_secs()
        ));
    }
    testing_score = testing_score.min(1.0);

    // --- Exploring signals (Part C) ---
    // Navigation-density, gated on edit activity in the same window being
    // low/absent — dense navigation *with* real editing alongside it is
    // just Implementing, not a distinct browsing/reading pattern.
    let mut exploring_score = 0.0f32;
    let mut exploring_evidence = Vec::new();
    if actions.navigation_events >= MIN_NAVIGATION_DENSITY_FOR_EXPLORING
        && actions.edit_events <= MAX_EDIT_DENSITY_FOR_LOW_EDIT_ACTIVITY
    {
        let counted = actions.navigation_events.min(MAX_NAVIGATION_DENSITY_COUNTED);
        exploring_score += WEIGHT_NAVIGATION_DENSITY
            + WEIGHT_NAVIGATION_DENSITY_PER_EXTRA * (counted - MIN_NAVIGATION_DENSITY_FOR_EXPLORING);
        exploring_evidence.push(format!(
            "frequent recent navigation with little/no editing (recency-weighted density {:.1}, \
            browsing/reading)",
            actions.navigation_events
        ));
    }
    if state.pause_seconds < 10 {
        exploring_score += WEIGHT_EXPLORING_CONTINUOUS;
        exploring_evidence.push("actively navigating, no long pauses".to_string());
    }
    exploring_score = exploring_score.min(1.0);

    // --- Configuring signals (Part D) ---
    // File-identity driven, not behavioral: requires *both* recent edit
    // activity (any decayed density > 0, i.e. an edit happened within
    // `DENSITY_RETENTION_WINDOW` — reuses Part A's edit-density signal
    // rather than `last_edit_magnitude`, which never expires) and that the
    // currently-active file is a recognized configuration file, so merely
    // having a config file open without touching it doesn't count.
    let mut configuring_score = 0.0f32;
    let mut configuring_evidence = Vec::new();
    if actions.edit_events > 0.0
        && let Some(active_file) = state.active_files.first()
        && is_recognized_config_file(active_file)
    {
        configuring_score += WEIGHT_CONFIG_FILE_EDIT;
        configuring_evidence.push(format!(
            "editing a recognized configuration file ({})",
            active_file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| active_file.display().to_string())
        ));
    }
    configuring_score = configuring_score.min(1.0);

    let idle_score = (1.0
        - debugging_score
        - implementing_score
        - testing_score
        - exploring_score
        - configuring_score)
        .max(0.0)
        .max(idle_activity_floor(state.pause_seconds));
    let total = debugging_score
        + implementing_score
        + idle_score
        + testing_score
        + exploring_score
        + configuring_score;

    let debugging_probability = debugging_score / total;
    let implementing_probability = implementing_score / total;
    let idle_probability = idle_score / total;
    let testing_probability = testing_score / total;
    let exploring_probability = exploring_score / total;
    let configuring_probability = configuring_score / total;

    let probabilities = DeveloperIntent::ALL
        .into_iter()
        .map(|intent| {
            let probability = match intent {
                DeveloperIntent::Debugging => debugging_probability,
                DeveloperIntent::Implementing => implementing_probability,
                DeveloperIntent::Idle => idle_probability,
                DeveloperIntent::Testing => testing_probability,
                DeveloperIntent::Exploring => exploring_probability,
                DeveloperIntent::Configuring => configuring_probability,
                _ => 0.0,
            };
            (intent, probability)
        })
        .collect();

    // Winner selection across all six categories: iterate in a fixed
    // priority order and only replace the current best on a *strict* win,
    // so earlier (higher-priority) entries keep exact ties — this
    // reproduces the pre-expansion behavior exactly for the original three
    // ("ties favor Debugging, then Implementing, over Idle") while giving
    // the three new categories a deliberate slot rather than an arbitrary
    // one:
    //   1. Testing — a test actually running/just passed is close to
    //      ground truth, the same "close to unambiguous" tier as
    //      `WEIGHT_CURSOR_AT_DIAGNOSTIC` below.
    //   2. Debugging — unchanged top behavioral-inference priority.
    //   3. Configuring — file-identity is also close to unambiguous once
    //      its (edit-gated) condition is met, but ranked below Debugging
    //      since an open error is a sharper signal than "this file has a
    //      config-shaped name."
    //   4. Implementing — unchanged.
    //   5. Exploring — the newest and softest of the added signals
    //      (frequency-based, no size/identity component), so it sits just
    //      above Idle rather than contending with the more decisive
    //      categories above it.
    //   6. Idle — unchanged lowest-priority fallback.
    let candidates = [
        (DeveloperIntent::Testing, testing_probability, testing_evidence),
        (DeveloperIntent::Debugging, debugging_probability, debugging_evidence),
        (
            DeveloperIntent::Configuring,
            configuring_probability,
            configuring_evidence,
        ),
        (
            DeveloperIntent::Implementing,
            implementing_probability,
            implementing_evidence,
        ),
        (DeveloperIntent::Exploring, exploring_probability, exploring_evidence),
    ];

    let mut winner: Option<(DeveloperIntent, f32, Vec<String>)> = None;
    let mut all_evidence = Vec::new();
    for (intent, probability, evidence) in candidates {
        if !evidence.is_empty() {
            all_evidence.push((intent, evidence.clone()));
        }
        let is_better = match &winner {
            Some((_, best_probability, _)) => probability > *best_probability,
            None => true,
        };
        if is_better {
            winner = Some((intent, probability, evidence));
        }
    }
    let (best_intent, best_probability, best_evidence) =
        winner.expect("candidates is non-empty, so winner is always set");

    let (intent, confidence, evidence) = if best_probability >= idle_probability {
        (best_intent, best_probability, best_evidence)
    } else {
        let evidence = if all_evidence.iter().all(|(_, lines)| lines.is_empty()) {
            vec!["No recent edits, test runs, navigation, or diagnostics activity".to_string()]
        } else {
            all_evidence
                .into_iter()
                .flat_map(|(_, lines)| lines)
                .map(|line| format!("{line} (not enough alone to indicate active work)"))
                .collect()
        };
        (DeveloperIntent::Idle, idle_probability, evidence)
    };

    IntentPrediction {
        prediction_id: PredictionId::new(),
        probabilities,
        intent,
        confidence,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// Three tests below write to and read back the real
    /// `~/.nucleus/logs/{today}.jsonl` file (deliberately, not a mock — see
    /// each test's doc comment). Each spins up its own `NucleusLogger`
    /// instance with its own independent `File` handle onto that same path;
    /// since `cargo test` runs `#[test]`s in parallel by default, without
    /// serialization their `writeln!` calls can interleave mid-line and
    /// corrupt the file (observed as "trailing characters" JSON parse
    /// failures). Production code never has this problem — a real
    /// `NucleusEngine` only ever owns one `NucleusLogger`. Poisoning is
    /// recovered from rather than propagated, so one test's panic while
    /// holding the lock doesn't spuriously fail the others.
    static REAL_LOG_FILE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock_real_log_file_tests() -> std::sync::MutexGuard<'static, ()> {
        REAL_LOG_FILE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ---- Required test suite: SessionState -> IntentPrediction, derived
    // from real logged data (`~/.nucleus/logs/2026-07-29.jsonl`) wherever a
    // concrete real example existed. `classify` takes a plain `&SessionState`
    // and returns a plain `IntentPrediction` with no GPUI/Entity/Context
    // involved, so every case here calls it directly. ----

    /// Required test 1 (the confirmed false positive, and the single most
    /// important test in this suite): the exact `SessionState` shape behind
    /// two real logged false positives — a ~35s pause after editing
    /// `main.py`, zero failed test runs, zero diagnostics errors, no other
    /// signal. Before the classifier fix, `classify` reported this as
    /// Debugging at 1.0 confidence.
    #[test]
    fn test_pause_alone_lands_on_idle_at_low_debugging_confidence() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions {
                test_runs: 0,
                failed_test_runs: 0,
                saves: 0,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary {
                errors: 0,
                warnings: 0,
            },
            current_symbol: None,
            pause_seconds: 38,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, None);
        println!(
            "pause-only: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        assert_eq!(
            prediction.intent,
            DeveloperIntent::Idle,
            "a lone ~35s pause with zero other signal should not win as Debugging"
        );
        let debugging_probability = prediction
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
            .unwrap()
            .1;
        assert!(
            debugging_probability < 0.3,
            "expected low Debugging confidence from a lone pause, got {debugging_probability}"
        );
        assert!(
            (debugging_probability - 1.0).abs() > f32::EPSILON,
            "must not reproduce the old degenerate 1.0 confidence, got {debugging_probability}"
        );
    }

    /// Required test 2 (true Debugging): `active_files` mirrors a real
    /// logged session (a Python project with `test_calculator.py` alongside
    /// `calculator.py`/`formatter.py`/`main.py` — genuinely both a test file
    /// and source files active at once), with `failed_test_runs: 3` (the
    /// real log's own genuine >= 2 threshold case) plus a co-occurring
    /// pause and open diagnostics error — the realistic shape of "just hit
    /// a failing test, now looking at why," not an isolated single signal
    /// (that's required test 6, which is deliberately the opposite case).
    /// Debugging should win with meaningfully high confidence, and the
    /// evidence should call out the failed runs specifically.
    #[test]
    fn test_multiple_failed_test_runs_is_confident_debugging() {
        let state = SessionState {
            active_files: vec![
                PathBuf::from("test_calculator.py"),
                PathBuf::from("calculator.py"),
                PathBuf::from("formatter.py"),
                PathBuf::from("main.py"),
            ],
            recent_actions: RecentActions {
                test_runs: 3,
                failed_test_runs: 3,
                saves: 0,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary {
                errors: 1,
                warnings: 0,
            },
            current_symbol: Some("test_calculator.py > def test_divide_by_zero".to_string()),
            pause_seconds: 38,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, None);
        println!(
            "multiple-failed-tests: intent={:?} confidence={:.3} probabilities={:?} evidence={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities, prediction.evidence
        );

        assert_eq!(prediction.intent, DeveloperIntent::Debugging);
        assert!(
            prediction.confidence > 0.6,
            "3 failed test runs plus a pause and an open error should be meaningfully \
            confident Debugging, got {}",
            prediction.confidence
        );
        assert!(
            prediction
                .evidence
                .iter()
                .any(|line| line.contains("failed test/task run")),
            "evidence should call out the failed runs specifically, got {:?}",
            prediction.evidence
        );
    }

    /// Required test 3 (true Implementing): continuous active editing (a
    /// large recent edit, a clean save, no long pause), zero test runs,
    /// zero errors — Implementing should win.
    #[test]
    fn test_continuous_editing_resolves_to_implementing() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions {
                test_runs: 0,
                failed_test_runs: 0,
                saves: 1,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary {
                errors: 0,
                warnings: 0,
            },
            current_symbol: Some("main.py".to_string()),
            pause_seconds: 5,
            diff_summary: Some("1 edit(s), +60/-0 chars in main.py".to_string()),
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, Some(60));
        println!(
            "continuous-editing: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        assert_eq!(prediction.intent, DeveloperIntent::Implementing);
    }

    /// Required test 4 (Idle): a 305s pause with zero recent edits and zero
    /// test activity — the exact `pause_seconds` from real logged usage.
    /// 305 is deliberately just past the classifier's 30-300s "investigating
    /// a bug" window (see `WEIGHT_PAUSE_INVESTIGATING`'s range check), so it
    /// contributes no Debugging signal either — a long-enough pause reads as
    /// away-from-keyboard, not mid-fix.
    #[test]
    fn test_long_pause_resolves_to_idle() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions::default(),
            diagnostics: DiagnosticsSummary::default(),
            current_symbol: None,
            pause_seconds: 305,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, None);
        println!(
            "long-pause: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        assert_eq!(prediction.intent, DeveloperIntent::Idle);
    }

    /// Required test 6 (edge: a single failed test run, not >= 2): should
    /// not alone push Debugging to high confidence — distinguishing this
    /// from required test 2, which combines multiple failed runs with a
    /// co-occurring signal.
    #[test]
    fn test_single_failed_test_run_alone_is_not_confident_debugging() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions {
                test_runs: 1,
                failed_test_runs: 1,
                saves: 0,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary::default(),
            current_symbol: None,
            pause_seconds: 0,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, None);
        println!(
            "single-failed-test: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        let debugging_probability = prediction
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
            .unwrap()
            .1;
        assert!(
            debugging_probability < 0.5,
            "a single failed test run alone should not reach high Debugging confidence, \
            got {debugging_probability}"
        );
    }

    /// Required test 7 (edge: zero signal anywhere, a fresh session with no
    /// activity yet): must not crash and must resolve to something sane —
    /// Idle or all-near-zero, not a spuriously confident result.
    #[test]
    fn test_zero_signal_fresh_session_resolves_sanely() {
        let state = SessionState::default();

        let prediction = classify(&state, None);
        println!(
            "fresh-session: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        assert_eq!(prediction.intent, DeveloperIntent::Idle);
        assert!(
            prediction.confidence > 0.5,
            "a fresh, empty session should resolve confidently to Idle, not a spurious \
            near-tie, got {}",
            prediction.confidence
        );
        let debugging_probability = prediction
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
            .unwrap()
            .1;
        let implementing_probability = prediction
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Implementing)
            .unwrap()
            .1;
        assert!(
            debugging_probability < 0.2 && implementing_probability < 0.2,
            "no activity should not produce a spuriously confident Debugging/Implementing \
            reading: debugging={debugging_probability}, implementing={implementing_probability}"
        );
    }

    /// Regression test for Bug A (holistic-review session): a realistic
    /// "sitting idle, not touching the app" state — a failed test run from
    /// a while ago that's still technically inside the rolling window, plus
    /// a pause that's grown well past the point of active investigation —
    /// must resolve to Idle, not Debugging. Before this fix,
    /// `WEIGHT_PAUSE_INVESTIGATING` applied at full strength anywhere in
    /// `[30s, 300s]`, so a 250s pause scored identically to a 31s one; a
    /// single stale failed-test-run signal combined with that flat weight
    /// was enough to tie or beat Idle's leftover mass regardless of how
    /// long the pause had actually run.
    #[test]
    fn test_stale_signal_with_long_pause_resolves_to_idle() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions {
                test_runs: 1,
                failed_test_runs: 1,
                saves: 0,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary::default(),
            current_symbol: None,
            pause_seconds: 250,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, None);
        println!(
            "stale-signal-long-pause: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        assert_eq!(
            prediction.intent,
            DeveloperIntent::Idle,
            "a stale failed-test-run signal plus a pause that's grown to 250s should read as \
            idle, not still-confident Debugging"
        );
        let debugging_probability = prediction
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
            .unwrap()
            .1;
        assert!(
            debugging_probability < 0.5,
            "Debugging confidence should have decayed well below a tie by this point in the \
            pause, got {debugging_probability}"
        );
    }

    /// Regression test for Bug B (holistic-review session), using the real
    /// burst shape from `~/.nucleus/logs/2026-07-29.jsonl` 12:09:02-12:09:22
    /// (200-254 char edits recurring with no gaps, tens of diagnostics
    /// errors open throughout — real numbers, not invented): a sustained
    /// burst of large edits must resolve to Implementing, not Idle. Before
    /// this fix, `last_edit_magnitude` only ever reflected the single most
    /// recent edit, so a 20-second burst scored no differently from one
    /// isolated large edit — never enough to outscore Idle's leftover mass
    /// once the (very ordinary, mid-edit) open diagnostics errors also
    /// contributed to Debugging.
    #[test]
    fn test_sustained_edit_burst_resolves_to_implementing() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions {
                test_runs: 0,
                failed_test_runs: 0,
                saves: 0,
                file_switches: 0,
                large_edits: 3,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary {
                errors: 16,
                warnings: 0,
            },
            current_symbol: Some("main.py".to_string()),
            pause_seconds: 2,
            diff_summary: Some("1 edit(s), +0/-220 chars in main.py".to_string()),
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, Some(220));
        println!(
            "sustained-edit-burst: intent={:?} confidence={:.3} probabilities={:?} \
            evidence={:?}",
            prediction.intent,
            prediction.confidence,
            prediction.probabilities,
            prediction.evidence
        );

        assert_eq!(
            prediction.intent,
            DeveloperIntent::Implementing,
            "a sustained burst of large edits (with ordinary mid-edit diagnostics errors) \
            should resolve to Implementing, not Idle"
        );
        assert!(
            prediction.confidence > 0.5,
            "a real, ongoing edit burst should be meaningfully confident, got {}",
            prediction.confidence
        );
        assert!(
            prediction
                .evidence
                .iter()
                .any(|line| line.contains("sustained burst")),
            "evidence should call out the recurring burst specifically, got {:?}",
            prediction.evidence
        );
    }

    /// Required test 5 (blended/ambiguous case — directly re-tests the
    /// second issue from the classifier-fix session: probabilities
    /// collapsing to a single 1.0/0.0 winner). One recent failed test run
    /// (a real Debugging signal) co-occurring with continuous active
    /// editing right now (a large recent edit, no pause — a real
    /// Implementing signal) should produce genuine three-way spread across
    /// Debugging/Implementing/Idle, not a hard split. If this fails, the
    /// blending fix from the prior session is not actually working
    /// regardless of what that session's report claimed.
    #[test]
    fn test_probabilities_can_genuinely_blend() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py"), PathBuf::from("utils.py")],
            recent_actions: RecentActions {
                test_runs: 1,
                failed_test_runs: 1,
                saves: 0,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary::default(),
            current_symbol: None,
            pause_seconds: 5,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, Some(80));
        println!(
            "blended: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        let probability_of = |intent: DeveloperIntent| {
            prediction
                .probabilities
                .iter()
                .find(|(candidate, _)| *candidate == intent)
                .unwrap()
                .1
        };
        let debugging_probability = probability_of(DeveloperIntent::Debugging);
        let implementing_probability = probability_of(DeveloperIntent::Implementing);
        assert!(debugging_probability > 0.0 && implementing_probability > 0.0);
        assert!(
            debugging_probability < 1.0 && implementing_probability < 1.0,
            "neither should be a hard 1.0/0.0 split when both have real signal: \
            debugging={debugging_probability}, implementing={implementing_probability}"
        );
    }

    // ---- Classifier-expansion session: Part A (activity-density),
    // Part B (Testing), Part C (Exploring), Part D (Configuring). ----

    // ---- decayed_density: the Part A decay math itself, independent of
    // classify()'s use of it. ----

    #[test]
    fn test_decayed_density_is_zero_for_no_events() {
        let now = Instant::now();
        assert_eq!(decayed_density(&VecDeque::new(), now), 0.0);
    }

    #[test]
    fn test_decayed_density_full_weight_for_a_just_happened_event() {
        let now = Instant::now();
        let mut timestamps = VecDeque::new();
        timestamps.push_back(now);
        let density = decayed_density(&timestamps, now);
        assert!(
            (density - 1.0).abs() < 1e-6,
            "an event with zero age should contribute ~1.0, got {density}"
        );
    }

    /// The defining property of exponential decay with time constant τ:
    /// after exactly τ seconds, a single event's contribution has dropped
    /// to `1/e` (~0.368) — not to zero (that's what a hard rolling-window
    /// cliff would do at its own boundary) and not still 1.0.
    #[test]
    fn test_decayed_density_drops_to_one_over_e_after_one_time_constant() {
        let now = Instant::now();
        let mut timestamps = VecDeque::new();
        timestamps.push_back(
            now - Duration::from_secs_f32(DENSITY_DECAY_TIME_CONSTANT_SECS),
        );
        let density = decayed_density(&timestamps, now);
        let expected = 1.0 / std::f32::consts::E;
        assert!(
            (density - expected).abs() < 1e-3,
            "expected ~{expected:.3} (1/e) one time constant out, got {density}"
        );
    }

    #[test]
    fn test_decayed_density_sums_contributions_across_multiple_events() {
        let now = Instant::now();
        let mut timestamps = VecDeque::new();
        timestamps.push_back(now);
        timestamps.push_back(now - Duration::from_secs_f32(DENSITY_DECAY_TIME_CONSTANT_SECS));
        let density = decayed_density(&timestamps, now);
        let expected = 1.0 + 1.0 / std::f32::consts::E;
        assert!(
            (density - expected).abs() < 1e-3,
            "expected the sum of each event's own contribution (~{expected:.3}), got {density}"
        );
    }

    /// The actual bug this decay design exists to fix, checked directly on
    /// `decayed_density` rather than through `classify`: two events an
    /// equal, small distance apart in time (one just before, one just after
    /// where the *old* hard-window design's 30s cliff used to sit) must
    /// decay to close values, not jump from "counts fully" to "counts not
    /// at all."
    #[test]
    fn test_decayed_density_has_no_cliff_near_the_old_hard_window_boundary() {
        let now = Instant::now();
        let mut just_inside = VecDeque::new();
        just_inside.push_back(now - Duration::from_secs(29));
        let mut just_outside = VecDeque::new();
        just_outside.push_back(now - Duration::from_secs(31));

        let inside_density = decayed_density(&just_inside, now);
        let outside_density = decayed_density(&just_outside, now);
        assert!(
            (inside_density - outside_density).abs() < 0.02,
            "events 2s apart in time must decay smoothly regardless of where the old \
            hard window boundary used to sit: inside={inside_density}, outside={outside_density}"
        );
        // And both must be meaningfully less than a just-happened event's
        // full 1.0 — decay is real, not a no-op.
        assert!(inside_density < 0.5 && outside_density < 0.5);
    }

    fn session_state_with(
        mutate: impl FnOnce(&mut SessionState),
    ) -> SessionState {
        let mut state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions::default(),
            diagnostics: DiagnosticsSummary::default(),
            current_symbol: None,
            pause_seconds: 1,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::Editor,
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };
        mutate(&mut state);
        state
    }

    /// The motivating bug this session exists to fix, reproduced directly:
    /// a real logged trace showed continuous, gapless editing (edits firing
    /// every 1-3 seconds, no pause) still landing as Idle repeatedly — e.g.
    /// "Idle 42% — small, localized edit (~1 chars changed) (not enough
    /// alone...); cursor within 2 line(s) of an active error (not enough
    /// alone...); continuous editing activity, no long pauses (not enough
    /// alone...)". Root cause: `last_edit_magnitude` only ever reflects the
    /// single most recent edit, and a normal editing session is mostly
    /// small ~1-char edits, so the "large edit" signal never fires and
    /// nothing else captured "many small edits with no gaps is itself real
    /// activity." This models that exact shape — a tiny most-recent edit,
    /// but a decayed edit-density of 6.0 (close to what 8 edits spread
    /// across ~14s at `DENSITY_DECAY_TIME_CONSTANT_SECS`'s scale actually
    /// sum to — see the constant's own doc comment for the arithmetic),
    /// plus the same nearby-error/no-pause/editor-focus signals — and
    /// asserts it now resolves to Implementing, with Idle no longer
    /// anywhere near dominant.
    #[test]
    fn test_dense_small_edits_no_pause_resolves_to_implementing_not_idle() {
        let state = session_state_with(|state| {
            state.recent_actions.edit_events = 6.0;
            state.diagnostics.errors = 2;
            state.cursor_at_diagnostic = true;
            state.pause_seconds = 1;
        });

        let prediction = classify(&state, Some(1));
        println!(
            "dense-small-edits: intent={:?} confidence={:.3} probabilities={:?} evidence={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities, prediction.evidence
        );

        assert_eq!(
            prediction.intent,
            DeveloperIntent::Implementing,
            "dense small edits with no pause must no longer lose to Idle"
        );
        let idle_probability = prediction
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Idle)
            .unwrap()
            .1;
        assert!(
            idle_probability < 0.1,
            "Idle should no longer be anywhere near dominant once edit-density is \
            accounted for, got {idle_probability}"
        );
        assert!(
            prediction
                .evidence
                .iter()
                .any(|line| line.contains("recency-weighted density")),
            "evidence should call out the edit-density signal specifically, got {:?}",
            prediction.evidence
        );
    }

    /// Part A #1, isolated: a single fresh edit — whose decayed contribution
    /// is ~1.0, below `MIN_EDIT_DENSITY_FOR_SIGNAL` (2.0) — must not trigger
    /// the density signal, preserving the existing "a lone small edit alone
    /// is not enough" principle
    /// (`test_pause_alone_lands_on_idle_at_low_debugging_confidence`'s whole
    /// reason for existing) rather than making every edit, however
    /// isolated, look like sustained activity.
    #[test]
    fn test_lone_edit_does_not_trigger_density_signal() {
        let state = session_state_with(|state| {
            state.recent_actions.edit_events = 1.0;
        });

        let prediction = classify(&state, Some(1));
        assert!(
            !prediction
                .evidence
                .iter()
                .any(|line| line.contains("recency-weighted density")),
            "a single edit_events must not trigger the density signal, got {:?}",
            prediction.evidence
        );
    }

    /// Part B: a test command currently running in a plain terminal must
    /// win as Testing, with meaningfully high confidence.
    #[test]
    fn test_running_test_command_resolves_to_testing() {
        let state = session_state_with(|state| {
            state.test_command_running = true;
            state.focused_pane = FocusedPane::Terminal;
        });

        let prediction = classify(&state, None);
        println!(
            "test-running: intent={:?} confidence={:.3} probabilities={:?} evidence={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities, prediction.evidence
        );

        assert_eq!(prediction.intent, DeveloperIntent::Testing);
        assert!(
            prediction.confidence > 0.4,
            "a currently-running test command should be meaningfully confident Testing, \
            got {}",
            prediction.confidence
        );
    }

    /// Part B: a test command that just passed (not currently running)
    /// must also win as Testing, at a somewhat lower confidence than the
    /// in-flight case (see `WEIGHT_TEST_RECENTLY_PASSED` <
    /// `WEIGHT_TEST_RUNNING`).
    #[test]
    fn test_recently_passed_test_command_resolves_to_testing() {
        let state = session_state_with(|state| {
            state.test_command_recently_passed = true;
        });

        let prediction = classify(&state, None);
        println!(
            "test-recently-passed: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        assert_eq!(prediction.intent, DeveloperIntent::Testing);
    }

    /// Part B boundary check: a *failed* test/task run (the existing,
    /// unrelated `RecentActions::failed_test_runs` path, driven by Zed's
    /// task runner) must still resolve to Debugging exactly as before —
    /// Testing's new signals are driven entirely by
    /// `test_command_running`/`test_command_recently_passed`, which are
    /// both false/absent here, so there is no path by which adding Testing
    /// could have regressed this existing behavior. Directly re-runs
    /// `test_multiple_failed_test_runs_is_confident_debugging`'s scenario
    /// through the expanded classifier to confirm that explicitly, not just
    /// by construction.
    #[test]
    fn test_failed_test_runs_still_resolve_to_debugging_not_testing() {
        let state = SessionState {
            active_files: vec![
                PathBuf::from("test_calculator.py"),
                PathBuf::from("calculator.py"),
            ],
            recent_actions: RecentActions {
                test_runs: 3,
                failed_test_runs: 3,
                ..RecentActions::default()
            },
            diagnostics: DiagnosticsSummary {
                errors: 1,
                warnings: 0,
            },
            current_symbol: None,
            pause_seconds: 38,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, None);
        assert_eq!(
            prediction.intent,
            DeveloperIntent::Debugging,
            "a failed test/task run must continue to resolve to Debugging, unaffected by \
            the new Testing intent existing"
        );
    }

    /// Part C: dense navigation (selection-changed events) with no editing
    /// and no long pause must resolve to Exploring — the exact shape from
    /// the motivating log (four consecutive selection-changed events, zero
    /// edits in between).
    #[test]
    fn test_dense_navigation_with_no_edits_resolves_to_exploring() {
        let state = session_state_with(|state| {
            state.recent_actions.navigation_events = 3.6;
            state.recent_actions.edit_events = 0.0;
            state.pause_seconds = 2;
        });

        let prediction = classify(&state, None);
        println!(
            "dense-navigation: intent={:?} confidence={:.3} probabilities={:?} evidence={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities, prediction.evidence
        );

        assert_eq!(prediction.intent, DeveloperIntent::Exploring);
        assert!(
            prediction
                .evidence
                .iter()
                .any(|line| line.contains("recency-weighted density") && line.contains("browsing")),
            "evidence should call out the navigation-density signal specifically, got {:?}",
            prediction.evidence
        );
    }

    /// Part C boundary: dense navigation *with* real editing alongside it
    /// in the same window must not resolve to Exploring — that's just
    /// Implementing, per Part A's own scoping ("specifically when
    /// edit-density is low/absent in that same window").
    #[test]
    fn test_dense_navigation_with_concurrent_editing_is_not_exploring() {
        let state = session_state_with(|state| {
            state.recent_actions.navigation_events = 3.6;
            state.recent_actions.edit_events = 6.0;
        });

        let prediction = classify(&state, Some(1));
        assert_ne!(
            prediction.intent,
            DeveloperIntent::Exploring,
            "navigation density alongside real concurrent editing should not read as \
            Exploring, got {:?}",
            prediction
        );
    }

    /// Part D: editing a recognized configuration file (`Cargo.toml`) must
    /// resolve to Configuring.
    #[test]
    fn test_editing_config_file_resolves_to_configuring() {
        let state = session_state_with(|state| {
            state.active_files = vec![PathBuf::from("Cargo.toml")];
            state.recent_actions.edit_events = 2.0;
        });

        let prediction = classify(&state, Some(5));
        println!(
            "config-file-edit: intent={:?} confidence={:.3} probabilities={:?} evidence={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities, prediction.evidence
        );

        assert_eq!(prediction.intent, DeveloperIntent::Configuring);
        assert!(
            prediction
                .evidence
                .iter()
                .any(|line| line.contains("configuration file")),
            "evidence should call out the config-file signal specifically, got {:?}",
            prediction.evidence
        );
    }

    /// Part D boundary: a recognized config file merely *open* (no recent
    /// edit activity) must not trigger Configuring — the signal requires
    /// actual recent edits, not just the file being active.
    #[test]
    fn test_config_file_open_without_edits_is_not_configuring() {
        let state = session_state_with(|state| {
            state.active_files = vec![PathBuf::from("Cargo.toml")];
            state.recent_actions.edit_events = 0.0;
        });

        let prediction = classify(&state, None);
        assert_ne!(
            prediction.intent,
            DeveloperIntent::Configuring,
            "a config file merely open with no recent edits should not trigger \
            Configuring, got {:?}",
            prediction
        );
    }

    /// Part D boundary: editing a non-config file must not trigger
    /// Configuring, even with real edit-density.
    #[test]
    fn test_editing_non_config_file_is_not_configuring() {
        let state = session_state_with(|state| {
            state.active_files = vec![PathBuf::from("main.py")];
            state.recent_actions.edit_events = 6.0;
        });

        let prediction = classify(&state, Some(5));
        assert_ne!(
            prediction.intent,
            DeveloperIntent::Configuring,
            "editing an ordinary source file should not trigger Configuring, got {:?}",
            prediction
        );
    }

    /// Part D: `.env`-family dotfiles are recognized even though they have
    /// no extension `Path::extension()` can see, and even though `.env`
    /// itself isn't a `CONFIG_FILE_EXTENSIONS` entry.
    #[test]
    fn test_dotenv_family_files_are_recognized_as_config() {
        assert!(is_recognized_config_file(std::path::Path::new(".env")));
        assert!(is_recognized_config_file(std::path::Path::new(
            ".env.local"
        )));
        assert!(is_recognized_config_file(std::path::Path::new(
            ".env.production"
        )));
        assert!(!is_recognized_config_file(std::path::Path::new(
            "main.py"
        )));
    }

    /// Normalization sanity check: with all six weighted categories now
    /// contributing, probabilities must still sum to ~1.0 and stay in
    /// range for a state where several signals from *different* new and
    /// old categories are simultaneously true — not just checked in
    /// isolation per-category above.
    #[test]
    fn test_probabilities_stay_well_formed_with_multiple_new_signals_active() {
        let state = session_state_with(|state| {
            state.recent_actions.edit_events = 5.0;
            state.recent_actions.navigation_events = 3.6;
            state.test_command_recently_passed = true;
            state.active_files = vec![PathBuf::from("Cargo.toml")];
            state.diagnostics.errors = 1;
        });

        let prediction = classify(&state, Some(3));
        println!(
            "multi-signal: intent={:?} confidence={:.3} probabilities={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities
        );

        let sum: f32 = prediction
            .probabilities
            .iter()
            .map(|(_, probability)| probability)
            .sum();
        assert!(
            (sum - 1.0).abs() < 1e-3,
            "probabilities must still sum to ~1.0 with multiple new signals active, \
            got {sum}: {:?}",
            prediction.probabilities
        );
    }

    /// Exercises `NucleusLogger`'s real write path (no mocked filesystem or
    /// injected path) against the actual `~/.nucleus/logs/` directory, to
    /// verify actual JSONL lines land on disk rather than just that the
    /// logging code compiles. Logs enough lines to cross `MAX_QUEUE_LEN` so
    /// the size-triggered immediate flush fires deterministically, without
    /// needing to fast-forward the executor's virtual clock past the
    /// timer-based flush path.
    #[gpui::test]
    async fn test_logger_writes_real_jsonl_lines(cx: &mut TestAppContext) {
        let _guard = lock_real_log_file_tests();
        let logger = NucleusLogger::new(cx.executor());

        let prediction = IntentPrediction {
            prediction_id: PredictionId::new(),
            probabilities: vec![
                (DeveloperIntent::Debugging, 0.8),
                (DeveloperIntent::Implementing, 0.2),
            ],
            intent: DeveloperIntent::Debugging,
            confidence: 0.8,
            evidence: vec![
                "3 consecutive failed test/task run(s), no passes in the current window"
                    .to_string(),
                "2 error diagnostic(s) currently open".to_string(),
            ],
        };
        let session_state = SessionState {
            active_files: vec![PathBuf::from("crates/nucleus/src/nucleus.rs")],
            recent_actions: RecentActions {
                test_runs: 3,
                failed_test_runs: 3,
                saves: 1,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary {
                errors: 2,
                warnings: 0,
            },
            current_symbol: Some("fn poll_task_terminals".to_string()),
            pause_seconds: 12,
            diff_summary: Some("2 edit(s), +10/-2 chars in nucleus.rs".to_string()),
            agent_active: false,
            focused_pane: FocusedPane::default(),
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };
        logger.log_intent_prediction(&prediction, &session_state);
        logger.log_raw_event(&RawEvent::Edit {
            file: Some(PathBuf::from("crates/nucleus/src/nucleus.rs")),
            symbol: Some("fn poll_task_terminals".to_string()),
            inserted_chars: 10,
            deleted_chars: 2,
        });
        logger.log_raw_event(&RawEvent::TaskFailed {
            label: Some("Run tests".to_string()),
        });

        // Cross the logger's internal MAX_QUEUE_LEN (50, private to
        // logging.rs) to trigger the immediate (non-timer) flush path.
        const EXTRA_LINES_TO_FORCE_FLUSH: usize = 50;
        for _ in 0..EXTRA_LINES_TO_FORCE_FLUSH {
            logger.log_raw_event(&RawEvent::SelectionChanged {
                file: Some(PathBuf::from("crates/nucleus/src/nucleus.rs")),
            });
        }

        cx.executor().run_until_parked();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let log_path = log_dir().join(format!("{today}.jsonl"));
        let contents =
            std::fs::read_to_string(&log_path).expect("logger should have written a real file");
        let line_count = contents.lines().count();
        assert!(
            line_count >= 53,
            "expected at least 53 lines (1 prediction + 2 events + {EXTRA_LINES_TO_FORCE_FLUSH} selections), got {line_count}"
        );
        for line in contents.lines() {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("every line must be valid JSON");
            assert!(value.get("type").is_some(), "line missing `type` field");
            assert!(
                value.get("timestamp").is_some(),
                "line missing `timestamp` field"
            );
        }
    }

    /// Reads back genuine, previously-written data from the real
    /// `~/.nucleus/logs/` directory (accumulated across this session and
    /// prior sessions' runs of `test_logger_writes_real_jsonl_lines`) via
    /// the actual `list_log_dates`/`read_log_file`/`parse_log_line`
    /// functions the log viewer panel calls — not a fixture, not a mock.
    /// This is what confirms the read side actually round-trips real
    /// on-disk output, including the `raw_event`-with-flattened-tag shape
    /// that `parse_log_line`'s doc comment flags as the risky part.
    #[test]
    fn test_reads_back_real_logged_data() {
        let _guard = lock_real_log_file_tests();
        let dates = list_log_dates().expect("listing ~/.nucleus/logs/ should succeed");
        assert!(
            !dates.is_empty(),
            "expected at least one *.jsonl file from prior test/session runs in {:?}",
            log_dir()
        );
        println!("found {} log date(s): {:?}", dates.len(), dates);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            dates.contains(&today),
            "today's file should be present given the logger test just wrote to it"
        );

        let entries = read_log_file(&today).expect("reading today's real log file should succeed");
        assert!(
            !entries.is_empty(),
            "expected at least one real parsed entry from today's file"
        );
        assert!(
            entries.len() <= MAX_HISTORY_LINES,
            "read_log_file must respect its own documented cap"
        );

        let predictions = entries
            .iter()
            .filter(|entry| matches!(entry, LogEntry::IntentPrediction { .. }))
            .count();
        let raw_events = entries
            .iter()
            .filter(|entry| matches!(entry, LogEntry::RawEvent { .. }))
            .count();
        println!(
            "parsed {} real entries from {today}.jsonl: {predictions} intent_prediction, \
            {raw_events} raw_event",
            entries.len()
        );
        for entry in entries.iter().take(3) {
            println!("sample entry: {entry:?}");
        }

        // The specific raw_event shape `parse_log_line`'s doc comment calls
        // out as the risky part (internally-tagged RawEvent flattened into
        // an already internally-tagged LogLine) — confirm at least one
        // survived the real round trip, not just that *some* line parsed.
        assert!(
            raw_events > 0,
            "expected at least one real raw_event to have parsed correctly"
        );
        assert!(
            predictions > 0,
            "expected at least one real intent_prediction to have parsed correctly"
        );
    }

    // ---- Feedback-telemetry session: Part B (agent gate) and Part C
    // (pane-focus prior + diagnostic-location correlation) required tests. ----

    /// Confirms Part B's gate actually *overrides* scoring, not just that
    /// `ConsultingAgent` exists as a variant: a session with `agent_active`
    /// true, but otherwise carrying strongly Debugging-shaped signals (a
    /// failed test run, an open error, terminal focus), must still resolve
    /// to `ConsultingAgent` at full confidence — the ground-truth gate must
    /// short-circuit before any of those signals get a chance to blend in.
    #[test]
    fn test_agent_active_gate_overrides_debugging_signals() {
        let state = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions {
                test_runs: 3,
                failed_test_runs: 3,
                saves: 0,
                file_switches: 0,
                large_edits: 0,
                edit_events: 0.0,
                navigation_events: 0.0,
            },
            diagnostics: DiagnosticsSummary {
                errors: 2,
                warnings: 0,
            },
            current_symbol: None,
            pause_seconds: 38,
            diff_summary: None,
            agent_active: true,
            focused_pane: FocusedPane::Terminal,
            cursor_at_diagnostic: true,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let prediction = classify(&state, None);
        println!(
            "agent-active-gate: intent={:?} confidence={:.3} probabilities={:?} evidence={:?}",
            prediction.intent, prediction.confidence, prediction.probabilities, prediction.evidence
        );

        assert_eq!(
            prediction.intent,
            DeveloperIntent::ConsultingAgent,
            "agent_active must gate ahead of and override Debugging-shaped signals, not blend \
            with them"
        );
        assert_eq!(prediction.confidence, 1.0);
        let debugging_probability = prediction
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
            .unwrap()
            .1;
        assert_eq!(
            debugging_probability, 0.0,
            "the gate must zero out every other intent's probability, not just win a tie"
        );
    }

    /// Part C, pane-focus prior: focus alone (no other signal) must not
    /// drive a confident classification — it's a tie-breaker, not a driver.
    /// Checked for both directions (terminal -> Debugging, editor ->
    /// Implementing) in one test since they're the same claim about the
    /// same weight magnitude.
    #[test]
    fn test_pane_focus_alone_is_insufficient_to_drive_classification() {
        let base = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions::default(),
            diagnostics: DiagnosticsSummary::default(),
            current_symbol: None,
            pause_seconds: 0,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::Other,
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        for focused_pane in [FocusedPane::Terminal, FocusedPane::Editor] {
            let state = SessionState {
                focused_pane,
                ..base.clone()
            };
            let prediction = classify(&state, None);
            println!(
                "focus-alone ({focused_pane:?}): intent={:?} confidence={:.3} probabilities={:?}",
                prediction.intent, prediction.confidence, prediction.probabilities
            );
            assert_eq!(
                prediction.intent,
                DeveloperIntent::Idle,
                "{focused_pane:?} focus alone, with no other signal, should not be enough to \
                beat Idle"
            );
            let debugging_probability = prediction
                .probabilities
                .iter()
                .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
                .unwrap()
                .1;
            let implementing_probability = prediction
                .probabilities
                .iter()
                .find(|(intent, _)| *intent == DeveloperIntent::Implementing)
                .unwrap()
                .1;
            assert!(
                debugging_probability < 0.2 && implementing_probability < 0.2,
                "{focused_pane:?} focus alone should not produce a spuriously confident \
                Debugging/Implementing reading: debugging={debugging_probability}, \
                implementing={implementing_probability}"
            );
        }
    }

    /// Part C, diagnostic-location correlation: cursor at a known error
    /// line should score meaningfully higher Debugging than the same error
    /// present but the cursor elsewhere — confirming
    /// `WEIGHT_CURSOR_AT_DIAGNOSTIC` actually shifts the score versus the
    /// prior generic `diagnostics.errors`-only signal, not just that the
    /// field exists.
    #[test]
    fn test_diagnostic_location_correlation_shifts_score_meaningfully() {
        let base = SessionState {
            active_files: vec![PathBuf::from("main.py")],
            recent_actions: RecentActions::default(),
            diagnostics: DiagnosticsSummary {
                errors: 1,
                warnings: 0,
            },
            current_symbol: None,
            pause_seconds: 5,
            diff_summary: None,
            agent_active: false,
            focused_pane: FocusedPane::Other,
            cursor_at_diagnostic: false,
            test_command_running: false,
            test_command_recently_passed: false,
        };

        let elsewhere = classify(&base, None);
        let at_diagnostic = classify(
            &SessionState {
                cursor_at_diagnostic: true,
                ..base
            },
            None,
        );

        let elsewhere_probability = elsewhere
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
            .unwrap()
            .1;
        let at_diagnostic_probability = at_diagnostic
            .probabilities
            .iter()
            .find(|(intent, _)| *intent == DeveloperIntent::Debugging)
            .unwrap()
            .1;
        println!(
            "diagnostic-location: elsewhere={elsewhere_probability:.3} \
            at_diagnostic={at_diagnostic_probability:.3}"
        );

        assert!(
            at_diagnostic_probability > elsewhere_probability + 0.1,
            "cursor at the error's location should meaningfully outscore the same error \
            present but the cursor elsewhere: elsewhere={elsewhere_probability}, \
            at_diagnostic={at_diagnostic_probability}"
        );
        assert!(
            at_diagnostic
                .evidence
                .iter()
                .any(|line| line.contains("line(s) of an active error")),
            "evidence should call out the location correlation specifically, got {:?}",
            at_diagnostic.evidence
        );
    }

    /// Part A: writes a real `Feedback` line via `NucleusLogger::log_feedback`
    /// against the actual `~/.nucleus/logs/` directory (same real-write-path
    /// discipline as `test_logger_writes_real_jsonl_lines` above), then
    /// confirms it round-trips back through `read_log_file`/`parse_log_line`
    /// as a `LogEntry::Feedback` with the same `prediction_id` and fields —
    /// not just that it parses as *some* entry.
    #[gpui::test]
    async fn test_feedback_log_round_trips(cx: &mut TestAppContext) {
        let _guard = lock_real_log_file_tests();
        let logger = NucleusLogger::new(cx.executor());
        let prediction_id = PredictionId::new();
        let feedback = Feedback {
            prediction_id,
            correct: false,
            actual_intent: Some(DeveloperIntent::Debugging),
        };
        logger.log_feedback(&feedback);

        // Cross MAX_QUEUE_LEN (private to logging.rs, 50) to force the
        // immediate flush path deterministically, same technique as
        // `test_logger_writes_real_jsonl_lines`.
        for _ in 0..50 {
            logger.log_raw_event(&RawEvent::SelectionChanged { file: None });
        }
        cx.executor().run_until_parked();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let entries = read_log_file(&today).expect("reading today's real log file should succeed");

        let round_tripped = entries.iter().find_map(|entry| match entry {
            LogEntry::Feedback {
                feedback: found, ..
            } if found.prediction_id == prediction_id => Some(found.clone()),
            _ => None,
        });

        let round_tripped = round_tripped
            .expect("the feedback line just written should round-trip back with a matching prediction_id");
        assert!(!round_tripped.correct);
        assert_eq!(
            round_tripped.actual_intent,
            Some(DeveloperIntent::Debugging)
        );
    }
}

/// Stress/property tests over `classify()`: rather than hand-picking cases
/// (the `tests` module above), these generate many combinations of
/// extreme/boundary `SessionState` values — including ones no real
/// `NucleusEngine` session could ever actually produce — to check the
/// function's own internal contract holds regardless of input shape. Pure
/// function, no GPUI involved, so plain `proptest!` is enough.
#[cfg(test)]
mod classify_stress_tests {
    use super::*;
    use proptest::prelude::*;

    fn focused_pane_strategy() -> impl Strategy<Value = FocusedPane> {
        prop_oneof![
            Just(FocusedPane::Editor),
            Just(FocusedPane::Terminal),
            Just(FocusedPane::Other),
        ]
    }

    fn recent_actions_strategy() -> impl Strategy<Value = RecentActions> {
        (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            // Full f32 range, deliberately including NaN/Infinity/negative
            // values a real engine's `decayed_density` could never actually
            // produce (that function only ever sums positive
            // `exp(-age/tau)` terms) — the whole point of this stress
            // module is checking `classify` stays well-behaved even for
            // shapes the real engine wouldn't produce, and every use of
            // these two fields in `classify` is behind a comparison
            // (`>=`/`<=`/`>`), which is always `false` for NaN, so a NaN
            // input is expected to just skip the relevant signal rather
            // than propagate into the output — exactly what
            // `classify_never_violates_its_own_contract` checks below.
            any::<f32>(),
            any::<f32>(),
        )
            .prop_map(
                |(
                    test_runs,
                    failed_test_runs,
                    saves,
                    file_switches,
                    large_edits,
                    edit_events,
                    navigation_events,
                )| {
                    RecentActions {
                        test_runs,
                        // failed_test_runs is meaningless above test_runs in
                        // any real session, but classify() takes a bare
                        // SessionState with no cross-field invariants
                        // enforced by the type system — deliberately
                        // generated independently to check classify() is
                        // robust to a shape the real engine would never
                        // produce, not just shapes it would. Same reasoning
                        // extends to edit_events/navigation_events below
                        // (also generated independently of large_edits).
                        failed_test_runs,
                        saves,
                        file_switches,
                        large_edits,
                        edit_events,
                        navigation_events,
                    }
                },
            )
    }

    fn diagnostics_strategy() -> impl Strategy<Value = DiagnosticsSummary> {
        (any::<usize>(), any::<usize>()).prop_map(|(errors, warnings)| DiagnosticsSummary {
            errors,
            warnings,
        })
    }

    /// Occasionally generates a recognized-config-file path (exercising
    /// `DeveloperIntent::Configuring`'s condition under stress too, not
    /// just leaving `active_files` always empty), alongside plain/empty
    /// shapes.
    fn active_files_strategy() -> impl Strategy<Value = Vec<PathBuf>> {
        prop_oneof![
            Just(Vec::new()),
            Just(vec![PathBuf::from("main.py")]),
            Just(vec![PathBuf::from("Cargo.toml")]),
            Just(vec![PathBuf::from(".env")]),
        ]
    }

    fn session_state_strategy() -> impl Strategy<Value = SessionState> {
        (
            recent_actions_strategy(),
            diagnostics_strategy(),
            any::<u64>(),
            any::<bool>(),
            focused_pane_strategy(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            active_files_strategy(),
        )
            .prop_map(
                |(
                    recent_actions,
                    diagnostics,
                    pause_seconds,
                    agent_active,
                    focused_pane,
                    cursor_at_diagnostic,
                    test_command_running,
                    test_command_recently_passed,
                    active_files,
                )| {
                    SessionState {
                        active_files,
                        recent_actions,
                        diagnostics,
                        current_symbol: None,
                        pause_seconds,
                        diff_summary: None,
                        agent_active,
                        focused_pane,
                        cursor_at_diagnostic,
                        test_command_running,
                        test_command_recently_passed,
                    }
                },
            )
    }

    fn last_edit_magnitude_strategy() -> impl Strategy<Value = Option<usize>> {
        prop_oneof![Just(None), any::<usize>().prop_map(Some)]
    }

    proptest! {
        /// The contract every caller of `classify()` relies on, checked
        /// against thousands of extreme/boundary combinations: never
        /// panics, always returns a well-formed probability distribution
        /// (11 entries, one per `DeveloperIntent::ALL`, each finite and in
        /// [0,1], summing to ~1.0), a finite in-range confidence that
        /// matches the selected intent's own probability, non-empty
        /// evidence, and a selected intent that's genuinely at least tied
        /// for the highest probability (not just claimed to be).
        #[test]
        fn classify_never_violates_its_own_contract(
            state in session_state_strategy(),
            last_edit_magnitude in last_edit_magnitude_strategy(),
        ) {
            let prediction = classify(&state, last_edit_magnitude);

            prop_assert_eq!(
                prediction.probabilities.len(),
                DeveloperIntent::ALL.len(),
                "must return exactly one probability per DeveloperIntent variant"
            );

            let mut seen = std::collections::HashSet::new();
            let mut sum = 0.0f32;
            for (intent, probability) in &prediction.probabilities {
                prop_assert!(
                    seen.insert(*intent),
                    "duplicate intent {intent:?} in probabilities"
                );
                prop_assert!(
                    probability.is_finite(),
                    "non-finite probability {probability} for {intent:?}, state={state:?}"
                );
                prop_assert!(
                    (-1e-4..=1.0 + 1e-4).contains(probability),
                    "probability {probability} for {intent:?} out of [0,1], state={state:?}"
                );
                sum += probability;
            }
            prop_assert!(
                (sum - 1.0).abs() < 1e-3,
                "probabilities summed to {sum}, expected ~1.0, state={state:?}"
            );

            prop_assert!(
                prediction.confidence.is_finite(),
                "non-finite confidence {}, state={state:?}",
                prediction.confidence
            );
            prop_assert!(
                (-1e-4..=1.0 + 1e-4).contains(&prediction.confidence),
                "confidence {} out of [0,1], state={state:?}",
                prediction.confidence
            );

            let selected_probability = prediction
                .probabilities
                .iter()
                .find(|(intent, _)| *intent == prediction.intent)
                .map(|(_, probability)| *probability)
                .expect("selected intent must appear in probabilities");
            prop_assert!(
                (selected_probability - prediction.confidence).abs() < 1e-3,
                "confidence {} doesn't match selected intent {:?}'s own probability {selected_probability}, state={state:?}",
                prediction.confidence,
                prediction.intent
            );

            let max_probability = prediction
                .probabilities
                .iter()
                .map(|(_, probability)| *probability)
                .fold(f32::MIN, f32::max);
            prop_assert!(
                selected_probability >= max_probability - 1e-4,
                "selected intent {:?} (probability {selected_probability}) is not the argmax \
                (max was {max_probability}), state={state:?}",
                prediction.intent
            );

            prop_assert!(
                !prediction.evidence.is_empty(),
                "evidence must never be empty, state={state:?}"
            );
        }

        /// `agent_active` must always win regardless of what else is set —
        /// the gate exists specifically so a strongly-Debugging-shaped
        /// state can't leak through it.
        #[test]
        fn agent_active_always_wins_at_full_confidence(
            state in session_state_strategy(),
            last_edit_magnitude in last_edit_magnitude_strategy(),
        ) {
            let mut state = state;
            state.agent_active = true;
            let prediction = classify(&state, last_edit_magnitude);
            prop_assert_eq!(prediction.intent, DeveloperIntent::ConsultingAgent);
            prop_assert!((prediction.confidence - 1.0).abs() < 1e-6);
        }

        /// `classify` takes `&SessionState` — calling it twice with an
        /// identical state must be deterministic (same intent/confidence/
        /// probabilities), even though each call mints a fresh
        /// `PredictionId`. Guards against any accidental reliance on hidden
        /// global/time-based state inside the scoring math itself.
        #[test]
        fn classify_is_deterministic_for_identical_input(
            state in session_state_strategy(),
            last_edit_magnitude in last_edit_magnitude_strategy(),
        ) {
            let first = classify(&state, last_edit_magnitude);
            let second = classify(&state, last_edit_magnitude);
            prop_assert_eq!(first.intent, second.intent);
            prop_assert!((first.confidence - second.confidence).abs() < 1e-9);
            prop_assert_eq!(first.probabilities, second.probabilities);
        }
    }
}
