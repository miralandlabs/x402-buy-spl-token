//! Retry helpers for transient Solana RPC failures.
//!
//! # Why this exists
//!
//! This service settles the buyer's payment **before** it reads on-chain balance
//! (a deliberate choice — see README and `docs/RESPONSE-FORMATS.md`: on Solana,
//! blockhash expiry means verify → serve → settle is not safe). Every RPC
//! failure after that settlement is revenue kept without a service rendered.
//! We do not refund automatically, so the best we can do is push the per-read
//! success rate as high as possible by retrying transient failures, and when
//! that still fails, return a clearly-labelled 503 that includes the
//! settlement signature so the buyer can reconcile.
//!
//! # What counts as transient
//!
//! The heuristic is conservative — we prefer "retry a few real outages" over
//! "silently replay a permanent failure":
//!
//! | `ClientErrorKind` variant   | Retryable? | Notes |
//! |-----------------------------|------------|-------|
//! | `Io`                        | yes        | TCP resets, connection closed |
//! | `Reqwest`                   | conditional| only for timeout / connect / body / decode errors, not for 4xx bodies |
//! | `Middleware`                | yes        | usually a wrapped transport failure |
//! | `RpcError::RpcResponseError`| conditional| 429 / 5xx-mapped errors retry; -32602 "invalid params" does not |
//! | `SerdeJson`                 | yes once   | response got truncated mid-stream; one retry catches flaps |
//! | `SigningError`              | no         | not applicable to read-only flows anyway |
//! | `TransactionError`          | no         | on-chain logic failure (not our read path) |
//! | `Custom`                    | no         | opaque string; safer not to retry |
//!
//! # Budget
//!
//! The retry loop is bounded by *both* a max attempt count and a total wall-clock
//! budget. Either limit short-circuits. Vercel serverless functions run under a
//! ~10 s wall limit on the Free / Pro plans; defaults (3 attempts × ≤6 s total)
//! leave headroom for the rest of the request.

use {
    rand::{rng, RngExt},
    solana_rpc_client_api::{
        client_error::{Error as ClientError, ErrorKind as ClientErrorKind},
        request::{RpcError, RpcResponseErrorData},
    },
    std::{future::Future, time::Duration},
    tokio::time::Instant,
    tracing::{debug, warn},
};

/// Tunable retry parameters — all come from env (with sensible defaults).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub total_budget: Duration,
}

impl RetryPolicy {
    /// Read policy from env with defaults safe for a ~10 s serverless budget.
    ///
    /// Env keys (all optional):
    /// - `X402_RPC_MAX_ATTEMPTS` — default **3**
    /// - `X402_RPC_INITIAL_BACKOFF_MS` — default **120**
    /// - `X402_RPC_TOTAL_BUDGET_MS` — default **6000**
    pub fn from_env() -> Self {
        fn env_u64(key: &str, default: u64) -> u64 {
            std::env::var(key)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        }
        let max_attempts = env_u64("X402_RPC_MAX_ATTEMPTS", 3).clamp(1, 10) as u32;
        let initial_ms = env_u64("X402_RPC_INITIAL_BACKOFF_MS", 120).clamp(10, 5_000);
        let total_ms = env_u64("X402_RPC_TOTAL_BUDGET_MS", 6_000).clamp(100, 30_000);
        Self {
            max_attempts,
            initial_backoff: Duration::from_millis(initial_ms),
            total_budget: Duration::from_millis(total_ms),
        }
    }
}

/// Classify a Solana RPC error as retryable (transient) or terminal.
///
/// `None` → treat as terminal (do not retry).
/// `Some(reason_str)` → retry, reason included in structured logs.
pub fn retryable_reason(err: &ClientError) -> Option<&'static str> {
    match err.kind() {
        ClientErrorKind::Io(_) => Some("io"),
        ClientErrorKind::Reqwest(e) => {
            // Only retry network-layer failures. A 4xx body arrives as
            // `RpcError` below; do not re-classify those as retryable here.
            if e.is_timeout() || e.is_connect() || e.is_body() || e.is_decode() {
                Some("reqwest-transport")
            } else {
                None
            }
        }
        ClientErrorKind::Middleware(_) => Some("middleware"),
        ClientErrorKind::RpcError(rpc) => classify_rpc_error(rpc),
        ClientErrorKind::SerdeJson(_) => Some("json-decode"),
        ClientErrorKind::SigningError(_) => None,
        ClientErrorKind::TransactionError(_) => None,
        ClientErrorKind::Custom(_) => None,
    }
}

fn classify_rpc_error(rpc: &RpcError) -> Option<&'static str> {
    match rpc {
        // Pure transport at the RPC layer (the server never answered with JSON).
        RpcError::RpcRequestError(_) | RpcError::ForUser(_) => Some("rpc-transport"),

        // The server answered with a JSON error. Retry only well-known
        // transient codes; everything else is almost certainly permanent.
        RpcError::RpcResponseError {
            code,
            message,
            data,
        } => {
            // Standard JSON-RPC "invalid params" = malformed request. Never retry.
            if *code == -32602 {
                return None;
            }
            // Preflight simulation failures are deterministic. Don't retry.
            if matches!(
                data,
                RpcResponseErrorData::SendTransactionPreflightFailure(_)
            ) {
                return None;
            }
            // Heuristics below are safe because we've already excluded the two
            // most common deterministic cases above.
            let msg_lower = message.to_lowercase();
            if msg_lower.contains("rate limit")
                || msg_lower.contains("too many requests")
                || msg_lower.contains("server error")
                || msg_lower.contains("temporarily unavailable")
                || msg_lower.contains("gateway")
                || msg_lower.contains("timeout")
            {
                Some("rpc-transient-response")
            } else {
                None
            }
        }

        // Parse-the-response failures are usually from truncated streams.
        RpcError::ParseError(_) => Some("rpc-parse"),
    }
}

/// Run `op` with exponential-backoff-with-jitter retries per `policy`.
///
/// Returns on the first `Ok` result or after exhausting both attempt count and
/// total wall budget. Terminal errors (non-retryable) short-circuit immediately.
///
/// `label` is included in structured logs so reviewers can correlate retries
/// with the call site.
pub async fn with_retry<F, Fut, T>(
    policy: RetryPolicy,
    label: &'static str,
    correlation_id: Option<&str>,
    mut op: F,
) -> Result<T, ClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ClientError>>,
{
    let started = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let op_started = Instant::now();
        match op().await {
            Ok(v) => {
                if attempt > 1 {
                    debug!(
                        target: "server_log",
                        op = label,
                        attempt,
                        attempt_ms = op_started.elapsed().as_millis() as u64,
                        total_ms = started.elapsed().as_millis() as u64,
                        correlation_id = correlation_id.unwrap_or(""),
                        "rpc retry succeeded"
                    );
                }
                return Ok(v);
            }
            Err(err) => {
                let reason = retryable_reason(&err);
                let budget_left = policy.total_budget.saturating_sub(started.elapsed());
                let attempts_left = policy.max_attempts.saturating_sub(attempt);
                let will_retry =
                    reason.is_some() && attempts_left > 0 && budget_left > Duration::from_millis(1);
                warn!(
                    target: "server_log",
                    op = label,
                    attempt,
                    attempts_left,
                    budget_left_ms = budget_left.as_millis() as u64,
                    retry_reason = reason.unwrap_or("terminal"),
                    retrying = will_retry,
                    correlation_id = correlation_id.unwrap_or(""),
                    error = %err,
                    "rpc call failed"
                );
                if !will_retry {
                    return Err(err);
                }
                // exponential backoff with full jitter: sleep in [0, base * 2^attempt-1).
                let base = policy
                    .initial_backoff
                    .saturating_mul(1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX));
                let base_ms = base.as_millis().min(budget_left.as_millis()) as u64;
                let sleep_ms = if base_ms > 0 {
                    rng().random_range(0..=base_ms)
                } else {
                    0
                };
                if Duration::from_millis(sleep_ms) >= budget_left {
                    // would overrun — give up now rather than sleep past the budget
                    return Err(err);
                }
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_rpc_client_api::request::RpcError;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn mk_io_error() -> ClientError {
        ClientError::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ))
    }

    fn mk_invalid_params_error() -> ClientError {
        ClientError::from(RpcError::RpcResponseError {
            code: -32602,
            message: "invalid params: malformed pubkey".into(),
            data: RpcResponseErrorData::Empty,
        })
    }

    fn mk_rate_limit_error() -> ClientError {
        ClientError::from(RpcError::RpcResponseError {
            code: -32000,
            message: "rate limit exceeded".into(),
            data: RpcResponseErrorData::Empty,
        })
    }

    #[test]
    fn io_errors_are_retryable() {
        assert_eq!(retryable_reason(&mk_io_error()), Some("io"));
    }

    #[test]
    fn invalid_params_are_terminal() {
        assert!(retryable_reason(&mk_invalid_params_error()).is_none());
    }

    #[test]
    fn rate_limit_responses_are_retryable() {
        assert_eq!(
            retryable_reason(&mk_rate_limit_error()),
            Some("rpc-transient-response")
        );
    }

    #[tokio::test]
    async fn success_on_first_attempt_does_not_retry() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            total_budget: Duration::from_millis(50),
        };
        let counter = AtomicU32::new(0);
        let out: Result<u32, ClientError> = with_retry(policy, "test", None, || async {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            total_budget: Duration::from_millis(200),
        };
        let counter = AtomicU32::new(0);
        let out: Result<u32, ClientError> = with_retry(policy, "test", None, || async {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 3 {
                Err(mk_io_error())
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn terminal_error_does_not_retry() {
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(1),
            total_budget: Duration::from_millis(200),
        };
        let counter = AtomicU32::new(0);
        let out: Result<u32, ClientError> = with_retry(policy, "test", None, || async {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(mk_invalid_params_error())
        })
        .await;
        assert!(out.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausts_attempts_on_persistent_transient() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            total_budget: Duration::from_millis(500),
        };
        let counter = AtomicU32::new(0);
        let out: Result<u32, ClientError> = with_retry(policy, "test", None, || async {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(mk_io_error())
        })
        .await;
        assert!(out.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }
}
