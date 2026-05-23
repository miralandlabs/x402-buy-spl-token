//! In-process purchase-order ledger for deployments without Postgres.
//!
//! Mirrors the state machine in [`crate::orders`] so the paid path behaves
//! identically whether persistence is backed by Postgres or memory.

use {
    crate::orders::{LedgerError, OrderRecord, OrderState, TransitionFields},
    dashmap::DashMap,
    std::{
        future::Future,
        sync::Arc,
        time::{Duration, Instant},
    },
    tokio::sync::Mutex,
    tracing::warn,
};

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Default)]
pub struct MemoryLedger {
    orders: DashMap<String, OrderRecord>,
    locks: DashMap<String, Arc<Mutex<()>>>,
}

impl MemoryLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock_for(&self, payment_uid: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(payment_uid.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn insert_pending(&self, payment_uid: &str) -> Result<(), LedgerError> {
        self.orders
            .entry(payment_uid.to_string())
            .or_insert_with(|| OrderRecord {
                payment_uid: payment_uid.to_string(),
                state: OrderState::PendingTransfer,
                transfer_signature: None,
                evidence_url: None,
                delivery_signature: None,
            });
        Ok(())
    }

    pub async fn load(&self, payment_uid: &str) -> Result<Option<OrderRecord>, LedgerError> {
        Ok(self.orders.get(payment_uid).map(|r| r.clone()))
    }

    pub async fn transition(
        &self,
        payment_uid: &str,
        from: OrderState,
        to: OrderState,
        fields: &TransitionFields,
    ) -> Result<(), LedgerError> {
        let Some(mut record) = self.orders.get_mut(payment_uid) else {
            return Err(LedgerError::NotFound {
                payment_uid: payment_uid.to_string(),
            });
        };

        if record.state != from {
            return Err(LedgerError::ZeroRowTransition {
                current: record.state,
            });
        }

        record.state = to;
        if let Some(sig) = &fields.transfer_signature {
            record.transfer_signature = Some(sig.clone());
        }
        if let Some(url) = &fields.evidence_url {
            record.evidence_url = Some(url.clone());
        }
        if let Some(sig) = &fields.delivery_signature {
            record.delivery_signature = Some(sig.clone());
        }
        Ok(())
    }

    pub async fn mark_failed(&self, payment_uid: &str, step: &str) -> Result<(), LedgerError> {
        if let Some(mut record) = self.orders.get_mut(payment_uid) {
            if !record.state.is_terminal() {
                record.state = OrderState::Failed;
            }
        }
        warn!(
            target: "server_log",
            payment_uid,
            step,
            "memory purchase_orders mark_failed"
        );
        Ok(())
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
        let lock = self.lock_for(payment_uid);
        let deadline = Instant::now() + timeout_budget;
        loop {
            match lock.try_lock() {
                Ok(guard) => {
                    let out = f().await;
                    drop(guard);
                    return out;
                }
                Err(_) if Instant::now() >= deadline => {
                    return Err(LedgerError::LockBusy {
                        payment_uid: payment_uid.to_string(),
                    });
                }
                Err(_) => {
                    tokio::time::sleep(LOCK_POLL_INTERVAL).await;
                }
            }
        }
    }
}
