//! Unified purchase-order ledger: Postgres (durable) or in-memory (lightweight).

use {
    crate::{
        db::ParametersDb,
        memory_ledger::MemoryLedger,
        orders::{self, LedgerError, OrderRecord, OrderState, TransitionFields},
    },
    deadpool_postgres::Pool,
    std::{future::Future, sync::Arc, time::Duration},
};

#[derive(Clone)]
pub enum PurchaseLedger {
    Postgres(Pool),
    Memory(Arc<MemoryLedger>),
}

impl PurchaseLedger {
    pub fn from_db(db: &ParametersDb) -> Self {
        Self::Postgres(db.pool().clone())
    }

    pub fn memory() -> Self {
        Self::Memory(MemoryLedger::new())
    }

    pub fn is_postgres(&self) -> bool {
        matches!(self, Self::Postgres(_))
    }

    pub async fn insert_pending(&self, payment_uid: &str) -> Result<(), LedgerError> {
        match self {
            Self::Postgres(pool) => orders::insert_pending(pool, payment_uid).await,
            Self::Memory(store) => store.insert_pending(payment_uid).await,
        }
    }

    pub async fn load(&self, payment_uid: &str) -> Result<Option<OrderRecord>, LedgerError> {
        match self {
            Self::Postgres(pool) => orders::load(pool, payment_uid).await,
            Self::Memory(store) => store.load(payment_uid).await,
        }
    }

    pub async fn transition(
        &self,
        payment_uid: &str,
        from: OrderState,
        to: OrderState,
        fields: &TransitionFields,
    ) -> Result<(), LedgerError> {
        match self {
            Self::Postgres(pool) => orders::transition(pool, payment_uid, from, to, fields).await,
            Self::Memory(store) => store.transition(payment_uid, from, to, fields).await,
        }
    }

    pub async fn mark_failed(&self, payment_uid: &str, step: &str) -> Result<(), LedgerError> {
        match self {
            Self::Postgres(pool) => orders::mark_failed(pool, payment_uid, step).await,
            Self::Memory(store) => store.mark_failed(payment_uid, step).await,
        }
    }

    pub async fn with_advisory_lock<T, F, Fut>(
        &self,
        payment_uid: &str,
        timeout_budget: Duration,
        f: F,
    ) -> Result<T, LedgerError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, LedgerError>>,
    {
        match self {
            Self::Postgres(pool) => {
                orders::with_advisory_lock(pool, payment_uid, timeout_budget, f).await
            }
            Self::Memory(store) => {
                store
                    .with_advisory_lock(payment_uid, timeout_budget, f)
                    .await
            }
        }
    }
}
