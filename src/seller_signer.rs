//! Seller signing key loaded once at cold start.
//!
//! The Buy SPL Token endpoint signs two on-chain transactions per paid
//! request: an SPL `TransferChecked` that delivers the purchased token to the
//! buyer, and a `SubmitDelivery` that records the delivery against the SLA.
//! Both are signed by the same seller key, and that key is sensitive enough
//! that we want to read and decode the secret material exactly once — at
//! cold start — and then reuse a shared in-memory handle for every request.
//!
//! This module owns that initialization. [`SellerSigner::from_env`] decodes
//! `SELLER_KEYPAIR_BASE58` once, returns a [`SellerSigner`] backed by an
//! `Arc<Keypair>`, and exposes a thin API:
//!
//! - [`SellerSigner::pubkey`] — the seller's [`Pubkey`].
//! - [`SellerSigner::sign_message`] — produce a [`Signature`] over a
//!   pre-serialized transaction message.
//! - [`SellerSigner::keypair`] — clone the underlying `Arc<Keypair>` for
//!   callers that need to pass it directly into Solana SDK builders.
//!
//! # Reuse across the two transactions in a request
//!
//! The same `Arc<Keypair>` returned by `keypair()` is intended to sign both
//! the SPL `TransferChecked` and the subsequent `SubmitDelivery` transaction
//! within a single request. The clone is cheap (an atomic refcount bump),
//! and the underlying secret bytes are never re-decoded after cold start.
//!
//! # Failure modes
//!
//! All three "we cannot start without a working seller key" conditions map to
//! [`SellerSignerError`] variants, which the cold-start path turns into a
//! fatal startup error. The variants intentionally do not include the secret
//! material itself, only enough context to tell the operator *which*
//! configuration value is wrong.

use {
    solana_sdk::{
        pubkey::Pubkey,
        signature::Signature,
        signer::{keypair::Keypair, Signer},
    },
    std::{env, fmt, sync::Arc},
};

/// Process env var holding the base58-encoded 64-byte seller keypair
/// (32-byte secret followed by 32-byte public key, the same layout
/// `Keypair::to_base58_string` emits).
pub const SELLER_KEYPAIR_BASE58: &str = "SELLER_KEYPAIR_BASE58";

/// Expected length of a Solana ed25519 keypair byte array (secret || public).
const KEYPAIR_LENGTH: usize = 64;

/// Cold-start-loaded seller signer.
///
/// Cheap to clone (just clones an `Arc`); the secret bytes are decoded once
/// in [`SellerSigner::from_env`] and never re-read.
#[derive(Clone)]
pub struct SellerSigner {
    keypair: Arc<Keypair>,
}

impl SellerSigner {
    /// Load the seller [`Keypair`] from the [`SELLER_KEYPAIR_BASE58`] env
    /// var. Intended to be called exactly once at cold start.
    pub fn from_env() -> Result<Self, SellerSignerError> {
        let raw = env::var(SELLER_KEYPAIR_BASE58).map_err(|_| SellerSignerError::MissingEnv)?;
        Self::from_base58(&raw)
    }

    /// Decode a base58-encoded keypair string into a [`SellerSigner`].
    ///
    /// Exposed so the loader can be unit-tested without touching process
    /// environment. Production code should prefer [`Self::from_env`].
    pub fn from_base58(s: &str) -> Result<Self, SellerSignerError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(SellerSignerError::MissingEnv);
        }

        let bytes =
            bs58::decode(trimmed)
                .into_vec()
                .map_err(|e| SellerSignerError::InvalidBase58 {
                    reason: e.to_string(),
                })?;

        if bytes.len() != KEYPAIR_LENGTH {
            return Err(SellerSignerError::InvalidLength {
                actual: bytes.len(),
                expected: KEYPAIR_LENGTH,
            });
        }

        // `Keypair::try_from(&[u8])` enforces that the trailing 32 bytes
        // (the embedded public key) actually match the public key derived
        // from the leading 32-byte secret. That guards against subtly
        // corrupted keypairs that happen to be 64 bytes long.
        let keypair =
            Keypair::try_from(bytes.as_slice()).map_err(|e| SellerSignerError::InvalidKeypair {
                reason: e.to_string(),
            })?;

        Ok(Self {
            keypair: Arc::new(keypair),
        })
    }

    /// Public key of the seller signer.
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    /// Sign a pre-serialized transaction message with the seller key.
    pub fn sign_message(&self, message: &[u8]) -> Signature {
        self.keypair.sign_message(message)
    }

    /// Shared handle to the underlying [`Keypair`].
    ///
    /// The same `Arc<Keypair>` is reused for both the SPL `TransferChecked`
    /// and the `SubmitDelivery` transactions in a single Buy endpoint
    /// request — see the module-level docs.
    pub fn keypair(&self) -> Arc<Keypair> {
        Arc::clone(&self.keypair)
    }
}

impl fmt::Debug for SellerSigner {
    /// Never print secret material, even in debug logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SellerSigner")
            .field("pubkey", &self.pubkey())
            .finish()
    }
}

/// Fail-fast errors for seller signer cold-start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellerSignerError {
    /// `SELLER_KEYPAIR_BASE58` is unset, empty, or whitespace-only.
    MissingEnv,
    /// `SELLER_KEYPAIR_BASE58` is set but is not valid base58.
    InvalidBase58 { reason: String },
    /// Decoded byte length is not 64.
    InvalidLength { actual: usize, expected: usize },
    /// Bytes are 64 long but do not form a valid Solana keypair (e.g.
    /// the embedded public key does not match the secret).
    InvalidKeypair { reason: String },
}

impl fmt::Display for SellerSignerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv => write!(
                f,
                "{} is not set or is empty",
                SELLER_KEYPAIR_BASE58
            ),
            Self::InvalidBase58 { reason } => write!(
                f,
                "{} is not valid base58: {}",
                SELLER_KEYPAIR_BASE58, reason
            ),
            Self::InvalidLength { actual, expected } => write!(
                f,
                "{} decodes to {} bytes; expected exactly {} (Solana keypair = 32-byte secret || 32-byte public key)",
                SELLER_KEYPAIR_BASE58, actual, expected
            ),
            Self::InvalidKeypair { reason } => write!(
                f,
                "{} bytes are 64 long but do not form a valid Solana keypair: {}",
                SELLER_KEYPAIR_BASE58, reason
            ),
        }
    }
}

impl std::error::Error for SellerSignerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signer::Signer;

    /// Helper: produce a fresh ephemeral keypair and base58-encode its
    /// 64-byte (secret || public) layout. Mirrors `Keypair::to_base58_string`
    /// without depending on its precise behavior.
    fn fresh_base58_keypair() -> (Keypair, String) {
        let kp = Keypair::new();
        let encoded = bs58::encode(kp.to_bytes()).into_string();
        (kp, encoded)
    }

    #[test]
    fn happy_path_loads_valid_base58_keypair() {
        let (kp, encoded) = fresh_base58_keypair();
        let signer = SellerSigner::from_base58(&encoded).expect("valid keypair should load");
        // Pubkey round-trips.
        assert_eq!(signer.pubkey(), kp.pubkey());
        // Same Arc backs `keypair()`; pubkey still matches after clone.
        let arc = signer.keypair();
        assert_eq!(arc.pubkey(), kp.pubkey());
    }

    #[test]
    fn sign_message_matches_underlying_keypair() {
        let (kp, encoded) = fresh_base58_keypair();
        let signer = SellerSigner::from_base58(&encoded).unwrap();
        let msg = b"x402-buy-spl-token canonical message";
        let sig = signer.sign_message(msg);
        // Solana ed25519 signatures verify against the signer's pubkey.
        assert!(sig.verify(kp.pubkey().as_ref(), msg));
    }

    #[test]
    fn missing_env_aborts() {
        // Empty / whitespace strings simulate "env var unset" without
        // mutating the actual process environment (which would race with
        // other parallel tests).
        let err = SellerSigner::from_base58("").unwrap_err();
        assert!(matches!(err, SellerSignerError::MissingEnv));

        let err = SellerSigner::from_base58("   ").unwrap_err();
        assert!(matches!(err, SellerSignerError::MissingEnv));
    }

    #[test]
    fn from_env_returns_missing_when_unset() {
        // Use a key that no sane process would set; the function must
        // surface MissingEnv rather than panicking.
        let saved = env::var(SELLER_KEYPAIR_BASE58).ok();
        env::remove_var(SELLER_KEYPAIR_BASE58);

        let err = SellerSigner::from_env().unwrap_err();
        assert!(matches!(err, SellerSignerError::MissingEnv));

        if let Some(v) = saved {
            env::set_var(SELLER_KEYPAIR_BASE58, v);
        }
    }

    #[test]
    fn malformed_base58_aborts() {
        // '0', 'O', 'I', 'l' are excluded from the base58 alphabet.
        let err = SellerSigner::from_base58("not-valid-base58-0OIl").unwrap_err();
        assert!(
            matches!(err, SellerSignerError::InvalidBase58 { .. }),
            "expected InvalidBase58, got {:?}",
            err
        );
    }

    #[test]
    fn wrong_length_bytes_abort_short() {
        // 32 random bytes (a plausible-looking secret-only payload) is
        // *not* the 64-byte keypair format we accept.
        let short = bs58::encode([7u8; 32]).into_string();
        let err = SellerSigner::from_base58(&short).unwrap_err();
        match err {
            SellerSignerError::InvalidLength { actual, expected } => {
                assert_eq!(actual, 32);
                assert_eq!(expected, 64);
            }
            other => panic!("expected InvalidLength, got {:?}", other),
        }
    }

    #[test]
    fn wrong_length_bytes_abort_long() {
        let long = bs58::encode([7u8; 65]).into_string();
        let err = SellerSigner::from_base58(&long).unwrap_err();
        assert!(matches!(
            err,
            SellerSignerError::InvalidLength {
                actual: 65,
                expected: 64
            }
        ));
    }

    #[test]
    fn corrupt_64_bytes_abort_as_invalid_keypair() {
        // Exactly 64 bytes, but the trailing 32 bytes do not match the
        // public key derived from the leading 32 bytes — `Keypair::try_from`
        // catches this.
        let mut bytes = Keypair::new().to_bytes();
        // Flip the last byte of the embedded public-key half.
        bytes[63] ^= 0xff;
        let encoded = bs58::encode(bytes).into_string();
        let err = SellerSigner::from_base58(&encoded).unwrap_err();
        assert!(
            matches!(err, SellerSignerError::InvalidKeypair { .. }),
            "expected InvalidKeypair, got {:?}",
            err
        );
    }

    #[test]
    fn debug_does_not_leak_secret() {
        let (_kp, encoded) = fresh_base58_keypair();
        let signer = SellerSigner::from_base58(&encoded).unwrap();
        let dbg = format!("{:?}", signer);
        // Must mention the pubkey for operability, must not embed the
        // base58 string we just decoded.
        assert!(dbg.contains("pubkey"));
        assert!(!dbg.contains(&encoded));
    }

    #[test]
    fn error_messages_name_offending_env_var() {
        let err = SellerSignerError::MissingEnv.to_string();
        assert!(err.contains(SELLER_KEYPAIR_BASE58));

        let err = SellerSignerError::InvalidLength {
            actual: 32,
            expected: 64,
        }
        .to_string();
        assert!(err.contains(SELLER_KEYPAIR_BASE58));
        assert!(err.contains("32"));
        assert!(err.contains("64"));
    }
}
