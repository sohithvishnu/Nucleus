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

use collections::{HashMap, VecDeque};
use editor::{Editor, EditorEvent};
use gpui::{App, Context, Entity, EntityId, Subscription, Task, WeakEntity};
use language::{Buffer, BufferEvent};
use project::Project;
use serde::Serialize;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::TerminalPanel;
use workspace::Workspace;
use workspace::item::ItemHandle;

mod logging;

pub use logging::{NucleusLogger, RawEvent, log_dir};

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

/// Minimum confidence delta (on top of an unchanged selected intent) that
/// counts as a "meaningful shift" worth logging an `intent_prediction` line
/// for. Chosen arbitrarily as "more than a rounding-error-sized move"; not
/// tuned against real usage yet.
const CONFIDENCE_LOG_THRESHOLD: f32 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
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
}

impl DeveloperIntent {
    pub const ALL: [DeveloperIntent; 10] = [
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
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RecentActions {
    pub test_runs: u32,
    pub failed_test_runs: u32,
    pub saves: u32,
    pub file_switches: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiagnosticsSummary {
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionState {
    pub active_files: Vec<PathBuf>,
    pub recent_actions: RecentActions,
    pub diagnostics: DiagnosticsSummary,
    pub current_symbol: Option<String>,
    pub pause_seconds: u64,
    pub diff_summary: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IntentPrediction {
    /// Probability per intent, in [`DeveloperIntent::ALL`] order.
    pub probabilities: Vec<(DeveloperIntent, f32)>,
    pub intent: DeveloperIntent,
    pub confidence: f32,
    pub evidence: Vec<String>,
}

/// Passive observer entity: subscribes to real workspace/project/editor/
/// terminal events, aggregates them into [`SessionState`], and classifies
/// [`DeveloperIntent`] from that state. Emits no events of its own; observers
/// should use `cx.observe` and read [`Self::session_state`] / [`Self::prediction`].
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
    last_activity_at: Instant,

    workspace: WeakEntity<Workspace>,
    /// Most recently observed `TaskStatus` per task-terminal, keyed by the
    /// terminal's `EntityId`. Used to detect the `Running` -> `Completed`
    /// transition rather than just "is this terminal currently completed" —
    /// terminals are commonly reused across reruns (`task::Rerun` defaults to
    /// `use_new_terminal: false`), so a plain "have we ever seen this
    /// terminal" set would only ever count the first run in a reused tab.
    last_seen_task_status: HashMap<EntityId, terminal::TaskStatus>,

    logger: NucleusLogger,
    /// The last `IntentPrediction` actually written to the log, compared
    /// against on every `refresh()` so we only log on a meaningful change
    /// (selected intent changes, or confidence moves by more than
    /// `CONFIDENCE_LOG_THRESHOLD`) rather than on every classifier tick.
    last_logged_prediction: Option<IntentPrediction>,

    _subscriptions: Vec<Subscription>,
    _project_subscription: Option<Subscription>,
    _active_item_subscriptions: Vec<Subscription>,
    _decay_task: Task<()>,
}

impl NucleusEngine {
    pub fn new(workspace: Entity<Workspace>, cx: &mut Context<Self>) -> Self {
        let weak_workspace = workspace.downgrade();
        let logger = NucleusLogger::new(cx.background_executor().clone());
        let workspace_subscription = cx.subscribe(&workspace, Self::handle_workspace_event);

        let decay_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PRUNE_INTERVAL).await;
                if this
                    .update(cx, |this, cx| this.prune_and_refresh(cx))
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
            last_activity_at: Instant::now(),
            workspace: weak_workspace,
            last_seen_task_status: HashMap::default(),
            logger,
            last_logged_prediction: None,
            _subscriptions: vec![workspace_subscription],
            _project_subscription: None,
            _active_item_subscriptions: Vec::new(),
            _decay_task: decay_task,
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
            self.record_file_switch(new_file_path.clone().unwrap());
        }
        self.active_file_path = new_file_path;

        self._active_item_subscriptions.clear();
        self.active_editor = None;
        self.active_buffer_snapshot = None;

        if let Some(editor) = editor {
            self._active_item_subscriptions
                .push(cx.subscribe(&editor, |this, _editor, event, cx| {
                    if let EditorEvent::SelectionsChanged { local: true } = event {
                        this.logger.log_raw_event(&RawEvent::SelectionChanged {
                            file: this.active_file_path.clone(),
                        });
                        this.last_activity_at = Instant::now();
                        this.refresh(cx);
                    }
                }));

            if let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() {
                self.active_buffer_snapshot = Some(buffer.read(cx).text_snapshot());
                self._active_item_subscriptions
                    .push(cx.subscribe(&buffer, |this, buffer, event, cx| match event {
                        BufferEvent::Edited { .. } => this.handle_buffer_edited(buffer, cx),
                        BufferEvent::Saved => this.handle_buffer_saved(cx),
                        _ => {}
                    }));
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
        self.last_edit_magnitude = Some(inserted + deleted);
        self.session_state.diff_summary = Some(format!(
            "{edit_count} edit(s), +{inserted}/-{deleted} chars in {file_label}"
        ));

        self.logger.log_raw_event(&RawEvent::Edit {
            file: self.active_file_path.clone(),
            symbol: self.current_symbol(cx),
            inserted_chars: inserted,
            deleted_chars: deleted,
        });

        self.last_activity_at = Instant::now();
        self.refresh(cx);
    }

    fn handle_buffer_saved(&mut self, cx: &mut Context<Self>) {
        self.save_timestamps.push_back(Instant::now());
        self.logger.log_raw_event(&RawEvent::Save {
            file: self.active_file_path.clone(),
        });
        self.last_activity_at = Instant::now();
        self.refresh(cx);
    }

    fn record_file_switch(&mut self, path: PathBuf) {
        self.file_switch_timestamps.push_back(Instant::now());
        self.logger.log_raw_event(&RawEvent::FileSwitch {
            file: path.clone(),
        });
        self.recent_files.retain(|existing| existing != &path);
        self.recent_files.insert(0, path);
        self.recent_files.truncate(MAX_ACTIVE_FILES);
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

        let mut newly_observed = Vec::new();
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
                    self.logger.log_raw_event(&RawEvent::TaskStarted {
                        label: Some(label.clone()),
                    });
                }

                if let terminal::TaskStatus::Completed { success } = status
                    && !matches!(previous_status, Some(terminal::TaskStatus::Completed { .. }))
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
                    self.logger.log_raw_event(&event);
                    newly_observed.push(success);
                }
            }
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

    fn prune_and_refresh(&mut self, cx: &mut Context<Self>) {
        let cutoff = Instant::now()
            .checked_sub(RECENT_WINDOW)
            .unwrap_or_else(Instant::now);
        self.save_timestamps.retain(|at| *at >= cutoff);
        self.file_switch_timestamps.retain(|at| *at >= cutoff);
        self.test_run_outcomes.retain(|(at, _)| *at >= cutoff);
        self.poll_task_terminals(cx);
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.session_state.active_files = self.recent_files.clone();
        self.session_state.pause_seconds = Instant::now()
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
        };

        let new_prediction = classify(&self.session_state, self.last_edit_magnitude);
        if self.is_meaningful_prediction_change(&new_prediction) {
            self.logger
                .log_intent_prediction(&new_prediction, &self.session_state);
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

/// Weighted rule-based scoring for [`DeveloperIntent::Debugging`] and
/// [`DeveloperIntent::Implementing`]. All other intents are stubs (score 0),
/// per the phase-4 "start narrow" plan. Weights are a first pass, not tuned
/// against real usage yet.
fn classify(state: &SessionState, last_edit_magnitude: Option<usize>) -> IntentPrediction {
    let actions = &state.recent_actions;

    let mut debugging_score = 0.0f32;
    let mut debugging_evidence = Vec::new();
    let mut implementing_score = 0.0f32;
    let mut implementing_evidence = Vec::new();

    // --- Debugging signals ---
    if actions.failed_test_runs > 0 {
        debugging_score += 3.0 * actions.failed_test_runs.min(5) as f32;
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
        debugging_score += 2.0 * state.diagnostics.errors.min(5) as f32;
        debugging_evidence.push(format!(
            "{} error diagnostic(s) currently open",
            state.diagnostics.errors
        ));
    }
    if actions.file_switches == 0 && actions.saves >= 2 {
        debugging_score += 2.0;
        debugging_evidence.push(format!(
            "{} saves to the same file with no file switches (iterating on a fix)",
            actions.saves
        ));
    }
    if let Some(magnitude) = last_edit_magnitude
        && magnitude < 10
    {
        debugging_score += 1.0;
        debugging_evidence.push(format!(
            "small, localized edit (~{magnitude} chars changed)"
        ));
    }
    if (30..=300).contains(&state.pause_seconds) {
        debugging_score += 1.0;
        debugging_evidence.push(format!(
            "{}s pause before resuming activity (investigating)",
            state.pause_seconds
        ));
    }

    // --- Implementing signals ---
    if state.diagnostics.errors == 0 && actions.saves >= 1 {
        implementing_score += 1.5;
        implementing_evidence.push(format!(
            "{} clean save(s) with no open errors",
            actions.saves
        ));
    }
    if actions.file_switches >= 2 {
        implementing_score += actions.file_switches.min(5) as f32;
        implementing_evidence.push(format!(
            "{} file switches in the last {} minutes (spreading work across files)",
            actions.file_switches,
            RECENT_WINDOW.as_secs() / 60
        ));
    }
    if let Some(magnitude) = last_edit_magnitude
        && magnitude >= 40
    {
        implementing_score += 2.0;
        implementing_evidence.push(format!(
            "large edit (~{magnitude} chars changed) suggesting new code being written"
        ));
    }
    if actions.test_runs > 0 && actions.failed_test_runs == 0 {
        implementing_score += 1.0;
        implementing_evidence.push(format!("{} passing test/task run(s)", actions.test_runs));
    }
    if state.pause_seconds < 10 {
        implementing_score += 0.5;
        implementing_evidence.push("continuous editing activity, no long pauses".to_string());
    }

    let total = debugging_score + implementing_score;
    if total <= f32::EPSILON {
        return IntentPrediction {
            probabilities: DeveloperIntent::ALL
                .into_iter()
                .map(|intent| {
                    let probability = if intent == DeveloperIntent::Idle {
                        1.0
                    } else {
                        0.0
                    };
                    (intent, probability)
                })
                .collect(),
            intent: DeveloperIntent::Idle,
            confidence: 1.0,
            evidence: vec!["No recent edits, test runs, or diagnostics activity".to_string()],
        };
    }

    let debugging_probability = debugging_score / total;
    let implementing_probability = implementing_score / total;

    let probabilities = DeveloperIntent::ALL
        .into_iter()
        .map(|intent| {
            let probability = match intent {
                DeveloperIntent::Debugging => debugging_probability,
                DeveloperIntent::Implementing => implementing_probability,
                _ => 0.0,
            };
            (intent, probability)
        })
        .collect();

    let (intent, confidence, evidence) = if debugging_score >= implementing_score {
        (
            DeveloperIntent::Debugging,
            debugging_probability,
            debugging_evidence,
        )
    } else {
        (
            DeveloperIntent::Implementing,
            implementing_probability,
            implementing_evidence,
        )
    };

    IntentPrediction {
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

    /// Exercises `NucleusLogger`'s real write path (no mocked filesystem or
    /// injected path) against the actual `~/.nucleus/logs/` directory, to
    /// verify actual JSONL lines land on disk rather than just that the
    /// logging code compiles. Logs enough lines to cross `MAX_QUEUE_LEN` so
    /// the size-triggered immediate flush fires deterministically, without
    /// needing to fast-forward the executor's virtual clock past the
    /// timer-based flush path.
    #[gpui::test]
    async fn test_logger_writes_real_jsonl_lines(cx: &mut TestAppContext) {
        let logger = NucleusLogger::new(cx.executor());

        let prediction = IntentPrediction {
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
            },
            diagnostics: DiagnosticsSummary {
                errors: 2,
                warnings: 0,
            },
            current_symbol: Some("fn poll_task_terminals".to_string()),
            pause_seconds: 12,
            diff_summary: Some("2 edit(s), +10/-2 chars in nucleus.rs".to_string()),
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
}
