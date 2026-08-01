//! Structured JSONL logging of everything [`crate::NucleusEngine`] observes
//! and infers, for Phase 8's (future) learned interruption policy to train
//! against and for evaluating classifier accuracy against real usage.
//!
//! Pure capture: async, best-effort, never blocks the caller. Write failures
//! go to stderr and are otherwise swallowed rather than panicking the editor.
//! No retention policy or SQLite migration here — see the session prompt
//! this was built for. This module also provides read-side helpers
//! ([`list_log_dates`], [`read_log_file`], [`parse_log_line`]) for the log
//! viewer panel (`engine_panel`) to browse past days; it does not render
//! anything itself.
//!
//! Deliberately excludes source code content: only paths, symbol names, and
//! size counts are ever written, never actual code text or diffs.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use gpui::{BackgroundExecutor, Task};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{DeveloperIntent, IntentPrediction, PredictionId, SessionState};

/// How long to let log lines sit buffered in memory before flushing to disk.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// Flush immediately once this many lines have queued up, rather than
/// waiting out `FLUSH_INTERVAL`.
const MAX_QUEUE_LEN: usize = 50;

/// One JSON object per observed edit/save/task-run/file-switch/selection
/// change/plain-terminal command. Never carries source code text or diffs —
/// only paths, symbol names, size counts, and (redacted) command strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RawEvent {
    Edit {
        file: Option<PathBuf>,
        symbol: Option<String>,
        inserted_chars: usize,
        deleted_chars: usize,
    },
    Save {
        file: Option<PathBuf>,
    },
    FileSwitch {
        file: PathBuf,
    },
    SelectionChanged {
        file: Option<PathBuf>,
    },
    TaskStarted {
        label: Option<String>,
    },
    TaskCompleted {
        label: Option<String>,
    },
    TaskFailed {
        label: Option<String>,
    },
    /// Phase 4b-1: a command started in a plain (non-task) terminal, detected
    /// via injected shell hooks — see `terminal_watcher`. `command` has
    /// already been through `terminal_watcher::redact_command` by the time
    /// it reaches here.
    TerminalCommandStarted {
        command: String,
    },
    /// Phase 4b-1: the matching finish for a `TerminalCommandStarted` in the
    /// same terminal. `command` is redacted the same way.
    TerminalCommandFinished {
        command: String,
        exit_code: i32,
        duration_ms: u64,
    },
}

/// Part A: a user's response to a periodic feedback nudge — see
/// `NucleusEngine::maybe_request_feedback_nudge`. `actual_intent` is only
/// meaningful (and only ever `Some`) when `correct` is `false` and the user
/// chose to specify what the intent should have been instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feedback {
    pub prediction_id: PredictionId,
    pub correct: bool,
    pub actual_intent: Option<DeveloperIntent>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum LogLine<'a> {
    #[serde(rename = "intent_prediction")]
    IntentPrediction {
        timestamp: String,
        prediction: &'a IntentPrediction,
        session_state: &'a SessionState,
    },
    #[serde(rename = "raw_event")]
    RawEvent {
        timestamp: String,
        #[serde(flatten)]
        event: &'a RawEvent,
    },
    #[serde(rename = "feedback")]
    Feedback {
        timestamp: String,
        #[serde(flatten)]
        feedback: &'a Feedback,
    },
}

struct LoggerState {
    queue: Vec<String>,
    flush_task: Option<Task<()>>,
    /// The currently-open file, tagged with the local date it was opened
    /// for, so a day boundary crossing reopens a fresh file rather than
    /// appending yesterday's lines into today's.
    open_file: Option<(String, File)>,
}

/// Async, best-effort JSONL writer for `~/.nucleus/logs/YYYY-MM-DD.jsonl`.
/// Cheap to clone — all clones share the same queue and file handle.
#[derive(Clone)]
pub struct NucleusLogger {
    executor: BackgroundExecutor,
    state: Arc<Mutex<LoggerState>>,
}

impl NucleusLogger {
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self {
            executor,
            state: Arc::new(Mutex::new(LoggerState {
                queue: Vec::new(),
                flush_task: None,
                open_file: None,
            })),
        }
    }

    pub fn log_intent_prediction(
        &self,
        prediction: &IntentPrediction,
        session_state: &SessionState,
    ) {
        self.enqueue(&LogLine::IntentPrediction {
            timestamp: Local::now().to_rfc3339(),
            prediction,
            session_state,
        });
    }

    pub fn log_raw_event(&self, event: &RawEvent) {
        self.enqueue(&LogLine::RawEvent {
            timestamp: Local::now().to_rfc3339(),
            event,
        });
    }

    pub fn log_feedback(&self, feedback: &Feedback) {
        self.enqueue(&LogLine::Feedback {
            timestamp: Local::now().to_rfc3339(),
            feedback,
        });
    }

    fn enqueue(&self, line: &LogLine) {
        let json = match serde_json::to_string(line) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("nucleus: failed to serialize log line: {err}");
                return;
            }
        };

        let mut state = self.state.lock();
        state.queue.push(json);
        if state.queue.len() >= MAX_QUEUE_LEN {
            drop(state);
            self.flush().detach();
        } else if state.flush_task.is_none() {
            let this = self.clone();
            state.flush_task = Some(self.executor.spawn(async move {
                this.executor.timer(FLUSH_INTERVAL).await;
                this.flush().detach();
            }));
        }
    }

    /// Drains the queue and writes it to disk. Spawned on the background
    /// executor so this never blocks the caller (or, transitively, input
    /// latency) — the actual file I/O is plain blocking `std::fs`, which is
    /// fine off the main thread, matching `client::Telemetry::flush_events`.
    fn flush(&self) -> Task<()> {
        let this = self.clone();
        self.executor.spawn(async move {
            let mut state = this.state.lock();
            state.flush_task.take();
            let lines = std::mem::take(&mut state.queue);
            if lines.is_empty() {
                return;
            }
            if let Err(err) = write_lines(&mut state.open_file, &lines) {
                eprintln!("nucleus: failed to write log lines to disk: {err}");
            }
        })
    }
}

fn write_lines(open_file: &mut Option<(String, File)>, lines: &[String]) -> std::io::Result<()> {
    let today = Local::now().format("%Y-%m-%d").to_string();
    let stale = match open_file {
        Some((date, _)) => date != &today,
        None => true,
    };
    if stale {
        let dir = log_dir();
        std::fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{today}.jsonl")))?;
        *open_file = Some((today, file));
    }
    let (_, file) = open_file.as_mut().expect("just set above if it was None");
    for line in lines {
        writeln!(file, "{line}")?;
    }
    file.flush()
}

/// `~/.nucleus/logs/` — the real user home directory via `paths::home_dir()`
/// (never relative to the repo), and deliberately outside Zed's own
/// `paths::data_dir()`/`paths::logs_dir()`: this is a separate, personal,
/// long-lived behavioral log, not Zed application state.
pub fn log_dir() -> PathBuf {
    paths::home_dir().join(".nucleus").join("logs")
}

/// A single parsed log line, shared between the live in-memory stream (built
/// directly from [`crate::NucleusEvent`], no serialization involved) and
/// historical file reads (parsed back with [`parse_log_line`]) so the log
/// viewer panel can render both through one code path.
#[derive(Debug, Clone)]
pub enum LogEntry {
    IntentPrediction {
        timestamp: DateTime<Local>,
        prediction: IntentPrediction,
        session_state: SessionState,
    },
    RawEvent {
        timestamp: DateTime<Local>,
        event: RawEvent,
    },
    Feedback {
        timestamp: DateTime<Local>,
        feedback: Feedback,
    },
}

impl LogEntry {
    pub fn timestamp(&self) -> DateTime<Local> {
        match self {
            LogEntry::IntentPrediction { timestamp, .. } => *timestamp,
            LogEntry::RawEvent { timestamp, .. } => *timestamp,
            LogEntry::Feedback { timestamp, .. } => *timestamp,
        }
    }
}

/// Parses one JSONL line written by this module. Returns `None` (never
/// panics or propagates an error) on anything malformed — a blank line, a
/// line truncated by a crash mid-write, an unrecognized `type`, whatever —
/// since this reads append-only external data that a previous session may
/// have left in a partial state.
///
/// Deliberately goes through a generic [`serde_json::Value`] first rather
/// than deriving `Deserialize` directly on a type shaped like [`LogLine`]:
/// `raw_event` lines flatten an internally-tagged [`RawEvent`] into an
/// already internally-tagged outer object, and round-tripping that nested
/// shape back through a single derive is exactly the kind of serde corner
/// worth just sidestepping — extract `type`/`timestamp` from the `Value`,
/// then re-deserialize the pieces individually.
pub fn parse_log_line(line: &str) -> Option<LogEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let entry_type = value.get("type")?.as_str()?;
    let timestamp_str = value.get("timestamp")?.as_str()?;
    let timestamp = DateTime::parse_from_rfc3339(timestamp_str)
        .ok()?
        .with_timezone(&Local);

    match entry_type {
        "intent_prediction" => {
            let prediction = serde_json::from_value(value.get("prediction")?.clone()).ok()?;
            let session_state = serde_json::from_value(value.get("session_state")?.clone()).ok()?;
            Some(LogEntry::IntentPrediction {
                timestamp,
                prediction,
                session_state,
            })
        }
        "raw_event" => {
            // RawEvent only looks at its own "event" tag and variant fields;
            // the surrounding "type"/"timestamp" keys are simply ignored.
            let event = serde_json::from_value(value).ok()?;
            Some(LogEntry::RawEvent { timestamp, event })
        }
        "feedback" => {
            // Same flattened-fields situation as raw_event above: Feedback's
            // fields (prediction_id/correct/actual_intent) sit flattened
            // alongside "type"/"timestamp" in the JSON object, and
            // Feedback::deserialize simply ignores the keys it doesn't
            // recognize.
            let feedback = serde_json::from_value(value).ok()?;
            Some(LogEntry::Feedback { timestamp, feedback })
        }
        _ => None,
    }
}

/// Most lines read from a historical file that are actually parsed and
/// handed to the viewer, most-recent-first. A busy day's file can run to
/// many thousands of lines; rather than building real virtualized/lazy
/// loading this session, this just caps how much a single "browse this
/// date" click can load and render.
pub const MAX_HISTORY_LINES: usize = 500;

/// Reads and parses up to [`MAX_HISTORY_LINES`] most-recent lines from
/// `~/.nucleus/logs/{date}.jsonl`, most recent first. Malformed lines are
/// skipped (see [`parse_log_line`]), not treated as an error. Blocking
/// `std::fs` I/O — call this from a background-spawned task, never directly
/// on the UI thread.
pub fn read_log_file(date: &str) -> std::io::Result<Vec<LogEntry>> {
    let contents = std::fs::read_to_string(log_dir().join(format!("{date}.jsonl")))?;
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(MAX_HISTORY_LINES);
    let mut entries: Vec<LogEntry> = lines[start..]
        .iter()
        .filter_map(|l| parse_log_line(l))
        .collect();
    entries.reverse();
    Ok(entries)
}

/// Scans [`log_dir`] for `*.jsonl` files and returns their date stems
/// (`"2026-07-29"`, ...), most recent first. Doesn't assume or hardcode any
/// particular retention window — whatever files are actually present.
pub fn list_log_dates() -> std::io::Result<Vec<String>> {
    let dir = log_dir();
    let mut dates = Vec::new();
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            dates.push(stem.to_string());
        }
    }
    dates.sort_unstable_by(|a, b| b.cmp(a));
    Ok(dates)
}
