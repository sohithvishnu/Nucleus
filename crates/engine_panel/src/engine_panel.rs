use anyhow::Result;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Styled, WeakEntity, Window, div, px,
};
use nucleus::NucleusEngine;
use ui::ProgressBar;
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const ENGINE_PANEL_KEY: &str = "EnginePanel";

/// Phase 4 debug panel: shows the live output of [`NucleusEngine`]'s passive
/// observation (inferred intent, confidence, evidence, raw session state).
/// Produces no suggestions of its own — see the crate's phase-4 doc comment.
pub struct EnginePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    engine: Entity<NucleusEngine>,
}

impl EnginePanel {
    pub fn new(_window: &mut Window, cx: &mut Context<Workspace>) -> Entity<Self> {
        let workspace = cx.entity();
        cx.new(|cx| {
            let engine = cx.new(|cx| NucleusEngine::new(workspace, cx));
            cx.observe(&engine, |_this, _engine, cx| cx.notify()).detach();

            Self {
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                engine,
            }
        })
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
                                .gap_1()
                                .child(
                                    Label::new("›")
                                        .size(LabelSize::Small)
                                        .color(Color::Accent),
                                )
                                .child(
                                    Label::new(evidence.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
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
                ),
        )
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
        let intent_section = self.render_intent_section(cx);
        let session_state = self.render_session_state(cx);

        v_flex()
            .key_context("EnginePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(panel_background)
            .child(header)
            .child(intent_section)
            .child(session_state)
    }
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
