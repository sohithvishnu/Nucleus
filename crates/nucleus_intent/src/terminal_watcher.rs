//! Phase 4b-1: plain-shell command start/finish detection, observation only.
//!
//! This fork has no OSC 133 (shell-integration) support anywhere in its
//! terminal stack (`crates/terminal` → `alacritty_terminal` → `vte` — no
//! relevant `osc_dispatch` arm exists; patching the forked
//! `alacritty_terminal` dependency was judged high-risk and stays out of
//! scope). Instead this mirrors `crates/terminal`'s own existing pattern for
//! exactly this class of problem — `INIT_COMMAND_STARTUP_MARKER_PREFIX`/
//! `_SUFFIX` plus `Event::Wakeup`-driven scanning — by injecting
//! `precmd`/`preexec`-equivalent shell hooks that print a distinguishable
//! marker line around each command, then scanning newly-arrived terminal
//! output for those markers.
//!
//! Deliberately narrow (4b-1 of a 3-part plan): detects and logs command
//! start/finish only. Nothing here is read by `classify()` — that's 4b-3,
//! later, only after this data has been dogfooded for a while.
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
//! the command's actual *output* is ever contiguous. The one addition here
//! (beyond that existing pattern) is a strict, anchored parse — the scanned
//! line must be nothing but `PREFIX<base64><SUFFIX>` — so even a coincidental
//! substring match against unrelated on-screen text can't parse as valid.

use base64::Engine as _;
use collections::{HashMap, HashSet};
use gpui::EntityId;
use regex::Regex;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use util::shell::ShellKind;

pub const CMD_START_MARKER_PREFIX: &str = "__nucleus_cmd_start__";
pub const CMD_START_MARKER_SUFFIX: &str = "__";
pub const CMD_END_MARKER_PREFIX: &str = "__nucleus_cmd_end__";
pub const CMD_END_MARKER_SUFFIX: &str = "__";

/// How many of the terminal's most-recent non-empty lines to scan on each
/// `Event::Wakeup` — mirrors `crates/terminal`'s own
/// `INIT_COMMAND_STARTUP_MARKER_SEARCH_LINES` (64), narrowed since our
/// markers are single lines emitted immediately before/after a prompt, not
/// something that needs to survive a long-running command's own output
/// scrolling past it.
pub const MARKER_SEARCH_LINES: usize = 32;

/// The POSIX-shell hook script, written once via
/// `Terminal::write_program_input` into any terminal whose `ShellKind` is
/// `Posix` (bash, zsh, and plain `sh` all report as `Posix` in this
/// codebase's `ShellKind` — see `util::shell::ShellKind` — so this single
/// script self-detects bash vs. zsh at *shell* runtime via
/// `$BASH_VERSION`/`$ZSH_VERSION` rather than relying on Zed's own,
/// coarser-grained classification).
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
pub const POSIX_HOOK_SCRIPT: &str = r#"if [ -n "$ZSH_VERSION" ]; then
  __nucleus_preexec() { printf '%s%s%s\n' __nucleus_cmd_start__ "$(printf '%s' "$1" | base64 | tr -d '\n')" __; }
  __nucleus_precmd() { __ne=$?; printf '%s%s%s\n' __nucleus_cmd_end__ "$__ne" __; }
  if typeset -f add-zsh-hook >/dev/null 2>&1; then
    add-zsh-hook preexec __nucleus_preexec
    add-zsh-hook precmd __nucleus_precmd
  else
    autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook preexec __nucleus_preexec && add-zsh-hook precmd __nucleus_precmd || { preexec_functions+=(__nucleus_preexec); precmd_functions+=(__nucleus_precmd); }
  fi
elif [ -n "$BASH_VERSION" ]; then
  __nucleus_preexec() { [ -n "$COMP_LINE" ] && return; [ "$BASH_COMMAND" = "$PROMPT_COMMAND" ] && return; printf '%s%s%s\n' __nucleus_cmd_start__ "$(printf '%s' "$BASH_COMMAND" | base64 | tr -d '\n')" __; }
  __nucleus_precmd() { __ne=$?; printf '%s%s%s\n' __nucleus_cmd_end__ "$__ne" __; }
  if [ -z "${__nucleus_prev_debug_trap+x}" ]; then
    __nucleus_prev_debug_trap="$(trap -p DEBUG | sed -e "s/^trap -- '//" -e "s/' DEBUG$//")"
  fi
  __nucleus_chained_preexec() { __nucleus_preexec; [ -n "$__nucleus_prev_debug_trap" ] && eval "$__nucleus_prev_debug_trap"; }
  trap '__nucleus_chained_preexec' DEBUG
  case ";$PROMPT_COMMAND;" in *";__nucleus_precmd;"*) ;; *) PROMPT_COMMAND="__nucleus_precmd;${PROMPT_COMMAND}" ;; esac
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
pub const FISH_HOOK_SCRIPT: &str = r#"function __nucleus_preexec --on-event fish_preexec
    printf '%s%s%s\n' __nucleus_cmd_start__ (echo -n $argv[1] | base64 | tr -d '\n') __
end
function __nucleus_precmd --on-event fish_prompt
    printf '%s%s%s\n' __nucleus_cmd_end__ $status __
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

/// Coarse, non-authoritative bucket for a detected command — observational
/// only this session, not fed into `classify()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        (
            Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
            "[REDACTED]",
        ),
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

/// Per-terminal state for hook injection and marker-driven command tracking.
/// Plain data — no GPUI involved, so this is unit-testable in isolation
/// (`scan_lines` in particular, which is the actual detection logic).
#[derive(Default)]
pub struct TerminalCommandWatcher {
    injected: HashSet<EntityId>,
    pending: HashMap<EntityId, PendingCommand>,
}

impl TerminalCommandWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn needs_injection(&self, terminal_id: EntityId) -> bool {
        !self.injected.contains(&terminal_id)
    }

    pub fn mark_injected(&mut self, terminal_id: EntityId) {
        self.injected.insert(terminal_id);
    }

    /// Drops state for terminals that no longer exist, so `injected`/
    /// `pending` don't grow unboundedly across a long session of opening and
    /// closing terminals.
    pub fn prune(&mut self, live_terminal_ids: &HashSet<EntityId>) {
        self.injected.retain(|id| live_terminal_ids.contains(id));
        self.pending.retain(|id, _| live_terminal_ids.contains(id));
    }

    /// Scans `lines` (the terminal's most-recent non-empty lines) for
    /// start/finish markers, updating per-terminal pending-command state and
    /// returning any resulting outcomes.
    ///
    /// A start marker only fires an outcome if no command is already
    /// pending for this terminal (guards against the same still-on-screen
    /// marker line being re-scanned on a later, unrelated `Wakeup`). A
    /// finish marker only fires if a command *is* pending, and clears it —
    /// a stale/duplicate finish marker with nothing pending is ignored.
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
                outcomes.push(TerminalCommandOutcome::Finished {
                    command: pending.command,
                    exit_code,
                    duration: pending.started_at.elapsed(),
                });
            }
        }
        outcomes
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
        assert_eq!(
            redacted,
            "curl -H 'Authorization: Bearer [REDACTED]'"
        );
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
        assert_eq!(categorize_command("git commit -am wip"), CommandCategory::Git);
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
        assert_eq!(categorize_command("cargo build --release"), CommandCategory::Build);
        assert_eq!(categorize_command("npm run build"), CommandCategory::Build);
    }

    #[test]
    fn test_categorize_lint_commands() {
        assert_eq!(categorize_command("eslint ."), CommandCategory::Lint);
        assert_eq!(categorize_command("cargo clippy"), CommandCategory::Lint);
    }

    #[test]
    fn test_categorize_package_commands() {
        assert_eq!(categorize_command("npm install lodash"), CommandCategory::Package);
        assert_eq!(categorize_command("pip install requests"), CommandCategory::Package);
        assert_eq!(categorize_command("cargo add serde"), CommandCategory::Package);
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

    /// Guards against re-scanning the same still-on-screen start marker (it
    /// stays within `MARKER_SEARCH_LINES` across multiple `Wakeup`s until
    /// enough new output pushes it out) from firing a second `Started` event
    /// for the same command.
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
        // Same marker line still visible on the next Wakeup, before the
        // command has finished.
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
    }
}
