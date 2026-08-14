//! Part A: the periodic feedback nudge's UI — a small, dismissible,
//! non-blocking toast (`Correct` / `Wrong`, with a `Wrong` follow-up asking
//! which of the currently-implemented intents it should have been). Built
//! by `EnginePanel` in reaction to `NucleusEvent::FeedbackNudgeRequested`;
//! `NucleusEngine` itself renders nothing (see the crate's module doc
//! comment: "produces no suggestions").
//!
//! Self-contained once constructed: it holds its own cloned `NucleusLogger`
//! and the `PredictionId`/`DeveloperIntent` snapshot from nudge time, and
//! logs feedback directly — no callback into `NucleusEngine` needed.

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Window,
};
use ui::prelude::*;
use workspace::{ToastAction, ToastView};

use crate::{DeveloperIntent, Feedback, NucleusLogger, PredictionId};

/// The intents a user can pick from when correcting a prediction — only the
/// ones with real classifier logic. Offering the 4 still-stubbed intents
/// (Refactoring, Reviewing, Documenting, Planning — always scored 0, never
/// actually predicted) would be misleading. Testing/Exploring/Configuring
/// joined the real-logic set in the classifier-expansion session (Parts
/// B/C/D) — see `nucleus.rs`'s `classify` doc comment.
const CORRECTABLE_INTENTS: [DeveloperIntent; 7] = [
    DeveloperIntent::Debugging,
    DeveloperIntent::Implementing,
    DeveloperIntent::Testing,
    DeveloperIntent::Exploring,
    DeveloperIntent::Configuring,
    DeveloperIntent::Idle,
    DeveloperIntent::ConsultingAgent,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    AskingCorrect,
    AskingActualIntent,
}

pub struct FeedbackNudgeToast {
    logger: NucleusLogger,
    prediction_id: PredictionId,
    predicted_intent: DeveloperIntent,
    stage: Stage,
    focus_handle: FocusHandle,
}

impl FeedbackNudgeToast {
    pub fn new(
        logger: NucleusLogger,
        prediction_id: PredictionId,
        predicted_intent: DeveloperIntent,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| Self {
            logger,
            prediction_id,
            predicted_intent,
            stage: Stage::AskingCorrect,
            focus_handle: cx.focus_handle(),
        })
    }

    fn log_and_dismiss(
        &mut self,
        correct: bool,
        actual_intent: Option<DeveloperIntent>,
        cx: &mut Context<Self>,
    ) {
        self.logger.log_feedback(&Feedback {
            prediction_id: self.prediction_id,
            correct,
            actual_intent,
        });
        cx.emit(DismissEvent);
    }
}

impl Render for FeedbackNudgeToast {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = h_flex()
            .id("feedback-nudge-toast")
            .elevation_3(cx)
            .gap_2()
            .py_1p5()
            .px_2p5()
            .flex_none()
            .bg(cx.theme().colors().surface_background)
            .shadow_lg();

        match self.stage {
            Stage::AskingCorrect => container
                .child(Label::new(format!(
                    "Nucleus thinks you're {}. Is that right?",
                    self.predicted_intent.label()
                )))
                .child(
                    Button::new("feedback-correct", "Correct")
                        .color(Color::Success)
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.log_and_dismiss(true, None, cx);
                        })),
                )
                .child(
                    Button::new("feedback-wrong", "Wrong")
                        .color(Color::Error)
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.stage = Stage::AskingActualIntent;
                            cx.notify();
                        })),
                ),
            Stage::AskingActualIntent => container
                .child(Label::new("What was it instead?"))
                .children(CORRECTABLE_INTENTS.into_iter().map(|intent| {
                    Button::new(("feedback-actual-intent", intent as usize), intent.label())
                        .color(Color::Muted)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.log_and_dismiss(false, Some(intent), cx);
                        }))
                })),
        }
    }
}

impl ToastView for FeedbackNudgeToast {
    fn action(&self) -> Option<ToastAction> {
        None
    }

    fn auto_dismiss(&self) -> bool {
        // Matches the "if dismissed or ignored, nothing happens" requirement
        // for free: timing out just hides the toast (`ToastLayer`'s own
        // behavior) without emitting anything from this view, so no feedback
        // gets logged unless a button was actually clicked.
        true
    }
}

impl Focusable for FeedbackNudgeToast {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FeedbackNudgeToast {}
