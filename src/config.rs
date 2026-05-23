use crate::error::Error;
use crate::x402::pricing::SOLANA_MAINNET;
use http::HeaderMap;
use std::env;

/// Runtime configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen_addr: String,
    pub solana_rpc_url: String,
    /// Facilitator base including `/api/v1/facilitator` (pr402).
    pub x402_facilitator_url: String,
    pub x402_network: String,
    pub x402_pay_to: String,
    pub x402_merchant_wallet: Option<String>,
    pub x402_timeout_sec: u64,
    /// When true (default if `DATABASE_URL` is set), Postgres backs parameters + purchase ledger.
    pub database_enabled: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, Error> {
        let listen_addr =
            env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

        let solana_rpc_url = env::var("RPC_URL")
            .unwrap_or_else(|_| "https://api.devnet.solana.com".to_string());

        let x402_facilitator_url = env::var("X402_FACILITATOR_URL").map_err(|_| {
            Error::Internal(
                "X402_FACILITATOR_URL required (e.g. https://<host>/api/v1/facilitator)".into(),
            )
        })?;

        let x402_network = env::var("X402_NETWORK").unwrap_or_else(|_| SOLANA_MAINNET.to_string());

        let x402_pay_to = env::var("X402_PAY_TO")
            .or_else(|_| env::var("X402_PAY_TO_WALLET"))
            .map_err(|_| {
                Error::Internal(
                    "X402_PAY_TO not set (sla-escrow rail expects the per-asset escrow PDA)".into(),
                )
            })?;

        let x402_merchant_wallet = env::var("X402_MERCHANT_WALLET")
            .or_else(|_| env::var("MERCHANT_WALLET"))
            .or_else(|_| env::var("SELLER_WALLET"))
            .ok();

        let x402_timeout_sec = env::var("X402_PAYMENT_TIMEOUT_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);

        let database_enabled = match env::var("DATABASE_ENABLED").ok().as_deref() {
            Some("0") | Some("false") | Some("FALSE") | Some("no") | Some("NO") => false,
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES") => true,
            _ => env::var("DATABASE_URL").is_ok(),
        };

        Ok(Self {
            listen_addr,
            solana_rpc_url,
            x402_facilitator_url,
            x402_network,
            x402_pay_to,
            x402_merchant_wallet,
            x402_timeout_sec,
            database_enabled,
        })
    }

    pub fn x402_resource_url(&self) -> String {
        env::var("X402_RESOURCE_URL").unwrap_or_else(|_| {
            format!(
                "http://{}/api/v1/buy-spl-token",
                self.listen_addr.replace("0.0.0.0", "127.0.0.1")
            )
        })
    }

    pub fn x402_resource_url_for_request(
        &self,
        headers: &HeaderMap,
        path: &str,
        query: &str,
    ) -> String {
        let host = headers
            .get("x-forwarded-host")
            .or_else(|| headers.get("host"))
            .and_then(|h| h.to_str().ok())
            .filter(|h| !h.is_empty());
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("https");
        if let Some(host) = host {
            let path_query = if query.is_empty() {
                path.to_string()
            } else {
                format!("{}?{}", path, query)
            };
            format!("{}://{}{}", proto, host, path_query)
        } else {
            self.x402_resource_url()
        }
    }
}
