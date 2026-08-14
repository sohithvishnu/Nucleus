# Phase 6 design notes: interruption policy (not this session)

Captured during the classifier-expansion session (Part E, prior-art validation) so
this doesn't get lost before a future phase actually builds an interruption/nudge
policy on top of `DeveloperIntent`. Nothing here is built yet — no code in this
session reads or acts on any of this. This is a landing pad for that later work, not
a spec for it.

## Why this matters for a future interruption policy

The current classifier (`NucleusEngine`/`classify` in `crates/nucleus_intent/src/
nucleus.rs`) only infers *what* the developer is probably doing. A Phase 6
interruption/notification policy would need a second, related judgment: *how costly
would an interruption be right now*, which is not the same question — someone can be
confidently `Debugging` and still be at a natural pause point, or confidently
`Implementing` while mid-way through a change that would be expensive to lose context
on.

## Relevant prior art

- **Fogarty et al., sensor-based interruptibility work** (the line of HCI research
  using low-cost sensors — keyboard/mouse activity, audio, etc. — to predict
  self-reported interruptibility in office/programming settings). The general
  finding relevant here: simple, cheaply-observable activity signals (many of which
  `NucleusEngine` already collects — edit/save/navigation events, focus state) can be
  meaningfully predictive of interruption cost, without needing anything more
  invasive than what's already being watched.
- **FlowLight (Züger, Fritz, et al.), field study of an IDE-driven "do not disturb"
  signal.** Inferred a developer's flow/focus state from IDE activity and surfaced it
  as a simple, ambient signal (a desk light) to coworkers, deployed at scale, and
  found it measurably reduced unwanted interruptions and correlated with improved
  self-reported flow. The relevant takeaway for Nucleus: the *signal* driving the
  policy doesn't need to be exotic — IDE-observable activity of the same general kind
  `NucleusEngine` already tracks was sufficient in a real deployment.

## Programming states most costly to interrupt (per this research direction)

Per the session prompt's own framing of this research, the states most worth
protecting from interruption are:

- **Edits with concurrent multi-location changes** — mid-refactor, touching several
  places at once; losing the mental map of what's been changed and what hasn't is
  expensive to reconstruct.
- **Navigation/search activity** — actively hunting for something (a definition, a
  usage, the source of a bug); this is also exactly the shape Part C's new
  `DeveloperIntent::Exploring` is meant to detect, which is a natural, already-existing
  hook a future interruption policy could read from.
- **Comprehending control flow** — reading/tracing through logic to build
  understanding, not obviously visible as "activity" at all (may look like a pause),
  which is a real tension with any policy that uses raw activity level as a costliness
  proxy.
- **IDE window unfocused** — somewhat counterintuitively also flagged as high-cost in
  this line of research (the person has likely moved their attention elsewhere
  entirely — a different kind of context that's also expensive to interrupt out of,
  not necessarily an invitation to interrupt just because Zed itself isn't active).

## Open questions for whoever picks this up

- Whether "interruption cost" should be its own model/score (separate from
  `DeveloperIntent`) or derived from the existing intent + confidence + a few
  additional signals (e.g. `SessionState::cursor_at_diagnostic`, multi-file edit
  spread already tracked via `RecentActions::file_switches`).
- Whether `focused_pane`/window-focus signals (already collected, `Part C`) are a
  good enough proxy for "window unfocused = costly to interrupt," or whether that
  needs its own dedicated tracking.
- How this interacts with the existing feedback-nudge mechanism
  (`FEEDBACK_NUDGE_INTERVAL`/`maybe_request_feedback_nudge`) — that nudge is itself an
  interruption today, gated only on "not Idle, not ConsultingAgent, not already asked
  about this prediction." A real interruption-cost model would presumably want to
  gate the nudge *harder*, not just decide separately whether some other
  hypothetical Phase 6 notification should fire.
