//! "Chat Clusters": an organizational, user-created label for grouping
//! threads (like Claude.ai's "Projects"). Scoped to the folder open at
//! creation time (`ChatCluster::folder_paths`) — a cluster created while
//! working in "stock-prediction" only shows in that folder's `/clusters`
//! grid, not in unrelated folders. Threads reference a cluster via
//! `ThreadMetadata::chat_cluster_id` (see thread_metadata_store.rs); this
//! store only owns the clusters themselves.

use anyhow::Result;
use chrono::{DateTime, Utc};
use collections::HashMap;
use db::{
    query,
    sqlez::{
        bindable::{Bind, Column, StaticColumnCount},
        domain::Domain,
        statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use gpui::{App, AppContext as _, Context, Entity, Global};
use ui::SharedString;
use util::ResultExt as _;
use workspace::PathList;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct ChatClusterId(uuid::Uuid);

impl ChatClusterId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// Stable, hyphenated string form suitable for use as an `ElementId`.
    pub fn to_key_string(&self) -> String {
        self.0.hyphenated().to_string()
    }
}

impl Bind for ChatClusterId {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        self.0.bind(statement, start_index)
    }
}

impl Column for ChatClusterId {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (uuid, next) = Column::column(statement, start_index)?;
        Ok((ChatClusterId(uuid), next))
    }
}

impl StaticColumnCount for ChatClusterId {}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatCluster {
    pub id: ChatClusterId,
    pub name: SharedString,
    pub created_at: DateTime<Utc>,
    /// The folder open when this cluster was created — `None` for clusters
    /// created before folder-scoping existed (or otherwise unassigned).
    /// Unassigned clusters are excluded from the normal per-folder grid;
    /// see `ChatClusterStore::entries_unassigned`/`assign_folder`.
    pub folder_paths: Option<PathList>,
}

impl Column for ChatCluster {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (id, next) = ChatClusterId::column(statement, start_index)?;
        let (name, next): (String, i32) = Column::column(statement, next)?;
        let (created_at_secs, next): (i64, i32) = Column::column(statement, next)?;
        let (folder_paths_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let created_at = DateTime::from_timestamp(created_at_secs, 0).unwrap_or_else(Utc::now);
        let folder_paths = folder_paths_str.map(|paths| {
            PathList::deserialize(&util::path_list::SerializedPathList {
                paths,
                order: folder_paths_order_str.unwrap_or_default(),
            })
        });
        Ok((
            Self {
                id,
                name: name.into(),
                created_at,
                folder_paths,
            },
            next,
        ))
    }
}

struct ChatClusterDb(ThreadSafeConnection);

impl Domain for ChatClusterDb {
    const NAME: &str = stringify!(ChatClusterDb);
    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS chat_projects(
                id BLOB PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
            ) STRICT;
        ),
        sql!(ALTER TABLE chat_projects ADD COLUMN folder_paths TEXT;),
        sql!(ALTER TABLE chat_projects ADD COLUMN folder_paths_order TEXT;),
        // Renaming "Chat Projects" to "Chat Clusters" in the UI; this table
        // was already shipped under the old name, so it's renamed via an
        // additive migration rather than editing the migrations above,
        // which would change their stored text and fail the migration
        // integrity check (and orphan any existing rows).
        sql!(ALTER TABLE chat_projects RENAME TO chat_clusters;),
    ];
}

db::static_connection!(ChatClusterDb, []);

impl ChatClusterDb {
    query! {
        pub fn list_clusters() -> Result<Vec<ChatCluster>> {
            SELECT id, name, created_at, folder_paths, folder_paths_order FROM chat_clusters ORDER BY created_at ASC
        }
    }

    query! {
        async fn insert_cluster(
            id: ChatClusterId,
            name: String,
            created_at: i64,
            folder_paths: Option<String>,
            folder_paths_order: Option<String>
        ) -> Result<()> {
            INSERT INTO chat_clusters (id, name, created_at, folder_paths, folder_paths_order) VALUES ((?), (?), (?), (?), (?));
        }
    }

    query! {
        async fn update_folder_paths(
            folder_paths: String,
            folder_paths_order: String,
            id: ChatClusterId
        ) -> Result<()> {
            UPDATE chat_clusters SET folder_paths = (?), folder_paths_order = (?) WHERE id = (?);
        }
    }
}

pub struct ChatClusterStore {
    db: ChatClusterDb,
    clusters: HashMap<ChatClusterId, ChatCluster>,
}

struct GlobalChatClusterStore(Entity<ChatClusterStore>);
impl Global for GlobalChatClusterStore {}

pub fn init(cx: &mut App) {
    ChatClusterStore::init_global(cx);
}

impl ChatClusterStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalChatClusterStore>() {
            return;
        }

        let db = ChatClusterDb::global(cx);
        let store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalChatClusterStore(store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalChatClusterStore>() {
            return;
        }

        let thread = std::thread::current();
        let test_name = thread.name().unwrap_or("unknown_test");
        let db_name = format!("CHAT_CLUSTER_DB_{test_name}");
        let db = gpui::block_on(db::open_test_db::<ChatClusterDb>(&db_name));
        let store = cx.new(|cx| Self::new(ChatClusterDb(db), cx));
        cx.set_global(GlobalChatClusterStore(store));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalChatClusterStore>().0.clone()
    }

    fn new(db: ChatClusterDb, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            db,
            clusters: HashMap::default(),
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let clusters = cx
                .background_spawn(async move { db.list_clusters() })
                .await
                .log_err();
            let Some(clusters) = clusters else { return };
            this.update(cx, |this, cx| {
                for cluster in clusters {
                    this.clusters.insert(cluster.id, cluster);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Creates a new chat cluster and returns its id immediately; the
    /// in-memory map is updated synchronously, the DB write happens in the
    /// background (best-effort, logged on failure — mirrors the pattern used
    /// throughout `ThreadMetadataStore`). `folder_paths` is the current
    /// workspace's folder (`None` leaves it unassigned, though the normal
    /// creation flow always has a folder open by the time this is called).
    pub fn create(
        &mut self,
        name: SharedString,
        folder_paths: Option<PathList>,
        cx: &mut Context<Self>,
    ) -> ChatClusterId {
        let id = ChatClusterId::new();
        let created_at = Utc::now();
        self.clusters.insert(
            id,
            ChatCluster {
                id,
                name: name.clone(),
                created_at,
                folder_paths: folder_paths.clone(),
            },
        );
        cx.notify();

        let db = self.db.clone();
        cx.background_spawn(async move {
            let (paths, order) = match &folder_paths {
                Some(path_list) => {
                    let serialized = path_list.serialize();
                    (Some(serialized.paths), Some(serialized.order))
                }
                None => (None, None),
            };
            db.insert_cluster(id, name.to_string(), created_at.timestamp(), paths, order)
                .await
                .log_err();
        })
        .detach();

        id
    }

    /// Clusters created while `path_list`'s folder was open, most-recently-
    /// created first.
    pub fn entries_for_path(&self, path_list: &PathList) -> Vec<&ChatCluster> {
        let mut entries: Vec<&ChatCluster> = self
            .clusters
            .values()
            .filter(|cluster| cluster.folder_paths.as_ref() == Some(path_list))
            .collect();
        entries.sort_by_key(|cluster| std::cmp::Reverse(cluster.created_at));
        entries
    }

    /// Clusters with no folder on record (created before folder-scoping
    /// existed), most-recently-created first — excluded from the normal
    /// grid; surfaced separately so they can be claimed via `assign_folder`.
    pub fn entries_unassigned(&self) -> Vec<&ChatCluster> {
        let mut entries: Vec<&ChatCluster> = self
            .clusters
            .values()
            .filter(|cluster| cluster.folder_paths.is_none())
            .collect();
        entries.sort_by_key(|cluster| std::cmp::Reverse(cluster.created_at));
        entries
    }

    /// Claims an unassigned cluster for `path_list`'s folder.
    pub fn assign_folder(
        &mut self,
        id: ChatClusterId,
        path_list: PathList,
        cx: &mut Context<Self>,
    ) {
        let Some(cluster) = self.clusters.get_mut(&id) else {
            return;
        };
        cluster.folder_paths = Some(path_list.clone());
        cx.notify();

        let db = self.db.clone();
        cx.background_spawn(async move {
            let serialized = path_list.serialize();
            db.update_folder_paths(serialized.paths, serialized.order, id)
                .await
                .log_err();
        })
        .detach();
    }
}
