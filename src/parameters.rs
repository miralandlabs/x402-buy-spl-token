//! DB-backed parameters with env fallback (pr402-style TTL cache).
//!
//! This crate shares a Postgres `parameters` table with sibling x402
//! services (notably `aethervane`). To keep the table key namespace clean,
//! parameter names use the same `X402_*` prefix the rest of the ecosystem
//! uses on Vercel — there is no separate `SPL_BALANCE_*` prefix for either
//! environment variables or DB rows. The DB row's `param_name` is exactly
//! the env var name (`X402_NETWORK`, `X402_PAY_TO`, …).
//!
//! Rows are scoped by the triple `(service, endpoint, param_name)`. The
//! per-crate compile-time [`SERVICE`] constant filters every read to this
//! crate's rows; `endpoint` is supplied by each handler (e.g.
//! `"check-balance"`, `"buy-spl-token"`). Resolution falls back through
//! the following sequence:
//!
//! 1. `(SERVICE, endpoint, key)` — endpoint-specific override.
//! 2. `(SERVICE, '*',      key)` — service-wide value.
//! 3. process env var `key` (when `env_fallback` is provided).
//! 4. `None` — caller decides the default.
//!
//! The [`PARAMETERS_CACHE_TTL_SEC`] env var (process-only, never read from
//! the DB) controls how often the cache reloads.

use crate::db::ParametersDb;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};
use tracing::warn;

/// Compile-time service identifier. Every DB read filters
/// `WHERE service = SERVICE`, so rows belonging to a sibling crate
/// (e.g. `aethervane`) are invisible to this process. The string is
/// part of the deployment contract and **must not** be configurable
/// from env — that would defeat the isolation guarantee.
pub const SERVICE: &str = "x402-buy-spl-token";

/// Endpoint id for the `GET /api/v1/buy-spl-token` route.
pub const ENDPOINT_BUY_SPL_TOKEN: &str = "buy-spl-token";

/// Wildcard endpoint string used by service-wide rows.
pub const ENDPOINT_WILDCARD: &str = "*";

pub struct ParametersCache {
    /// Key: `(endpoint, param_name)`. Wildcard rows live under
    /// `("*", param_name)` and are consulted by [`resolve_string`] only
    /// after an endpoint-specific lookup misses.
    map: HashMap<(String, String), String>,
    last_fetch: Option<Instant>,
}

impl ParametersCache {
    fn empty() -> Self {
        Self {
            map: HashMap::new(),
            last_fetch: None,
        }
    }
}

pub static PARAMETERS: OnceLock<RwLock<ParametersCache>> = OnceLock::new();

fn cache_store() -> &'static RwLock<ParametersCache> {
    PARAMETERS.get_or_init(|| RwLock::new(ParametersCache::empty()))
}

/// Env **only** — not read from `parameters` table. The cache TTL is a
/// per-process knob; reading it from the same table that the cache
/// caches would be a chicken-and-egg problem.
pub fn parameters_cache_ttl() -> Duration {
    static TTL: OnceLock<Duration> = OnceLock::new();
    *TTL.get_or_init(|| {
        std::env::var("PARAMETERS_CACHE_TTL_SEC")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(60))
    })
}

fn cache_needs_refresh(cache: &ParametersCache, ttl: Duration) -> bool {
    match cache.last_fetch {
        None => true,
        Some(t) => t.elapsed() > ttl,
    }
}

pub async fn refresh_parameters_from_db(db: Option<&ParametersDb>) {
    let Some(db) = db else {
        return;
    };
    let ttl = parameters_cache_ttl();
    {
        let r = cache_store().read().ok();
        if let Some(c) = r {
            if !cache_needs_refresh(&c, ttl) {
                return;
            }
        }
    }

    let now = Instant::now();
    match db.fetch_parameters_map(SERVICE).await {
        Ok(map) => {
            if let Ok(mut w) = cache_store().write() {
                w.map = map;
                w.last_fetch = Some(now);
            }
        }
        Err(e) => {
            warn!(error = %e, "parameters table read failed (run migrations/init.sql?)");
            if let Ok(mut w) = cache_store().write() {
                w.last_fetch = Some(now);
            }
        }
    }
}

// --- Parameter names ---
//
// Each constant doubles as the Postgres `parameters.param_name` value and
// the process environment variable name. Sharing a single key namespace
// with the rest of the x402 ecosystem (notably `aethervane`) lets one
// `parameters` table back several Vercel deployments without per-service
// prefixes.

pub const X402_NETWORK: &str = "X402_NETWORK";
pub const X402_ACCEPTS_JSON: &str = "X402_ACCEPTS_JSON";
pub const X402_PAY_TO: &str = "X402_PAY_TO";
pub const X402_MERCHANT_WALLET: &str = "X402_MERCHANT_WALLET";
pub const X402_BENEFICIARY: &str = "X402_BENEFICIARY";
pub const X402_SCHEME: &str = "X402_SCHEME";
pub const X402_PAYMENT_TIMEOUT_SECONDS: &str = "X402_PAYMENT_TIMEOUT_SECONDS";
pub const X402_PAYMENT_AMOUNT_USDC: &str = "X402_PAYMENT_AMOUNT_USDC";

/// Comma- or JSON-array-encoded list of oracle authority pubkeys (base58)
/// that the SLA-Escrow `FundPayment` is allowed to bind to. pr402's
/// `build-sla-escrow-payment-tx` requires this in `accepted.extra` and
/// rejects any oracle pubkey not listed. Source order:
/// `(service, endpoint, ORACLE_AUTHORITIES)` → wildcard → env.
pub const ORACLE_AUTHORITIES: &str = "ORACLE_AUTHORITIES";

/// Resolve a parameter following the four-step rule documented at the
/// module level: endpoint-specific row → wildcard row → env → `None`.
///
/// `env_var` may be either a single name (`"X402_PAY_TO"`) or a
/// pipe-separated list of fallbacks (`"X402_PAY_TO|X402_PAY_TO_WALLET"`)
/// — useful for migrating off legacy env-var names without touching
/// every operator's deployment at once.
pub async fn resolve_string(
    db: Option<&ParametersDb>,
    endpoint: &str,
    param_key: &str,
    env_var: Option<&str>,
) -> Option<String> {
    if db.is_some() {
        refresh_parameters_from_db(db).await;
    }

    // Endpoint-specific → wildcard. We hold the read lock for both
    // lookups so a refresh between the two cannot race us into a
    // half-applied view.
    let from_db = cache_store().read().ok().and_then(|c| {
        c.map
            .get(&(endpoint.to_string(), param_key.to_string()))
            .cloned()
            .or_else(|| {
                c.map
                    .get(&(ENDPOINT_WILDCARD.to_string(), param_key.to_string()))
                    .cloned()
            })
    });
    let from_db = from_db.filter(|s| !s.is_empty());

    if from_db.is_some() {
        return from_db;
    }

    env_var
        .and_then(|name| {
            // `|`-separated list: first non-empty wins. Single-name is the
            // common case; the loop handles both.
            for p in name.split('|') {
                let p = p.trim();
                if p.is_empty() {
                    continue;
                }
                if let Ok(v) = std::env::var(p) {
                    if !v.trim().is_empty() {
                        return Some(v);
                    }
                }
            }
            None
        })
        .filter(|s| !s.is_empty())
}

pub async fn resolve_network(db: Option<&ParametersDb>, endpoint: &str) -> Option<String> {
    resolve_string(db, endpoint, X402_NETWORK, Some(X402_NETWORK)).await
}

pub async fn resolve_pay_to(db: Option<&ParametersDb>, endpoint: &str) -> Option<String> {
    // Check X402_PAY_TO then fallback to legacy X402_PAY_TO_WALLET.
    resolve_string(
        db,
        endpoint,
        X402_PAY_TO,
        Some("X402_PAY_TO|X402_PAY_TO_WALLET"),
    )
    .await
}

pub async fn resolve_merchant_wallet(db: Option<&ParametersDb>, endpoint: &str) -> Option<String> {
    resolve_string(
        db,
        endpoint,
        X402_MERCHANT_WALLET,
        Some("X402_MERCHANT_WALLET|MERCHANT_WALLET|SELLER_WALLET"),
    )
    .await
}

/// Optional collection wallet; when set, pr402 prefers this over `merchantWallet`
/// for `FundPayment.seller` / `ReleasePayment`.
pub async fn resolve_beneficiary(db: Option<&ParametersDb>, endpoint: &str) -> Option<String> {
    resolve_string(
        db,
        endpoint,
        X402_BENEFICIARY,
        Some("X402_BENEFICIARY|BENEFICIARY"),
    )
    .await
}

/// Payout identity encoded as on-chain `payment.seller` (matches pr402 build order).
pub async fn resolve_fund_payment_seller(
    db: Option<&ParametersDb>,
    endpoint: &str,
) -> Option<String> {
    resolve_beneficiary(db, endpoint)
        .await
        .filter(|s| !s.is_empty())
        .or(resolve_merchant_wallet(db, endpoint).await)
        .filter(|s| !s.is_empty())
}

pub async fn resolve_scheme(db: Option<&ParametersDb>, endpoint: &str) -> Option<String> {
    resolve_string(db, endpoint, X402_SCHEME, Some(X402_SCHEME)).await
}

pub async fn resolve_accepts_json(db: Option<&ParametersDb>, endpoint: &str) -> Option<String> {
    resolve_string(db, endpoint, X402_ACCEPTS_JSON, Some(X402_ACCEPTS_JSON)).await
}

/// Parse `ORACLE_AUTHORITIES` (JSON array or comma/whitespace-separated pubkeys).
pub fn parse_oracle_authorities(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.starts_with('[') {
        if let Ok(arr) = serde_json::from_str::<Vec<String>>(trimmed) {
            return arr
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    trimmed
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub async fn resolve_timeout_sec(db: Option<&ParametersDb>, endpoint: &str, default: u64) -> u64 {
    let s = resolve_string(
        db,
        endpoint,
        X402_PAYMENT_TIMEOUT_SECONDS,
        Some(X402_PAYMENT_TIMEOUT_SECONDS),
    )
    .await;
    if let Some(ref raw) = s {
        if let Ok(v) = raw.parse::<u64>() {
            return v;
        }
    }
    default
}
