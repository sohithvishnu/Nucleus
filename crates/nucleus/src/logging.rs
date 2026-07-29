//! Structured JSONL logging of everything [`crate::NucleusEngine`] observes
//! and infers, for Phase 8's (future) learned interruption policy to train
//! against and for evaluating classifier accuracy against real usage.
//!
//! Pure capture: async, best-effort, never blocks the caller. Write failures
//! go to stderr and are otherwise swallowed rather than panicking the editor.
//! No log viewer, retention policy, or SQLite migration here — see the
//! session prompt this was built for.
//!
//! Deliberately excludes source code content: only paths, symbol names, and
//! size counts are ever written, never actual code text or diffs.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use gpui::{BackgroundExecutor, Task};
use parking_lot::Mutex;
use serde::Serialize;

use crate::{IntentPrediction, SessionState};

/// How long to let log lines sit buffered in memory before flushing to disk.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// Flush immediately once this many lines have queued up, rather than
/// waiting out `FLUSH_INTERVAL`.
const MAX_QUEUE_LEN: usize = 50;

/// One JSON object per observed edit/save/task-run/file-switch/selection
/// change. Never carries source code text or diffs — only paths, symbol
/// names, and size counts.
#[derive(Debug, Clone, Serialize)]
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
