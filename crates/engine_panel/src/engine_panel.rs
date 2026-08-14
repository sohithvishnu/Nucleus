use anyhow::Result;
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Render, Styled, WeakEntity, Window, div, px,
};
use nucleus_intent::{FeedbackNudgeToast, LogEntry, NucleusEngine, NucleusEvent, RawEvent};
use ui::CopyButton;
use ui::ProgressBar;
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

mod log_view;

use log_view::{LogTypeFilter, LogView, LogViewMode};

const ENGINE_PANEL_KEY: &str = "EnginePanel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnginePanelTab {
    Overview,
    Logs,
}

/// Phase 4 debug panel: shows the live output of [`NucleusEngine`]'s passive
/// observation (inferred intent, confidence, evidence, raw session state),
/// plus a "Logs" tab that tails the same data live or browses past days from
/// `~/.nucleus/logs/`. Produces no suggestions of its own — see the crate's
/// phase-4 doc comment.
pub struct EnginePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    engine: Entity<NucleusEngine>,
    tab: EnginePanelTab,
    log_view: LogView,
    /// Needed to show [`FeedbackNudgeToast`] (`workspace.toggle_status_toast`
    /// is a `Workspace` method) in reaction to
    /// `NucleusEvent::FeedbackNudgeRequested` — `NucleusEngine` itself
    /// renders nothing (see the crate's module doc comment).
    workspace: WeakEntity<Workspace>,
}

impl EnginePanel {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Entity<Self> {
        let workspace_entity = cx.entity();
        let workspace = workspace_entity.downgrade();
        cx.new(|cx| {
            let engine = cx.new(|cx| NucleusEngine::new(workspace_entity, window, cx));
            cx.observe(&engine, |_this, _engine, cx| cx.notify()).detach();
            // Tap the same in-memory data the engine hands to its logger,
            // emitted at the same call sites in addition to (not instead of)
            // the logger call — the live tail below never reads the log
            // file back off disk, only this subscription.
            cx.subscribe(&engine, |this: &mut Self, engine, event: &NucleusEvent, cx| {
                match event {
                    NucleusEvent::RawEvent(_) | NucleusEvent::IntentPrediction { .. } => {
                        this.log_view.push_live(log_entry_from_event(event));
                    }
                    NucleusEvent::FeedbackNudgeRequested {
                        prediction_id,
                        intent,
                    } => {
                        this.show_feedback_nudge(engine, *prediction_id, *intent, cx);
                    }
                }
                cx.notify();
            })
            .detach();

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                engine,
                tab: EnginePanelTab::Overview,
                log_view: LogView::new(),
                workspace,
            };
            this.refresh_available_log_dates(cx);
            this
        })
    }

    fn show_feedback_nudge(
        &self,
        engine: Entity<NucleusEngine>,
        prediction_id: nucleus_intent::PredictionId,
        intent: nucleus_intent::DeveloperIntent,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let logger = engine.read(cx).logger();
        workspace.update(cx, |workspace, cx| {
            let toast = FeedbackNudgeToast::new(logger, prediction_id, intent, cx);
            workspace.toggle_status_toast(toast, cx);
        });
    }

    fn set_tab(&mut self, tab: EnginePanelTab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn set_log_mode(&mut self, mode: LogViewMode, cx: &mut Context<Self>) {
        self.log_view.set_mode(mode);
        if mode == LogViewMode::History
            && !self.log_view.history_is_loaded()
            && !self.log_view.history_loading()
            && let Some(date) = self.log_view.selected_date().map(str::to_string)
        {
            self.load_log_date(date, cx);
        }
        cx.notify();
    }

    fn set_log_filter(&mut self, filter: LogTypeFilter, cx: &mut Context<Self>) {
        self.log_view.set_filter(filter);
        cx.notify();
    }

    fn select_log_date(&mut self, date: String, cx: &mut Context<Self>) {
        self.load_log_date(date, cx);
    }

    fn toggle_log_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.log_view.toggle_expanded(index);
        cx.notify();
    }

    /// Background directory scan (`nucleus_intent::list_log_dates`) so
    /// opening the panel never blocks on disk I/O, however cheap that scan
    /// usually is.
    fn refresh_available_log_dates(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let dates = cx
                .background_spawn(async { nucleus_intent::list_log_dates().unwrap_or_default() })
                .await;
            this.update(cx, |this, cx| {
                this.log_view.set_available_dates(dates);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads and parses one day's log file off the background executor,
    /// never on the UI thread — historical files can be large (see
    /// `nucleus_intent::MAX_HISTORY_LINES` for the cap actually applied).
    fn load_log_date(&mut self, date: String, cx: &mut Context<Self>) {
        self.log_view.begin_loading(date.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    nucleus_intent::read_log_file(&date).map_err(|err| err.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.log_view.finish_loading(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |_workspace, window, cx| {
            EnginePanel::new(window, cx)
        })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        h_flex()
            .h(px(38.))
            .items_center()
            .gap_2()
            .px_4()
            .border_b_1()
            .border_color(colors.border)
            .child(Icon::new(IconName::Sparkle).color(Color::Accent))
            .child(
                Label::new("Engine")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_intent_section(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let prediction = self.engine.read(cx).prediction().clone();
        let confidence_percent = prediction.confidence * 100.0;

        div().px_4().pt_4().child(
            div()
                .rounded_lg()
                .border_1()
                .border_color(colors.border)
                .bg(colors.panel_background)
                .p_3()
                .child(
                    h_flex()
                        .justify_between()
                        .items_baseline()
                        .mb_2()
                        .child(Label::new(prediction.intent.label()).size(LabelSize::Default))
                        .child(
                            Label::new(format!("{confidence_percent:.0}%"))
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        ),
                )
                .child(
                    div().mb_3().child(
                        ProgressBar::new(
                            "engine_intent_confidence",
                            confidence_percent,
                            100.,
                            cx,
                        )
                        .bg_color(colors.border)
                        .fg_color(colors.text_accent),
                    ),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .children(prediction.evidence.iter().map(|evidence| {
                            h_flex()
                                .items_start()
                                .gap_1()
                                .child(
                                    Label::new("›")
                                        .size(LabelSize::Small)
                                        .color(Color::Accent),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Label::new(evidence.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                                )
                        })),
                ),
        )
    }

    fn render_session_state(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let state = self.engine.read(cx).session_state().clone();

        let row = |label: &'static str, value: String| {
            h_flex()
                .justify_between()
                .gap_2()
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(Label::new(value).size(LabelSize::XSmall))
        };

        let active_files = if state.active_files.is_empty() {
            "—".to_string()
        } else {
            state
                .active_files
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        };

        div().px_4().pt_5().pb_3().child(
            v_flex()
                .gap_2()
                .child(
                    Label::new("Session state")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .p_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.panel_background)
                        .child(row("active files", active_files))
                        .child(row(
                            "current symbol",
                            state.current_symbol.unwrap_or_else(|| "—".to_string()),
                        ))
                        .child(row("pause", format!("{}s", state.pause_seconds)))
                        .child(row(
                            "test/task runs",
                            state.recent_actions.test_runs.to_string(),
                        ))
                        .child(row(
                            "failed test/task runs",
                            state.recent_actions.failed_test_runs.to_string(),
                        ))
                        .child(row("saves", state.recent_actions.saves.to_string()))
                        .child(row(
                            "file switches",
                            state.recent_actions.file_switches.to_string(),
                        ))
                        .child(row("errors", state.diagnostics.errors.to_string()))
                        .child(row("warnings", state.diagnostics.warnings.to_string()))
                        .child(row(
                            "last diff",
                            state.diff_summary.unwrap_or_else(|| "—".to_string()),
                        )),
                )
                .child(
                    Label::new(
                        "Task runs are detected via Zed's task runner only (task::Spawn) — \
                        commands typed directly into a plain shell terminal aren't seen.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
        )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        h_flex()
            .gap_1()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(colors.border)
            .child(
                Button::new("engine_tab_overview", "Overview")
                    .toggle_state(self.tab == EnginePanelTab::Overview)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_tab(EnginePanelTab::Overview, cx);
                    })),
            )
            .child(
                Button::new("engine_tab_logs", "Logs")
                    .toggle_state(self.tab == EnginePanelTab::Logs)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_tab(EnginePanelTab::Logs, cx);
                    })),
            )
    }

    fn render_logs_tab(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mode = self.log_view.mode();

        let mode_row = h_flex()
            .gap_1()
            .px_4()
            .pt_3()
            .child(
                Button::new("engine_log_mode_live", "Live")
                    .toggle_state(mode == LogViewMode::Live)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_log_mode(LogViewMode::Live, cx);
                    })),
            )
            .child(
                Button::new("engine_log_mode_history", "History")
                    .toggle_state(mode == LogViewMode::History)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_log_mode(LogViewMode::History, cx);
                    })),
            );

        let filter = self.log_view.filter();
        let visible_lines = self.log_view.visible_lines();
        let visible_count = visible_lines.len();
        let copy_all_text = visible_lines
            .into_iter()
            .map(|(_, entry)| format_log_entry_for_clipboard(entry))
            .collect::<Vec<_>>()
            .join("\n");
        let filter_row = h_flex()
            .justify_between()
            .px_4()
            .pt_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("engine_log_filter_all", "All")
                            .label_size(LabelSize::Small)
                            .toggle_state(filter == LogTypeFilter::All)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_log_filter(LogTypeFilter::All, cx);
                            })),
                    )
                    .child(
                        Button::new("engine_log_filter_predictions", "Predictions")
                            .label_size(LabelSize::Small)
                            .toggle_state(filter == LogTypeFilter::Predictions)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_log_filter(LogTypeFilter::Predictions, cx);
                            })),
                    )
                    .child(
                        Button::new("engine_log_filter_raw", "Raw events")
                            .label_size(LabelSize::Small)
                            .toggle_state(filter == LogTypeFilter::RawEvents)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_log_filter(LogTypeFilter::RawEvents, cx);
                            })),
                    ),
            )
            .child(
                // Copies exactly what's on screen right now — the same
                // filtered, mode-scoped `visible_lines()` the rows below are
                // rendered from, so this always matches the active
                // Live/History mode and All/Predictions/Raw-events filter.
                CopyButton::new("engine_log_copy_all", copy_all_text)
                    .icon_size(IconSize::Small)
                    .tooltip_label("Copy all visible")
                    .disabled(visible_count == 0),
            );

        let date_row = (mode == LogViewMode::History).then(|| {
            h_flex()
                .gap_1()
                .px_4()
                .pt_2()
                .flex_wrap()
                .children(self.log_view.available_dates().iter().enumerate().map(
                    |(index, date)| {
                        let is_selected = self.log_view.selected_date() == Some(date.as_str());
                        let date_for_click = date.clone();
                        Button::new(("engine_log_date", index as u64), date.clone())
                            .label_size(LabelSize::Small)
                            .toggle_state(is_selected)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_log_date(date_for_click.clone(), cx);
                            }))
                    },
                ))
        });

        let body = self.render_log_body(mode, cx);

        v_flex()
            .flex_1()
            .min_h_0()
            .child(mode_row)
            .child(filter_row)
            .children(date_row)
            .child(body)
    }

    fn render_log_body(&self, mode: LogViewMode, cx: &mut Context<Self>) -> AnyElement {
        if mode == LogViewMode::History && self.log_view.history_loading() {
            return div()
                .px_4()
                .pt_3()
                .child(Label::new("Loading…").size(LabelSize::Small).color(Color::Muted))
                .into_any_element();
        }
        if mode == LogViewMode::History
            && let Some(err) = self.log_view.history_error()
        {
            return div()
                .px_4()
                .pt_3()
                .child(
                    Label::new(format!("Failed to read log: {err}"))
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .into_any_element();
        }

        let lines = self.log_view.visible_lines();
        if lines.is_empty() {
            let message = if mode == LogViewMode::Live {
                "No live events yet this session."
            } else {
                "No entries for this date."
            };
            return div()
                .px_4()
                .pt_3()
                .child(Label::new(message).size(LabelSize::Small).color(Color::Muted))
                .into_any_element();
        }

        div()
            .id("engine_log_rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .pt_2()
            .pb_3()
            .child(
                v_flex().gap_1().children(
                    lines
                        .into_iter()
                        .map(|(index, entry)| self.render_log_row(index, entry, cx)),
                ),
            )
            .into_any_element()
    }

    fn render_log_row(
        &self,
        index: usize,
        entry: &LogEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let is_expanded = self.log_view.is_expanded(index);
        let time = entry.timestamp().format("%H:%M:%S").to_string();
        let (badge, summary, badge_color) = summarize_log_entry(entry);

        let json_block = is_expanded.then(|| {
            div()
                .mt_1()
                .p_2()
                .rounded_md()
                .bg(colors.editor_background)
                .child(
                    Label::new(log_entry_to_pretty_json(entry))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        });

        v_flex()
            .id(("engine_log_row", index as u64))
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_log_row(index, cx);
            }))
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(Label::new(time).size(LabelSize::XSmall).color(Color::Muted))
                            .child(
                                div()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(colors.element_background)
                                    .child(
                                        Label::new(badge)
                                            .size(LabelSize::XSmall)
                                            .color(badge_color),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                CopyButton::new(
                                    ("engine_log_row_copy", index as u64),
                                    format_log_entry_for_clipboard(entry),
                                )
                                .icon_size(IconSize::XSmall)
                                .tooltip_label("Copy entry"),
                            )
                            .child(
                                Label::new(if is_expanded { "▾" } else { "▸" })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    ),
            )
            .child(Label::new(summary).size(LabelSize::Small))
            .children(json_block)
    }
}

impl EventEmitter<PanelEvent> for EnginePanel {}

impl Focusable for EnginePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for EnginePanel {
    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _window: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(308.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Sparkle)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Engine Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(zed_actions::engine_panel::ToggleFocus)
    }

    fn persistent_name() -> &'static str {
        "Engine Panel"
    }

    fn panel_key() -> &'static str {
        ENGINE_PANEL_KEY
    }

    fn activation_priority(&self) -> u32 {
        6
    }
}

impl Render for EnginePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_background = cx.theme().colors().panel_background;
        let header = self.render_header(cx);
        let tabs = self.render_tabs(cx);

        let body: AnyElement = match self.tab {
            EnginePanelTab::Overview => {
                let intent_section = self.render_intent_section(cx);
                let session_state = self.render_session_state(cx);
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(intent_section)
                    .child(session_state)
                    .into_any_element()
            }
            EnginePanelTab::Logs => self.render_logs_tab(cx).into_any_element(),
        };

        v_flex()
            .key_context("EnginePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(panel_background)
            .child(header)
            .child(tabs)
            .child(body)
    }
}

/// Converts a live [`NucleusEvent`] into the same [`LogEntry`] shape used
/// for historical file reads, so both render through `render_log_row`.
/// Stamped with "now" at receipt — accurate enough since the emit that
/// produced this event happens synchronously at the moment it occurred.
///
/// Only ever called for `RawEvent`/`IntentPrediction` — the caller (this
/// file's `NucleusEvent` subscription) handles `FeedbackNudgeRequested`
/// itself (it triggers a toast, not a log-view row) before reaching this
/// function.
fn log_entry_from_event(event: &NucleusEvent) -> LogEntry {
    let timestamp = chrono::Local::now();
    match event.clone() {
        NucleusEvent::RawEvent(event) => LogEntry::RawEvent { timestamp, event },
        NucleusEvent::IntentPrediction {
            prediction,
            session_state,
        } => LogEntry::IntentPrediction {
            timestamp,
            prediction,
            session_state,
        },
        NucleusEvent::FeedbackNudgeRequested { .. } => {
            unreachable!("caller filters this variant out before calling log_entry_from_event")
        }
    }
}

fn summarize_log_entry(entry: &LogEntry) -> (&'static str, String, Color) {
    match entry {
        LogEntry::IntentPrediction { prediction, .. } => {
            let evidence = if prediction.evidence.is_empty() {
                String::new()
            } else {
                format!(" — {}", prediction.evidence.join("; "))
            };
            (
                "prediction",
                format!(
                    "{} {:.0}%{evidence}",
                    prediction.intent.label(),
                    prediction.confidence * 100.0
                ),
                Color::Accent,
            )
        }
        LogEntry::RawEvent { event, .. } => {
            let summary = match event {
                RawEvent::Edit {
                    file,
                    inserted_chars,
                    deleted_chars,
                    ..
                } => format!(
                    "edit {} (+{inserted_chars}/-{deleted_chars} chars)",
                    file_label(file.as_deref())
                ),
                RawEvent::Save { file } => format!("save {}", file_label(file.as_deref())),
                RawEvent::FileSwitch { file } => {
                    format!("switched to {}", file_label(Some(file)))
                }
                RawEvent::SelectionChanged { file } => {
                    format!("selection changed in {}", file_label(file.as_deref()))
                }
                RawEvent::TaskStarted { label } => {
                    format!("task started: {}", label_or_dash(label))
                }
                RawEvent::TaskCompleted { label } => {
                    format!("task completed: {}", label_or_dash(label))
                }
                RawEvent::TaskFailed { label } => {
                    format!("task failed: {}", label_or_dash(label))
                }
                RawEvent::TerminalCommandStarted { command } => {
                    let category = nucleus_intent::categorize_command(command).label();
                    format!("terminal command started ({category}): {command}")
                }
                RawEvent::TerminalCommandFinished {
                    command,
                    exit_code,
                    duration_ms,
                } => {
                    let category = nucleus_intent::categorize_command(command).label();
                    format!(
                        "terminal command finished ({category}, exit {exit_code}, {duration_ms}ms): {command}"
                    )
                }
            };
            ("event", summary, Color::Muted)
        }
        LogEntry::Feedback { feedback, .. } => {
            let summary = if feedback.correct {
                "confirmed correct".to_string()
            } else {
                match feedback.actual_intent {
                    Some(actual) => format!("marked wrong — actually {}", actual.label()),
                    None => "marked wrong".to_string(),
                }
            };
            ("feedback", summary, Color::Warning)
        }
    }
}

/// Plain-text, single-line rendering of a log entry for the clipboard.
/// Reuses `summarize_log_entry`'s badge/summary computation — the same
/// values already rendered in `render_log_row` — rather than a second
/// formatter that could drift out of sync with what's on screen.
fn format_log_entry_for_clipboard(entry: &LogEntry) -> String {
    let time = entry.timestamp().format("%H:%M:%S").to_string();
    let (badge, summary, _) = summarize_log_entry(entry);
    format!("[{time}] {badge}: {summary}")
}

fn file_label(file: Option<&std::path::Path>) -> String {
    file.and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "—".to_string())
}

fn label_or_dash(label: &Option<String>) -> &str {
    label.as_deref().unwrap_or("—")
}

/// Pretty-prints the full payload underneath an expanded row: the full
/// `SessionState` snapshot for predictions, the full event payload for raw
/// events — never source code, since none of these types ever carry any.
fn log_entry_to_pretty_json(entry: &LogEntry) -> String {
    let value = match entry {
        LogEntry::IntentPrediction {
            prediction,
            session_state,
            ..
        } => serde_json::json!({
            "prediction": prediction,
            "session_state": session_state,
        }),
        LogEntry::RawEvent { event, .. } => serde_json::json!(event),
        LogEntry::Feedback { feedback, .. } => serde_json::json!(feedback),
    };
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "<failed to render JSON>".to_string())
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &zed_actions::engine_panel::ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<EnginePanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &zed_actions::engine_panel::Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<EnginePanel>(window, cx) {
                workspace.close_panel::<EnginePanel>(window, cx);
            }
        });
    })
    .detach();
}
