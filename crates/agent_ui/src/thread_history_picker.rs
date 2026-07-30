//! `/history` and `/list` slash commands: a searchable picker over the
//! threads belonging to the current project, opened as a modal instead of a
//! persistent sidebar column. Selecting an entry loads it into the current
//! window's `AgentPanel`, mirroring what `Sidebar::confirm_switcher_selection`
//! does for the full sidebar, but scoped to the current workspace only (no
//! `MultiWorkspace` access is available from `agent_ui`).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Subscription, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{ThreadItem, prelude::*};
use workspace::{ModalView, PathList, Workspace};

use crate::chat_cluster_store::ChatClusterId;
use crate::threads_archive_view::format_history_entry_timestamp;
use crate::thread_metadata_store::{ThreadMetadata, ThreadMetadataStore};
use crate::{Agent, AgentPanel, AgentThreadSource};

/// What set of threads a `ThreadHistoryPicker` lists: the current
/// filesystem workspace's threads (`/history`, `/list`, `/delete`), or a
/// single organizational Chat Cluster's threads (the `/clusters` drill-down).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThreadListScope {
    Workspace,
    ChatCluster(ChatClusterId),
}

/// Takes `&Workspace` directly (not a `WeakEntity<Workspace>` to re-read)
/// because this always runs from inside `workspace.update(cx, ...)` (see
/// `ThreadHistoryPicker::toggle`) — re-entering the entity handle with
/// `.read(cx)`/`.upgrade()` while its own `update` is still on the stack
/// double-lease-panics. `workspace.project()` and other `&Workspace` methods
/// are plain field access, not entity reads, so they're safe here. Unused
/// when `scope` is `ChatCluster`, but kept as a uniform parameter since both
/// call sites already hold a `&Workspace` at the point they call this.
fn thread_history_entries(
    workspace: &Workspace,
    scope: ThreadListScope,
    cx: &App,
) -> Vec<ThreadMetadata> {
    let Some(store) = ThreadMetadataStore::try_global(cx) else {
        return Vec::new();
    };
    let store = store.read(cx);

    let mut entries: Vec<ThreadMetadata> = match scope {
        ThreadListScope::Workspace => {
            let path_list = PathList::new(&workspace.root_paths(cx));
            let remote_connection = workspace.project().read(cx).remote_connection_options(cx);
            store
                .entries_for_path(&path_list, remote_connection.as_ref())
                .filter(|thread| thread.agent_id == *agent::ZED_AGENT_ID)
                .cloned()
                .collect()
        }
        ThreadListScope::ChatCluster(chat_cluster_id) => store
            .entries_for_chat_cluster(chat_cluster_id)
            .filter(|thread| thread.agent_id == *agent::ZED_AGENT_ID)
            .cloned()
            .collect(),
    };
    entries.sort_by_key(|thread| std::cmp::Reverse(thread.interacted_at.unwrap_or(thread.updated_at)));
    entries
}

/// Whether selecting an entry switches to it or archives it. Both share the
/// same search-and-list UI since the only thing that differs is what
/// `confirm` does with the selected thread.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThreadHistoryPickerMode {
    Switch,
    Delete,
}

pub struct ThreadHistoryDelegate {
    workspace: WeakEntity<Workspace>,
    mode: ThreadHistoryPickerMode,
    entries: Vec<ThreadMetadata>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl ThreadHistoryDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        entries: Vec<ThreadMetadata>,
        mode: ThreadHistoryPickerMode,
    ) -> Self {
        let matches = entries
            .iter()
            .enumerate()
            .map(|(candidate_id, entry)| StringMatch {
                candidate_id,
                score: 0.,
                positions: Vec::new(),
                string: entry.display_title().to_string(),
            })
            .collect();
        Self {
            workspace,
            mode,
            entries,
            matches,
            selected_index: 0,
        }
    }
}

impl PickerDelegate for ThreadHistoryDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "thread history"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        match self.mode {
            ThreadHistoryPickerMode::Switch => "Search threads in this project…".into(),
            ThreadHistoryPickerMode::Delete => "Search threads to delete…".into(),
        }
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let entries = self.entries.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |picker, cx| {
            let matches = if query.is_empty() {
                entries
                    .iter()
                    .enumerate()
                    .map(|(candidate_id, entry)| StringMatch {
                        candidate_id,
                        score: 0.,
                        positions: Vec::new(),
                        string: entry.display_title().to_string(),
                    })
                    .collect()
            } else {
                let candidates: Vec<StringMatchCandidate> = entries
                    .iter()
                    .enumerate()
                    .map(|(candidate_id, entry)| {
                        StringMatchCandidate::new(candidate_id, entry.display_title().as_ref())
                    })
                    .collect();
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Arc::new(AtomicBool::default()),
                    executor,
                )
                .await
            };

            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let entry = self
            .matches
            .get(self.selected_index)
            .and_then(|mat| self.entries.get(mat.candidate_id))
            .cloned();

        if let Some(entry) = entry {
            match self.mode {
                ThreadHistoryPickerMode::Switch => {
                    if let Some(workspace) = self.workspace.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                panel.update(cx, |panel, cx| {
                                    panel.load_agent_thread(
                                        Agent::from(entry.agent_id.clone()),
                                        entry.thread_id,
                                        Some(entry.folder_paths().clone()),
                                        entry.title.clone(),
                                        true,
                                        AgentThreadSource::HistoryPicker,
                                        window,
                                        cx,
                                    );
                                });
                                workspace.focus_panel::<AgentPanel>(window, cx);
                            }
                        });
                    }
                }
                ThreadHistoryPickerMode::Delete => {
                    if let Some(store) = ThreadMetadataStore::try_global(cx) {
                        store.update(cx, |store, cx| {
                            store.archive(entry.thread_id, None, cx);
                        });
                    }
                }
            }
        }

        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let entry = self.entries.get(mat.candidate_id)?;
        let timestamp =
            format_history_entry_timestamp(entry.interacted_at.unwrap_or(entry.updated_at));

        Some(
            ThreadItem::new(("thread-history-entry", ix), entry.display_title())
                .rounded(true)
                .highlight_positions(mat.positions.clone())
                .timestamp(timestamp)
                .selected(selected)
                .base_bg(cx.theme().colors().elevated_surface_background)
                .when(self.mode == ThreadHistoryPickerMode::Delete, |item| {
                    item.icon(IconName::Trash)
                        .icon_color(Color::Error)
                })
                .into_any_element(),
        )
    }
}

pub struct ThreadHistoryPicker {
    picker: Entity<Picker<ThreadHistoryDelegate>>,
    _subscription: Subscription,
}

impl ThreadHistoryPicker {
    /// Opens the picker as a modal over `workspace`. Threads are scoped to
    /// that workspace's project — there is no `MultiWorkspace` handle
    /// available from `agent_ui` to browse other projects' threads.
    pub fn toggle(
        workspace: WeakEntity<Workspace>,
        scope: ThreadListScope,
        mode: ThreadHistoryPickerMode,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace_entity) = workspace.upgrade() else {
            return;
        };
        workspace_entity.update(cx, |ws, cx| {
            let entries = thread_history_entries(ws, scope, cx);
            let weak_workspace = workspace.clone();
            ws.toggle_modal(window, cx, |window, cx| {
                Self::new(weak_workspace, entries, mode, window, cx)
            });
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        entries: Vec<ThreadMetadata>,
        mode: ThreadHistoryPickerMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = ThreadHistoryDelegate::new(workspace, entries, mode);
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        let subscription = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription: subscription,
        }
    }
}

impl ModalView for ThreadHistoryPicker {}

impl EventEmitter<DismissEvent> for ThreadHistoryPicker {}

impl Focusable for ThreadHistoryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ThreadHistoryPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.picker.clone()
    }
}
