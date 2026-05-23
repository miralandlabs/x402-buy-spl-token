use {
    crate::{
        catalog::TokenCatalog, config::Config, db::ParametersDb, error::Error,
        purchase_ledger::PurchaseLedger, seller_signer::SellerSigner, x402::FacilitatorClient,
    },
    solana_commitment_config::CommitmentConfig,
    std::sync::Arc,
};

/// Shared application state for the Axum server.
pub struct AppState {
    pub config: Arc<Config>,
    pub rpc_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    /// Optional Postgres parameters store. Absent when `DATABASE_ENABLED=false`.
    pub db: Option<Arc<ParametersDb>>,
    pub ledger: PurchaseLedger,
    pub facilitator: Arc<FacilitatorClient>,
    pub catalog: Option<Arc<TokenCatalog>>,
    pub seller_signer: Option<Arc<SellerSigner>>,
}

impl AppState {
    pub fn new(config: &Config) -> Result<Self, Error> {
        let rpc_client = Arc::new(
            solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
                config.solana_rpc_url.clone(),
                CommitmentConfig::confirmed(),
            ),
        );

        let (db, ledger) = if config.database_enabled {
            match ParametersDb::from_env_var("DATABASE_URL") {
                None => {
                    tracing::warn!(
                        "DATABASE_ENABLED but DATABASE_URL unset; using in-memory ledger"
                    );
                    (None, PurchaseLedger::memory())
                }
                Some(Ok(d)) => {
                    let db = Arc::new(d);
                    (Some(db.clone()), PurchaseLedger::from_db(db.as_ref()))
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "DATABASE_URL set but connection failed; using in-memory ledger");
                    (None, PurchaseLedger::memory())
                }
            }
        } else {
            tracing::info!("database disabled; using in-memory purchase ledger");
            (None, PurchaseLedger::memory())
        };

        let facilitator = FacilitatorClient::new(config.x402_facilitator_url.clone());

        Ok(Self {
            config: Arc::new(config.clone()),
            rpc_client,
            db,
            ledger,
            facilitator: Arc::new(facilitator),
            catalog: None,
            seller_signer: None,
        })
    }
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            rpc_client: Arc::clone(&self.rpc_client),
            db: self.db.as_ref().map(Arc::clone),
            ledger: self.ledger.clone(),
            facilitator: Arc::clone(&self.facilitator),
            catalog: self.catalog.as_ref().map(Arc::clone),
            seller_signer: self.seller_signer.as_ref().map(Arc::clone),
        }
    }
}
