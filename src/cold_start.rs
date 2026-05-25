//! Cold-start wiring for the buy-spl-token reference seller.
//!
//! The buy endpoint has three pieces of state that must be valid before the
//! first request is served:
//!
//! 1. A parsed and statically-validated [`TokenCatalog`].
//! 2. A loaded [`SellerSigner`] (decoded once from `SELLER_KEYPAIR_BASE58`).
//! 3. A loaded [`MerchantSigner`](crate::merchant_signer::MerchantSigner)
//!    (`MERCHANT_SIGNER_KEYPAIR_BASE58`) matching the fund-payment payout identity.
//! 4. sla-escrow operator config: merchant wallet, oracle allow-list, registry.
//! 5. The `purchase_orders` migration applied
//!    to the configured Postgres database, alongside the base
//!    `parameters` migration (`init.sql`).
//!
//! The catalog is additionally cross-checked against on-chain mint metadata —
//! configured `decimals` must match the byte stored in each Mint account.
//! Any failure in the sequence aborts startup with a clear log line so the
//! Vercel deployment surfaces it immediately rather than serving misconfigured
//! requests.
//!
//! # Test seam
//!
//! The on-chain validation is factored behind a small [`MintFetcher`] trait
//! so unit tests can drive the cold-start sequence without a live Solana RPC.
//! Production code constructs an [`RpcMintFetcher`] backed by the same
//! [`RpcClient`] held in [`AppState`].

use {
    crate::{
        catalog::{CatalogError, TokenCatalog},
        db::ParametersDb,
        merchant_signer::{self},
        parameters::{
            parse_oracle_authorities, resolve_fund_payment_seller, resolve_merchant_wallet,
            resolve_pay_to, resolve_string, ENDPOINT_BUY_SPL_TOKEN, ORACLE_AUTHORITIES,
        },
        registry_client::{REGISTRY_BASE_URL, REGISTRY_BEARER_TOKEN},
        rpc_retry::{with_retry, RetryPolicy},
        seller_signer::{KeypairLoadError, SellerSigner, SellerSignerError},
        AppState,
    },
    solana_client::nonblocking::rpc_client::RpcClient,
    solana_sdk::{account::Account, pubkey::Pubkey},
    std::{fmt, str::FromStr, sync::Arc},
};

/// Abstract source of on-chain Mint accounts, used by the cold-start
/// validation loop.
///
/// The real implementation ([`RpcMintFetcher`]) wraps a Solana
/// [`RpcClient`]; tests provide an in-memory map.
pub trait MintFetcher: Send + Sync {
    /// Fetch the on-chain Mint account at `mint`. Errors are surfaced as a
    /// human-readable reason and treated as "unreachable" by the caller.
    fn get_mint_account<'a>(
        &'a self,
        mint: &'a Pubkey,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Account, String>> + Send + 'a>>;
}

/// Production [`MintFetcher`] backed by Solana RPC + [`RetryPolicy`].
pub struct RpcMintFetcher {
    rpc: Arc<RpcClient>,
    retry: RetryPolicy,
}

impl RpcMintFetcher {
    pub fn new(rpc: Arc<RpcClient>, retry: RetryPolicy) -> Self {
        Self { rpc, retry }
    }
}

impl MintFetcher for RpcMintFetcher {
    fn get_mint_account<'a>(
        &'a self,
        mint: &'a Pubkey,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Account, String>> + Send + 'a>>
    {
        let rpc = self.rpc.clone();
        let retry = self.retry;
        let mint = *mint;
        Box::pin(async move {
            with_retry(retry, "get_account:catalog_mint", None, || {
                let rpc = rpc.clone();
                async move { rpc.get_account(&mint).await }
            })
            .await
            .map_err(|e| e.to_string())
        })
    }
}

/// Validate a parsed [`TokenCatalog`] against on-chain Mint metadata using
/// the supplied [`MintFetcher`]. Mirrors
/// [`TokenCatalog::validate_against_chain`] but is parameterized over the
/// fetcher so tests can drive it without RPC.
pub async fn validate_catalog_with_fetcher(
    catalog: &TokenCatalog,
    fetcher: &dyn MintFetcher,
) -> Result<(), CatalogError> {
    for entry in catalog.entries() {
        let mint_pk = entry.mint_pubkey()?;
        let account = fetcher.get_mint_account(&mint_pk).await.map_err(|reason| {
            CatalogError::UnreachableMint {
                entry_name: entry.name.clone(),
                mint: entry.mint.clone(),
                reason,
            }
        })?;

        // SPL Token / Token-2022 base mint layouts both place the decimals
        // byte at offset 44 (mint_auth_opt(4) + mint_auth(32) + supply(8)).
        if account.data.len() < 45 {
            return Err(CatalogError::UnreachableMint {
                entry_name: entry.name.clone(),
                mint: entry.mint.clone(),
                reason: format!(
                    "account data length {} < 45 (not a Mint account)",
                    account.data.len()
                ),
            });
        }
        let on_chain = account.data[44];
        if on_chain != entry.decimals {
            return Err(CatalogError::DecimalsMismatch {
                entry_name: entry.name.clone(),
                mint: entry.mint.clone(),
                configured: entry.decimals,
                on_chain,
            });
        }
    }
    Ok(())
}

/// All possible cold-start failures, each one a startup-aborting condition.
#[derive(Debug)]
pub enum ColdStartError {
    /// The token catalog is missing, malformed, or fails on-chain validation.
    Catalog(CatalogError),
    /// The seller keypair could not be loaded from the environment.
    SellerSigner(SellerSignerError),
    /// The merchant signer keypair could not be loaded from the environment.
    MerchantSigner(KeypairLoadError),
    /// Merchant signer pubkey does not match fund-payment payout identity.
    MerchantSignerMismatch { expected: String, actual: String },
    /// Merchant payout wallet not configured (Postgres or env alias chain).
    MissingMerchantWallet,
    /// Merchant wallet is not a valid base58 pubkey.
    InvalidMerchantWallet { value: String, reason: String },
    /// Merchant wallet must differ from sla-escrow escrow PDA (`payTo`).
    MerchantEqualsEscrowPayTo { merchant: String, pay_to: String },
    /// Oracle allow-list empty or unset.
    MissingOracleAuthorities,
    /// Required process env var missing (registry, etc.).
    MissingEnvVar { name: &'static str },
    /// Applying a migration script failed.
    Migration {
        script: &'static str,
        reason: String,
    },
}

impl fmt::Display for ColdStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(e) => write!(f, "catalog cold-start failure: {}", e),
            Self::SellerSigner(e) => write!(f, "seller signer cold-start failure: {}", e),
            Self::MerchantSigner(e) => write!(f, "merchant signer cold-start failure: {}", e),
            Self::MerchantSignerMismatch { expected, actual } => write!(
                f,
                "merchant signer pubkey {:?} does not match fund-payment payout identity {:?} (extra.beneficiary ?? extra.merchantWallet); FundPayment.seller and SubmitDelivery require this pubkey",
                actual, expected
            ),
            Self::MissingMerchantWallet => write!(
                f,
                "merchant wallet not configured: set X402_MERCHANT_WALLET or MERCHANT_WALLET (Postgres parameters row or env); this is the ReleasePayment beneficiary and must not be omitted"
            ),
            Self::InvalidMerchantWallet { value, reason } => write!(
                f,
                "merchant wallet {:?} is not a valid base58 pubkey: {}",
                value, reason
            ),
            Self::MerchantEqualsEscrowPayTo { merchant, pay_to } => write!(
                f,
                "merchant wallet {:?} must not equal sla-escrow escrow PDA (payTo) {:?}; configure X402_MERCHANT_WALLET separately from X402_PAY_TO",
                merchant, pay_to
            ),
            Self::MissingOracleAuthorities => write!(
                f,
                "ORACLE_AUTHORITIES not set (parameters table or env); pr402 requires accepted.extra.oracleAuthorities for sla-escrow builds"
            ),
            Self::MissingEnvVar { name } => write!(f, "required env var {} is not set", name),
            Self::Migration { script, reason } => {
                write!(f, "migration cold-start failure ({}): {}", script, reason)
            }
        }
    }
}

impl std::error::Error for ColdStartError {}

impl From<CatalogError> for ColdStartError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

impl From<SellerSignerError> for ColdStartError {
    fn from(e: SellerSignerError) -> Self {
        Self::SellerSigner(e)
    }
}

/// Pure cold-start preparation: parse the catalog, validate it against the
/// supplied [`MintFetcher`], and decode the seller keypair.
///
/// No I/O beyond what the fetcher performs; safe to call from unit tests.
/// `catalog_json` is the byte-exact JSON the operator configured (whichever
/// of Postgres / env won the source-priority tie); `seller_keypair_base58`
/// is the raw env-var value.
pub async fn prepare_buy_runtime(
    catalog_json: &str,
    seller_keypair_base58: &str,
    fetcher: &dyn MintFetcher,
) -> Result<(TokenCatalog, SellerSigner), ColdStartError> {
    let catalog = crate::catalog::parse_catalog_json(catalog_json)?;
    validate_catalog_with_fetcher(&catalog, fetcher).await?;
    let signer = SellerSigner::from_base58(
        crate::seller_signer::SELLER_KEYPAIR_BASE58,
        seller_keypair_base58,
    )?;
    Ok((catalog, signer))
}

/// Validate sla-escrow operator config before serving traffic.
pub async fn validate_operator_config(
    config: &crate::config::Config,
    db: Option<&ParametersDb>,
) -> Result<(), ColdStartError> {
    let pay_to = resolve_pay_to(db, ENDPOINT_BUY_SPL_TOKEN)
        .await
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| config.x402_pay_to.clone());

    let merchant = resolve_merchant_wallet(db, ENDPOINT_BUY_SPL_TOKEN)
        .await
        .filter(|s| !s.is_empty())
        .ok_or(ColdStartError::MissingMerchantWallet)?;

    Pubkey::from_str(&merchant).map_err(|e| ColdStartError::InvalidMerchantWallet {
        value: merchant.clone(),
        reason: e.to_string(),
    })?;

    if merchant == pay_to {
        return Err(ColdStartError::MerchantEqualsEscrowPayTo { merchant, pay_to });
    }

    let oracle_raw = resolve_string(
        db,
        ENDPOINT_BUY_SPL_TOKEN,
        ORACLE_AUTHORITIES,
        Some(ORACLE_AUTHORITIES),
    )
    .await;
    let oracles = oracle_raw
        .as_deref()
        .map(parse_oracle_authorities)
        .unwrap_or_default();
    if oracles.is_empty() {
        return Err(ColdStartError::MissingOracleAuthorities);
    }

    for name in [REGISTRY_BASE_URL, REGISTRY_BEARER_TOKEN] {
        if std::env::var(name)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            return Err(ColdStartError::MissingEnvVar { name });
        }
    }

    Ok(())
}

/// Ensure the merchant signer's pubkey matches what pr402 will encode as
/// `FundPayment.seller` (`beneficiary` preferred, else `merchantWallet`).
pub async fn validate_merchant_signer_matches_payout(
    db: Option<&ParametersDb>,
    merchant_signer: &Pubkey,
) -> Result<(), ColdStartError> {
    let payout = resolve_fund_payment_seller(db, ENDPOINT_BUY_SPL_TOKEN)
        .await
        .filter(|s| !s.is_empty())
        .ok_or(ColdStartError::MissingMerchantWallet)?;

    let expected =
        Pubkey::from_str(&payout).map_err(|e| ColdStartError::InvalidMerchantWallet {
            value: payout.clone(),
            reason: e.to_string(),
        })?;

    if *merchant_signer != expected {
        return Err(ColdStartError::MerchantSignerMismatch {
            expected: payout,
            actual: merchant_signer.to_string(),
        });
    }

    Ok(())
}

/// Production cold-start orchestration.
///
/// 1. Base [`AppState`] (RPC, facilitator, ledger backend).
/// 2. Catalog from Postgres parameters or `BUY_SPL_TOKEN_CATALOG_JSON`.
/// 3. Delivery hot key from `SELLER_KEYPAIR_BASE58`.
/// 4. Merchant signer from `MERCHANT_SIGNER_KEYPAIR_BASE58`.
/// 5. sla-escrow operator config (merchant, oracles, registry env).
/// 6. On-chain mint decimals validation.
/// 7. Postgres migrations when the ledger backend is Postgres.
pub async fn cold_start(config: &crate::config::Config) -> Result<AppState, ColdStartError> {
    // (1) Base state — RPC, optional parameters DB, facilitator, ledger.
    let mut state = AppState::new(config).map_err(|e| ColdStartError::Migration {
        script: "(AppState::new)",
        reason: e.to_string(),
    })?;

    // (2) Catalog source: DB beats env per requirement 1.2.
    let catalog = crate::catalog::load(
        state.db.as_deref(),
        crate::parameters::ENDPOINT_BUY_SPL_TOKEN,
    )
    .await
    .map_err(|e| {
        tracing::error!(target: "server_log", error = %e, "buy endpoint: catalog cold-start failed");
        ColdStartError::from(e)
    })?;

    // (2b) Delivery hot key + merchant payout signer.
    let seller = SellerSigner::from_env().map_err(|e| {
        tracing::error!(target: "server_log", error = %e, "buy endpoint: seller signer cold-start failed");
        ColdStartError::from(e)
    })?;
    let merchant = merchant_signer::from_env().map_err(|e| {
        tracing::error!(target: "server_log", error = %e, "buy endpoint: merchant signer cold-start failed");
        ColdStartError::MerchantSigner(e)
    })?;

    // (2c) Merchant wallet, oracle allow-list, registry env — abort if misconfigured.
    validate_operator_config(config, state.db.as_deref())
        .await
        .map_err(|e| {
            tracing::error!(target: "server_log", error = %e, "buy endpoint: operator config cold-start failed");
            e
        })?;

    validate_merchant_signer_matches_payout(state.db.as_deref(), &merchant.pubkey())
        .await
        .map_err(|e| {
            tracing::error!(target: "server_log", error = %e, "buy endpoint: merchant signer payout mismatch");
            e
        })?;

    // (3) On-chain decimals validation.
    let fetcher = RpcMintFetcher::new(state.rpc_client.clone(), RetryPolicy::from_env());
    validate_catalog_with_fetcher(&catalog, &fetcher).await.map_err(|e| {
        tracing::error!(target: "server_log", error = %e, "buy endpoint: on-chain catalog validation failed");
        ColdStartError::from(e)
    })?;

    // (4) Migrations when Postgres backs the ledger.
    if state.ledger.is_postgres() {
        if let Some(db) = state.db.as_deref() {
            apply_migrations(db).await?;
        }
    } else {
        tracing::info!(
            target: "server_log",
            "in-memory purchase ledger active; Postgres migrations skipped"
        );
    }

    state.catalog = Some(Arc::new(catalog));
    state.seller_signer = Some(Arc::new(seller));
    state.merchant_signer = Some(Arc::new(merchant));
    Ok(state)
}

const MIGRATION_INIT: &str = include_str!("../migrations/init.sql");

/// Apply the buy-endpoint migrations in order.
///
/// Both migration scripts are idempotent (`CREATE TABLE IF NOT EXISTS`,
/// `CREATE INDEX IF NOT EXISTS`), so running this on every cold start is
/// safe and reduces operator burden.
pub async fn apply_migrations(db: &ParametersDb) -> Result<(), ColdStartError> {
    db.execute_batch(MIGRATION_INIT)
        .await
        .map_err(|e| ColdStartError::Migration {
            script: "migrations/init.sql",
            reason: e.to_string(),
        })?;
    tracing::info!(target: "server_log", script = "migrations/init.sql", "applied migration");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seller_signer::KeypairLoadError;
    use solana_sdk::{account::Account, pubkey::Pubkey, signer::keypair::Keypair, signer::Signer};
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::Mutex;

    /// Real Solana mint pubkey from the spec (Merry Xmas devnet token).
    const VALID_MINT: &str = "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP";

    fn fresh_seller_b58() -> String {
        bs58::encode(Keypair::new().to_bytes()).into_string()
    }

    /// Build a minimal SPL Mint account whose decimals byte (offset 44)
    /// equals `decimals`. The leading 44 bytes can be zero — the decimals
    /// check looks only at byte 44.
    fn mint_account_with_decimals(decimals: u8) -> Account {
        let mut data = vec![0u8; 82];
        data[44] = decimals;
        Account {
            lamports: 1,
            data,
            owner: spl_token::ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    #[derive(Default)]
    struct MockFetcher {
        // Map from mint pubkey -> account, or `None` to simulate "not found".
        responses: Mutex<HashMap<Pubkey, Result<Account, String>>>,
    }

    impl MockFetcher {
        fn with_decimals(mint: &str, decimals: u8) -> Self {
            let mut map = HashMap::new();
            map.insert(
                Pubkey::from_str(mint).unwrap(),
                Ok(mint_account_with_decimals(decimals)),
            );
            Self {
                responses: Mutex::new(map),
            }
        }
    }

    impl MintFetcher for MockFetcher {
        fn get_mint_account<'a>(
            &'a self,
            mint: &'a Pubkey,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Account, String>> + Send + 'a>>
        {
            let lookup = self
                .responses
                .lock()
                .unwrap()
                .get(mint)
                .cloned()
                .unwrap_or_else(|| Err(format!("not configured: {}", mint)));
            Box::pin(async move { lookup })
        }
    }

    fn catalog_json(decimals: u8) -> String {
        format!(
            r#"[{{"mint":"{}","decimals":{},"price_usdc_ui":"0.42","deliver_amount_ui":"1000","name":"Merry Xmas"}}]"#,
            VALID_MINT, decimals
        )
    }

    // ---- Happy path: produces a populated AppState (validated catalog + signer). ----

    #[tokio::test]
    async fn happy_path_produces_populated_runtime() {
        let json = catalog_json(6);
        let seller_b58 = fresh_seller_b58();
        let fetcher = MockFetcher::with_decimals(VALID_MINT, 6);

        let (cat, signer) = prepare_buy_runtime(&json, &seller_b58, &fetcher)
            .await
            .expect("happy path should produce runtime");

        assert_eq!(cat.entries().len(), 1);
        assert_eq!(cat.entries()[0].decimals, 6);

        // Signer round-trips: the loaded keypair signs and verifies.
        let msg = b"cold-start-happy-path";
        let sig = signer.sign_message(msg);
        assert!(sig.verify(signer.pubkey().as_ref(), msg));
    }

    // ---- Catalog env missing → cold start fails. ----

    #[tokio::test]
    async fn cold_start_fails_when_catalog_env_missing() {
        // load_from_strings(None, None) is the public seam for the
        // missing-env condition. We assert the cold-start orchestration
        // surfaces it through `prepare_buy_runtime` when the JSON is empty.
        let seller_b58 = fresh_seller_b58();
        let fetcher = MockFetcher::default();

        // Empty string is treated by `parse_catalog_json` as a JSON parse
        // error; an actually-missing env in the production loader surfaces
        // as `CatalogError::MissingEnv`. We exercise both paths.
        let err = prepare_buy_runtime("", &seller_b58, &fetcher)
            .await
            .expect_err("empty catalog string must fail cold start");
        match err {
            ColdStartError::Catalog(CatalogError::ParseError { .. }) => {}
            other => panic!("expected Catalog(ParseError), got {:?}", other),
        }

        // Direct missing-env path via the catalog loader.
        let direct_err = crate::catalog::load_from_strings(None, None).expect_err("must fail");
        assert!(matches!(direct_err, CatalogError::MissingEnv));
    }

    // ---- On-chain decimals mismatch → cold start fails. ----

    #[tokio::test]
    async fn cold_start_fails_on_decimals_mismatch() {
        let json = catalog_json(6); // configured 6
        let seller_b58 = fresh_seller_b58();
        // Fetcher returns a mint with decimals=9.
        let fetcher = MockFetcher::with_decimals(VALID_MINT, 9);

        let err = prepare_buy_runtime(&json, &seller_b58, &fetcher)
            .await
            .expect_err("decimals mismatch must fail cold start");
        match err {
            ColdStartError::Catalog(CatalogError::DecimalsMismatch {
                configured,
                on_chain,
                ..
            }) => {
                assert_eq!(configured, 6);
                assert_eq!(on_chain, 9);
            }
            other => panic!("expected DecimalsMismatch, got {:?}", other),
        }
    }

    // ---- Unreachable mint surfaces as Catalog(UnreachableMint). ----

    #[tokio::test]
    async fn cold_start_fails_when_mint_unreachable() {
        let json = catalog_json(6);
        let seller_b58 = fresh_seller_b58();
        // Empty fetcher → every lookup returns Err.
        let fetcher = MockFetcher::default();

        let err = prepare_buy_runtime(&json, &seller_b58, &fetcher)
            .await
            .expect_err("missing on-chain mint must fail cold start");
        assert!(matches!(
            err,
            ColdStartError::Catalog(CatalogError::UnreachableMint { .. })
        ));
    }

    // ---- Operator config validation ------------------------------------

    static ENV_TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    fn clear_operator_env() {
        for key in [
            "X402_MERCHANT_WALLET",
            "MERCHANT_WALLET",
            "SELLER_WALLET",
            "X402_BENEFICIARY",
            "BENEFICIARY",
            "ORACLE_AUTHORITIES",
            "REGISTRY_BASE_URL",
            "REGISTRY_BEARER_TOKEN",
            "MERCHANT_SIGNER_KEYPAIR_BASE58",
        ] {
            std::env::remove_var(key);
        }
    }

    fn minimal_config(pay_to: &str) -> crate::config::Config {
        crate::config::Config {
            listen_addr: "127.0.0.1:8080".into(),
            solana_rpc_url: "https://api.devnet.solana.com".into(),
            x402_facilitator_url: "https://example.com/api/v1/facilitator".into(),
            x402_network: "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1".into(),
            x402_pay_to: pay_to.into(),
            x402_merchant_wallet: None,
            x402_timeout_sec: 300,
            database_enabled: false,
        }
    }

    #[tokio::test]
    async fn operator_config_rejects_missing_merchant() {
        let _guard = ENV_TEST_LOCK.lock().await;
        clear_operator_env();
        let cfg = minimal_config("EscrowPda1111111111111111111111111111111111");
        let err = validate_operator_config(&cfg, None)
            .await
            .expect_err("must fail without merchant");
        assert!(matches!(err, ColdStartError::MissingMerchantWallet));
    }

    #[tokio::test]
    async fn operator_config_rejects_merchant_equal_to_pay_to() {
        let _guard = ENV_TEST_LOCK.lock().await;
        let pay = "BeALNhc8tykF6wJBZWyXGEkb9Mfvk8JZk8miUL2JDuhw";
        clear_operator_env();
        std::env::set_var("X402_MERCHANT_WALLET", pay);
        std::env::set_var(
            "ORACLE_AUTHORITIES",
            "[\"oraG62Mr5hDYeSbAtKMpEYFw22SLpZdebXvDe2Qr7xV\"]",
        );
        std::env::set_var("REGISTRY_BASE_URL", "https://registry.example.com");
        std::env::set_var("REGISTRY_BEARER_TOKEN", "test-token");

        let cfg = minimal_config(pay);
        let err = validate_operator_config(&cfg, None)
            .await
            .expect_err("merchant must differ from pay_to");
        assert!(matches!(
            err,
            ColdStartError::MerchantEqualsEscrowPayTo { .. }
        ));
    }

    #[tokio::test]
    async fn operator_config_happy_path_with_env() {
        let _guard = ENV_TEST_LOCK.lock().await;
        let pay = "EscrowPda1111111111111111111111111111111111";
        let merchant = "BeALNhc8tykF6wJBZWyXGEkb9Mfvk8JZk8miUL2JDuhw";
        clear_operator_env();
        std::env::set_var("X402_MERCHANT_WALLET", merchant);
        std::env::set_var(
            "ORACLE_AUTHORITIES",
            "[\"oraG62Mr5hDYeSbAtKMpEYFw22SLpZdebXvDe2Qr7xV\"]",
        );
        std::env::set_var("REGISTRY_BASE_URL", "https://registry.example.com");
        std::env::set_var("REGISTRY_BEARER_TOKEN", "test-token");

        let cfg = minimal_config(pay);
        validate_operator_config(&cfg, None)
            .await
            .expect("valid operator env should pass");
    }

    #[tokio::test]
    async fn merchant_signer_must_match_fund_payment_seller() {
        let _guard = ENV_TEST_LOCK.lock().await;
        let pay = "EscrowPda1111111111111111111111111111111111";
        let merchant = Keypair::new();
        let wrong_signer = Keypair::new();
        clear_operator_env();
        std::env::set_var("X402_MERCHANT_WALLET", merchant.pubkey().to_string());
        std::env::set_var(
            "ORACLE_AUTHORITIES",
            "[\"oraG62Mr5hDYeSbAtKMpEYFw22SLpZdebXvDe2Qr7xV\"]",
        );
        std::env::set_var("REGISTRY_BASE_URL", "https://registry.example.com");
        std::env::set_var("REGISTRY_BEARER_TOKEN", "test-token");

        let err = validate_merchant_signer_matches_payout(None, &wrong_signer.pubkey())
            .await
            .expect_err("mismatch must fail");
        assert!(matches!(err, ColdStartError::MerchantSignerMismatch { .. }));

        validate_merchant_signer_matches_payout(None, &merchant.pubkey())
            .await
            .expect("matching merchant signer should pass");

        let _cfg = minimal_config(pay);
    }

    #[tokio::test]
    async fn merchant_signer_must_match_beneficiary_when_set() {
        let _guard = ENV_TEST_LOCK.lock().await;
        let beneficiary = Keypair::new();
        let merchant = Keypair::new();
        clear_operator_env();
        std::env::set_var("X402_BENEFICIARY", beneficiary.pubkey().to_string());
        std::env::set_var("X402_MERCHANT_WALLET", merchant.pubkey().to_string());

        validate_merchant_signer_matches_payout(None, &merchant.pubkey())
            .await
            .expect_err("merchantWallet must not match when beneficiary is set");

        validate_merchant_signer_matches_payout(None, &beneficiary.pubkey())
            .await
            .expect("beneficiary pubkey should match");
    }

    // ---- Bad seller key surfaces as SellerSigner error after catalog passes. ----

    #[tokio::test]
    async fn cold_start_fails_on_bad_seller_keypair() {
        let json = catalog_json(6);
        let fetcher = MockFetcher::with_decimals(VALID_MINT, 6);

        let err = prepare_buy_runtime(&json, "not-valid-base58-0OIl", &fetcher)
            .await
            .expect_err("bad seller key must fail cold start");
        assert!(matches!(
            err,
            ColdStartError::SellerSigner(KeypairLoadError::InvalidBase58 { .. })
        ));
    }

    // ---- Sanity: error Display surfaces the underlying reason. ----

    #[test]
    fn cold_start_error_display_chains_underlying_reason() {
        let err = ColdStartError::Catalog(CatalogError::MissingEnv);
        let s = err.to_string();
        assert!(s.contains("catalog cold-start failure"), "{}", s);
        assert!(s.contains("BUY_SPL_TOKEN_CATALOG_JSON"), "{}", s);

        let err = ColdStartError::Migration {
            script: "x.sql",
            reason: "boom".into(),
        };
        let s = err.to_string();
        assert!(s.contains("x.sql"));
        assert!(s.contains("boom"));
    }
}
