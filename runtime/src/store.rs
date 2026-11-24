//! Where chat sessions are kept: the history (rig's session store), the
//! workspace (plan and files) and a lock that lets a session run one turn at
//! a time. With `DATABASE_URL` all three live in Postgres and every replica
//! shares them, so any pod can serve any message. Without it they live in
//! this process's memory, as for `cargo run`.
//!
//! With `DATABASE_SCHEMA` as well, the tables live in that schema, which is
//! made at startup: several agents then share one database without seeing
//! each other's sessions.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use rig::memory::MemoryStore;
use rig::postgres::PostgresSessionStore;
use std::str::FromStr;

use anyhow::bail;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};

use crate::tools::Workspace;

/// The workspace table, also the list of sessions: every turn touches its
/// row first, so `updated_at` is the session's last activity and expiry
/// goes by it. Made at startup under an advisory lock, so replicas that
/// start together make it once.
const SCHEMA: &str = "
SELECT pg_advisory_xact_lock(hashtext('agent_workspaces_schema'));
CREATE TABLE IF NOT EXISTS agent_workspaces (
    session_id TEXT PRIMARY KEY,
    workspace JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- The user (token `sub`) a session belongs to; NULL without sign-in.
ALTER TABLE agent_workspaces ADD COLUMN IF NOT EXISTS owner TEXT;
";

/// The conversation store rig's agent loads before a turn and saves after it.
pub enum History {
    Postgres(PostgresSessionStore),
    Memory(MemoryStore),
}

pub struct Store {
    pub history: History,
    backend: Backend,
}

enum Backend {
    Postgres {
        /// With `search_path` set to the schema, for the pool and the turn locks.
        options: Box<PgConnectOptions>,
        pool: PgPool,
        /// Part of every advisory lock key, so agents sharing a database do not block each other.
        scope: String,
    },
    Memory {
        workspaces: Mutex<HashMap<String, Workspace>>,
        owners: Mutex<HashMap<String, Option<String>>>,
        busy: Arc<Mutex<HashSet<String>>>,
    },
}

/// Whether a user may use a session that has this owner. A session without
/// an owner (made without sign-in) becomes the first signed-in user's.
fn may_use(owner: Option<&str>, user: Option<&str>) -> bool {
    owner.is_none() || owner == user
}

/// Held for the length of one turn; dropping it lets the session's next turn start.
pub enum TurnLock {
    /// A session-level advisory lock, released when this connection closes,
    /// so also when the stream is dropped or the pod dies.
    Postgres(#[allow(dead_code)] sqlx::PgConnection),
    Memory { busy: Arc<Mutex<HashSet<String>>>, session: String },
}

impl Drop for TurnLock {
    fn drop(&mut self) {
        if let TurnLock::Memory { busy, session } = self {
            busy.lock().unwrap_or_else(PoisonError::into_inner).remove(session);
        }
    }
}

impl Store {
    pub async fn open(database_url: Option<String>, schema: Option<String>) -> anyhow::Result<Self> {
        let Some(url) = database_url else {
            return Ok(Self {
                history: History::Memory(MemoryStore::default()),
                backend: Backend::Memory { workspaces: Mutex::default(), owners: Mutex::default(), busy: Arc::default() },
            });
        };
        let mut options = PgConnectOptions::from_str(&url)?;
        let scope = match &schema {
            Some(schema) => {
                if schema.is_empty() || !schema.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    bail!("DATABASE_SCHEMA `{schema}` may only have lowercase letters, digits and _");
                }
                let mut connection = options.connect().await?;
                // Safe: the name was checked above to be [a-z0-9_]+.
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\"")))
                    .execute(&mut connection)
                    .await?;
                options = options.options([("search_path", schema.as_str())]);
                schema.clone()
            }
            None => "public".to_owned(),
        };
        let pool = PgPoolOptions::new().connect_with(options.clone()).await?;
        let history = History::Postgres(PostgresSessionStore::open(pool.clone()).await?);
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(SCHEMA).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Self { history, backend: Backend::Postgres { options: Box::new(options), pool, scope } })
    }

    pub fn kind(&self) -> &'static str {
        match self.backend {
            Backend::Postgres { .. } => "postgres",
            Backend::Memory { .. } => "memory",
        }
    }

    /// The session's turn lock, or `None` while another turn of it runs, on any replica.
    pub async fn lock(&self, session: &str) -> anyhow::Result<Option<TurnLock>> {
        match &self.backend {
            Backend::Postgres { options, scope, .. } => {
                // Its own connection, not a pooled one: a pooled connection
                // would go back to the pool still holding the lock.
                let mut connection = options.connect().await?;
                let (locked,): (bool,) =
                    sqlx::query_as("SELECT pg_try_advisory_lock(hashtext($1), hashtext($2))")
                        .bind(format!("agent_turn/{scope}"))
                        .bind(session)
                        .fetch_one(&mut connection)
                        .await?;
                Ok(locked.then_some(TurnLock::Postgres(connection)))
            }
            Backend::Memory { busy, .. } => {
                let fresh = busy.lock().unwrap_or_else(PoisonError::into_inner).insert(session.to_owned());
                Ok(fresh.then(|| TurnLock::Memory { busy: busy.clone(), session: session.to_owned() }))
            }
        }
    }

    /// Marks the session as used now by `user`, so it expires counting from
    /// its last turn. A new session becomes the user's; `false` when it is
    /// someone else's (then nothing changes).
    pub async fn touch(&self, session: &str, user: Option<&str>) -> anyhow::Result<bool> {
        match &self.backend {
            Backend::Postgres { pool, .. } => {
                let done = sqlx::query(
                    "INSERT INTO agent_workspaces (session_id, workspace, owner) VALUES ($1, $2, $3)
                     ON CONFLICT (session_id) DO UPDATE
                       SET updated_at = now(), owner = COALESCE(agent_workspaces.owner, EXCLUDED.owner)
                       WHERE agent_workspaces.owner IS NULL OR agent_workspaces.owner = EXCLUDED.owner",
                )
                .bind(session)
                .bind(sqlx::types::Json(Workspace::default()))
                .bind(user)
                .execute(pool)
                .await?;
                Ok(done.rows_affected() == 1)
            }
            Backend::Memory { owners, .. } => {
                let mut owners = owners.lock().unwrap_or_else(PoisonError::into_inner);
                let owner = owners.entry(session.to_owned()).or_insert_with(|| user.map(str::to_owned));
                if !may_use(owner.as_deref(), user) {
                    return Ok(false);
                }
                if owner.is_none() {
                    *owner = user.map(str::to_owned);
                }
                Ok(true)
            }
        }
    }

    /// Deletes the sessions idle for longer than `idle`, with their history,
    /// and returns how many went. One replica at a time does it; the others
    /// return 0. In memory nothing expires: the process does not live that long.
    pub async fn expire(&self, idle: std::time::Duration) -> anyhow::Result<u64> {
        let Backend::Postgres { pool, scope, .. } = &self.backend else { return Ok(0) };
        let mut tx = pool.begin().await?;
        let (ours,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_xact_lock(hashtext($1))")
            .bind(format!("agent_expiry/{scope}"))
            .fetch_one(&mut *tx)
            .await?;
        if !ours {
            return Ok(0);
        }
        sqlx::query("DELETE FROM agent_workspaces WHERE updated_at < now() - make_interval(secs => $1)")
            .bind(idle.as_secs_f64())
            .execute(&mut *tx)
            .await?;
        // A conversation whose session row is gone: just expired, or from
        // before sessions were touched. A running turn touched its row
        // before rig saves its log, so it is never caught here.
        sqlx::query("DELETE FROM rig_memory_log_chunks WHERE name NOT IN (SELECT session_id FROM agent_workspaces)")
            .execute(&mut *tx)
            .await?;
        let gone = sqlx::query("DELETE FROM rig_memory_logs WHERE name NOT IN (SELECT session_id FROM agent_workspaces)")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(gone)
    }

    pub async fn load_workspace(&self, session: &str) -> anyhow::Result<Workspace> {
        match &self.backend {
            Backend::Postgres { pool, .. } => {
                let row: Option<(sqlx::types::Json<Workspace>,)> =
                    sqlx::query_as("SELECT workspace FROM agent_workspaces WHERE session_id = $1")
                        .bind(session)
                        .fetch_optional(pool)
                        .await?;
                Ok(row.map(|(workspace,)| workspace.0).unwrap_or_default())
            }
            Backend::Memory { workspaces, .. } => Ok(workspaces
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(session)
                .cloned()
                .unwrap_or_default()),
        }
    }

    pub async fn save_workspace(&self, session: &str, workspace: &Workspace) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Postgres { pool, .. } => {
                sqlx::query(
                    "INSERT INTO agent_workspaces (session_id, workspace) VALUES ($1, $2)
                     ON CONFLICT (session_id) DO UPDATE SET workspace = EXCLUDED.workspace, updated_at = now()",
                )
                .bind(session)
                .bind(sqlx::types::Json(workspace))
                .execute(pool)
                .await?;
            }
            Backend::Memory { workspaces, .. } => {
                workspaces
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(session.to_owned(), workspace.clone());
            }
        }
        Ok(())
    }

    /// Forgets the session's history and workspace; `false` when it is someone else's.
    pub async fn reset(&self, session: &str, user: Option<&str>) -> anyhow::Result<bool> {
        match &self.backend {
            Backend::Postgres { pool, .. } => {
                // rig's store has no delete, so its rows go directly: a
                // conversation is the log named after the session.
                let mut tx = pool.begin().await?;
                let owner: Option<(Option<String>,)> =
                    sqlx::query_as("SELECT owner FROM agent_workspaces WHERE session_id = $1 FOR UPDATE")
                        .bind(session)
                        .fetch_optional(&mut *tx)
                        .await?;
                if let Some((owner,)) = owner
                    && !may_use(owner.as_deref(), user)
                {
                    return Ok(false);
                }
                for statement in [
                    "DELETE FROM rig_memory_log_chunks WHERE name = $1",
                    "DELETE FROM rig_memory_logs WHERE name = $1",
                    "DELETE FROM agent_workspaces WHERE session_id = $1",
                ] {
                    sqlx::query(statement).bind(session).execute(&mut *tx).await?;
                }
                tx.commit().await?;
            }
            Backend::Memory { workspaces, owners, .. } => {
                let mut owners = owners.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(owner) = owners.get(session)
                    && !may_use(owner.as_deref(), user)
                {
                    return Ok(false);
                }
                owners.remove(session);
                // MemoryStore cannot delete one log either; the UI starts a new
                // session id after a reset, so the old history is just unused.
                workspaces.lock().unwrap_or_else(PoisonError::into_inner).remove(session);
            }
        }
        Ok(true)
    }

    /// Whether the database answers; always true in memory.
    pub async fn ready(&self) -> bool {
        match &self.backend {
            Backend::Postgres { pool, .. } => sqlx::query("SELECT 1").execute(pool).await.is_ok(),
            Backend::Memory { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_session_belongs_to_its_first_user() {
        let store = Store::open(None, None).await.unwrap();
        assert!(store.touch("s1", Some("alice")).await.unwrap());
        assert!(store.touch("s1", Some("alice")).await.unwrap());
        assert!(!store.touch("s1", Some("bob")).await.unwrap());
        assert!(!store.touch("s1", None).await.unwrap());
        assert!(!store.reset("s1", Some("bob")).await.unwrap());
        assert!(store.reset("s1", Some("alice")).await.unwrap());
        // Gone after the reset, so the id is free again.
        assert!(store.touch("s1", Some("bob")).await.unwrap());
    }

    #[tokio::test]
    async fn sessions_without_sign_in_stay_open_until_claimed() {
        let store = Store::open(None, None).await.unwrap();
        assert!(store.touch("s2", None).await.unwrap());
        assert!(store.touch("s2", None).await.unwrap());
        assert!(store.touch("s2", Some("alice")).await.unwrap());
        assert!(!store.touch("s2", None).await.unwrap());
    }
}
