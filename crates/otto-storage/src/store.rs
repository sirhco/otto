//! Async SQLite persistence for sessions, messages, and parts — a Rust port of
//! opencode's `packages/core/src/session/sql.ts` (the table schema) and the
//! row hydration in `packages/opencode/src/session/message-v2.ts`.
//!
//! Three tables mirror `sql.ts`:
//!
//! * `session` (`sql.ts:22-66`) — a pragmatic subset of columns.
//! * `message` (`sql.ts:68-80`) — `id` / `session_id` / `time_created` /
//!   `time_updated` columns plus a JSON `data` blob (`Info` minus
//!   `id`/`sessionID`).
//! * `part` (`sql.ts:82-98`) — `id` / `message_id` / `session_id` /
//!   `time_created` / `time_updated` columns plus a JSON `data` blob (`Part`
//!   minus `id`/`sessionID`/`messageID`).

use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::model::{Info, Part, WithParts};

/// Errors returned by [`Store`] operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// An underlying `sqlx` / SQLite error.
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A JSON (de)serialization error while (un)packing a `data` blob.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Session prompt-cache token counts (`sql.ts:47-48`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionCacheTokens {
    /// Cache-read tokens.
    pub read: i64,
    /// Cache-write tokens.
    pub write: i64,
}

/// Session token accounting (`sql.ts:44-48`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionTokens {
    /// Prompt/input tokens.
    pub input: i64,
    /// Completion/output tokens.
    pub output: i64,
    /// Reasoning tokens.
    pub reasoning: i64,
    /// Prompt-cache breakdown.
    pub cache: SessionCacheTokens,
}

/// A persisted session — a pragmatic subset of the `session` table
/// (`sql.ts:22-66`) keeping the required id/fk/directory/title/time columns and
/// the cost/token accounting columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Session id (`ses_…`).
    pub id: String,
    /// Owning project id (`sql.ts:26`).
    pub project_id: String,
    /// Optional parent session id (`sql.ts:31`).
    pub parent_id: Option<String>,
    /// Working directory (`sql.ts:33`).
    pub directory: String,
    /// Session title (`sql.ts:35`).
    pub title: String,
    /// opencode version string (`sql.ts:36`).
    pub version: String,
    /// Accumulated cost (`sql.ts:43`).
    pub cost: f64,
    /// Accumulated token accounting (`sql.ts:44-48`).
    pub tokens: SessionTokens,
    /// Free-form metadata JSON (`sql.ts:42`).
    pub metadata: Option<serde_json::Value>,
    /// Creation timestamp (`Timestamps`, `sql.ts:57`).
    pub time_created: i64,
    /// Last-update timestamp (`Timestamps`, `sql.ts:57`).
    pub time_updated: i64,
}

/// A row of the `workflow_task` table — SDD/TDD per-task status persistence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowTaskRow {
    /// Task row id.
    pub id: String,
    /// Owning session id.
    pub session_id: String,
    /// Workflow kind (e.g. `sdd`).
    pub workflow_kind: String,
    /// Ordinal position of the task within the workflow.
    pub task_index: i64,
    /// Status wire string (e.g. `DONE`, `NEEDS_CONTEXT`).
    pub status: String,
    /// Optional free-form notes.
    pub notes: Option<String>,
    /// Last-update timestamp.
    pub updated_at: i64,
}

/// `CREATE TABLE IF NOT EXISTS` migrations mirroring `sql.ts:22-98`.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS session (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL,
        parent_id TEXT,
        directory TEXT NOT NULL,
        title TEXT NOT NULL,
        version TEXT NOT NULL,
        metadata TEXT,
        cost REAL NOT NULL DEFAULT 0,
        tokens_input INTEGER NOT NULL DEFAULT 0,
        tokens_output INTEGER NOT NULL DEFAULT 0,
        tokens_reasoning INTEGER NOT NULL DEFAULT 0,
        tokens_cache_read INTEGER NOT NULL DEFAULT 0,
        tokens_cache_write INTEGER NOT NULL DEFAULT 0,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS session_project_idx ON session (project_id)",
    "CREATE INDEX IF NOT EXISTS session_parent_idx ON session (parent_id)",
    "CREATE TABLE IF NOT EXISTS message (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES session (id) ON DELETE CASCADE,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL,
        data TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS message_session_time_created_id_idx
        ON message (session_id, time_created, id)",
    "CREATE TABLE IF NOT EXISTS part (
        id TEXT PRIMARY KEY,
        message_id TEXT NOT NULL REFERENCES message (id) ON DELETE CASCADE,
        session_id TEXT NOT NULL,
        time_created INTEGER NOT NULL,
        time_updated INTEGER NOT NULL,
        data TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS part_message_id_id_idx ON part (message_id, id)",
    "CREATE INDEX IF NOT EXISTS part_session_idx ON part (session_id)",
    "CREATE TABLE IF NOT EXISTS workflow_task (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        workflow_kind TEXT NOT NULL,
        task_index INTEGER NOT NULL,
        status TEXT NOT NULL,
        notes TEXT,
        updated_at INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS workflow_task_session_idx
        ON workflow_task (session_id, task_index)",
];

/// Async SQLite store over a `sqlx` [`SqlitePool`].
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Opens (creating if missing) a file-backed store and runs migrations.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on connection or migration failure.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            // WAL lets readers proceed while the streaming writer commits;
            // the default rollback journal serializes them, which stalls the
            // per-delta persistence path under concurrent history reads.
            .journal_mode(SqliteJournalMode::Wal)
            // Wait for a contended lock instead of failing immediately with
            // SQLITE_BUSY (e.g. the title-generation writer racing a turn).
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Opens an in-memory store (single shared connection) and runs migrations.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on connection or migration failure.
    pub async fn open_in_memory() -> Result<Self, StorageError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("static in-memory connection string is valid")
            .foreign_keys(true);
        // An in-memory database lives on a single connection, so cap the pool
        // at one connection to keep every query on the same database.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Borrows the underlying connection pool.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Runs the idempotent `CREATE TABLE IF NOT EXISTS` migrations.
    async fn migrate(&self) -> Result<(), StorageError> {
        for stmt in MIGRATIONS {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }

    // -- sessions -----------------------------------------------------------

    /// Inserts a session (`sql.ts:22-66`).
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn create_session(&self, session: &Session) -> Result<(), StorageError> {
        let metadata = session
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        sqlx::query(
            "INSERT INTO session (
                id, project_id, parent_id, directory, title, version, metadata,
                cost, tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, time_created, time_updated
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&session.id)
        .bind(&session.project_id)
        .bind(&session.parent_id)
        .bind(&session.directory)
        .bind(&session.title)
        .bind(&session.version)
        .bind(&metadata)
        .bind(session.cost)
        .bind(session.tokens.input)
        .bind(session.tokens.output)
        .bind(session.tokens.reasoning)
        .bind(session.tokens.cache.read)
        .bind(session.tokens.cache.write)
        .bind(session.time_created)
        .bind(session.time_updated)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Updates a session's `title`. Used to auto-name a session from its first
    /// prompt once a short summary has been generated.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite failure.
    pub async fn update_session_title(&self, id: &str, title: &str) -> Result<(), StorageError> {
        sqlx::query("UPDATE session SET title = ? WHERE id = ?")
            .bind(title)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Fetches a session by id.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn get_session(&self, id: &str) -> Result<Option<Session>, StorageError> {
        let row = sqlx::query(
            "SELECT id, project_id, parent_id, directory, title, version, metadata,
                    cost, tokens_input, tokens_output, tokens_reasoning,
                    tokens_cache_read, tokens_cache_write, time_created, time_updated
             FROM session WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(session_from_row).transpose()
    }

    /// Lists all sessions ordered by `(time_created, id)`.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn list_sessions(&self) -> Result<Vec<Session>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, project_id, parent_id, directory, title, version, metadata,
                    cost, tokens_input, tokens_output, tokens_reasoning,
                    tokens_cache_read, tokens_cache_write, time_created, time_updated
             FROM session ORDER BY time_created, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(session_from_row).collect()
    }

    // -- workflow tasks -----------------------------------------------------

    /// Inserts or updates a workflow-task row by `id` (advances status/notes).
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite failure.
    pub async fn upsert_workflow_task(&self, row: &WorkflowTaskRow) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO workflow_task
                (id, session_id, workflow_kind, task_index, status, notes, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 status = excluded.status,
                 notes = excluded.notes,
                 updated_at = excluded.updated_at",
        )
        .bind(&row.id)
        .bind(&row.session_id)
        .bind(&row.workflow_kind)
        .bind(row.task_index)
        .bind(&row.status)
        .bind(&row.notes)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lists a session's workflow tasks ordered by `task_index`.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite failure.
    pub async fn list_workflow_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<WorkflowTaskRow>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, session_id, workflow_kind, task_index, status, notes, updated_at
             FROM workflow_task WHERE session_id = ? ORDER BY task_index, id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(workflow_task_from_row).collect()
    }

    // -- messages -----------------------------------------------------------

    /// Inserts a message, persisting `data` as `Info` minus `id`/`sessionID`
    /// (`sql.ts:19,68-80`).
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn insert_message(&self, info: &Info) -> Result<(), StorageError> {
        let data = info.data_json()?;
        let time_created = info.time_created();
        sqlx::query(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(info.id())
        .bind(&info.session_id)
        .bind(time_created)
        .bind(time_created)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upserts a message by id — inserts if absent, otherwise overwrites its
    /// `data` blob and bumps `time_updated`. Used by the session processor to
    /// mutate an assistant message in place (opencode `session.updateMessage`).
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn update_message(&self, info: &Info) -> Result<(), StorageError> {
        let data = info.data_json()?;
        let time_created = info.time_created();
        sqlx::query(
            "INSERT INTO message (id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 data = excluded.data,
                 time_updated = excluded.time_updated",
        )
        .bind(info.id())
        .bind(&info.session_id)
        .bind(time_created)
        .bind(time_created)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetches a message by session + message id, hydrating the typed [`Info`]
    /// (`message-v2.ts:80-85,506-519`).
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn get_message(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<Info>, StorageError> {
        let row =
            sqlx::query("SELECT id, session_id, data FROM message WHERE id = ? AND session_id = ?")
                .bind(message_id)
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(info_from_row).transpose()
    }

    /// Lists a session's messages ordered by `(time_created, id)`
    /// (`sql.ts:79`), hydrating each typed [`Info`].
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn list_messages(&self, session_id: &str) -> Result<Vec<Info>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, session_id, data FROM message
             WHERE session_id = ? ORDER BY time_created, id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(info_from_row).collect()
    }

    // -- parts --------------------------------------------------------------

    /// Inserts a part, persisting `data` as `Part` minus
    /// `id`/`sessionID`/`messageID` (`sql.ts:20,82-98`).
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn insert_part(&self, part: &Part) -> Result<(), StorageError> {
        let data = part.data_json()?;
        // Parts are ordered by id, not time; derive a stable time from the id.
        let time_created = otto_id::timestamp(&part.id).map_or(0, |t| t as i64);
        sqlx::query(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&part.id)
        .bind(&part.message_id)
        .bind(&part.session_id)
        .bind(time_created)
        .bind(time_created)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upserts a part by id — inserts if absent, otherwise overwrites its
    /// `data` blob and bumps `time_updated`. Used by the session processor to
    /// transition a tool part in place (pending → running → completed/error),
    /// mirroring opencode `session.updatePart` (which is itself an upsert).
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn update_part(&self, part: &Part) -> Result<(), StorageError> {
        let data = part.data_json()?;
        // Parts are ordered by id, not time; derive a stable time from the id.
        let time_created = otto_id::timestamp(&part.id).map_or(0, |t| t as i64);
        sqlx::query(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 data = excluded.data,
                 time_updated = excluded.time_updated",
        )
        .bind(&part.id)
        .bind(&part.message_id)
        .bind(&part.session_id)
        .bind(time_created)
        .bind(time_created)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lists a message's parts ordered by id (`message-v2.ts:492-503`),
    /// hydrating each typed [`Part`].
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn list_parts(&self, message_id: &str) -> Result<Vec<Part>, StorageError> {
        let rows = sqlx::query(
            "SELECT id, session_id, message_id, data FROM part
             WHERE message_id = ? ORDER BY id",
        )
        .bind(message_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(part_from_row).collect()
    }

    /// Deletes every part belonging to `message_id`. Used to purge a
    /// partially-streamed assistant message's parts before a retry re-streams
    /// the turn, so a retried turn does not accumulate duplicate parts under
    /// the same message id.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite failure.
    pub async fn delete_parts_for_message(&self, message_id: &str) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM part WHERE message_id = ?")
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Deletes a single part by id. Used by the retry salvage path to drop a
    /// failed attempt's incomplete (pending/running) tool parts while keeping
    /// its completed tool work.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite failure.
    pub async fn delete_part(&self, part_id: &str) -> Result<(), StorageError> {
        sqlx::query("DELETE FROM part WHERE id = ?")
            .bind(part_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Returns a session's messages, each with its ordered parts — the Rust
    /// analog of `hydrate()` (`message-v2.ts:98-123`).
    ///
    /// Two queries total (messages + all session parts, grouped in memory)
    /// rather than 1 + one-per-message: the run loop re-reads the full history
    /// every step, so the N+1 fan-out grows with session length.
    ///
    /// # Errors
    /// Returns a [`StorageError`] on SQLite or JSON failure.
    pub async fn messages_with_parts(
        &self,
        session_id: &str,
    ) -> Result<Vec<WithParts>, StorageError> {
        let messages = self.list_messages(session_id).await?;
        let rows = sqlx::query(
            "SELECT id, session_id, message_id, data FROM part
             WHERE session_id = ? ORDER BY message_id, id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        let mut by_message: std::collections::HashMap<String, Vec<Part>> =
            std::collections::HashMap::new();
        for row in rows {
            let part = part_from_row(row)?;
            by_message
                .entry(part.message_id.clone())
                .or_default()
                .push(part);
        }
        Ok(messages
            .into_iter()
            .map(|info| {
                let parts = by_message.remove(info.id()).unwrap_or_default();
                WithParts { info, parts }
            })
            .collect())
    }
}

/// Hydrates an [`Info`] from a `message` row (`id`, `session_id`, `data`).
fn info_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Info, StorageError> {
    let id: String = row.try_get("id")?;
    let session_id: String = row.try_get("session_id")?;
    let data: String = row.try_get("data")?;
    Ok(Info::from_row(id, session_id, &data)?)
}

/// Hydrates a [`Part`] from a `part` row (`id`, `session_id`, `message_id`,
/// `data`).
fn part_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Part, StorageError> {
    let id: String = row.try_get("id")?;
    let session_id: String = row.try_get("session_id")?;
    let message_id: String = row.try_get("message_id")?;
    let data: String = row.try_get("data")?;
    Ok(Part::from_row(id, session_id, message_id, &data)?)
}

/// Hydrates a [`Session`] from a `session` row.
fn session_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Session, StorageError> {
    let metadata: Option<String> = row.try_get("metadata")?;
    let metadata = metadata.map(|m| serde_json::from_str(&m)).transpose()?;
    Ok(Session {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        parent_id: row.try_get("parent_id")?,
        directory: row.try_get("directory")?,
        title: row.try_get("title")?,
        version: row.try_get("version")?,
        cost: row.try_get("cost")?,
        tokens: SessionTokens {
            input: row.try_get("tokens_input")?,
            output: row.try_get("tokens_output")?,
            reasoning: row.try_get("tokens_reasoning")?,
            cache: SessionCacheTokens {
                read: row.try_get("tokens_cache_read")?,
                write: row.try_get("tokens_cache_write")?,
            },
        },
        metadata,
        time_created: row.try_get("time_created")?,
        time_updated: row.try_get("time_updated")?,
    })
}

/// Hydrates a [`WorkflowTaskRow`] from a `workflow_task` row.
fn workflow_task_from_row(row: sqlx::sqlite::SqliteRow) -> Result<WorkflowTaskRow, StorageError> {
    Ok(WorkflowTaskRow {
        id: row.try_get("id")?,
        session_id: row.try_get("session_id")?,
        workflow_kind: row.try_get("workflow_kind")?,
        task_index: row.try_get("task_index")?,
        status: row.try_get("status")?,
        notes: row.try_get("notes")?,
        updated_at: row.try_get("updated_at")?,
    })
}
