//! Phase 4b-1: plain-shell command start/finish detection, observation only.
//!
//! This fork has no OSC 133 (shell-integration) support anywhere in its
//! terminal stack (`crates/terminal` → `alacritty_terminal` → `vte`), and —
//! confirmed by reading `vte`'s own `ansi::Processor::osc_dispatch` directly
//! — an *unrecognized* OSC number's payload is silently discarded inside
//! that parser before it ever reaches anything this crate (or even
//! `alacritty_terminal`'s own `Handler` trait) could observe. So an
//! OSC-wrapped marker would be invisible on screen (true of any OSC
//! sequence, by construction of the VT parsing spec) but also completely
//! undetectable here, short of patching that forked, externally-hosted
//! parser — judged high-risk and out of scope, same call the original
//! Phase 4b-1 investigation made.
//!
//! Given that, this module's markers go to a **dedicated per-terminal file**
//! (`__nucleus_marker_file`, redirected via plain shell `>>`), not to the
//! terminal's own stdout. This was a deliberate revision from this
//! module's original design, which streamed markers through stdout and
//! scanned the terminal's own rendered lines for them (mirroring
//! `crates/terminal`'s `INIT_COMMAND_STARTUP_MARKER_PREFIX`/`_SUFFIX` plus
//! `Event::Wakeup`-driven scanning) — that worked, but meant every
//! detected command printed two marker lines directly into the user's
//! visible terminal output, forever, which real usage surfaced as
//! unacceptable clutter. Writing to a file instead of stdout sidesteps the
//! whole OSC/visibility tradeoff above: nothing related to detection ever
//! touches the terminal's rendered content, so there's no visibility
//! problem to solve, and no `Event::Wakeup`/terminal-render dependency
//! either — `NucleusEngine` tails the file on its own poll cycle instead
//! (see `TerminalCommandWatcher::poll_marker_file`), trading near-instant
//! detection for up to one `PRUNE_INTERVAL` of latency, the same tradeoff
//! this crate already accepts for task-terminal detection.
//!
//! Deliberately narrow (4b-1 of a 3-part plan): detects and logs command
//! start/finish only. Feeds `DeveloperIntent::Testing` (the
//! classifier-expansion session's Part B) via command categorization below,
//! and — since the terminal-engagement session — also `Debugging`/
//! `Exploring` via [`TerminalCommandWatcher::last_completed_command`], the
//! most recently completed command's category and exit code for a specific
//! terminal (see `nucleus::classify`'s doc comment for how that's routed).
//!
//! ## Marker anti-collision, mirroring the existing init-command marker
//!
//! [`crates/terminal`'s own `init_command_startup_marker_command`] documents
//! the same problem this module has: writing the *literal source* of a
//! marker-emitting command into an echoing PTY would echo that literal text
//! back before it ever runs. The fix there — and the one mirrored here — is
//! to pass the marker's pieces as *separate, space-delimited* arguments to
//! `printf '%s%s%s\n' PREFIX PAYLOAD SUFFIX`: the echoed, as-typed command
//! line has whitespace between the pieces (so it can't match a search for
//! the contiguous marker), while the format string itself has none, so only
//! the command's actual *output* is ever contiguous. This anti-collision
//! concern is much less important now that markers are file-only rather
//! than shared with visible terminal text, but the strict anchored parse
//! (a scanned line must be nothing but `PREFIX<base64><SUFFIX>`) is kept
//! regardless — it costs nothing and the marker file could in principle
//! still pick up stray content if e.g. a user's own script happened to
//! write to it directly.

use base64::Engine as _;
use collections::{HashMap, HashSet};
use gpui::EntityId;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use util::shell::ShellKind;

pub const CMD_START_MARKER_PREFIX: &str = "__nucleus_cmd_start__";
pub const CMD_START_MARKER_SUFFIX: &str = "__";
pub const CMD_END_MARKER_PREFIX: &str = "__nucleus_cmd_end__";
pub const CMD_END_MARKER_SUFFIX: &str = "__";

/// The POSIX-shell hook script. Installed by writing this content to a
/// temp file and sourcing that file with a single-line command (see
/// [`hook_install_command`]) into any terminal whose `ShellKind` is
/// `Posix` (bash, zsh, and plain `sh` all report as `Posix` in this
/// codebase's `ShellKind` — see `util::shell::ShellKind` — so this single
/// script self-detects bash vs. zsh at *shell* runtime via
/// `$BASH_VERSION`/`$ZSH_VERSION` rather than relying on Zed's own,
/// coarser-grained classification).
///
/// This is never streamed directly into the PTY as raw multi-line text:
/// writing bytes with embedded newlines into an interactive shell's PTY is
/// indistinguishable, from the shell's line-editor's perspective, from the
/// user typing each line and pressing enter — which fragments a multi-line
/// `if`/`elif`/`case` construct like this one across separate line-editor
/// submissions and, if it races the shell's own startup (rc files, `conda
/// init`, etc.) still running, can interleave with that output and produce
/// real parse errors. A single physical line sourcing a temp file has
/// neither problem: it's one line-editor submission no matter when it
/// lands, and shell syntax for `. file` (or `source file` for fish) can't
/// itself be malformed by interleaving.
///
/// `__nucleus_skip_next_end_marker` suppresses precmd's very first firing:
/// the hooks become active the instant this script finishes sourcing, and
/// the *installing* command (the `. file` / `source file` line itself) is
/// still in flight when that happens — its own completion is the next
/// thing precmd sees, which would otherwise write a spurious end marker for
/// a "command" that was really just hook installation. Kept even though
/// markers no longer print to the visible terminal (see the module doc
/// comment) — a spurious marker in the file is harmless either way (nothing
/// would be pending to match it against), but there's no reason to write
/// one that's known-meaningless.
///
/// `$__nucleus_marker_file` (set by the single-line command that sources
/// this script — see `hook_install_command`) is where `printf` writes both
/// markers, via plain `>>` append redirection — never to this shell's own
/// stdout. A local (non-exported) variable is enough: `__nucleus_preexec`/
/// `__nucleus_precmd` run in this same shell process, not a subshell, so
/// they see it without `export`.
///
/// zsh branch uses `add-zsh-hook` when available (zsh's own official,
/// designed-to-compose mechanism — the same one starship/oh-my-zsh use),
/// falling back to direct `precmd_functions`/`preexec_functions` array
/// appends otherwise. Both append rather than prepend, so existing hooks
/// keep running and keep their relative order; the one caveat (documented in
/// the report, not hidden here) is that `$?`  capture in our `precmd` is
/// only reliable if no earlier-registered `precmd` hook itself runs a
/// command before ours executes — a real, inherent limitation of composing
/// with other zsh tools via a shared array, not something this hook can
/// fully close.
///
/// bash has no equivalent array for `DEBUG` traps (it's a single global
/// slot), so this captures whatever trap is already installed *once* at
/// injection time and chains it — composes with anything already present
/// *before* injection, but can't retroactively chain a trap some other tool
/// installs *after* us later in the session. `PROMPT_COMMAND` is prepended
/// (required so our exit-code capture runs before anything else in it can
/// change `$?`), not appended — the existing contents are preserved, just
/// reordered to run after ours.
///
/// `trap ... DEBUG` is deliberately the *last* statement in the bash
/// branch. Once armed it applies to every subsequent simple command bash
/// executes, including sibling statements still left in the same script —
/// not just commands submitted later — so anything after it (the
/// `PROMPT_COMMAND` `case`, the temp-file cleanup, `clear`) would otherwise
/// immediately trigger `preexec` on itself and print a spurious start
/// marker. `__nucleus_preexec`'s guard also checks the exact literal
/// `__nucleus_precmd` (not `$PROMPT_COMMAND` as a whole) for the same
/// reason: bash evaluates `$PROMPT_COMMAND` one `;`-separated piece at a
/// time, so `$BASH_COMMAND` during our own piece's execution is just
/// `__nucleus_precmd`, never the full `$PROMPT_COMMAND` string — comparing
/// to the whole string only worked when nothing else had ever set
/// `$PROMPT_COMMAND` before us, which doesn't hold for most real shells
/// (fancy prompts, direnv, starship, etc. all set it).
///
/// `__nucleus_hook_path` (removal of the now-unneeded temp file, and the
/// `clear` that hides this whole installation from the visible scrollback)
/// is likewise handled *inside* the script rather than appended to the
/// outer command that sources it, for the same sibling-command reason —
/// see `hook_install_command`.
pub const POSIX_HOOK_SCRIPT: &str = r#"if [ -n "$ZSH_VERSION" ]; then
  __nucleus_skip_next_end_marker=1
  __nucleus_preexec() { printf '%s%s%s\n' __nucleus_cmd_start__ "$(printf '%s' "$1" | base64 | tr -d '\n')" __ >> "$__nucleus_marker_file" 2>/dev/null; }
  __nucleus_precmd() { __ne=$?; if [ -n "${__nucleus_skip_next_end_marker:-}" ]; then unset __nucleus_skip_next_end_marker; return; fi; printf '%s%s%s\n' __nucleus_cmd_end__ "$__ne" __ >> "$__nucleus_marker_file" 2>/dev/null; }
  if typeset -f add-zsh-hook >/dev/null 2>&1; then
    add-zsh-hook preexec __nucleus_preexec
    add-zsh-hook precmd __nucleus_precmd
  else
    autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook preexec __nucleus_preexec && add-zsh-hook precmd __nucleus_precmd || { preexec_functions+=(__nucleus_preexec); precmd_functions+=(__nucleus_precmd); }
  fi
  [ -n "${__nucleus_hook_path:-}" ] && rm -f "$__nucleus_hook_path" 2>/dev/null
  unset __nucleus_hook_path
  clear
elif [ -n "$BASH_VERSION" ]; then
  __nucleus_skip_next_end_marker=1
  __nucleus_preexec() { [ -n "$COMP_LINE" ] && return; [ "$BASH_COMMAND" = "__nucleus_precmd" ] && return; printf '%s%s%s\n' __nucleus_cmd_start__ "$(printf '%s' "$BASH_COMMAND" | base64 | tr -d '\n')" __ >> "$__nucleus_marker_file" 2>/dev/null; }
  __nucleus_precmd() { __ne=$?; if [ -n "${__nucleus_skip_next_end_marker:-}" ]; then unset __nucleus_skip_next_end_marker; return; fi; printf '%s%s%s\n' __nucleus_cmd_end__ "$__ne" __ >> "$__nucleus_marker_file" 2>/dev/null; }
  if [ -z "${__nucleus_prev_debug_trap+x}" ]; then
    __nucleus_prev_debug_trap="$(trap -p DEBUG | sed -e "s/^trap -- '//" -e "s/' DEBUG$//")"
  fi
  __nucleus_chained_preexec() { __nucleus_preexec; [ -n "$__nucleus_prev_debug_trap" ] && eval "$__nucleus_prev_debug_trap"; }
  case ";$PROMPT_COMMAND;" in *";__nucleus_precmd;"*) ;; *) PROMPT_COMMAND="__nucleus_precmd;${PROMPT_COMMAND}" ;; esac
  [ -n "${__nucleus_hook_path:-}" ] && rm -f "$__nucleus_hook_path" 2>/dev/null
  unset __nucleus_hook_path
  clear
  trap '__nucleus_chained_preexec' DEBUG
fi
"#;

/// The fish hook script. Fish's `--on-event` mechanism natively supports
/// multiple functions subscribing to the same event with no clobbering
/// (unlike bash's single `DEBUG` trap slot) — the safest of the three
/// shells for this. `function` redefinition is also idempotent, so
/// re-injecting this (e.g. if this module's own `injected` tracking were
/// ever lost) is harmless. The same `$status`-capture-ordering caveat as
/// zsh's `precmd_functions` applies in principle (an earlier-registered
/// `fish_prompt` handler that itself runs a command before ours fires could
/// stale our capture), though fish prompt tooling generally captures
/// `$status` as its first action by convention, same as here.
///
/// `__nucleus_skip_next_end_marker` mirrors the POSIX script's: it
/// suppresses the first `fish_prompt` firing, which happens right after
/// hook installation finishes and would otherwise write a spurious end
/// marker for the installing command itself. `$__nucleus_marker_file` is
/// set as a global (`set -g`) by the single-line command that sources this
/// script (see `hook_install_command`) — needs `-g` since `__nucleus_
/// preexec`/`__nucleus_precmd` are fish functions with their own local
/// scope, unlike POSIX shells where a plain variable is already visible to
/// functions defined in the same shell.
pub const FISH_HOOK_SCRIPT: &str = r#"set -g __nucleus_skip_next_end_marker 1
function __nucleus_preexec --on-event fish_preexec
    printf '%s%s%s\n' __nucleus_cmd_start__ (echo -n $argv[1] | base64 | tr -d '\n') __ >> "$__nucleus_marker_file" 2>/dev/null
end
function __nucleus_precmd --on-event fish_prompt
    set -l __nucleus_exit_status $status
    if set -q __nucleus_skip_next_end_marker
        set -e __nucleus_skip_next_end_marker
        return
    end
    printf '%s%s%s\n' __nucleus_cmd_end__ $__nucleus_exit_status __ >> "$__nucleus_marker_file" 2>/dev/null
end
"#;

/// The shell hook script for `shell_kind`, or `None` for shells this session
/// doesn't cover (PowerShell, Pwsh, Cmd, Nushell, Xonsh, Elvish, Csh, Tcsh,
/// Rc — only bash/zsh (via `Posix`) and Fish are in scope per the session
/// prompt's "at least bash, zsh, fish").
pub fn shell_hook_script(shell_kind: ShellKind) -> Option<&'static str> {
    match shell_kind {
        ShellKind::Posix => Some(POSIX_HOOK_SCRIPT),
        ShellKind::Fish => Some(FISH_HOOK_SCRIPT),
        _ => None,
    }
}

/// Builds the single physical line of input that installs the hook: source
/// `script_path` (which the caller must already have written the relevant
/// `*_HOOK_SCRIPT` content to), after making `marker_file_path` available
/// to it as `$__nucleus_marker_file`. `. ` (dot) is used for POSIX shells
/// and `source` for fish — the two are not interchangeable in fish. Ends
/// with a bare `\r` (not `\r\n`, matching the convention already used for
/// programmatic PTY input elsewhere in this codebase — see
/// `AgentPanel::terminal_init_command_input`) so it submits as one line
/// regardless of shell.
///
/// `marker_file_path` itself doesn't need to exist yet — `>>` redirection
/// creates it on first write, and the read side
/// (`TerminalCommandWatcher::poll_marker_file`) already treats a missing
/// file as "no new lines" rather than an error.
///
/// For fish (safe: its hooks are event-based, not per-simple-command),
/// temp-file cleanup and a `clear` are appended directly to this same line
/// — installation happens shortly after a terminal is created (see
/// `mark_observed` in `TerminalCommandWatcher`), the same window Zed's own
/// init-command mechanism already treats as safe to clear. For POSIX
/// shells that cleanup happens *inside* the sourced script instead (see
/// `POSIX_HOOK_SCRIPT`'s doc comment for why: bash's `DEBUG` trap, once
/// armed, applies to sibling commands still left in the very line that
/// armed it) — this line instead sets `__nucleus_hook_path` so the script
/// knows what to remove. `clear` here is about hiding this install line's
/// own echo (still visible — it's genuinely typed into the PTY), an
/// entirely separate concern from marker visibility, which this whole
/// file-redirection design already solves independently.
///
/// Callers must only invoke this for a `shell_kind` where
/// [`shell_hook_script`] returns `Some` — for any other shell kind this
/// still returns a POSIX-style line, which is meaningless input for an
/// unsupported shell.
pub fn hook_install_command(
    shell_kind: ShellKind,
    script_path: &Path,
    marker_file_path: &Path,
) -> Vec<u8> {
    // Paths here are always our own generated temp-file paths in practice
    // (never attacker- or user-controlled), so they can never actually
    // contain a newline — but stripping CR/LF costs nothing and makes the
    // single-line guarantee hold unconditionally rather than resting on
    // that assumption.
    let path = script_path.display().to_string().replace(['\n', '\r'], "");
    let marker_path = marker_file_path
        .display()
        .to_string()
        .replace(['\n', '\r'], "");
    let line = match shell_kind {
        ShellKind::Fish => format!(
            "set -g __nucleus_marker_file '{marker_path}'; source '{path}' 2>/dev/null; \
            rm -f '{path}'; clear"
        ),
        _ => format!(
            "__nucleus_hook_path='{path}'; __nucleus_marker_file='{marker_path}'; \
            . '{path}' 2>/dev/null"
        ),
    };
    let mut input = line.into_bytes();
    input.push(b'\r');
    input
}

/// Parses a scanned line as a command-start marker, returning the decoded
/// command text. Strictly anchored (the whole trimmed line must be exactly
/// `PREFIX<base64><SUFFIX>`) so it can't accidentally match unrelated
/// on-screen text that merely contains the prefix as a substring — see the
/// module doc comment.
pub fn parse_start_marker(line: &str) -> Option<String> {
    let line = line.trim();
    let payload = line
        .strip_prefix(CMD_START_MARKER_PREFIX)?
        .strip_suffix(CMD_START_MARKER_SUFFIX)?;
    // An empty payload is a legitimate encoding of an empty command (base64
    // of "" is ""), not a sign of a malformed line — the anchoring above
    // (exact prefix, then only base64 characters, then exact suffix) is
    // already what makes a coincidental match astronomically unlikely, with
    // or without a payload in between. Rejecting empty payloads here used
    // to silently drop real empty-command markers; found by
    // `start_marker_round_trip_holds_for_arbitrary_strings`.
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    String::from_utf8(decoded).ok()
}

/// Parses a scanned line as a command-finish marker, returning the exit code.
pub fn parse_end_marker(line: &str) -> Option<i32> {
    let line = line.trim();
    let payload = line
        .strip_prefix(CMD_END_MARKER_PREFIX)?
        .strip_suffix(CMD_END_MARKER_SUFFIX)?;
    payload.parse::<i32>().ok()
}

/// Coarse, non-authoritative bucket for a detected command. Fed into
/// `classify()` two ways: `has_pending_command_of_category` (in-flight,
/// `DeveloperIntent::Testing`) and, since the terminal-engagement session,
/// [`LastCommandOutcome::category`] (most recently *completed* command,
/// `DeveloperIntent::Debugging`/`Testing`/`Exploring`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandCategory {
    Test,
    Build,
    Git,
    Package,
    Lint,
    Other,
}

impl CommandCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Build => "build",
            Self::Git => "git",
            Self::Package => "package",
            Self::Lint => "lint",
            Self::Other => "other",
        }
    }
}

/// Buckets `command` by simple prefix/substring matching on its first word —
/// deliberately not exhaustive; easy to extend later. Matches against the
/// *pre-redaction* command, per the session prompt (a command name is never
/// itself the secret-shaped part `redact_command` targets, so running
/// categorization first and redaction second on the same input is safe).
pub fn categorize_command(command: &str) -> CommandCategory {
    let first_word = command.split_whitespace().next().unwrap_or("");
    let has_subcommand = |needle: &str| command.split_whitespace().any(|word| word == needle);

    if first_word == "git" {
        return CommandCategory::Git;
    }
    if matches!(first_word, "pytest" | "jest" | "rspec" | "phpunit") {
        return CommandCategory::Test;
    }
    if matches!(
        first_word,
        "eslint" | "flake8" | "rubocop" | "clippy" | "prettier"
    ) {
        return CommandCategory::Lint;
    }
    if matches!(first_word, "make" | "cmake" | "ninja" | "gradle" | "mvn") {
        return CommandCategory::Build;
    }

    // These wrap both package-management and test/build subcommands
    // (`npm test`, `cargo build`, `go test`) — check the subcommand word
    // before falling back.
    if matches!(
        first_word,
        "cargo" | "npm" | "yarn" | "pnpm" | "go" | "pip" | "pip3" | "bundle" | "gem" | "composer"
    ) {
        if has_subcommand("test") {
            return CommandCategory::Test;
        }
        if has_subcommand("clippy") {
            return CommandCategory::Lint;
        }
        if has_subcommand("build") || has_subcommand("check") {
            return CommandCategory::Build;
        }
        if has_subcommand("add") || has_subcommand("install") {
            return CommandCategory::Package;
        }
        return CommandCategory::Other;
    }

    CommandCategory::Other
}

/// Regex-based redaction of obviously-secret-shaped substrings. Not
/// exhaustive — a small, clearly-named, easy-to-extend allowlist:
///
/// - `--token=...` / `--password=...` (any value up to the next whitespace)
/// - `Authorization: Bearer <token>`
/// - AWS-style access key IDs (`AKIA[0-9A-Z]{16}`)
/// - generic `key=`/`secret=`/`pass=` assignments with a long (8+ char)
///   opaque value
static REDACTION_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (
            Regex::new(r#"(?i)(--token=)[^\s'"]+"#).unwrap(),
            "${1}[REDACTED]",
        ),
        (
            Regex::new(r#"(?i)(--password=)[^\s'"]+"#).unwrap(),
            "${1}[REDACTED]",
        ),
        (
            Regex::new(r#"(?i)(Authorization:\s*Bearer\s+)[^\s'"]+"#).unwrap(),
            "${1}[REDACTED]",
        ),
        (Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(), "[REDACTED]"),
        (
            Regex::new(r"(?i)\b((?:key|secret|pass)=)[A-Za-z0-9+/_.\-]{8,}\b").unwrap(),
            "${1}[REDACTED]",
        ),
    ]
});

/// Masks obviously-secret-shaped substrings in `command` before it's ever
/// written to a log line. See `REDACTION_PATTERNS` for exactly which
/// patterns are covered.
pub fn redact_command(command: &str) -> String {
    let mut result = command.to_string();
    for (pattern, replacement) in REDACTION_PATTERNS.iter() {
        result = pattern.replace_all(&result, *replacement).into_owned();
    }
    result
}

struct PendingCommand {
    command: String,
    started_at: Instant,
}

/// A command's start or finish, as detected from marker scanning. `command`
/// is the raw (not-yet-redacted) text — callers must run it through
/// `redact_command` before logging.
pub enum TerminalCommandOutcome {
    Started {
        command: String,
    },
    Finished {
        command: String,
        exit_code: i32,
        duration: Duration,
    },
}

/// A completed command's category and exit code — deliberately small (no
/// command text, no timestamp): this exists to answer exactly one question
/// for `nucleus::classify`'s terminal-engagement signal, "how did the most
/// recent command in this terminal turn out," not to duplicate the fuller
/// `TerminalCommandOutcome::Finished` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastCommandOutcome {
    pub category: CommandCategory,
    pub exit_code: i32,
}

/// Read-side state for a single terminal's marker file: where it is, and
/// how many bytes of it have already been consumed.
struct MarkerFileReader {
    path: PathBuf,
    bytes_read: u64,
}

/// Per-terminal state for hook injection and marker-driven command tracking.
/// Plain data — no GPUI involved, so this is unit-testable in isolation
/// (`scan_lines` in particular, which is the actual detection logic).
#[derive(Default)]
pub struct TerminalCommandWatcher {
    injected: HashSet<EntityId>,
    /// Terminals seen (needing injection) on some earlier poll tick, but not
    /// yet injected — see `mark_observed`.
    observed: HashSet<EntityId>,
    pending: HashMap<EntityId, PendingCommand>,
    /// Where each injected terminal's markers land, and how much of that
    /// file has already been read — see `set_marker_file`/`poll_marker_file`.
    marker_files: HashMap<EntityId, MarkerFileReader>,
    /// The most recently *completed* command's category and exit code, per
    /// terminal — separate from `pending` above (which only tracks a
    /// command currently in flight). Only ever holds the single latest
    /// outcome per terminal; an earlier one is overwritten, never
    /// accumulated. See `last_completed_command`.
    last_completed: HashMap<EntityId, LastCommandOutcome>,
}

impl TerminalCommandWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn needs_injection(&self, terminal_id: EntityId) -> bool {
        !self.injected.contains(&terminal_id)
    }

    /// Records that `terminal_id` needs injection as of this poll tick.
    /// Returns `true` the *first* time it's called for a given terminal —
    /// callers should wait for a later tick before actually injecting, so a
    /// freshly-opened terminal's own shell startup (rc files, `conda init`,
    /// etc.) has at least one full poll interval to finish before anything
    /// is written to its PTY. Returns `false` on every subsequent call,
    /// signaling the terminal has now waited out at least one interval and
    /// injection may proceed.
    pub fn mark_observed(&mut self, terminal_id: EntityId) -> bool {
        self.observed.insert(terminal_id)
    }

    pub fn mark_injected(&mut self, terminal_id: EntityId) {
        self.injected.insert(terminal_id);
        self.observed.remove(&terminal_id);
    }

    /// Drops state for terminals that no longer exist, so `injected`/
    /// `observed`/`pending`/`marker_files`/`last_completed` don't grow
    /// unboundedly across a long session of opening and closing terminals —
    /// and best-effort deletes each dropped terminal's now-unneeded marker
    /// file from disk, so those don't accumulate either.
    pub fn prune(&mut self, live_terminal_ids: &HashSet<EntityId>) {
        self.injected.retain(|id| live_terminal_ids.contains(id));
        self.observed.retain(|id| live_terminal_ids.contains(id));
        self.pending.retain(|id, _| live_terminal_ids.contains(id));
        self.last_completed
            .retain(|id, _| live_terminal_ids.contains(id));
        self.marker_files.retain(|id, reader| {
            let keep = live_terminal_ids.contains(id);
            if !keep && let Err(error) = std::fs::remove_file(&reader.path) {
                log::debug!("failed to remove nucleus terminal marker file: {error}");
            }
            keep
        });
    }

    /// Records where `terminal_id`'s markers will be written, so
    /// `poll_marker_file` knows what to read. Called once per terminal, at
    /// injection time, alongside `mark_injected`.
    pub fn set_marker_file(&mut self, terminal_id: EntityId, path: PathBuf) {
        self.marker_files.insert(
            terminal_id,
            MarkerFileReader {
                path,
                bytes_read: 0,
            },
        );
    }

    /// Reads whatever *complete* lines have been appended to `terminal_id`'s
    /// marker file since the last call, and feeds them through
    /// [`Self::scan_lines`] exactly as if they'd come from the terminal's
    /// own rendered output — same detection logic, different source.
    ///
    /// A missing file (nothing written yet, or the terminal was never
    /// injected/doesn't support hooks) is treated as "no new lines," not an
    /// error — this is polled unconditionally on every tick for every
    /// tracked terminal, and emptiness is the overwhelmingly common case.
    ///
    /// Only consumes up through the last `\n` in the newly-read bytes: a
    /// partial final line means the shell is still mid-`printf`, and
    /// re-reading it whole once the newline lands next tick is simpler and
    /// safer than tracking partial-line state across polls.
    pub fn poll_marker_file(&mut self, terminal_id: EntityId) -> Vec<TerminalCommandOutcome> {
        let Some(reader) = self.marker_files.get(&terminal_id) else {
            return Vec::new();
        };
        let path = reader.path.clone();
        let bytes_read = reader.bytes_read;

        let Ok(contents) = std::fs::read(&path) else {
            return Vec::new();
        };
        if (contents.len() as u64) <= bytes_read {
            return Vec::new();
        }
        let new_bytes = &contents[bytes_read as usize..];
        let Some(last_newline) = new_bytes.iter().rposition(|&byte| byte == b'\n') else {
            return Vec::new();
        };
        let consumed_len = last_newline + 1;
        let text = String::from_utf8_lossy(&new_bytes[..consumed_len]).into_owned();

        if let Some(reader) = self.marker_files.get_mut(&terminal_id) {
            reader.bytes_read = bytes_read + consumed_len as u64;
        }

        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        self.scan_lines(terminal_id, &lines)
    }

    /// Whether any plain terminal currently has a command in flight whose
    /// (pre-redaction) text categorizes as `category` — used by
    /// `NucleusEngine` to feed `SessionState::test_command_running` (the
    /// `DeveloperIntent::Testing` signal) without duplicating this crate's
    /// own start/finish bookkeeping into a second, separately-maintained
    /// set. Re-categorizes on every call rather than storing the category
    /// alongside `PendingCommand` — `categorize_command` is a cheap, pure
    /// string match, and only a handful of terminals are ever pending at
    /// once, so recomputing costs nothing and keeps `PendingCommand` itself
    /// free of a second concern.
    pub fn has_pending_command_of_category(&self, category: CommandCategory) -> bool {
        self.pending
            .values()
            .any(|pending| categorize_command(&pending.command) == category)
    }

    /// Scans `lines` for start/finish markers, updating per-terminal
    /// pending-command state and returning any resulting outcomes. Called
    /// by [`Self::poll_marker_file`] with lines newly read from a
    /// terminal's marker file; kept as its own public, `lines`-taking
    /// method (rather than folded entirely into `poll_marker_file`) since
    /// it's the actual detection logic and is unit-tested directly,
    /// independent of file I/O.
    ///
    /// A start marker only fires an outcome if no command is already
    /// pending for this terminal (guards against the same line being
    /// re-processed, though `poll_marker_file`'s byte-offset tracking
    /// already prevents that in practice). A finish marker only fires if a
    /// command *is* pending, and clears it — a stale/duplicate finish
    /// marker with nothing pending is ignored.
    pub fn scan_lines(
        &mut self,
        terminal_id: EntityId,
        lines: &[String],
    ) -> Vec<TerminalCommandOutcome> {
        let mut outcomes = Vec::new();
        for line in lines {
            if let Some(command) = parse_start_marker(line) {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.pending.entry(terminal_id)
                {
                    entry.insert(PendingCommand {
                        command: command.clone(),
                        started_at: Instant::now(),
                    });
                    outcomes.push(TerminalCommandOutcome::Started { command });
                }
                continue;
            }
            if let Some(exit_code) = parse_end_marker(line)
                && let Some(pending) = self.pending.remove(&terminal_id)
            {
                self.last_completed.insert(
                    terminal_id,
                    LastCommandOutcome {
                        category: categorize_command(&pending.command),
                        exit_code,
                    },
                );
                outcomes.push(TerminalCommandOutcome::Finished {
                    command: pending.command,
                    exit_code,
                    duration: pending.started_at.elapsed(),
                });
            }
        }
        outcomes
    }

    /// The most recently completed command's category and exit code for
    /// `terminal_id`, if any command has finished in it yet — `None` for a
    /// freshly-opened terminal (or one that's never had a command finish).
    /// Used by `NucleusEngine` to feed `SessionState::
    /// focused_terminal_last_command`, which `nucleus::classify`'s
    /// terminal-engagement signal routes by (see that function's doc
    /// comment).
    pub fn last_completed_command(&self, terminal_id: EntityId) -> Option<LastCommandOutcome> {
        self.last_completed.get(&terminal_id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Redaction ----

    #[test]
    fn test_redact_token_flag() {
        let redacted = redact_command("curl --token=abc123XYZ https://example.com");
        assert_eq!(redacted, "curl --token=[REDACTED] https://example.com");
    }

    #[test]
    fn test_redact_password_flag() {
        let redacted = redact_command("mysql --password=hunter2 --user=root");
        assert_eq!(redacted, "mysql --password=[REDACTED] --user=root");
    }

    #[test]
    fn test_redact_authorization_bearer() {
        let redacted = redact_command("curl -H 'Authorization: Bearer sk-abc123def456'");
        assert_eq!(redacted, "curl -H 'Authorization: Bearer [REDACTED]'");
    }

    #[test]
    fn test_redact_aws_access_key() {
        let redacted = redact_command("export AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP");
        assert_eq!(redacted, "export AWS_ACCESS_KEY_ID=[REDACTED]");
    }

    #[test]
    fn test_redact_generic_secret_assignment() {
        assert_eq!(
            redact_command("deploy.sh secret=xJ9kL2mP8qR5"),
            "deploy.sh secret=[REDACTED]"
        );
        assert_eq!(
            redact_command("run key=aBcDeFgHiJkLmN"),
            "run key=[REDACTED]"
        );
        assert_eq!(
            redact_command("login pass=SuperSecret1"),
            "login pass=[REDACTED]"
        );
    }

    #[test]
    fn test_redact_leaves_ordinary_commands_untouched() {
        let command = "git commit -m 'fix off-by-one in classify()'";
        assert_eq!(redact_command(command), command);
    }

    // ---- Categorization ----

    #[test]
    fn test_categorize_git() {
        assert_eq!(categorize_command("git status"), CommandCategory::Git);
        assert_eq!(
            categorize_command("git commit -am wip"),
            CommandCategory::Git
        );
    }

    #[test]
    fn test_categorize_test_commands() {
        assert_eq!(categorize_command("pytest tests/"), CommandCategory::Test);
        assert_eq!(
            categorize_command("cargo test -p nucleus_intent"),
            CommandCategory::Test
        );
        assert_eq!(categorize_command("npm test"), CommandCategory::Test);
        assert_eq!(categorize_command("go test ./..."), CommandCategory::Test);
    }

    #[test]
    fn test_categorize_build_commands() {
        assert_eq!(categorize_command("make"), CommandCategory::Build);
        assert_eq!(
            categorize_command("cargo build --release"),
            CommandCategory::Build
        );
        assert_eq!(categorize_command("npm run build"), CommandCategory::Build);
    }

    #[test]
    fn test_categorize_lint_commands() {
        assert_eq!(categorize_command("eslint ."), CommandCategory::Lint);
        assert_eq!(categorize_command("cargo clippy"), CommandCategory::Lint);
    }

    #[test]
    fn test_categorize_package_commands() {
        assert_eq!(
            categorize_command("npm install lodash"),
            CommandCategory::Package
        );
        assert_eq!(
            categorize_command("pip install requests"),
            CommandCategory::Package
        );
        assert_eq!(
            categorize_command("cargo add serde"),
            CommandCategory::Package
        );
    }

    #[test]
    fn test_categorize_falls_back_to_other() {
        assert_eq!(categorize_command("ls -la"), CommandCategory::Other);
        assert_eq!(categorize_command("cd /tmp"), CommandCategory::Other);
        assert_eq!(categorize_command(""), CommandCategory::Other);
    }

    // ---- Marker parsing ----

    #[test]
    fn test_start_marker_round_trips_command_text() {
        let command = "echo hello world";
        let encoded = base64::engine::general_purpose::STANDARD.encode(command);
        let line = format!("{CMD_START_MARKER_PREFIX}{encoded}{CMD_START_MARKER_SUFFIX}");
        assert_eq!(parse_start_marker(&line).as_deref(), Some(command));
    }

    #[test]
    fn test_end_marker_parses_exit_code() {
        let line = format!("{CMD_END_MARKER_PREFIX}127{CMD_END_MARKER_SUFFIX}");
        assert_eq!(parse_end_marker(&line), Some(127));
    }

    /// The exact false-positive this module's anchored parsing exists to
    /// prevent: the *echoed source* of the hook's own `printf` call (which
    /// contains the marker prefix as a bareword, immediately followed by a
    /// space and a shell variable reference, not real base64) must not
    /// parse as a valid marker.
    #[test]
    fn test_echoed_hook_source_does_not_parse_as_marker() {
        let echoed_source_line =
            r#"__nucleus_preexec() { printf '%s%s%s\n' __nucleus_cmd_start__ "$1" __; }"#;
        assert_eq!(parse_start_marker(echoed_source_line), None);
    }

    #[test]
    fn test_unrelated_line_does_not_parse_as_marker() {
        assert_eq!(parse_start_marker("$ ls -la"), None);
        assert_eq!(parse_end_marker("total 48"), None);
    }

    // ---- TerminalCommandWatcher state machine ----

    #[test]
    fn test_scan_lines_detects_start_then_finish() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(1);
        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("cargo build")
        );

        let outcomes = watcher.scan_lines(terminal_id, &[start_line]);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            TerminalCommandOutcome::Started { command } if command == "cargo build"
        ));

        let end_line = format!("{CMD_END_MARKER_PREFIX}0{CMD_END_MARKER_SUFFIX}");
        let outcomes = watcher.scan_lines(terminal_id, &[end_line]);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            TerminalCommandOutcome::Finished { command, exit_code: 0, .. }
                if command == "cargo build"
        ));
    }

    /// Guards against re-scanning the same start marker line twice (e.g. if
    /// a caller ever passed overlapping line ranges) from firing a second
    /// `Started` event for the same command. In practice
    /// `poll_marker_file`'s byte-offset tracking already prevents a marker
    /// line from ever being handed to `scan_lines` twice, but `scan_lines`
    /// itself doesn't rely on that — this is its own, independent guard.
    #[test]
    fn test_scan_lines_does_not_double_fire_start_for_same_visible_marker() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(2);
        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("sleep 30")
        );

        let first = watcher.scan_lines(terminal_id, std::slice::from_ref(&start_line));
        assert_eq!(first.len(), 1);
        // The same line handed to scan_lines a second time (still pending,
        // command hasn't finished) must not double-fire.
        let second = watcher.scan_lines(terminal_id, &[start_line]);
        assert!(second.is_empty());
    }

    /// A finish marker with nothing pending (e.g. a stale marker still
    /// visible from a much earlier command, after this module's own state
    /// was reset) must not fire a spurious `Finished` outcome.
    #[test]
    fn test_scan_lines_ignores_finish_marker_with_nothing_pending() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(3);
        let end_line = format!("{CMD_END_MARKER_PREFIX}1{CMD_END_MARKER_SUFFIX}");
        assert!(watcher.scan_lines(terminal_id, &[end_line]).is_empty());
    }

    #[test]
    fn test_prune_drops_state_for_closed_terminals() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(4);
        watcher.mark_injected(terminal_id);
        assert!(!watcher.needs_injection(terminal_id));

        watcher.prune(&HashSet::default());
        assert!(watcher.needs_injection(terminal_id));
    }

    // ---- has_pending_command_of_category (classifier-expansion session,
    // Part B: DeveloperIntent::Testing) ----

    #[test]
    fn test_has_pending_command_of_category_true_while_test_command_in_flight() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(10);
        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("pytest tests/")
        );

        watcher.scan_lines(terminal_id, &[start_line]);
        assert!(watcher.has_pending_command_of_category(CommandCategory::Test));
        assert!(!watcher.has_pending_command_of_category(CommandCategory::Build));
    }

    #[test]
    fn test_has_pending_command_of_category_false_once_command_finishes() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(11);
        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("cargo test")
        );
        let end_line = format!("{CMD_END_MARKER_PREFIX}0{CMD_END_MARKER_SUFFIX}");

        watcher.scan_lines(terminal_id, &[start_line]);
        assert!(watcher.has_pending_command_of_category(CommandCategory::Test));

        watcher.scan_lines(terminal_id, &[end_line]);
        assert!(!watcher.has_pending_command_of_category(CommandCategory::Test));
    }

    #[test]
    fn test_has_pending_command_of_category_false_for_non_test_command() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(12);
        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("cargo build --release")
        );

        watcher.scan_lines(terminal_id, &[start_line]);
        assert!(!watcher.has_pending_command_of_category(CommandCategory::Test));
        assert!(watcher.has_pending_command_of_category(CommandCategory::Build));
    }

    // ---- poll_marker_file / set_marker_file / prune's file cleanup
    // (invisible-marker revision: markers move from the terminal's own
    // stdout to a dedicated per-terminal file) ----

    fn scratch_marker_file_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("nucleus_test_markers_{}.log", uuid::Uuid::new_v4()))
    }

    fn append_to(path: &std::path::Path, contents: &str) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("scratch marker file should be writable in a test");
        file.write_all(contents.as_bytes())
            .expect("scratch marker file write should succeed in a test");
    }

    #[test]
    fn test_poll_marker_file_empty_when_never_set() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(20);
        assert!(watcher.poll_marker_file(terminal_id).is_empty());
    }

    #[test]
    fn test_poll_marker_file_empty_when_file_does_not_exist_yet() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(21);
        // A path that's never actually written to — matches the real
        // sequence, where `set_marker_file` records a path before the
        // shell has necessarily written anything (or ever will, for an
        // unsupported shell).
        watcher.set_marker_file(terminal_id, scratch_marker_file_path());
        assert!(watcher.poll_marker_file(terminal_id).is_empty());
    }

    #[test]
    fn test_poll_marker_file_detects_start_then_finish() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(22);
        let path = scratch_marker_file_path();
        watcher.set_marker_file(terminal_id, path.clone());

        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}\n",
            base64::engine::general_purpose::STANDARD.encode("echo hi")
        );
        append_to(&path, &start_line);
        let outcomes = watcher.poll_marker_file(terminal_id);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            TerminalCommandOutcome::Started { command } if command == "echo hi"
        ));

        let end_line = format!("{CMD_END_MARKER_PREFIX}0{CMD_END_MARKER_SUFFIX}\n");
        append_to(&path, &end_line);
        let outcomes = watcher.poll_marker_file(terminal_id);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            TerminalCommandOutcome::Finished { command, exit_code: 0, .. } if command == "echo hi"
        ));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_poll_marker_file_does_not_reprocess_already_read_lines() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(23);
        let path = scratch_marker_file_path();
        watcher.set_marker_file(terminal_id, path.clone());

        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}\n",
            base64::engine::general_purpose::STANDARD.encode("ls")
        );
        append_to(&path, &start_line);

        assert_eq!(watcher.poll_marker_file(terminal_id).len(), 1);
        // Nothing new was appended — re-polling the same file content must
        // not re-fire the same outcome a second time.
        assert!(watcher.poll_marker_file(terminal_id).is_empty());

        std::fs::remove_file(&path).ok();
    }

    /// A `printf` write landing mid-write (no trailing `\n` yet) must not
    /// be treated as a complete line — re-reading it whole once the
    /// newline actually lands is the whole point of only consuming up
    /// through the last `\n` in newly-read bytes.
    #[test]
    fn test_poll_marker_file_ignores_partial_trailing_line() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(24);
        let path = scratch_marker_file_path();
        watcher.set_marker_file(terminal_id, path.clone());

        let full_prefix = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("ls")
        );
        // Write the marker text without a trailing newline yet.
        append_to(&path, &full_prefix);
        assert!(
            watcher.poll_marker_file(terminal_id).is_empty(),
            "a line with no trailing newline yet must not be consumed"
        );

        // The newline lands; now the whole line is complete.
        append_to(&path, "\n");
        let outcomes = watcher.poll_marker_file(terminal_id);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            TerminalCommandOutcome::Started { .. }
        ));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_prune_deletes_marker_file_from_disk() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(25);
        let path = scratch_marker_file_path();
        append_to(&path, "");
        watcher.set_marker_file(terminal_id, path.clone());
        assert!(path.exists());

        watcher.prune(&HashSet::default());
        assert!(
            !path.exists(),
            "prune should delete a dropped terminal's marker file from disk"
        );
    }

    /// A terminal must be observed on one tick before it's eligible for
    /// injection on the next — the mechanism that gives a freshly-opened
    /// terminal's shell startup (rc files, `conda init`, etc.) at least one
    /// full poll interval to settle before anything is written to its PTY.
    #[test]
    fn test_mark_observed_requires_two_ticks_before_ready() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(7);

        let first_tick_is_first_observation = watcher.mark_observed(terminal_id);
        assert!(first_tick_is_first_observation);

        let second_tick_is_first_observation = watcher.mark_observed(terminal_id);
        assert!(!second_tick_is_first_observation);
    }

    #[test]
    fn test_mark_injected_clears_observed_state() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(8);

        watcher.mark_observed(terminal_id);
        watcher.mark_injected(terminal_id);

        // Re-observing after injection (e.g. a stale entity id reused, or a
        // logic bug re-adding an already-injected terminal to the to-inject
        // set) should behave as a fresh first observation, not silently
        // reuse leftover state from before injection.
        assert!(watcher.mark_observed(terminal_id));
    }

    #[test]
    fn test_prune_drops_observed_state_for_closed_terminals() {
        let mut watcher = TerminalCommandWatcher::new();
        let terminal_id = fake_entity_id(9);
        watcher.mark_observed(terminal_id);

        watcher.prune(&HashSet::default());

        // Pruned away, so the next observation is treated as the first
        // again rather than immediately ready.
        assert!(watcher.mark_observed(terminal_id));
    }

    // ---- hook_install_command ----

    #[test]
    fn test_hook_install_command_is_single_line_for_posix() {
        let path = std::path::Path::new("/tmp/nucleus_hook_test.sh");
        let marker_path = std::path::Path::new("/tmp/nucleus_markers_test.log");
        let input = hook_install_command(ShellKind::Posix, path, marker_path);
        let text = String::from_utf8(input).expect("install command must be valid UTF-8");
        assert_eq!(
            text.matches('\n').count(),
            0,
            "must be a single line: {text:?}"
        );
        assert_eq!(
            text.matches('\r').count(),
            1,
            "must end in exactly one CR: {text:?}"
        );
        assert!(text.starts_with("__nucleus_hook_path='/tmp/nucleus_hook_test.sh'"));
        assert!(text.contains(". '/tmp/nucleus_hook_test.sh'"));
        assert!(text.contains("__nucleus_marker_file='/tmp/nucleus_markers_test.log'"));
        // Cleanup and `clear` are *not* appended to this outer line for
        // POSIX shells — see the module doc comment on why bash requires
        // that to happen inside the sourced script instead.
        assert!(!text.contains("rm -f"));
        assert!(!text.contains("clear"));
    }

    #[test]
    fn test_hook_install_command_uses_source_for_fish() {
        let path = std::path::Path::new("/tmp/nucleus_hook_test.sh");
        let marker_path = std::path::Path::new("/tmp/nucleus_markers_test.log");
        let input = hook_install_command(ShellKind::Fish, path, marker_path);
        let text = String::from_utf8(input).expect("install command must be valid UTF-8");
        assert!(text.starts_with("set -g __nucleus_marker_file '/tmp/nucleus_markers_test.log'"));
        assert!(text.contains("source '/tmp/nucleus_hook_test.sh'"));
        assert_eq!(
            text.matches('\n').count(),
            0,
            "must be a single line: {text:?}"
        );
        assert!(
            text.contains("clear"),
            "must clear the screen so the install line's own echo doesn't linger: {text:?}"
        );
    }

    /// Both hook scripts must arm the skip-once guard so `precmd`'s very
    /// first firing — which happens the instant hook installation finishes,
    /// against the installing command itself — doesn't print a spurious end
    /// marker. Only a content sanity check: exercising the actual
    /// first-invocation-is-skipped behavior needs a real shell/PTY, out of
    /// reach for a unit test.
    #[test]
    fn test_hook_scripts_arm_skip_next_end_marker_guard() {
        assert!(POSIX_HOOK_SCRIPT.contains("__nucleus_skip_next_end_marker"));
        assert!(FISH_HOOK_SCRIPT.contains("__nucleus_skip_next_end_marker"));
    }

    /// In the bash branch, `trap ... DEBUG` must be the *last* statement:
    /// once armed it applies to every subsequent simple command bash
    /// executes, including sibling statements still left in the same
    /// script, not just future ones — so the cleanup and `PROMPT_COMMAND`
    /// setup must run before it or they'd trigger `preexec` on themselves.
    #[test]
    fn test_bash_debug_trap_is_armed_after_cleanup_and_prompt_command_setup() {
        let bash_branch = POSIX_HOOK_SCRIPT
            .split("BASH_VERSION")
            .nth(1)
            .expect("script must have a bash branch");

        let trap_index = bash_branch
            .find("trap '__nucleus_chained_preexec' DEBUG")
            .expect("bash branch must arm the DEBUG trap");
        let cleanup_index = bash_branch
            .find("rm -f \"$__nucleus_hook_path\"")
            .expect("bash branch must clean up its temp file");
        let clear_index = bash_branch
            .find("clear")
            .expect("bash branch must clear the screen");
        let prompt_command_index = bash_branch
            .find("PROMPT_COMMAND=")
            .expect("bash branch must set up PROMPT_COMMAND");

        assert!(cleanup_index < trap_index);
        assert!(clear_index < trap_index);
        assert!(prompt_command_index < trap_index);
    }

    /// `preexec`'s guard against firing on its own `precmd` invocation must
    /// compare against the literal command bash actually runs
    /// (`__nucleus_precmd`, evaluated as its own simple command since
    /// `$PROMPT_COMMAND` is executed piece by piece), not against the whole
    /// `$PROMPT_COMMAND` string — that only ever matched when nothing else
    /// had set `$PROMPT_COMMAND` before hook installation, which doesn't
    /// hold once any other tool (a themed prompt, direnv, etc.) already has.
    #[test]
    fn test_bash_preexec_guard_checks_literal_precmd_command() {
        assert!(POSIX_HOOK_SCRIPT.contains(r#"[ "$BASH_COMMAND" = "__nucleus_precmd" ]"#));
    }

    /// `NucleusEngine::poll_plain_terminals`'s actual overlap gate
    /// (`terminal.task().is_none()`) needs a real PTY-backed `Terminal`
    /// entity to exercise directly, which is out of proportion for this
    /// session (see `poll_plain_terminals`'s doc comment) — this instead
    /// guards the narrower, unit-testable claim: `TerminalCommandWatcher`'s
    /// bookkeeping (this module) and task-terminal bookkeeping
    /// (`NucleusEngine::last_seen_task_status`) are structurally separate
    /// data, with no shared key space or mutation path between them — so a
    /// terminal tracked by one can never be silently conflated with the
    /// other purely as a side effect of how this module's state is shaped.
    #[test]
    fn test_injected_and_task_status_bookkeeping_are_independent() {
        let mut watcher = TerminalCommandWatcher::new();
        let plain_terminal_id = fake_entity_id(5);
        let task_terminal_id = fake_entity_id(6);

        // Simulate: only the plain terminal ever gets injected/tracked here.
        watcher.mark_injected(plain_terminal_id);
        let start_line = format!(
            "{CMD_START_MARKER_PREFIX}{}{CMD_START_MARKER_SUFFIX}",
            base64::engine::general_purpose::STANDARD.encode("echo plain")
        );
        watcher.scan_lines(plain_terminal_id, &[start_line]);

        // The task terminal's id never appears in this watcher's state at
        // all — nothing here can accidentally treat it as a plain terminal.
        assert!(watcher.needs_injection(task_terminal_id));

        // Pruning to "only the task terminal is live" drops the plain
        // terminal's state without needing to know anything about task
        // terminals — confirming there's no shared bookkeeping to get out
        // of sync between the two polls.
        let mut live = HashSet::default();
        live.insert(task_terminal_id);
        watcher.prune(&live);
        assert!(watcher.needs_injection(plain_terminal_id));
    }

    fn fake_entity_id(raw: u64) -> EntityId {
        EntityId::from(raw)
    }
}

/// Stress tests over the pure parsing/redaction/categorization functions —
/// arbitrary strings (including huge, empty, non-ASCII, and control-
/// character input a real terminal could never actually produce as a
/// command, but that these functions must not panic or hang on regardless).
#[cfg(test)]
mod stress_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Must never panic on arbitrary input, regardless of shape.
        #[test]
        fn redact_command_never_panics(command in ".*") {
            let _ = redact_command(&command);
        }

        /// Redaction is idempotent: running it twice must equal running it
        /// once — if a first pass ever left behind something a pattern
        /// would match again (e.g. if `[REDACTED]` itself could
        /// accidentally match `key=` style patterns), a second pass would
        /// diverge and this would fail.
        #[test]
        fn redact_command_is_idempotent(command in ".*") {
            let once = redact_command(&command);
            let twice = redact_command(&once);
            prop_assert_eq!(once, twice);
        }

        /// Redaction must never let a marked-obviously-secret substring
        /// survive verbatim in the output — checked directly for the
        /// AWS-key pattern since it needs no surrounding context to trigger.
        #[test]
        fn redact_command_removes_aws_keys(
            prefix in ".{0,50}",
            suffix in ".{0,50}",
            key_suffix in "[0-9A-Z]{16}",
        ) {
            let command = format!("{prefix}AKIA{key_suffix}{suffix}");
            let redacted = redact_command(&command);
            prop_assert!(
                !redacted.contains(&format!("AKIA{key_suffix}")),
                "AWS key survived redaction: {redacted:?}"
            );
        }

        /// Must never panic, and a huge input must not visibly hang the
        /// test (proptest's own timeout would catch a real catastrophic-
        /// backtracking blowup) — the `regex` crate guarantees linear-time
        /// matching by construction (no backtracking engine), but this
        /// exercises that guarantee against a large adversarial input
        /// rather than just trusting the crate's docs.
        #[test]
        fn redact_command_handles_large_input(
            repeated in "[a-zA-Z0-9=_ -]{0,20000}",
        ) {
            let _ = redact_command(&repeated);
        }

        #[test]
        fn categorize_command_never_panics(command in ".*") {
            let _ = categorize_command(&command);
        }

        #[test]
        fn parse_start_marker_never_panics(line in ".*") {
            let _ = parse_start_marker(&line);
        }

        #[test]
        fn parse_end_marker_never_panics(line in ".*") {
            let _ = parse_end_marker(&line);
        }

        /// The false-positive this module's strict anchoring exists to
        /// prevent, generalized: any line that merely *contains* the start
        /// marker prefix as a substring (not as the whole line) must never
        /// parse as a valid marker — only an exact `PREFIX<base64>SUFFIX`
        /// line should.
        #[test]
        fn parse_start_marker_rejects_prefix_as_mere_substring(
            garbage_before in ".{1,20}",
            garbage_after in ".{1,20}",
        ) {
            let line = format!("{garbage_before}{CMD_START_MARKER_PREFIX}{garbage_after}");
            // Only assert when the constructed line doesn't happen to be a
            // genuinely valid marker by coincidence (garbage_after isn't
            // required to end in the suffix after valid base64).
            if !garbage_after.ends_with(CMD_START_MARKER_SUFFIX) {
                prop_assert_eq!(parse_start_marker(&line), None);
            }
        }

        /// Round-trip property: any command text, once base64-encoded into
        /// a well-formed start marker, must decode back to exactly the
        /// original text — across arbitrary strings, not just the ASCII
        /// examples in the hand-written tests above.
        #[test]
        fn start_marker_round_trip_holds_for_arbitrary_strings(command in ".*") {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&command);
            let line = format!("{CMD_START_MARKER_PREFIX}{encoded}{CMD_START_MARKER_SUFFIX}");
            prop_assert_eq!(parse_start_marker(&line), Some(command));
        }

        /// The `TerminalCommandWatcher` state machine must never panic
        /// regardless of what sequence of arbitrary lines it's fed, across
        /// many terminals.
        #[test]
        fn scan_lines_never_panics_on_arbitrary_input(
            lines in proptest::collection::vec(".*", 0..50),
            terminal_id_raw in 1u64..5,
        ) {
            let mut watcher = TerminalCommandWatcher::new();
            let terminal_id = EntityId::from(terminal_id_raw);
            let _ = watcher.scan_lines(terminal_id, &lines);
        }

        /// Must never panic regardless of the path's contents, and must
        /// always produce exactly one line (the property the whole fix is
        /// built on: this can never be fragmented across the shell's
        /// line-based input handling the way the old raw multi-line stream
        /// was).
        #[test]
        fn hook_install_command_is_always_a_single_line(
            path_segment in "[^\\x00]{0,100}",
            marker_path_segment in "[^\\x00]{0,100}",
            use_fish in proptest::bool::ANY,
        ) {
            let shell_kind = if use_fish { ShellKind::Fish } else { ShellKind::Posix };
            let path = std::path::Path::new(&path_segment);
            let marker_path = std::path::Path::new(&marker_path_segment);
            let input = hook_install_command(shell_kind, path, marker_path);
            prop_assert_eq!(input.iter().filter(|&&b| b == b'\n').count(), 0);
        }
    }
}
