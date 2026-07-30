//! `/clusters` slash command: a grid ("bento box") browser over Chat
//! Clusters (see `chat_cluster_store`). Selecting a tile opens that
//! cluster's thread list (reusing `thread_history_picker`, scoped to the
//! cluster instead of the workspace); the tile's "+" button starts a new
//! thread filed directly under that cluster.

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Subscription, WeakEntity, Window,
};
use notifications::status_toast::StatusToast;
use ui::prelude::*;
use ui::{Divider, Tooltip};
use ui_input::InputField;
use workspace::{ModalView, PathList, Workspace};

use crate::chat_cluster_store::{ChatCluster, ChatClusterId, ChatClusterStore};
use crate::thread_history_picker::{ThreadHistoryPicker, ThreadHistoryPickerMode, ThreadListScope};
use crate::thread_metadata_store::ThreadMetadataStore;
use crate::{AgentPanel, NewThread};

/// The current workspace's folder, if one is open — `ChatCluster`s are
/// scoped to this (see the module doc comment on `chat_cluster_store`).
fn workspace_path_list(workspace: &WeakEntity<Workspace>, cx: &App) -> Option<PathList> {
    let workspace = workspace.upgrade()?;
    let path_list = PathList::new(&workspace.read(cx).root_paths(cx));
    (!path_list.is_empty()).then_some(path_list)
}

pub struct ChatClusterPicker {
    workspace: WeakEntity<Workspace>,
    store: Entity<ChatClusterStore>,
    focus_handle: FocusHandle,
    creating: bool,
    name_input: Entity<InputField>,
    _subscriptions: Vec<Subscription>,
}

impl ChatClusterPicker {
    /// For callers that only hold a `WeakEntity<Workspace>` (no existing
    /// lease on it) — upgrades and enters `Workspace::update` itself. If the
    /// caller already has `&mut Workspace`/`Context<Workspace>` in hand
    /// (e.g. a `Workspace::register_action` handler), call `open_in`
    /// directly instead: re-entering `.update` on an already-leased
    /// `Workspace` double-lease-panics.
    pub fn toggle(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
        let Some(workspace_entity) = workspace.upgrade() else {
            return;
        };
        workspace_entity.update(cx, |ws, cx| {
            Self::open_in(ws, window, cx);
        });
    }

    pub fn open_in(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let weak_workspace = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(weak_workspace, window, cx)
        });
    }

    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = ChatClusterStore::global(cx);
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        let name_input = cx.new(|cx| InputField::new(window, cx, "Cluster name"));

        Self {
            workspace,
            store,
            focus_handle: cx.focus_handle(),
            creating: false,
            name_input,
            _subscriptions: vec![subscription],
        }
    }

    fn start_creating(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.creating = true;
        let focus_handle = self.name_input.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    fn cancel_creating(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if !self.creating {
            cx.emit(DismissEvent);
            return;
        }
        self.creating = false;
        let editor = self.name_input.read(cx).editor().clone();
        editor.clear(window, cx);
        cx.notify();
    }

    fn confirm_creating(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if !self.creating {
            return;
        }
        let name = self.name_input.read(cx).text(cx);
        let name = name.trim();
        if !name.is_empty() {
            let folder_paths = workspace_path_list(&self.workspace, cx);
            self.store.update(cx, |store, cx| {
                store.create(SharedString::from(name.to_string()), folder_paths, cx);
            });
        }
        self.creating = false;
        let editor = self.name_input.read(cx).editor().clone();
        editor.clear(window, cx);
        cx.notify();
    }

    /// Dismisses the grid and opens `cluster`'s thread list, scoped via
    /// `ThreadListScope::ChatCluster` — reuses `thread_history_picker`
    /// entirely rather than duplicating list/search UI.
    fn browse_cluster(
        &mut self,
        cluster_id: ChatClusterId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        ThreadHistoryPicker::toggle(
            workspace,
            ThreadListScope::ChatCluster(cluster_id),
            ThreadHistoryPickerMode::Switch,
            window,
            cx,
        );
    }

    /// Starts a new thread the normal way (`AgentPanel::new_thread`) and
    /// then tags it with `cluster_id`. The tag is applied via `cx.defer`
    /// rather than immediately after `new_thread` returns: metadata for a
    /// brand-new thread is created reactively off a `RootThreadUpdated`
    /// event (see `thread_metadata_store::handle_conversation_event`), and
    /// deferring gives that subscription a chance to run first so
    /// `set_chat_cluster` finds a row to update instead of silently no-oping.
    fn new_thread_in_cluster(
        &mut self,
        cluster_id: ChatClusterId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        cx.emit(DismissEvent);
        workspace.update(cx, |workspace, cx| {
            // `AgentPanel::new_thread` silently no-ops without an open
            // project folder (it needs a working directory for the agent to
            // operate in) — surface that instead of leaving the click
            // looking like it did nothing.
            if workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .is_none()
            {
                let toast = StatusToast::new(
                    "Open a project folder before starting a new chat.",
                    cx,
                    |toast, _cx| {
                        toast.icon(Icon::new(IconName::Warning).color(Color::Warning))
                    },
                );
                workspace.toggle_status_toast(toast, cx);
                return;
            }
            let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                return;
            };
            let thread_id = panel.update(cx, |panel, cx| {
                panel.new_thread(&NewThread, window, cx);
                panel.active_thread_id(cx)
            });
            if let Some(thread_id) = thread_id {
                cx.defer(move |cx| {
                    if let Some(store) = ThreadMetadataStore::try_global(cx) {
                        store.update(cx, |store, cx| {
                            store.set_chat_cluster(thread_id, Some(cluster_id), cx);
                        });
                    }
                });
            }
            workspace.focus_panel::<AgentPanel>(window, cx);
        });
    }

    /// Claims an unassigned cluster (one with no folder on record — created
    /// before folder-scoping existed) for the current folder.
    fn claim_cluster(&mut self, cluster_id: ChatClusterId, path_list: PathList, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.assign_folder(cluster_id, path_list, cx);
        });
    }

    fn render_tile(
        &self,
        cluster: &ChatCluster,
        thread_count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cluster_id = cluster.id;
        let key = cluster_id.to_key_string();

        v_flex()
            .id(SharedString::from(format!("chat-cluster-tile-{key}")))
            .w(rems(11.))
            .h(rems(7.))
            .p_2()
            .gap_1()
            .justify_between()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().elevated_surface_background)
            .hover(|this| this.border_color(cx.theme().colors().border_focused))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.browse_cluster(cluster_id, window, cx);
            }))
            .child(
                h_flex()
                    .justify_between()
                    .child(Icon::new(IconName::Thread).color(Color::Muted))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("chat-cluster-new-thread-{key}")),
                            IconName::Plus,
                        )
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("New Thread in This Cluster"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.new_thread_in_cluster(cluster_id, window, cx);
                        })),
                    ),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new(cluster.name.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(format!(
                            "{thread_count} thread{}",
                            if thread_count == 1 { "" } else { "s" }
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
    }
}

impl ModalView for ChatClusterPicker {}

impl EventEmitter<DismissEvent> for ChatClusterPicker {}

impl Focusable for ChatClusterPicker {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

const TILES_PER_ROW: usize = 3;

impl Render for ChatClusterPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let thread_store = ThreadMetadataStore::try_global(cx);
        let current_path_list = workspace_path_list(&self.workspace, cx);

        // Collect owned (cluster, thread_count) pairs first: both `read(cx)`
        // calls below borrow `cx` immutably, which must end before
        // `render_tile` can borrow it mutably in the loop after.
        let clusters: Vec<(ChatCluster, usize)> = current_path_list
            .as_ref()
            .map(|path_list| {
                self.store
                    .read(cx)
                    .entries_for_path(path_list)
                    .into_iter()
                    .map(|cluster| {
                        let thread_count = thread_store
                            .as_ref()
                            .map(|store| store.read(cx).entries_for_chat_cluster(cluster.id).count())
                            .unwrap_or(0);
                        (cluster.clone(), thread_count)
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Clusters created before folder-scoping existed have no folder on
        // record — they're excluded from the grid above, but still worth
        // surfacing (with a way to claim them for this folder) rather than
        // leaving them permanently invisible.
        let unassigned: Vec<ChatCluster> = self
            .store
            .read(cx)
            .entries_unassigned()
            .into_iter()
            .cloned()
            .collect();

        let tiles: Vec<_> = clusters
            .iter()
            .map(|(cluster, thread_count)| {
                self.render_tile(cluster, *thread_count, cx).into_any_element()
            })
            .collect();

        let mut tiles = tiles.into_iter();
        let mut rows = Vec::new();
        loop {
            let row_tiles: Vec<_> = tiles.by_ref().take(TILES_PER_ROW).collect();
            if row_tiles.is_empty() {
                break;
            }
            rows.push(h_flex().gap_2().children(row_tiles).into_any_element());
        }

        v_flex()
            .key_context("ChatClusterPicker")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm_creating))
            .on_action(cx.listener(Self::cancel_creating))
            .p_2()
            .gap_2()
            .w(rems(38.))
            .elevation_3(cx)
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("Clusters").size(LabelSize::Large))
                    .child(
                        IconButton::new("new-chat-cluster", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Cluster"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_creating(window, cx);
                            })),
                    ),
            )
            .when(self.creating, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(div().flex_1().child(self.name_input.clone()))
                        .child(
                            Button::new("confirm-new-chat-cluster", "Create").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.confirm_creating(&menu::Confirm, window, cx);
                                }),
                            ),
                        ),
                )
            })
            .child(
                v_flex()
                    .id("chat-cluster-grid")
                    .gap_2()
                    .max_h(rems(28.))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .when_some(
                current_path_list.filter(|_| !unassigned.is_empty()),
                |this, path_list| {
                    this.child(Divider::horizontal()).child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("Unassigned Clusters")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .children(unassigned.into_iter().map(|cluster| {
                                let cluster_id = cluster.id;
                                let path_list = path_list.clone();
                                h_flex()
                                    .id(SharedString::from(format!(
                                        "unassigned-{}",
                                        cluster_id.to_key_string()
                                    )))
                                    .justify_between()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .hover(|this| {
                                        this.bg(cx.theme().colors().element_hover)
                                    })
                                    .child(Label::new(cluster.name).size(LabelSize::Small))
                                    .child(
                                        Button::new(
                                            SharedString::from(format!(
                                                "claim-cluster-{}",
                                                cluster_id.to_key_string()
                                            )),
                                            "Claim for this folder",
                                        )
                                        .label_size(LabelSize::XSmall)
                                        .on_click(cx.listener(move |this, _, _window, cx| {
                                            this.claim_cluster(cluster_id, path_list.clone(), cx);
                                        })),
                                    )
                            })),
                    )
                },
            )
    }
}
