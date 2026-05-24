//! Delivery hot-key loaded once at cold start (`SELLER_KEYPAIR_BASE58`).
//!
//! The Buy SPL Token endpoint signs an SPL `TransferChecked` per paid
//! request with this key. `SubmitDelivery` is signed separately by the
//! merchant payout key ([`crate::merchant_signer::MerchantSigner`]).
//!
//! This module owns that initialization. [`SellerSigner::from_env`] decodes
//! `SELLER_KEYPAIR_BASE58` once, returns a [`SellerSigner`] backed by an
//! `Arc<Keypair>`, and exposes a thin API:
//!
//! - [`SellerSigner::pubkey`] — the seller's [`Pubkey`].
//! - [`SellerSigner::sign_message`] — produce a [`Signature`] over a
//!   pre-serialized transaction message.
//! - [`KeypairSigner::keypair`] — clone the underlying `Arc<Keypair>` for
//!   callers that need to pass it directly into Solana SDK builders.
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

/// Cold-start-loaded ed25519 keypair signer.
///
/// Cheap to clone (just clones an `Arc`); the secret bytes are decoded once
/// and never re-read.
#[derive(Clone)]
pub struct KeypairSigner {
    keypair: Arc<Keypair>,
}

/// Delivery hot-key signer (`SELLER_KEYPAIR_BASE58`).
pub type SellerSigner = KeypairSigner;

impl KeypairSigner {
    /// Load a [`Keypair`] from the named env var (exactly once at cold start).
    pub fn from_env_var(var: &'static str) -> Result<Self, KeypairLoadError> {
        let raw = env::var(var).map_err(|_| KeypairLoadError::MissingEnv { var })?;
        Self::from_base58(var, &raw)
    }

    /// Load the delivery hot key from [`SELLER_KEYPAIR_BASE58`].
    pub fn from_env() -> Result<SellerSigner, KeypairLoadError> {
        Self::from_env_var(SELLER_KEYPAIR_BASE58)
    }

    /// Decode a base58-encoded keypair string.
    pub fn from_base58(var: &'static str, s: &str) -> Result<Self, KeypairLoadError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(KeypairLoadError::MissingEnv { var });
        }

        let bytes =
            bs58::decode(trimmed)
                .into_vec()
                .map_err(|e| KeypairLoadError::InvalidBase58 {
                    var,
                    reason: e.to_string(),
                })?;

        if bytes.len() != KEYPAIR_LENGTH {
            return Err(KeypairLoadError::InvalidLength {
                var,
                actual: bytes.len(),
                expected: KEYPAIR_LENGTH,
            });
        }

        let keypair =
            Keypair::try_from(bytes.as_slice()).map_err(|e| KeypairLoadError::InvalidKeypair {
                var,
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
    pub fn keypair(&self) -> Arc<Keypair> {
        Arc::clone(&self.keypair)
    }
}

impl fmt::Debug for KeypairSigner {
    /// Never print secret material, even in debug logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SellerSigner")
            .field("pubkey", &self.pubkey())
            .finish()
    }
}

/// Fail-fast errors for cold-start keypair loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeypairLoadError {
    MissingEnv {
        var: &'static str,
    },
    InvalidBase58 {
        var: &'static str,
        reason: String,
    },
    InvalidLength {
        var: &'static str,
        actual: usize,
        expected: usize,
    },
    InvalidKeypair {
        var: &'static str,
        reason: String,
    },
}

/// Back-compat alias used by cold-start error wrapping.
pub type SellerSignerError = KeypairLoadError;

impl fmt::Display for KeypairLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv { var } => write!(f, "{var} is not set or is empty"),
            Self::InvalidBase58 { var, reason } => {
                write!(f, "{var} is not valid base58: {reason}")
            }
            Self::InvalidLength { var, actual, expected } => write!(
                f,
                "{var} decodes to {actual} bytes; expected exactly {expected} (Solana keypair = 32-byte secret || 32-byte public key)"
            ),
            Self::InvalidKeypair { var, reason } => write!(
                f,
                "{var} bytes are 64 long but do not form a valid Solana keypair: {reason}"
            ),
        }
    }
}

impl std::error::Error for KeypairLoadError {}

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
        let signer = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, &encoded)
            .expect("valid keypair should load");
        // Pubkey round-trips.
        assert_eq!(signer.pubkey(), kp.pubkey());
        // Same Arc backs `keypair()`; pubkey still matches after clone.
        let arc = signer.keypair();
        assert_eq!(arc.pubkey(), kp.pubkey());
    }

    #[test]
    fn sign_message_matches_underlying_keypair() {
        let (kp, encoded) = fresh_base58_keypair();
        let signer = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, &encoded).unwrap();
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
        let err = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, "").unwrap_err();
        assert!(matches!(
            err,
            KeypairLoadError::MissingEnv {
                var: SELLER_KEYPAIR_BASE58
            }
        ));

        let err = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, "   ").unwrap_err();
        assert!(matches!(
            err,
            KeypairLoadError::MissingEnv {
                var: SELLER_KEYPAIR_BASE58
            }
        ));
    }

    #[test]
    fn from_env_returns_missing_when_unset() {
        // Use a key that no sane process would set; the function must
        // surface MissingEnv rather than panicking.
        let saved = env::var(SELLER_KEYPAIR_BASE58).ok();
        env::remove_var(SELLER_KEYPAIR_BASE58);

        let err = SellerSigner::from_env().unwrap_err();
        assert!(matches!(
            err,
            KeypairLoadError::MissingEnv {
                var: SELLER_KEYPAIR_BASE58
            }
        ));

        if let Some(v) = saved {
            env::set_var(SELLER_KEYPAIR_BASE58, v);
        }
    }

    #[test]
    fn malformed_base58_aborts() {
        // '0', 'O', 'I', 'l' are excluded from the base58 alphabet.
        let err =
            SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, "not-valid-base58-0OIl").unwrap_err();
        assert!(
            matches!(err, KeypairLoadError::InvalidBase58 { .. }),
            "expected InvalidBase58, got {:?}",
            err
        );
    }

    #[test]
    fn wrong_length_bytes_abort_short() {
        // 32 random bytes (a plausible-looking secret-only payload) is
        // *not* the 64-byte keypair format we accept.
        let short = bs58::encode([7u8; 32]).into_string();
        let err = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, &short).unwrap_err();
        match err {
            KeypairLoadError::InvalidLength {
                actual, expected, ..
            } => {
                assert_eq!(actual, 32);
                assert_eq!(expected, 64);
            }
            other => panic!("expected InvalidLength, got {:?}", other),
        }
    }

    #[test]
    fn wrong_length_bytes_abort_long() {
        let long = bs58::encode([7u8; 65]).into_string();
        let err = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, &long).unwrap_err();
        assert!(matches!(
            err,
            KeypairLoadError::InvalidLength {
                actual: 65,
                expected: 64,
                ..
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
        let err = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, &encoded).unwrap_err();
        assert!(
            matches!(err, KeypairLoadError::InvalidKeypair { .. }),
            "expected InvalidKeypair, got {:?}",
            err
        );
    }

    #[test]
    fn debug_does_not_leak_secret() {
        let (_kp, encoded) = fresh_base58_keypair();
        let signer = SellerSigner::from_base58(SELLER_KEYPAIR_BASE58, &encoded).unwrap();
        let dbg = format!("{:?}", signer);
        // Must mention the pubkey for operability, must not embed the
        // base58 string we just decoded.
        assert!(dbg.contains("pubkey"));
        assert!(!dbg.contains(&encoded));
    }

    #[test]
    fn error_messages_name_offending_env_var() {
        let err = KeypairLoadError::MissingEnv {
            var: SELLER_KEYPAIR_BASE58,
        }
        .to_string();
        assert!(err.contains(SELLER_KEYPAIR_BASE58));

        let err = KeypairLoadError::InvalidLength {
            var: SELLER_KEYPAIR_BASE58,
            actual: 32,
            expected: 64,
        }
        .to_string();
        assert!(err.contains(SELLER_KEYPAIR_BASE58));
        assert!(err.contains("32"));
        assert!(err.contains("64"));
    }
}
