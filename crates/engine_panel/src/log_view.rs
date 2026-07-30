//! State for the "Logs" tab: a live tail of [`nucleus_intent::NucleusEvent`]
//! plus a read-only historical browser over `~/.nucleus/logs/*.jsonl`. Pure
//! state — rendering lives in `engine_panel.rs` alongside the rest of the
//! panel's render methods, following the same pattern already used there.

use collections::HashSet;

use nucleus_intent::LogEntry;

/// Cap on how many live entries are kept in memory — a rolling window over
/// the *current session's* tail, independent of the logger's own on-disk
/// output (which has no such limit). Not a virtualized list: this session's
/// log-viewer work treats "keep the most recent N" as a sufficient, much
/// simpler substitute per the session's own performance guidance.
const MAX_LIVE_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogViewMode {
    Live,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogTypeFilter {
    All,
    Predictions,
    RawEvents,
}

impl LogTypeFilter {
    fn matches(self, entry: &LogEntry) -> bool {
        match self {
            LogTypeFilter::All => true,
            LogTypeFilter::Predictions => matches!(entry, LogEntry::IntentPrediction { .. }),
            LogTypeFilter::RawEvents => matches!(entry, LogEntry::RawEvent { .. }),
        }
    }
}

pub struct LogView {
    mode: LogViewMode,
    filter: LogTypeFilter,
    /// Most-recent-first.
    live_lines: Vec<LogEntry>,
    /// Available `YYYY-MM-DD` date stems, most recent first; populated once
    /// from [`nucleus_intent::list_log_dates`] and not re-scanned automatically.
    available_dates: Vec<String>,
    selected_date: Option<String>,
    /// Most-recent-first, capped at [`nucleus_intent::MAX_HISTORY_LINES`] by
    /// `read_log_file` itself.
    history_lines: Vec<LogEntry>,
    history_loading: bool,
    history_error: Option<String>,
    /// Indices into whichever of `live_lines`/`history_lines` is currently
    /// showing, for click-to-expand full-JSON rows.
    expanded: HashSet<usize>,
}

impl LogView {
    pub fn new() -> Self {
        Self {
            mode: LogViewMode::Live,
            filter: LogTypeFilter::All,
            live_lines: Vec::new(),
            available_dates: Vec::new(),
            selected_date: None,
            history_lines: Vec::new(),
            history_loading: false,
            history_error: None,
            expanded: HashSet::default(),
        }
    }

    pub fn push_live(&mut self, entry: LogEntry) {
        self.live_lines.insert(0, entry);
        self.live_lines.truncate(MAX_LIVE_LINES);
    }

    pub fn mode(&self) -> LogViewMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: LogViewMode) {
        self.mode = mode;
        self.expanded.clear();
    }

    pub fn filter(&self) -> LogTypeFilter {
        self.filter
    }

    pub fn set_filter(&mut self, filter: LogTypeFilter) {
        self.filter = filter;
        self.expanded.clear();
    }

    pub fn available_dates(&self) -> &[String] {
        &self.available_dates
    }

    pub fn selected_date(&self) -> Option<&str> {
        self.selected_date.as_deref()
    }

    /// Records the dates found on disk. Picks the most recent one as the
    /// initial selection if nothing's selected yet — doesn't load its
    /// contents; that only happens once History is actually viewed.
    pub fn set_available_dates(&mut self, dates: Vec<String>) {
        if self.selected_date.is_none() {
            self.selected_date = dates.first().cloned();
        }
        self.available_dates = dates;
    }

    pub fn history_loading(&self) -> bool {
        self.history_loading
    }

    pub fn history_error(&self) -> Option<&str> {
        self.history_error.as_deref()
    }

    pub fn history_is_loaded(&self) -> bool {
        !self.history_lines.is_empty() || self.history_error.is_some()
    }

    pub fn begin_loading(&mut self, date: String) {
        self.selected_date = Some(date);
        self.history_lines.clear();
        self.history_loading = true;
        self.history_error = None;
        self.expanded.clear();
    }

    pub fn finish_loading(&mut self, result: Result<Vec<LogEntry>, String>) {
        self.history_loading = false;
        match result {
            Ok(lines) => self.history_lines = lines,
            Err(err) => self.history_error = Some(err),
        }
    }

    pub fn is_expanded(&self, index: usize) -> bool {
        self.expanded.contains(&index)
    }

    pub fn toggle_expanded(&mut self, index: usize) {
        if !self.expanded.remove(&index) {
            self.expanded.insert(index);
        }
    }

    /// The entries for whichever mode is active, filtered, paired with the
    /// index to pass back to [`Self::toggle_expanded`]/[`Self::is_expanded`].
    pub fn visible_lines(&self) -> Vec<(usize, &LogEntry)> {
        let source = match self.mode {
            LogViewMode::Live => &self.live_lines,
            LogViewMode::History => &self.history_lines,
        };
        source
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.filter.matches(entry))
            .collect()
    }
}
