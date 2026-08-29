use serde::{Deserialize, Serialize};
use sqlx::{Error as SqlxError, FromRow, Sqlite, SqlitePool};
use tracing::info;

/// Hard cap on nesting. Bounds the recursive ancestor walk so a corrupt row cannot
/// hang the query, and is enforced on create and move so no chain can outgrow it.
const MAX_FOLDER_DEPTH: i64 = 64;

/// Folder row joined with the number of meetings assigned directly to it.
/// The count is direct, not the subtree total - the sidebar nests the rows itself.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct FolderWithCount {
    pub id: String,
    pub name: String,
    pub created_at: String,
    /// Enclosing folder (folders.id); `None` for a top-level folder.
    pub parent_id: Option<String>,
    pub meeting_count: i64,
}

/// Folder-tree failures that callers need to distinguish from plain database errors.
#[derive(Debug)]
pub enum FolderError {
    Db(SqlxError),
    NotFound,
    ParentNotFound,
    /// Deletion is blocked while the folder still has subfolders.
    HasSubfolders(i64),
    /// Moving a folder into itself or into one of its own descendants.
    Cycle,
    /// The move or create would push nesting past `MAX_FOLDER_DEPTH`.
    TooDeep,
}

/// Result of walking a folder's ancestors looking for another folder.
enum AncestorCheck {
    /// Not an ancestor; carries how many levels sit above the folder that was walked.
    Absent(i64),
    Found,
    /// The walk hit `MAX_FOLDER_DEPTH` without reaching a root, so the answer is unknown.
    DepthExceeded,
}

impl From<SqlxError> for FolderError {
    fn from(err: SqlxError) -> Self {
        FolderError::Db(err)
    }
}

impl std::fmt::Display for FolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The raw sqlx text reaches the user as a toast; keep the detail in the logs.
            FolderError::Db(_) => write!(f, "Could not update folders. See the app logs for details."),
            FolderError::NotFound => write!(f, "Folder not found"),
            FolderError::ParentNotFound => write!(f, "Parent folder not found"),
            FolderError::HasSubfolders(count) => write!(
                f,
                "Folder still has {} subfolder{}. Move or delete them first.",
                count,
                if *count == 1 { "" } else { "s" }
            ),
            FolderError::Cycle => {
                write!(f, "A folder cannot be moved inside itself or its subfolders")
            }
            FolderError::TooDeep => write!(
                f,
                "Folders cannot be nested more than {} levels deep",
                MAX_FOLDER_DEPTH
            ),
        }
    }
}

impl std::error::Error for FolderError {}

pub struct FoldersRepository;

impl FoldersRepository {
    /// Every folder, flat. Callers nest the rows by `parent_id`.
    pub async fn get_folders(pool: &SqlitePool) -> Result<Vec<FolderWithCount>, SqlxError> {
        let folders = sqlx::query_as::<_, FolderWithCount>(
            "SELECT f.id, f.name, f.created_at, f.parent_id, COUNT(m.id) AS meeting_count
             FROM folders f
             LEFT JOIN meetings m ON m.folder_id = f.id
             GROUP BY f.id, f.name, f.created_at, f.parent_id
             ORDER BY f.name COLLATE NOCASE ASC",
        )
        .fetch_all(pool)
        .await?;
        Ok(folders)
    }

    /// Create a folder, optionally nested inside `parent_id`.
    /// The parent check and the insert share one transaction so the parent cannot
    /// disappear between them.
    pub async fn create_folder(
        pool: &SqlitePool,
        id: &str,
        name: &str,
        created_at: &str,
        parent_id: Option<&str>,
    ) -> Result<(), FolderError> {
        let mut tx = pool.begin().await?;

        if let Some(parent) = parent_id {
            if !Self::folder_exists(&mut *tx, parent).await? {
                return Err(FolderError::ParentNotFound);
            }
            // Refuse before the chain outgrows the walk that guards moves against cycles.
            if Self::ancestor_depth(&mut *tx, parent).await? + 1 >= MAX_FOLDER_DEPTH {
                return Err(FolderError::TooDeep);
            }
        }

        sqlx::query("INSERT INTO folders (id, name, created_at, parent_id) VALUES (?, ?, ?, ?)")
            .bind(id)
            .bind(name)
            .bind(created_at)
            .bind(parent_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        info!("Created folder {} ({}) under {:?}", name, id, parent_id);
        Ok(())
    }

    pub async fn rename_folder(
        pool: &SqlitePool,
        folder_id: &str,
        name: &str,
    ) -> Result<bool, SqlxError> {
        let result = sqlx::query("UPDATE folders SET name = ? WHERE id = ?")
            .bind(name)
            .bind(folder_id)
            .execute(pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Reparent a folder. `None` moves it back to the top level.
    /// The cycle check and the update share one transaction; running them apart lets two
    /// concurrent moves each see a clean tree and commit a cycle between them.
    pub async fn move_folder(
        pool: &SqlitePool,
        folder_id: &str,
        parent_id: Option<&str>,
    ) -> Result<(), FolderError> {
        let mut tx = pool.begin().await?;

        if !Self::folder_exists(&mut *tx, folder_id).await? {
            return Err(FolderError::NotFound);
        }

        if let Some(parent) = parent_id {
            if parent == folder_id {
                return Err(FolderError::Cycle);
            }
            if !Self::folder_exists(&mut *tx, parent).await? {
                return Err(FolderError::ParentNotFound);
            }
            // Dropping a folder into its own subtree would detach that subtree from the root.
            // The same walk yields the parent's depth, so no second traversal is needed.
            let parent_depth = match Self::ancestor_check(&mut *tx, parent, folder_id).await? {
                AncestorCheck::Found => return Err(FolderError::Cycle),
                // The walk ran out of depth, so "no cycle" would be a guess. Refuse instead.
                AncestorCheck::DepthExceeded => return Err(FolderError::TooDeep),
                AncestorCheck::Absent(depth) => depth,
            };

            // The folder brings its whole subtree along, so check the deepest leaf, not just itself.
            let landing_depth = parent_depth + 1;
            if landing_depth + Self::subtree_height(&mut *tx, folder_id).await? >= MAX_FOLDER_DEPTH {
                return Err(FolderError::TooDeep);
            }
        }

        sqlx::query("UPDATE folders SET parent_id = ? WHERE id = ?")
            .bind(parent_id)
            .bind(folder_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        info!("Moved folder {} under {:?}", folder_id, parent_id);
        Ok(())
    }

    /// Delete a folder; its meetings become unfiled (folder_id = NULL).
    /// Refused while the folder still has subfolders, so nothing is lost silently.
    pub async fn delete_folder(pool: &SqlitePool, folder_id: &str) -> Result<(), FolderError> {
        let mut tx = pool.begin().await?;

        // Inside the transaction: a subfolder created after an outside check would make the
        // DELETE fail on the parent_id foreign key with a raw constraint error.
        let subfolders = Self::count_subfolders(&mut *tx, folder_id).await?;
        if subfolders > 0 {
            return Err(FolderError::HasSubfolders(subfolders));
        }

        sqlx::query("UPDATE meetings SET folder_id = NULL WHERE folder_id = ?")
            .bind(folder_id)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query("DELETE FROM folders WHERE id = ?")
            .bind(folder_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        if result.rows_affected() == 0 {
            return Err(FolderError::NotFound);
        }

        info!("Deleted folder {}", folder_id);
        Ok(())
    }

    /// Assign a meeting to a folder, or unfile it with `None`.
    pub async fn set_meeting_folder(
        pool: &SqlitePool,
        meeting_id: &str,
        folder_id: Option<&str>,
    ) -> Result<bool, SqlxError> {
        let result = sqlx::query("UPDATE meetings SET folder_id = ? WHERE id = ?")
            .bind(folder_id)
            .bind(meeting_id)
            .execute(pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Number of folders nested directly inside `folder_id`.
    async fn count_subfolders<'e, E>(executor: E, folder_id: &str) -> Result<i64, SqlxError>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM folders WHERE parent_id = ?")
            .bind(folder_id)
            .fetch_one(executor)
            .await?;
        Ok(count)
    }

    async fn folder_exists<'e, E>(executor: E, folder_id: &str) -> Result<bool, SqlxError>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM folders WHERE id = ?")
            .bind(folder_id)
            .fetch_one(executor)
            .await?;
        Ok(count > 0)
    }

    /// Walk upward from `candidate`, capped at `MAX_FOLDER_DEPTH`, reporting both whether
    /// `ancestor` was reached and whether the walk ran out of depth before finishing.
    /// `candidate` counts as its own ancestor, which is what the self-move check needs.
    async fn ancestor_check<'e, E>(
        executor: E,
        candidate: &str,
        ancestor: &str,
    ) -> Result<AncestorCheck, SqlxError>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        let (hits, max_depth): (Option<i64>, Option<i64>) = sqlx::query_as(
            "WITH RECURSIVE ancestors(id, parent_id, depth) AS (
                 SELECT id, parent_id, 0 FROM folders WHERE id = ?
                 UNION ALL
                 SELECT f.id, f.parent_id, a.depth + 1
                 FROM folders f
                 JOIN ancestors a ON f.id = a.parent_id
                 WHERE a.depth < ?
             )
             SELECT SUM(CASE WHEN id = ? THEN 1 ELSE 0 END), MAX(depth) FROM ancestors",
        )
        .bind(candidate)
        .bind(MAX_FOLDER_DEPTH)
        .bind(ancestor)
        .fetch_one(executor)
        .await?;

        let depth = max_depth.unwrap_or(0);
        if hits.unwrap_or(0) > 0 {
            Ok(AncestorCheck::Found)
        } else if depth >= MAX_FOLDER_DEPTH {
            Ok(AncestorCheck::DepthExceeded)
        } else {
            Ok(AncestorCheck::Absent(depth))
        }
    }

    /// Levels below `folder_id`; 0 when it has no subfolders.
    async fn subtree_height<'e, E>(executor: E, folder_id: &str) -> Result<i64, SqlxError>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        let (max_depth,): (Option<i64>,) = sqlx::query_as(
            "WITH RECURSIVE descendants(id, depth) AS (
                 SELECT id, 0 FROM folders WHERE id = ?
                 UNION ALL
                 SELECT f.id, d.depth + 1
                 FROM folders f
                 JOIN descendants d ON f.parent_id = d.id
                 WHERE d.depth < ?
             )
             SELECT MAX(depth) FROM descendants",
        )
        .bind(folder_id)
        .bind(MAX_FOLDER_DEPTH)
        .fetch_one(executor)
        .await?;
        Ok(max_depth.unwrap_or(0))
    }

    /// How many levels sit above `folder_id`; 0 for a top-level folder.
    async fn ancestor_depth<'e, E>(executor: E, folder_id: &str) -> Result<i64, SqlxError>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        let (max_depth,): (Option<i64>,) = sqlx::query_as(
            "WITH RECURSIVE ancestors(id, parent_id, depth) AS (
                 SELECT id, parent_id, 0 FROM folders WHERE id = ?
                 UNION ALL
                 SELECT f.id, f.parent_id, a.depth + 1
                 FROM folders f
                 JOIN ancestors a ON f.id = a.parent_id
                 WHERE a.depth < ?
             )
             SELECT MAX(depth) FROM ancestors",
        )
        .bind(folder_id)
        .bind(MAX_FOLDER_DEPTH)
        .fetch_one(executor)
        .await?;
        Ok(max_depth.unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real database on the real schema, so the tree rules are exercised against the
    /// same constraints production runs under.
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");
        pool
    }

    async fn folder(pool: &SqlitePool, id: &str, parent: Option<&str>) {
        FoldersRepository::create_folder(pool, id, id, "2026-08-28T00:00:00Z", parent)
            .await
            .unwrap_or_else(|e| panic!("create {}: {}", id, e));
    }

    async fn seed_meeting(pool: &SqlitePool, id: &str, folder_id: Option<&str>) {
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at, folder_id) VALUES (?, ?, ?, ?, ?)")
            .bind(id)
            .bind("Test meeting")
            .bind("2026-08-28T00:00:00Z")
            .bind("2026-08-28T00:00:00Z")
            .bind(folder_id)
            .execute(pool)
            .await
            .expect("seed meeting");
    }

    async fn parent_of(pool: &SqlitePool, id: &str) -> Option<String> {
        FoldersRepository::get_folders(pool)
            .await
            .expect("get_folders")
            .into_iter()
            .find(|f| f.id == id)
            .expect("folder present")
            .parent_id
    }

    #[tokio::test]
    async fn create_nests_under_parent_and_counts_only_direct_meetings() {
        let pool = test_pool().await;
        folder(&pool, "work", None).await;
        folder(&pool, "clients", Some("work")).await;
        seed_meeting(&pool, "m1", Some("clients")).await;
        seed_meeting(&pool, "m2", Some("clients")).await;

        let folders = FoldersRepository::get_folders(&pool).await.expect("get_folders");
        let work = folders.iter().find(|f| f.id == "work").unwrap();
        let clients = folders.iter().find(|f| f.id == "clients").unwrap();

        assert_eq!(work.parent_id, None);
        assert_eq!(clients.parent_id, Some("work".to_string()));
        // The count is direct, so the parent does not absorb its child's meetings
        assert_eq!(work.meeting_count, 0);
        assert_eq!(clients.meeting_count, 2);
    }

    #[tokio::test]
    async fn create_under_missing_parent_is_refused() {
        let pool = test_pool().await;
        let err = FoldersRepository::create_folder(&pool, "x", "X", "t", Some("ghost"))
            .await
            .expect_err("missing parent");
        assert!(matches!(err, FolderError::ParentNotFound));
        assert!(FoldersRepository::get_folders(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn move_reparents_and_returns_to_top_level() {
        let pool = test_pool().await;
        folder(&pool, "work", None).await;
        folder(&pool, "personal", None).await;
        folder(&pool, "notes", Some("work")).await;

        FoldersRepository::move_folder(&pool, "notes", Some("personal")).await.unwrap();
        assert_eq!(parent_of(&pool, "notes").await, Some("personal".to_string()));

        FoldersRepository::move_folder(&pool, "notes", None).await.unwrap();
        assert_eq!(parent_of(&pool, "notes").await, None);
    }

    #[tokio::test]
    async fn move_into_own_subtree_or_self_is_refused() {
        let pool = test_pool().await;
        folder(&pool, "a", None).await;
        folder(&pool, "b", Some("a")).await;
        folder(&pool, "c", Some("b")).await;

        // Into itself
        assert!(matches!(
            FoldersRepository::move_folder(&pool, "a", Some("a")).await.expect_err("self"),
            FolderError::Cycle
        ));
        // Into a direct child
        assert!(matches!(
            FoldersRepository::move_folder(&pool, "a", Some("b")).await.expect_err("child"),
            FolderError::Cycle
        ));
        // Into a grandchild
        assert!(matches!(
            FoldersRepository::move_folder(&pool, "a", Some("c")).await.expect_err("grandchild"),
            FolderError::Cycle
        ));
        // The tree is untouched by the refusals
        assert_eq!(parent_of(&pool, "a").await, None);
        assert_eq!(parent_of(&pool, "c").await, Some("b".to_string()));
    }

    #[tokio::test]
    async fn move_of_unknown_folder_or_to_unknown_parent_is_refused() {
        let pool = test_pool().await;
        folder(&pool, "a", None).await;

        assert!(matches!(
            FoldersRepository::move_folder(&pool, "ghost", None).await.expect_err("no folder"),
            FolderError::NotFound
        ));
        assert!(matches!(
            FoldersRepository::move_folder(&pool, "a", Some("ghost")).await.expect_err("no parent"),
            FolderError::ParentNotFound
        ));
    }

    #[tokio::test]
    async fn nesting_stops_at_the_depth_cap() {
        let pool = test_pool().await;
        folder(&pool, "d0", None).await;
        // Deepest allowed leaf sits at MAX_FOLDER_DEPTH - 1
        for level in 1..MAX_FOLDER_DEPTH {
            let id = format!("d{}", level);
            let parent = format!("d{}", level - 1);
            FoldersRepository::create_folder(&pool, &id, &id, "t", Some(&parent))
                .await
                .unwrap_or_else(|e| panic!("create at depth {}: {}", level, e));
        }

        let too_deep = format!("d{}", MAX_FOLDER_DEPTH);
        let parent = format!("d{}", MAX_FOLDER_DEPTH - 1);
        assert!(matches!(
            FoldersRepository::create_folder(&pool, &too_deep, &too_deep, "t", Some(&parent))
                .await
                .expect_err("past the cap"),
            FolderError::TooDeep
        ));
    }

    #[tokio::test]
    async fn move_is_refused_when_the_subtree_would_not_fit() {
        let pool = test_pool().await;
        // A chain two short of the cap, plus a three-level subtree to drop on the end
        folder(&pool, "c0", None).await;
        for level in 1..(MAX_FOLDER_DEPTH - 2) {
            let id = format!("c{}", level);
            let parent = format!("c{}", level - 1);
            FoldersRepository::create_folder(&pool, &id, &id, "t", Some(&parent)).await.unwrap();
        }
        folder(&pool, "s0", None).await;
        folder(&pool, "s1", Some("s0")).await;
        folder(&pool, "s2", Some("s1")).await;

        let deepest = format!("c{}", MAX_FOLDER_DEPTH - 3);
        assert!(matches!(
            FoldersRepository::move_folder(&pool, "s0", Some(&deepest))
                .await
                .expect_err("subtree overflows the cap"),
            FolderError::TooDeep
        ));
        // Refusing left the subtree where it was
        assert_eq!(parent_of(&pool, "s0").await, None);
    }

    #[tokio::test]
    async fn delete_is_blocked_while_subfolders_remain() {
        let pool = test_pool().await;
        folder(&pool, "work", None).await;
        folder(&pool, "clients", Some("work")).await;

        let err = FoldersRepository::delete_folder(&pool, "work").await.expect_err("has children");
        assert!(matches!(err, FolderError::HasSubfolders(1)), "got {:?}", err);
        assert_eq!(FoldersRepository::get_folders(&pool).await.unwrap().len(), 2);

        // Clearing the child first unblocks the parent
        FoldersRepository::delete_folder(&pool, "clients").await.unwrap();
        FoldersRepository::delete_folder(&pool, "work").await.unwrap();
        assert!(FoldersRepository::get_folders(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_unfiles_its_meetings_and_leaves_others_alone() {
        let pool = test_pool().await;
        folder(&pool, "work", None).await;
        folder(&pool, "personal", None).await;
        seed_meeting(&pool, "m1", Some("work")).await;
        seed_meeting(&pool, "m2", Some("personal")).await;

        FoldersRepository::delete_folder(&pool, "work").await.unwrap();

        let filed: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, folder_id FROM meetings ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("read meetings");
        assert_eq!(filed, vec![
            ("m1".to_string(), None),
            ("m2".to_string(), Some("personal".to_string())),
        ]);
    }

    #[tokio::test]
    async fn delete_of_unknown_folder_reports_not_found() {
        let pool = test_pool().await;
        assert!(matches!(
            FoldersRepository::delete_folder(&pool, "ghost").await.expect_err("absent"),
            FolderError::NotFound
        ));
    }
}
