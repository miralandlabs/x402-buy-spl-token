//! Optional Postgres `parameters` store (pr402-compatible table shape).
//!
//! All read/write paths here run inside an explicit transaction whose
//! `statement_timeout` is bounded by [`Self::PG_STATEMENT_TIMEOUT`] and whose
//! every wire step (`BEGIN`, the actual query, `COMMIT`) is wrapped in a
//! `tokio::time::timeout`. The pattern mirrors `aethervane-srv/src/db/mod.rs`
//! and exists for the same reason: Vercel + Supabase pooled connections can
//! go half-open in ways that hang the underlying socket without ever
//! surfacing an error to the client. A wall-clock-bounded timeout at every
//! step turns a stalled connection into a quick error so the request stops
//! eating the function's budget.

use deadpool_postgres::{Client, Config, Pool, PoolConfig, Runtime};
use openssl::ssl::{SslConnector, SslMethod};
use postgres_openssl::MakeTlsConnector;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;
use tokio_postgres::types::ToSql;
use tracing::error;

use crate::error::Error;

#[derive(Clone)]
pub struct ParametersDb {
    pool: Pool,
}

impl ParametersDb {
    // --- Pool timeouts (deadpool-level) ---
    pub(crate) const WAIT: Duration = Duration::from_secs(15);
    pub(crate) const CREATE: Duration = Duration::from_secs(10);
    pub(crate) const RECYCLE: Duration = Duration::from_secs(30);

    // --- Per-call timeouts (tokio-level, wrap every wire step) ---
    /// Cap on `pool.get()` — surfaces pool exhaustion / stalled creates.
    pub(crate) const POOL_GET_TIMEOUT: Duration = Duration::from_secs(20);
    /// Cap on `client.transaction()` / `BEGIN` — protects against
    /// half-open pooled connections that never reply to `BEGIN`.
    pub(crate) const TX_BEGIN_TIMEOUT: Duration = Duration::from_secs(20);
    /// Cap on the `SET LOCAL statement_timeout = …` command itself.
    pub(crate) const SET_LOCAL_CMD_TIMEOUT: Duration = Duration::from_secs(5);
    /// Cap on each query and the `COMMIT` round-trip.
    pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
    /// Best-effort `DEALLOCATE ALL` cap (used by [`fetch_parameters_map`]).
    pub(crate) const DEALLOCATE_TIMEOUT: Duration = Duration::from_secs(5);

    /// Per-statement ceiling enforced by Postgres. Kept below
    /// [`Self::QUERY_TIMEOUT`] so tokio's `timeout()` always fires first
    /// and we surface a uniform error message.
    pub(crate) const PG_STATEMENT_TIMEOUT: &'static str = "25s";

    pub fn connect(database_url: impl Into<String>) -> Result<Self, Error> {
        let mut cfg = Config::new();
        cfg.url = Some(database_url.into());
        cfg.pool = Some(PoolConfig {
            max_size: 5,
            timeouts: deadpool_postgres::Timeouts {
                wait: Some(Self::WAIT),
                create: Some(Self::CREATE),
                recycle: Some(Self::RECYCLE),
            },
            ..Default::default()
        });

        let mut builder =
            SslConnector::builder(SslMethod::tls()).map_err(|e| Error::Internal(e.to_string()))?;
        builder.set_verify(openssl::ssl::SslVerifyMode::NONE);
        let tls = MakeTlsConnector::new(builder.build());
        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), tls)
            .map_err(|e| Error::Internal(format!("db pool: {}", e)))?;
        Ok(Self { pool })
    }

    /// `None` if unset; `Some(Err)` if URL unusable.
    pub fn from_env_var(var_name: &str) -> Option<Result<Self, Error>> {
        let Ok(url) = std::env::var(var_name) else {
            return None;
        };
        if url.is_empty() {
            return None;
        }
        Some(Self::connect(url))
    }

    async fn conn(&self) -> Result<Client, Error> {
        timeout(Self::POOL_GET_TIMEOUT, self.pool.get())
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "db pool get timed out after {:?}",
                    Self::POOL_GET_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("db pool: {}", e)))
    }

    /// Borrow the underlying pool. The buy-endpoint paid-path needs a
    /// `&Pool` to feed into `orders::with_advisory_lock` and the ledger
    /// helpers, which all take a `Pool` directly rather than a
    /// [`ParametersDb`]. The pool is shared by `parameters` and
    /// `purchase_orders` reads/writes; both happen on short-lived
    /// connections so contention is bounded by `max_size`.
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Load every active `parameters` row for `service` into a
    /// `HashMap<(endpoint, param_name), param_value>`.
    ///
    /// `service` is the compile-time per-crate constant
    /// ([`crate::parameters::SERVICE`]). Filtering at the SQL layer keeps
    /// the in-memory cache small even when a single Postgres database
    /// backs several services. Resolution semantics
    /// (endpoint-specific → wildcard `'*'` → env) live in
    /// [`crate::parameters::resolve_string`].
    pub async fn fetch_parameters_map(
        &self,
        service: &str,
    ) -> Result<HashMap<(String, String), String>, Error> {
        let mut client = self.conn().await?;
        let label = "fetch parameters";

        let tx = Self::begin_transaction(&mut client, label).await?;

        // Per-statement ceiling. If this command itself stalls we just
        // log and continue — the explicit `timeout(QUERY_TIMEOUT, …)`
        // wrappers below remain authoritative.
        Self::set_statement_timeout_local(&tx).await;

        // Best-effort: prepared statements occasionally outlive a pooled
        // connection across function invocations on Supabase, and a
        // colliding name here would surface as a confusing "prepared
        // statement already exists" error. Drop them before we run the
        // real query. Bounded by its own timeout so a hung
        // `DEALLOCATE ALL` cannot pin us.
        let _ = timeout(Self::DEALLOCATE_TIMEOUT, tx.execute("DEALLOCATE ALL", &[])).await;

        let rows = timeout(
            Self::QUERY_TIMEOUT,
            tx.query(
                r#"
                SELECT endpoint, param_name, param_value
                FROM parameters
                WHERE service = $1
                  AND inactive = false
                  AND (effective_from IS NULL OR effective_from <= NOW())
                  AND (expires_at IS NULL OR expires_at > NOW())
                ORDER BY endpoint ASC, param_name ASC
                "#,
                &[&service],
            ),
        )
        .await
        .map_err(|_| {
            Error::Internal(format!(
                "{} timed out after {:?}",
                label,
                Self::QUERY_TIMEOUT
            ))
        })?
        .map_err(|e| Error::Internal(format!("{} query failed: {}", label, e)))?;

        let map: HashMap<(String, String), String> = rows
            .iter()
            .map(|row| {
                let endpoint: String = row.get("endpoint");
                let name: String = row.get("param_name");
                let value: String = row.get("param_value");
                ((endpoint, name), value)
            })
            .collect();

        timeout(Self::QUERY_TIMEOUT, tx.commit())
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} commit timed out after {:?}",
                    label,
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} commit failed: {}", label, e)))?;
        Ok(map)
    }

    /// Execute a batch of SQL statements using the underlying tokio-postgres
    /// connection's `batch_execute` (which accepts multiple semicolon-separated
    /// statements without parameters).
    ///
    /// Used by the cold-start migration runner. We wrap the migration in an
    /// explicit `BEGIN; SET LOCAL statement_timeout = …; <sql> COMMIT;` so
    /// the whole batch runs as a single transaction with the same
    /// per-statement ceiling that the rest of this module uses, in a single
    /// network round-trip. The outer `timeout()` is the wall-clock
    /// authority. Migration files are expected to be idempotent
    /// (`CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`) so
    /// re-running them on every cold start is safe.
    pub async fn execute_batch(&self, sql: &str) -> Result<(), Error> {
        let client = self.conn().await?;
        let wrapped = format!(
            "BEGIN; SET LOCAL statement_timeout = '{}'; {} COMMIT;",
            Self::PG_STATEMENT_TIMEOUT,
            sql
        );
        timeout(Self::QUERY_TIMEOUT, client.batch_execute(&wrapped))
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "migration batch timed out after {:?}",
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("migration batch failed: {}", e)))?;
        Ok(())
    }

    // --- Private helpers (mirror aethervane-srv/src/db/mod.rs) ------------

    /// Open a `BEGIN` with a wall-clock cap. Surfaces a clear error if the
    /// pooled connection has gone half-open.
    pub(crate) async fn begin_transaction<'a>(
        client: &'a mut Client,
        label: &str,
    ) -> Result<deadpool_postgres::Transaction<'a>, Error> {
        timeout(Self::TX_BEGIN_TIMEOUT, client.transaction())
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} transaction start timed out after {:?} (pool connection may be stale)",
                    label,
                    Self::TX_BEGIN_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} transaction start failed: {}", label, e)))
    }

    /// Apply `SET LOCAL statement_timeout = '<PG_STATEMENT_TIMEOUT>'` on
    /// the open transaction. Best-effort: a failure here only means the
    /// per-statement ceiling stays at the server default; the outer
    /// `timeout()` wrappers are still authoritative. Logged so operators
    /// can spot pathological pool churn.
    pub(crate) async fn set_statement_timeout_local(tx: &deadpool_postgres::Transaction<'_>) {
        let sql = format!(
            "SET LOCAL statement_timeout = '{}'",
            Self::PG_STATEMENT_TIMEOUT
        );
        match timeout(Self::SET_LOCAL_CMD_TIMEOUT, tx.execute(sql.as_str(), &[])).await {
            Ok(Ok(_)) => (),
            Ok(Err(e)) => error!(error = %e, "SET LOCAL statement_timeout failed"),
            Err(_) => error!(
                "SET LOCAL statement_timeout timed out after {:?}",
                Self::SET_LOCAL_CMD_TIMEOUT
            ),
        }
    }

    /// Run a single `UPDATE` / `INSERT` / `DELETE` inside a fresh
    /// transaction, with the standard timeout wrapping. Returns the
    /// affected-row count.
    #[allow(dead_code)]
    pub(crate) async fn exec_in_tx(
        &self,
        mut client: Client,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        label: &str,
    ) -> Result<u64, Error> {
        let tx = Self::begin_transaction(&mut client, label).await?;

        Self::set_statement_timeout_local(&tx).await;

        let rows = timeout(Self::QUERY_TIMEOUT, tx.execute(sql, params))
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} timed out after {:?}",
                    label,
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} query failed: {}", label, e)))?;

        timeout(Self::QUERY_TIMEOUT, tx.commit())
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} commit timed out after {:?}",
                    label,
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} commit failed: {}", label, e)))?;

        Ok(rows)
    }

    /// `query_opt` inside a fresh, timeout-wrapped transaction.
    #[allow(dead_code)]
    pub(crate) async fn query_opt_in_tx(
        &self,
        mut client: Client,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        label: &str,
    ) -> Result<Option<tokio_postgres::Row>, Error> {
        let tx = Self::begin_transaction(&mut client, label).await?;

        Self::set_statement_timeout_local(&tx).await;

        let row = timeout(Self::QUERY_TIMEOUT, tx.query_opt(sql, params))
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} timed out after {:?}",
                    label,
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} query failed: {}", label, e)))?;

        timeout(Self::QUERY_TIMEOUT, tx.commit())
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} commit timed out after {:?}",
                    label,
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} commit failed: {}", label, e)))?;

        Ok(row)
    }

    /// `query` (returns Vec<Row>) inside a fresh, timeout-wrapped transaction.
    #[allow(dead_code)]
    pub(crate) async fn query_in_tx(
        &self,
        mut client: Client,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        label: &str,
    ) -> Result<Vec<tokio_postgres::Row>, Error> {
        let tx = Self::begin_transaction(&mut client, label).await?;

        Self::set_statement_timeout_local(&tx).await;

        let rows = timeout(Self::QUERY_TIMEOUT, tx.query(sql, params))
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} timed out after {:?}",
                    label,
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} query failed: {}", label, e)))?;

        timeout(Self::QUERY_TIMEOUT, tx.commit())
            .await
            .map_err(|_| {
                Error::Internal(format!(
                    "{} commit timed out after {:?}",
                    label,
                    Self::QUERY_TIMEOUT
                ))
            })?
            .map_err(|e| Error::Internal(format!("{} commit failed: {}", label, e)))?;

        Ok(rows)
    }
}
