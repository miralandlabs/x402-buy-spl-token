//! Purchase-order ledger for the buy-spl-token endpoint.
//!
//! The ledger is a Postgres-backed state machine keyed on `payment_uid`. It
//! enforces idempotency across retries — a request that lands the SPL
//! transfer once must never land it twice — and surfaces a strict resume
//! policy when a request is replayed mid-flight:
//!
//! ```text
//!     pending_transfer ──► transfer_landed ──► delivery_submitted ──► completed
//!            │                    │                     │
//!            └────────────────────┴─────────────────────┴──────► failed
//! ```
//!
//! Every forward transition is performed as `UPDATE ... WHERE state = $from`.
//! When the update affects zero rows, the caller's `from` no longer matches
//! the persisted state — either because the request lost a race to another
//! retry, or because it is replaying a step that already advanced. The
//! ledger surfaces this as [`LedgerError::ZeroRowTransition`] carrying the
//! current persisted state, and callers short-circuit the request based on
//! that state (resuming from the next pending step, returning the stored
//! signatures verbatim, or rejecting on `failed`).
//!
//! # Concurrency
//!
//! [`with_advisory_lock`] takes a Postgres advisory transaction lock keyed
//! on a deterministic 64-bit hash of the `payment_uid`. The lock is held on
//! a *dedicated* pooled connection (separate from the one the closure uses
//! to talk to the ledger), so two concurrent requests for the same
//! `payment_uid` serialize at the `pg_try_advisory_xact_lock` boundary while
//! a third request whose configured timeout would be exceeded surfaces
//! [`LedgerError::LockBusy`] without blocking indefinitely.
//!
//! # Live-DB tests
//!
//! The pure-logic tests below (state parsing, `hash64` determinism,
//! transition-fields shape) run by default. Tests that exercise actual
//! Postgres behavior — schema-level `CHECK` enforcement, zero-row updates,
//! the advisory lock — are `#[ignore]`-gated and require
//! `TEST_DATABASE_URL` to be set. Run them with:
//!
//! ```bash
//! TEST_DATABASE_URL=postgres://user:pw@localhost/spl_token_balance_test \
//!     cargo test -p spl-token-balance orders -- --ignored
//! ```

use {
    crate::db::ParametersDb,
    deadpool_postgres::{Client as PoolClient, Pool},
    serde::{Deserialize, Serialize},
    std::{fmt, future::Future, time::Duration},
    tokio::time::{sleep, timeout, Instant},
};

/// Persisted lifecycle state of a purchase order.
///
/// The string repr (see [`OrderState::as_str`]) mirrors the values listed
/// in the `purchase_orders.state` `CHECK` constraint defined by
/// `migrations/0002_purchase_orders.sql`. Adding a variant here without
/// also extending the migration's `CHECK` clause will produce a Postgres
/// error at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderState {
    PendingTransfer,
    TransferLanded,
    DeliverySubmitted,
    Completed,
    Failed,
}

impl OrderState {
    /// Wire-format string stored in `purchase_orders.state`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PendingTransfer => "pending_transfer",
            Self::TransferLanded => "transfer_landed",
            Self::DeliverySubmitted => "delivery_submitted",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// Parse the wire-format string. Returns
    /// [`LedgerError::UnknownState`] for any value not present in the
    /// migration's `CHECK` clause.
    pub fn parse(s: &str) -> Result<Self, LedgerError> {
        match s {
            "pending_transfer" => Ok(Self::PendingTransfer),
            "transfer_landed" => Ok(Self::TransferLanded),
            "delivery_submitted" => Ok(Self::DeliverySubmitted),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(LedgerError::UnknownState(other.to_string())),
        }
    }

    /// Whether this state forbids any further forward transition. A
    /// terminal state never moves; concretely, [`Self::Completed`] and
    /// [`Self::Failed`].
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

impl fmt::Display for OrderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Snapshot of one row in `purchase_orders`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRecord {
    pub payment_uid: String,
    pub state: OrderState,
    pub transfer_signature: Option<String>,
    pub evidence_url: Option<String>,
    pub delivery_signature: Option<String>,
}

/// Optional column updates that travel with a [`transition`] call.
///
/// Each `Some(_)` value overwrites the corresponding column; each `None`
/// leaves the existing column untouched (`COALESCE` semantics in the
/// generated SQL). This lets each step of the state machine persist only
/// the artifact it just produced — the SPL transfer signature in the
/// `pending_transfer → transfer_landed` step, the evidence URL in the
/// `transfer_landed → delivery_submitted` step, and the `SubmitDelivery`
/// signature in the final `delivery_submitted → completed` step — without
/// clobbering values written by earlier steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransitionFields {
    pub transfer_signature: Option<String>,
    pub evidence_url: Option<String>,
    pub delivery_signature: Option<String>,
}

impl TransitionFields {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_transfer_signature(mut self, sig: impl Into<String>) -> Self {
        self.transfer_signature = Some(sig.into());
        self
    }

    pub fn with_evidence_url(mut self, url: impl Into<String>) -> Self {
        self.evidence_url = Some(url.into());
        self
    }

    pub fn with_delivery_signature(mut self, sig: impl Into<String>) -> Self {
        self.delivery_signature = Some(sig.into());
        self
    }
}

/// Errors surfaced by the ledger layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    /// A Postgres call failed (connection, pool, or server error).
    Db(String),
    /// A row's `state` column held a value not covered by [`OrderState`].
    /// Indicates schema drift — the migration's `CHECK` clause should make
    /// this unreachable in production.
    UnknownState(String),
    /// `UPDATE ... WHERE state = $from` matched zero rows. The current
    /// persisted state is included so callers can short-circuit the
    /// request based on it.
    ZeroRowTransition { current: OrderState },
    /// The requested order row does not exist.
    NotFound { payment_uid: String },
    /// [`with_advisory_lock`] could not acquire the advisory lock within
    /// the configured timeout.
    LockBusy { payment_uid: String },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(reason) => write!(f, "ledger db error: {}", reason),
            Self::UnknownState(s) => {
                write!(f, "purchase_orders.state contained unknown value {:?}", s)
            }
            Self::ZeroRowTransition { current } => write!(
                f,
                "purchase_orders transition matched zero rows; current state is {}",
                current
            ),
            Self::NotFound { payment_uid } => {
                write!(
                    f,
                    "purchase_orders row not found for payment_uid={:?}",
                    payment_uid
                )
            }
            Self::LockBusy { payment_uid } => {
                write!(f, "advisory lock busy for payment_uid={:?}", payment_uid)
            }
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<tokio_postgres::Error> for LedgerError {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Db(e.to_string())
    }
}

/// Deterministic 64-bit hash of `payment_uid`, suitable for the BIGINT key
/// argument of [`pg_try_advisory_xact_lock`][advisory].
///
/// We use FNV-1a (64-bit). It is not cryptographically strong — and that
/// is fine: the lock is purely a serialization device for two concurrent
/// retries of the *same* `payment_uid`, not a security primitive. What we
/// require, and what FNV-1a gives us, is:
///
/// - **Determinism across processes and runs.** Unlike `std`'s
///   `DefaultHasher`, which the standard library explicitly says may
///   change between releases, FNV-1a always produces the same 64-bit
///   digest for a given byte sequence.
/// - **Zero allocations and zero dependencies.** This crate already
///   avoids the `siphasher` crate; FNV's 5-line definition keeps it that
///   way.
///
/// The output is reinterpreted as `i64` (Postgres `BIGINT`) without
/// truncation — the bit pattern is preserved end-to-end.
///
/// [advisory]: https://www.postgresql.org/docs/current/explicit-locking.html#ADVISORY-LOCKS
pub fn hash64(payment_uid: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = FNV_OFFSET;
    for b in payment_uid.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h as i64
}

// ---------------------------------------------------------------------------
// Pool-based ledger operations
// ---------------------------------------------------------------------------
//
// All wire steps below run inside an explicit transaction with the per-
// statement ceiling set via [`ParametersDb::set_statement_timeout_local`]
// and every step (`BEGIN`, the actual query, `COMMIT`) wrapped in
// `tokio::time::timeout`. The pattern mirrors `aethervane-srv/src/db/mod.rs`
// — the same Vercel + Supabase failure mode applies here (half-open pooled
// connections that hang the socket without surfacing errors).
//
// Constants come from [`ParametersDb`] so the ledger and the parameters
// store stay in lockstep.

async fn pool_client(pool: &Pool) -> Result<PoolClient, LedgerError> {
    timeout(ParametersDb::POOL_GET_TIMEOUT, pool.get())
        .await
        .map_err(|_| {
            LedgerError::Db(format!(
                "pool get timed out after {:?}",
                ParametersDb::POOL_GET_TIMEOUT
            ))
        })?
        .map_err(|e| LedgerError::Db(format!("pool get: {}", e)))
}

/// Open a `BEGIN` with a wall-clock cap. Mirrors
/// [`ParametersDb::begin_transaction`] but surfaces [`LedgerError::Db`] so
/// the ledger error type stays self-contained.
async fn begin_tx<'a>(
    client: &'a mut PoolClient,
    label: &str,
) -> Result<deadpool_postgres::Transaction<'a>, LedgerError> {
    timeout(ParametersDb::TX_BEGIN_TIMEOUT, client.transaction())
        .await
        .map_err(|_| {
            LedgerError::Db(format!(
                "{} transaction start timed out after {:?} (pool connection may be stale)",
                label,
                ParametersDb::TX_BEGIN_TIMEOUT
            ))
        })?
        .map_err(|e| LedgerError::Db(format!("{} transaction start failed: {}", label, e)))
}

/// Wrap a `tx.execute(...)` in [`ParametersDb::QUERY_TIMEOUT`].
async fn tx_execute_bounded(
    tx: &deadpool_postgres::Transaction<'_>,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    label: &str,
) -> Result<u64, LedgerError> {
    timeout(ParametersDb::QUERY_TIMEOUT, tx.execute(sql, params))
        .await
        .map_err(|_| {
            LedgerError::Db(format!(
                "{} timed out after {:?}",
                label,
                ParametersDb::QUERY_TIMEOUT
            ))
        })?
        .map_err(|e| LedgerError::Db(format!("{} query failed: {}", label, e)))
}

/// Wrap a `tx.query_opt(...)` in [`ParametersDb::QUERY_TIMEOUT`].
async fn tx_query_opt_bounded(
    tx: &deadpool_postgres::Transaction<'_>,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    label: &str,
) -> Result<Option<tokio_postgres::Row>, LedgerError> {
    timeout(ParametersDb::QUERY_TIMEOUT, tx.query_opt(sql, params))
        .await
        .map_err(|_| {
            LedgerError::Db(format!(
                "{} timed out after {:?}",
                label,
                ParametersDb::QUERY_TIMEOUT
            ))
        })?
        .map_err(|e| LedgerError::Db(format!("{} query failed: {}", label, e)))
}

/// Wrap `tx.commit()` in [`ParametersDb::QUERY_TIMEOUT`].
async fn tx_commit_bounded(
    tx: deadpool_postgres::Transaction<'_>,
    label: &str,
) -> Result<(), LedgerError> {
    timeout(ParametersDb::QUERY_TIMEOUT, tx.commit())
        .await
        .map_err(|_| {
            LedgerError::Db(format!(
                "{} commit timed out after {:?}",
                label,
                ParametersDb::QUERY_TIMEOUT
            ))
        })?
        .map_err(|e| LedgerError::Db(format!("{} commit failed: {}", label, e)))
}

/// Insert a new order row keyed on `payment_uid` in state
/// `pending_transfer`. Idempotent — a re-insert for the same `payment_uid`
/// is a no-op (relies on the primary-key conflict path).
pub async fn insert_pending(pool: &Pool, payment_uid: &str) -> Result<(), LedgerError> {
    let mut client = pool_client(pool).await?;
    let label = "insert_pending";

    let tx = begin_tx(&mut client, label).await?;
    ParametersDb::set_statement_timeout_local(&tx).await;

    tx_execute_bounded(
        &tx,
        "INSERT INTO purchase_orders (payment_uid, state) \
         VALUES ($1, $2) \
         ON CONFLICT (payment_uid) DO NOTHING",
        &[&payment_uid, &OrderState::PendingTransfer.as_str()],
        label,
    )
    .await?;

    tx_commit_bounded(tx, label).await
}

/// Advance the row keyed on `payment_uid` from `from` to `to`, optionally
/// persisting the columns named in `fields`.
///
/// On a zero-row update — the typical signal that another retry already
/// advanced the row, or that the row is in a state from which `from` is
/// no longer reachable — returns
/// [`LedgerError::ZeroRowTransition`] carrying the current persisted
/// state so the caller can short-circuit the request.
///
/// Both the UPDATE and the (resume-state) re-load run in the **same**
/// transaction so a concurrent writer cannot slip a different state in
/// between the two reads. Each step is timeout-bounded individually and
/// the commit also has its own timeout.
pub async fn transition(
    pool: &Pool,
    payment_uid: &str,
    from: OrderState,
    to: OrderState,
    fields: &TransitionFields,
) -> Result<(), LedgerError> {
    let mut client = pool_client(pool).await?;
    let label = "transition";

    let tx = begin_tx(&mut client, label).await?;
    ParametersDb::set_statement_timeout_local(&tx).await;

    let updated = tx_execute_bounded(
        &tx,
        "UPDATE purchase_orders \
         SET state = $1, \
             transfer_signature = COALESCE($2, transfer_signature), \
             evidence_url = COALESCE($3, evidence_url), \
             delivery_signature = COALESCE($4, delivery_signature), \
             updated_at = NOW() \
         WHERE payment_uid = $5 AND state = $6",
        &[
            &to.as_str(),
            &fields.transfer_signature,
            &fields.evidence_url,
            &fields.delivery_signature,
            &payment_uid,
            &from.as_str(),
        ],
        label,
    )
    .await?;

    if updated == 0 {
        // Read the current state inside the same transaction so the
        // caller's resume decision sees a consistent snapshot.
        let row = tx_query_opt_bounded(
            &tx,
            "SELECT payment_uid, state, transfer_signature, evidence_url, delivery_signature \
             FROM purchase_orders WHERE payment_uid = $1",
            &[&payment_uid],
            "transition resume-load",
        )
        .await?;

        let result = match row {
            Some(row) => {
                let state_str: &str = row.get("state");
                match OrderState::parse(state_str) {
                    Ok(state) => Err(LedgerError::ZeroRowTransition { current: state }),
                    Err(e) => Err(e),
                }
            }
            None => Err(LedgerError::NotFound {
                payment_uid: payment_uid.to_string(),
            }),
        };

        // Commit the (no-op) transaction so we release the connection
        // promptly. We deliberately ignore the commit error here — the
        // resume-state read already gave us the answer the caller needs.
        let _ = tx_commit_bounded(tx, label).await;
        return result;
    }

    tx_commit_bounded(tx, label).await
}

/// Force the row for `payment_uid` into state `failed`. Idempotent: a row
/// already in `failed` (or `completed`, which is also terminal) is left
/// untouched.
///
/// `step` is the name of the pipeline step that aborted (e.g.
/// `"transfer"`, `"submit_delivery"`); it is not persisted because the
/// migration intentionally keeps the schema thin, but it is logged at
/// `WARN` so operators can correlate the failure with downstream
/// telemetry.
pub async fn mark_failed(pool: &Pool, payment_uid: &str, step: &str) -> Result<(), LedgerError> {
    let mut client = pool_client(pool).await?;
    let label = "mark_failed";

    let tx = begin_tx(&mut client, label).await?;
    ParametersDb::set_statement_timeout_local(&tx).await;

    let updated = tx_execute_bounded(
        &tx,
        "UPDATE purchase_orders \
         SET state = $1, updated_at = NOW() \
         WHERE payment_uid = $2 AND state NOT IN ($3, $1)",
        &[
            &OrderState::Failed.as_str(),
            &payment_uid,
            &OrderState::Completed.as_str(),
        ],
        label,
    )
    .await?;

    tx_commit_bounded(tx, label).await?;

    tracing::warn!(
        target: "server_log",
        payment_uid,
        step,
        rows_marked_failed = updated,
        "purchase_orders mark_failed"
    );
    Ok(())
}

/// Load the row for `payment_uid`. Returns `None` when no row exists.
pub async fn load(pool: &Pool, payment_uid: &str) -> Result<Option<OrderRecord>, LedgerError> {
    let mut client = pool_client(pool).await?;
    load_with_client(&mut client, payment_uid).await
}

/// Same as [`load`] but reuses an already-acquired pooled client. Callers
/// that already hold a client (the rare case in this crate) avoid a
/// second pool round-trip.
async fn load_with_client(
    client: &mut PoolClient,
    payment_uid: &str,
) -> Result<Option<OrderRecord>, LedgerError> {
    let label = "load";

    let tx = begin_tx(client, label).await?;
    ParametersDb::set_statement_timeout_local(&tx).await;

    let row = tx_query_opt_bounded(
        &tx,
        "SELECT payment_uid, state, transfer_signature, evidence_url, delivery_signature \
         FROM purchase_orders WHERE payment_uid = $1",
        &[&payment_uid],
        label,
    )
    .await?;

    tx_commit_bounded(tx, label).await?;

    let Some(row) = row else { return Ok(None) };
    let state_str: &str = row.get("state");
    let state = OrderState::parse(state_str)?;
    Ok(Some(OrderRecord {
        payment_uid: row.get("payment_uid"),
        state,
        transfer_signature: row.get("transfer_signature"),
        evidence_url: row.get("evidence_url"),
        delivery_signature: row.get("delivery_signature"),
    }))
}

// ---------------------------------------------------------------------------
// Advisory lock
// ---------------------------------------------------------------------------

/// Polling interval used while waiting for `pg_try_advisory_xact_lock` to
/// succeed. Short enough that the average wait is dominated by Postgres
/// rather than our sleep, but long enough that a busy lock does not pin
/// a CPU.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Run `f` while holding a Postgres advisory transaction lock keyed on
/// `hash64(payment_uid)`.
///
/// The lock is acquired on a dedicated pooled connection that runs an
/// empty `BEGIN ... ROLLBACK` block solely to scope the
/// `pg_try_advisory_xact_lock` call. The closure `f` is free to use any
/// other connection from the pool — its work is *not* in the lock
/// holder's transaction. This is fine and intentional: the lock is a
/// serialization primitive across requests, not a database-isolation
/// primitive across statements.
///
/// If the lock cannot be acquired within `timeout` (a budget set by the
/// caller, typically tied to the inbound HTTP request timeout), the
/// dedicated connection's transaction is rolled back and the call
/// returns [`LedgerError::LockBusy`].
pub async fn with_advisory_lock<T, F, Fut>(
    pool: &Pool,
    payment_uid: &str,
    timeout_budget: Duration,
    f: F,
) -> Result<T, LedgerError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, LedgerError>>,
{
    let lock_key = hash64(payment_uid);
    let mut holder = pool_client(pool).await?;

    // Open a transaction explicitly so the advisory lock is xact-scoped.
    // Using raw SQL (vs. `Client::transaction()`) avoids the
    // `Transaction<'a>` lifetime tying the holder to the closure's
    // future, which would force a much more awkward closure signature.
    //
    // Both the `BEGIN` and the eventual `ROLLBACK` are wrapped in their
    // own tokio timeouts so a half-open pooled connection cannot wedge
    // the holder indefinitely (Vercel + Supabase failure mode — same
    // reason the rest of this module wraps every wire step).
    let begin_label = "advisory_lock BEGIN";
    timeout(
        ParametersDb::TX_BEGIN_TIMEOUT,
        holder.batch_execute("BEGIN"),
    )
    .await
    .map_err(|_| {
        LedgerError::Db(format!(
            "{} timed out after {:?} (pool connection may be stale)",
            begin_label,
            ParametersDb::TX_BEGIN_TIMEOUT
        ))
    })?
    .map_err(|e| LedgerError::Db(format!("{} failed: {}", begin_label, e)))?;

    let acquired = match try_acquire_lock(&mut holder, lock_key, timeout_budget).await {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            // Best-effort rollback on infrastructure error. Bounded so a
            // wedged connection cannot stall the request further.
            let _ = timeout(
                ParametersDb::QUERY_TIMEOUT,
                holder.batch_execute("ROLLBACK"),
            )
            .await;
            return Err(e);
        }
    };

    if !acquired {
        let _ = timeout(
            ParametersDb::QUERY_TIMEOUT,
            holder.batch_execute("ROLLBACK"),
        )
        .await;
        return Err(LedgerError::LockBusy {
            payment_uid: payment_uid.to_string(),
        });
    }

    // Lock is held — run the closure. Whatever it returns, we then
    // ROLLBACK the empty transaction to release the lock; the closure's
    // work is committed (or not) on its own connection.
    let result = f().await;
    let _ = timeout(
        ParametersDb::QUERY_TIMEOUT,
        holder.batch_execute("ROLLBACK"),
    )
    .await;
    result
}

/// Try to acquire the advisory lock, polling until `timeout_budget`
/// elapses. Returns `Ok(true)` on success, `Ok(false)` on budget
/// exhaustion, and `Err(_)` only on actual Postgres failures.
async fn try_acquire_lock(
    holder: &mut PoolClient,
    lock_key: i64,
    timeout_budget: Duration,
) -> Result<bool, LedgerError> {
    let started = Instant::now();
    loop {
        let row = timeout(
            ParametersDb::QUERY_TIMEOUT,
            holder.query_one("SELECT pg_try_advisory_xact_lock($1)", &[&lock_key]),
        )
        .await
        .map_err(|_| {
            LedgerError::Db(format!(
                "advisory lock probe timed out after {:?}",
                ParametersDb::QUERY_TIMEOUT
            ))
        })?
        .map_err(|e| LedgerError::Db(format!("advisory lock probe failed: {}", e)))?;
        let acquired: bool = row.get(0);
        if acquired {
            return Ok(true);
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout_budget {
            return Ok(false);
        }
        // Don't oversleep past the budget.
        let remaining = timeout_budget - elapsed;
        sleep(LOCK_POLL_INTERVAL.min(remaining)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- pure logic (no DB) ---------------------------------------------

    #[test]
    fn order_state_string_round_trip() {
        for s in [
            OrderState::PendingTransfer,
            OrderState::TransferLanded,
            OrderState::DeliverySubmitted,
            OrderState::Completed,
            OrderState::Failed,
        ] {
            assert_eq!(OrderState::parse(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn order_state_parse_rejects_unknown() {
        let err = OrderState::parse("bogus").unwrap_err();
        match err {
            LedgerError::UnknownState(s) => assert_eq!(s, "bogus"),
            other => panic!("expected UnknownState, got {:?}", other),
        }
    }

    #[test]
    fn order_state_terminality() {
        assert!(OrderState::Completed.is_terminal());
        assert!(OrderState::Failed.is_terminal());
        assert!(!OrderState::PendingTransfer.is_terminal());
        assert!(!OrderState::TransferLanded.is_terminal());
        assert!(!OrderState::DeliverySubmitted.is_terminal());
    }

    #[test]
    fn order_state_display_matches_wire_format() {
        assert_eq!(OrderState::PendingTransfer.to_string(), "pending_transfer");
        assert_eq!(OrderState::TransferLanded.to_string(), "transfer_landed");
        assert_eq!(
            OrderState::DeliverySubmitted.to_string(),
            "delivery_submitted"
        );
        assert_eq!(OrderState::Completed.to_string(), "completed");
        assert_eq!(OrderState::Failed.to_string(), "failed");
    }

    #[test]
    fn hash64_is_deterministic() {
        let a = hash64("payment-uid-abc-123");
        let b = hash64("payment-uid-abc-123");
        assert_eq!(a, b);
    }

    #[test]
    fn hash64_distinguishes_inputs() {
        let a = hash64("payment-uid-abc-123");
        let b = hash64("payment-uid-abc-124");
        assert_ne!(a, b);
    }

    #[test]
    fn hash64_known_value_for_empty_input() {
        // FNV-1a 64 of the empty byte sequence is the offset basis,
        // verbatim. Reinterpreted as i64, the high bit is set so it
        // shows up as a negative number — that's intentional and what
        // Postgres BIGINT receives.
        assert_eq!(hash64(""), 0xcbf2_9ce4_8422_2325_u64 as i64);
    }

    #[test]
    fn transition_fields_builder_records_each_setter() {
        let fields = TransitionFields::new()
            .with_transfer_signature("sig-tx")
            .with_evidence_url("https://registry/evidence/abc")
            .with_delivery_signature("sig-deliver");
        assert_eq!(fields.transfer_signature.as_deref(), Some("sig-tx"));
        assert_eq!(
            fields.evidence_url.as_deref(),
            Some("https://registry/evidence/abc")
        );
        assert_eq!(fields.delivery_signature.as_deref(), Some("sig-deliver"));
    }

    #[test]
    fn ledger_error_display_includes_context() {
        let err = LedgerError::ZeroRowTransition {
            current: OrderState::TransferLanded,
        };
        let s = err.to_string();
        assert!(s.contains("zero rows"), "{}", s);
        assert!(s.contains("transfer_landed"), "{}", s);

        let err = LedgerError::LockBusy {
            payment_uid: "uid-xyz".into(),
        };
        let s = err.to_string();
        assert!(s.contains("uid-xyz"), "{}", s);
    }

    // --- live-DB tests (gated; require TEST_DATABASE_URL) ---------------
    //
    // Run with:
    //   TEST_DATABASE_URL=postgres://user:pw@localhost/db \
    //     cargo test -p spl-token-balance orders -- --ignored
    //
    // Each test allocates a unique payment_uid so concurrent runs don't
    // collide, and cleans up its row at the end.

    use crate::db::ParametersDb;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn db_url_or_skip() -> Option<String> {
        std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
    }

    async fn fresh_pool() -> Option<Pool> {
        let url = db_url_or_skip()?;
        let db = ParametersDb::connect(url).expect("connect");
        // Apply both migrations idempotently so tests can run on a fresh DB.
        db.execute_batch(include_str!("../migrations/init.sql"))
            .await
            .expect("init.sql");
        db.execute_batch(include_str!("../migrations/0002_purchase_orders.sql"))
            .await
            .expect("0002_purchase_orders.sql");
        // Reach into the pool through a thin shim — we only need the pool
        // for the orders helpers; the parameters db has its own.
        Some(reconnect_to_pool(&db_url_or_skip().unwrap()))
    }

    fn reconnect_to_pool(url: &str) -> Pool {
        use deadpool_postgres::{Config, PoolConfig, Runtime};
        use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
        use postgres_openssl::MakeTlsConnector;

        let mut cfg = Config::new();
        cfg.url = Some(url.to_string());
        cfg.pool = Some(PoolConfig {
            max_size: 8,
            ..Default::default()
        });
        let mut builder = SslConnector::builder(SslMethod::tls()).expect("ssl");
        builder.set_verify(SslVerifyMode::NONE);
        let tls = MakeTlsConnector::new(builder.build());
        cfg.create_pool(Some(Runtime::Tokio1), tls).expect("pool")
    }

    fn uniq_uid(label: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("test-{}-{}", label, nanos)
    }

    async fn cleanup(pool: &Pool, uid: &str) {
        if let Ok(c) = pool.get().await {
            let _ = c
                .execute(
                    "DELETE FROM purchase_orders WHERE payment_uid = $1",
                    &[&uid],
                )
                .await;
        }
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing at a writable Postgres"]
    async fn ledger_happy_path_transitions_through_completion() {
        let Some(pool) = fresh_pool().await else {
            return;
        };
        let uid = uniq_uid("happy");

        insert_pending(&pool, &uid).await.expect("insert_pending");

        let rec = load(&pool, &uid).await.expect("load").expect("row exists");
        assert_eq!(rec.state, OrderState::PendingTransfer);
        assert_eq!(rec.transfer_signature, None);

        transition(
            &pool,
            &uid,
            OrderState::PendingTransfer,
            OrderState::TransferLanded,
            &TransitionFields::new().with_transfer_signature("sig-transfer"),
        )
        .await
        .expect("→transfer_landed");

        transition(
            &pool,
            &uid,
            OrderState::TransferLanded,
            OrderState::DeliverySubmitted,
            &TransitionFields::new().with_evidence_url("https://reg/e"),
        )
        .await
        .expect("→delivery_submitted");

        transition(
            &pool,
            &uid,
            OrderState::DeliverySubmitted,
            OrderState::Completed,
            &TransitionFields::new().with_delivery_signature("sig-deliver"),
        )
        .await
        .expect("→completed");

        let rec = load(&pool, &uid).await.expect("load").expect("row");
        assert_eq!(rec.state, OrderState::Completed);
        assert_eq!(rec.transfer_signature.as_deref(), Some("sig-transfer"));
        assert_eq!(rec.evidence_url.as_deref(), Some("https://reg/e"));
        assert_eq!(rec.delivery_signature.as_deref(), Some("sig-deliver"));

        cleanup(&pool, &uid).await;
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing at a writable Postgres"]
    async fn ledger_zero_row_transition_is_observable() {
        let Some(pool) = fresh_pool().await else {
            return;
        };
        let uid = uniq_uid("zerorow");

        insert_pending(&pool, &uid).await.unwrap();
        // Advance once.
        transition(
            &pool,
            &uid,
            OrderState::PendingTransfer,
            OrderState::TransferLanded,
            &TransitionFields::new().with_transfer_signature("sig-1"),
        )
        .await
        .unwrap();

        // Re-attempt the same `from=pending_transfer` — must be a
        // ZeroRowTransition pointing at the new current state.
        let err = transition(
            &pool,
            &uid,
            OrderState::PendingTransfer,
            OrderState::TransferLanded,
            &TransitionFields::new().with_transfer_signature("sig-replay"),
        )
        .await
        .unwrap_err();

        match err {
            LedgerError::ZeroRowTransition { current } => {
                assert_eq!(current, OrderState::TransferLanded);
            }
            other => panic!("expected ZeroRowTransition, got {:?}", other),
        }

        // Original signature is preserved (the replay attempt never wrote).
        let rec = load(&pool, &uid).await.unwrap().unwrap();
        assert_eq!(rec.transfer_signature.as_deref(), Some("sig-1"));

        cleanup(&pool, &uid).await;
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing at a writable Postgres"]
    async fn ledger_failed_is_terminal() {
        let Some(pool) = fresh_pool().await else {
            return;
        };
        let uid = uniq_uid("failed");

        insert_pending(&pool, &uid).await.unwrap();
        mark_failed(&pool, &uid, "transfer").await.unwrap();

        let rec = load(&pool, &uid).await.unwrap().unwrap();
        assert_eq!(rec.state, OrderState::Failed);

        // Forward transition out of `failed` must not succeed — the row's
        // state is no longer `pending_transfer`, so the UPDATE matches
        // zero rows.
        let err = transition(
            &pool,
            &uid,
            OrderState::PendingTransfer,
            OrderState::TransferLanded,
            &TransitionFields::new().with_transfer_signature("sig"),
        )
        .await
        .unwrap_err();
        match err {
            LedgerError::ZeroRowTransition { current } => {
                assert_eq!(current, OrderState::Failed);
            }
            other => panic!("expected ZeroRowTransition(Failed), got {:?}", other),
        }

        // mark_failed is idempotent.
        mark_failed(&pool, &uid, "transfer").await.unwrap();
        let rec = load(&pool, &uid).await.unwrap().unwrap();
        assert_eq!(rec.state, OrderState::Failed);

        cleanup(&pool, &uid).await;
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing at a writable Postgres"]
    async fn ledger_completed_returns_stored_signatures_unchanged() {
        let Some(pool) = fresh_pool().await else {
            return;
        };
        let uid = uniq_uid("completed");

        insert_pending(&pool, &uid).await.unwrap();
        transition(
            &pool,
            &uid,
            OrderState::PendingTransfer,
            OrderState::TransferLanded,
            &TransitionFields::new().with_transfer_signature("sig-T"),
        )
        .await
        .unwrap();
        transition(
            &pool,
            &uid,
            OrderState::TransferLanded,
            OrderState::DeliverySubmitted,
            &TransitionFields::new().with_evidence_url("https://reg/e2"),
        )
        .await
        .unwrap();
        transition(
            &pool,
            &uid,
            OrderState::DeliverySubmitted,
            OrderState::Completed,
            &TransitionFields::new().with_delivery_signature("sig-D"),
        )
        .await
        .unwrap();

        // load() must surface exactly the persisted artifacts.
        let r1 = load(&pool, &uid).await.unwrap().unwrap();
        // load() again — values must not drift.
        let r2 = load(&pool, &uid).await.unwrap().unwrap();
        assert_eq!(r1, r2);
        assert_eq!(r1.state, OrderState::Completed);
        assert_eq!(r1.transfer_signature.as_deref(), Some("sig-T"));
        assert_eq!(r1.evidence_url.as_deref(), Some("https://reg/e2"));
        assert_eq!(r1.delivery_signature.as_deref(), Some("sig-D"));

        // A re-transition attempt from Completed must zero-row out.
        let err = transition(
            &pool,
            &uid,
            OrderState::DeliverySubmitted,
            OrderState::Completed,
            &TransitionFields::new().with_delivery_signature("sig-replay"),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            LedgerError::ZeroRowTransition {
                current: OrderState::Completed
            }
        ));

        // Stored signatures are unchanged after the failed replay.
        let r3 = load(&pool, &uid).await.unwrap().unwrap();
        assert_eq!(r3.delivery_signature.as_deref(), Some("sig-D"));

        cleanup(&pool, &uid).await;
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing at a writable Postgres"]
    async fn advisory_lock_serializes_concurrent_holders_and_times_out_third() {
        let Some(pool) = fresh_pool().await else {
            return;
        };
        let uid = uniq_uid("lock");

        // Holder A grabs the lock and parks for 300 ms.
        let pool_a = pool.clone();
        let uid_a = uid.clone();
        let a = tokio::spawn(async move {
            with_advisory_lock(&pool_a, &uid_a, Duration::from_secs(2), || async {
                sleep(Duration::from_millis(300)).await;
                Ok::<_, LedgerError>(Instant::now())
            })
            .await
        });

        // Give holder A time to actually take the lock.
        sleep(Duration::from_millis(50)).await;

        // Holder B is willing to wait up to 2 s; it should serialize
        // behind A and succeed shortly after A releases.
        let pool_b = pool.clone();
        let uid_b = uid.clone();
        let b = tokio::spawn(async move {
            with_advisory_lock(&pool_b, &uid_b, Duration::from_secs(2), || async {
                Ok::<_, LedgerError>(Instant::now())
            })
            .await
        });

        // Holder C's budget is too short to wait for A — it must surface
        // LockBusy without disturbing A's hold.
        let pool_c = pool.clone();
        let uid_c = uid.clone();
        let c = tokio::spawn(async move {
            with_advisory_lock(&pool_c, &uid_c, Duration::from_millis(50), || async {
                Ok::<_, LedgerError>(())
            })
            .await
        });

        let a_finished_at = a.await.expect("a join").expect("a ok");
        let b_finished_at = b.await.expect("b join").expect("b ok");
        let c_result = c.await.expect("c join");

        // B must not have completed before A released.
        assert!(
            b_finished_at >= a_finished_at,
            "advisory_lock did not serialize: B finished at {:?} but A at {:?}",
            b_finished_at,
            a_finished_at
        );

        // C must surface LockBusy.
        match c_result {
            Err(LedgerError::LockBusy { payment_uid }) => {
                assert_eq!(payment_uid, uid);
            }
            other => panic!("expected LockBusy, got {:?}", other),
        }

        // Lock is released after all holders finish — a fresh acquire
        // succeeds immediately.
        let post = with_advisory_lock(&pool, &uid, Duration::from_millis(500), || async {
            Ok::<_, LedgerError>(())
        })
        .await;
        assert!(post.is_ok(), "post-release acquire failed: {:?}", post);

        cleanup(&pool, &uid).await;
    }
}
