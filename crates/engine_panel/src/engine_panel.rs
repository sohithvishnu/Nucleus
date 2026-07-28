use anyhow::Result;
use editor::Editor;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, SharedString, Styled, WeakEntity, Window, div, px, relative,
};
use ui::ProgressBar;
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const ENGINE_PANEL_KEY: &str = "EnginePanel";

/// Placeholder shape for what a real behavioral-observation engine would eventually
/// produce. Nothing in this crate performs any actual classification — every value
/// here is static, mirroring the design mockup this panel was built from.
struct InferredIntent {
    label: SharedString,
    confidence_percent: u32,
    evidence: Vec<SharedString>,
}

struct Suggestion {
    line: u32,
    text: SharedString,
    age: SharedString,
}

enum ChatSender {
    Agent,
    You,
}

struct ChatMessage {
    from: ChatSender,
    text: SharedString,
}

pub struct EnginePanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    intent: InferredIntent,
    suggestions: Vec<Suggestion>,
    chat_messages: Vec<ChatMessage>,
    chat_input: Entity<Editor>,
    show_chat: bool,
    highlighted_line: Option<u32>,
}

impl EnginePanel {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Entity<Self> {
        cx.new(|cx| {
            let chat_input = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Ask a follow-up…", window, cx);
                editor
            });

            Self {
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                intent: InferredIntent {
                    label: "Debugging".into(),
                    confidence_percent: 82,
                    evidence: vec![
                        "3 failed test runs in 4 min".into(),
                        "no new symbols added".into(),
                        "repeated edits to same fn".into(),
                    ],
                },
                suggestions: vec![Suggestion {
                    line: 6,
                    text: "ctx may be stale here".into(),
                    age: "surfaced 14s ago".into(),
                }],
                chat_messages: vec![ChatMessage {
                    from: ChatSender::Agent,
                    text: "This recomputes ctx on every call — session may be unset on the \
                        first observed event."
                        .into(),
                }],
                chat_input,
                show_chat: false,
                highlighted_line: None,
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

    fn open_suggestion(&mut self, line: u32, cx: &mut Context<Self>) {
        self.highlighted_line = Some(line);
        self.show_chat = true;
        cx.notify();
    }

    fn send_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.chat_input.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        self.chat_messages.push(ChatMessage {
            from: ChatSender::You,
            text: text.into(),
        });
        // Static placeholder reply — no real chat backend is wired up yet.
        self.chat_messages.push(ChatMessage {
            from: ChatSender::Agent,
            text: "Recomputing only on diagnostic_changed would fix that — want me to draft \
                the patch?"
                .into(),
        });
        self.chat_input.update(cx, |editor, cx| {
            editor.clear(window, cx);
        });
        cx.notify();
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

    fn render_intent_card(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let intent = &self.intent;

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
                        .child(Label::new(intent.label.clone()).size(LabelSize::Default))
                        .child(
                            Label::new(format!("{}%", intent.confidence_percent))
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        ),
                )
                .child(
                    div().mb_3().child(
                        ProgressBar::new(
                            "engine_intent_confidence",
                            intent.confidence_percent as f32,
                            100.,
                            cx,
                        )
                        .bg_color(colors.border)
                        .fg_color(colors.text_accent),
                    ),
                )
                .child(v_flex().gap_1().children(intent.evidence.iter().map(|e| {
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new("›")
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        )
                        .child(
                            Label::new(e.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                }))),
        )
    }

    fn render_suggestions(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();

        div().px_4().pt_5().child(
            v_flex()
                .gap_2()
                .child(
                    Label::new("Suggestions")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    div()
                        .id("engine_suggestions")
                        .overflow_y_scroll()
                        .child(v_flex().gap_2().children(self.suggestions.iter().map(
                            |suggestion| {
                                let line = suggestion.line;
                                let is_highlighted = self.highlighted_line == Some(line);
                                h_flex()
                                    .id(("engine_suggestion", line as u64))
                                    .flex_col()
                                    .items_start()
                                    .gap_1()
                                    .p_3()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(if is_highlighted {
                                        colors.border_selected
                                    } else {
                                        colors.border
                                    })
                                    .bg(if is_highlighted {
                                        colors.element_selected
                                    } else {
                                        colors.panel_background
                                    })
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.open_suggestion(line, cx);
                                    }))
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .child(
                                                Label::new(format!("LN {}", line))
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Accent),
                                            )
                                            .child(
                                                Label::new(suggestion.age.clone())
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted),
                                            ),
                                    )
                                    .child(
                                        Label::new(suggestion.text.clone())
                                            .size(LabelSize::Small),
                                    )
                                    .child(
                                        Label::new("Discuss →")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Accent),
                                    )
                            },
                        ))),
                ),
        )
    }

    fn render_chat(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();

        v_flex()
            .flex_1()
            .min_h_0()
            .mt_4()
            .px_4()
            .pb_3()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        Label::new("Ask about this")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .id("engine_chat_close")
                            .cursor_pointer()
                            .child(Label::new("✕").size(LabelSize::Small).color(Color::Muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_chat = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("engine_chat_messages")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().gap_3().py_1().children(self.chat_messages.iter().map(
                        |message| {
                            let (from_label, is_you) = match message.from {
                                ChatSender::Agent => ("agent", false),
                                ChatSender::You => ("you", true),
                            };
                            v_flex()
                                .when(is_you, |this| this.items_end())
                                .when(!is_you, |this| this.items_start())
                                .child(
                                    Label::new(from_label)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    div()
                                        .rounded_lg()
                                        .px_3()
                                        .py_2()
                                        .max_w(relative(0.92))
                                        .bg(if is_you {
                                            colors.text_accent
                                        } else {
                                            colors.element_background
                                        })
                                        .child(
                                            Label::new(message.text.clone())
                                                .size(LabelSize::Small)
                                                .color(if is_you {
                                                    Color::Custom(gpui::white())
                                                } else {
                                                    Color::Default
                                                }),
                                        ),
                                )
                        },
                    ))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_2()
                    .child(self.chat_input.clone())
                    .child(Button::new("engine_chat_send", "Send").on_click(cx.listener(
                        |this, _, window, cx| {
                            this.send_chat(window, cx);
                        },
                    ))),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_background = cx.theme().colors().panel_background;
        let header = self.render_header(cx);
        let intent_card = self.render_intent_card(cx);
        let suggestions = self.render_suggestions(cx);
        let chat = self.show_chat.then(|| self.render_chat(window, cx));

        v_flex()
            .key_context("EnginePanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(panel_background)
            .child(header)
            .child(intent_card)
            .child(suggestions)
            .children(chat)
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
