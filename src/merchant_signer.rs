//! Merchant payout signer loaded once at cold start (`MERCHANT_SIGNER_KEYPAIR_BASE58`).
//!
//! Signs sla-escrow `SubmitDelivery`. The pubkey must match the payout identity
//! pr402 encodes as `FundPayment.seller` (`extra.beneficiary` preferred, else
//! `extra.merchantWallet`). See [`crate::parameters::resolve_fund_payment_seller`].

pub use crate::seller_signer::{KeypairLoadError, KeypairSigner as MerchantSigner};

/// Process env var holding the base58-encoded merchant signer keypair.
pub const MERCHANT_SIGNER_KEYPAIR_BASE58: &str = "MERCHANT_SIGNER_KEYPAIR_BASE58";

/// Load the merchant signer from [`MERCHANT_SIGNER_KEYPAIR_BASE58`].
pub fn from_env() -> Result<MerchantSigner, KeypairLoadError> {
    MerchantSigner::from_env_var(MERCHANT_SIGNER_KEYPAIR_BASE58)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signer::{keypair::Keypair, Signer};

    #[test]
    fn from_env_missing_var() {
        let saved = std::env::var(MERCHANT_SIGNER_KEYPAIR_BASE58).ok();
        std::env::remove_var(MERCHANT_SIGNER_KEYPAIR_BASE58);
        let err = from_env().unwrap_err();
        assert!(matches!(
            err,
            KeypairLoadError::MissingEnv {
                var: MERCHANT_SIGNER_KEYPAIR_BASE58
            }
        ));
        if let Some(v) = saved {
            std::env::set_var(MERCHANT_SIGNER_KEYPAIR_BASE58, v);
        }
    }

    #[test]
    fn loads_valid_keypair() {
        let kp = Keypair::new();
        let encoded = bs58::encode(kp.to_bytes()).into_string();
        let signer = MerchantSigner::from_base58(MERCHANT_SIGNER_KEYPAIR_BASE58, &encoded).unwrap();
        assert_eq!(signer.pubkey(), kp.pubkey());
    }
}
