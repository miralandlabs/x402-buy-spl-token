//! SLA builder: canonical TransferSla JSON and `sla_hash` for the Buy endpoint.
//!
//! The Buy SPL Token endpoint commits a TransferSla document on the unpaid
//! 402 path: it constructs the SLA from the request's query parameters plus
//! catalog metadata, hashes the canonical JSON bytes with SHA-256, and
//! returns that hash in `accepts[].extra.slaHash`. The buyer then signs a
//! `FundPayment` against the same `sla_hash` and re-issues the request.
//!
//! For the buyer, seller, and oracle to all agree on what was committed,
//! the bytes hashed must be **byte-identical** across implementations.
//! `serde_json::to_vec` is *not* by itself byte-stable — JSON object key
//! ordering depends on the serializer's internal map type and on how the
//! caller built the value. We therefore canonicalize explicitly:
//!
//! 1. Build a [`serde_json::Value`] tree with the SLA fields.
//! 2. Recursively serialize that tree with object keys sorted lexicographically
//!    and no extra whitespace ([`canonicalize_to_bytes`]).
//! 3. Compute `sla_hash = SHA256(canonical_bytes)`.
//!
//! The canonical bytes are exactly the bytes uploaded to the evidence
//! registry (see Requirement 3.5 / 7.5) and exactly the bytes hashed.
//!
//! # SLA shape
//!
//! The shape mirrors the `oracle-onchain-transfer` profile so a downstream
//! oracle can verify delivery without per-seller code:
//!
//! ```jsonc
//! {
//!   "buyer_nonce": "<64 lower-hex>",
//!   "deadline_unix": 1735603200,    // optional
//!   "expected_transfers": [
//!     {
//!       "decimals": 6,
//!       "direction": "in",
//!       "min_amount": "1000000",    // price_units, raw decimal string
//!       "mint": "<base58 mint>",
//!       "recipient_owner": "<base58 owner>",
//!       "sender_owner": "<base58 seller pubkey>"
//!     }
//!   ],
//!   "profile_id": "x402/oracles/onchain-transfer/v1",
//!   "version": 1
//! }
//! ```
//!
//! # Validation
//!
//! [`TransferSlaInputs::validate`] enforces:
//! - `recipient_owner` parses as a base58 [`Pubkey`].
//! - `buyer_nonce` is exactly 64 lowercase hex characters (`[0-9a-f]`).
//! - `mint` and `seller_pubkey` parse as base58 [`Pubkey`].
//! - All required fields are non-empty.

use {
    serde_json::{json, Value},
    sha2::{Digest, Sha256},
    solana_sdk::pubkey::Pubkey,
    std::{fmt, io::Write, str::FromStr},
};

/// Profile id every TransferSla we emit binds itself to. Matches the oracle's
/// `oracle-onchain-transfer/v1` constant so the same registry path / oracle
/// can verify deliveries.
pub const PROFILE_ID: &str = "x402/oracles/onchain-transfer/v1";

/// Current TransferSla schema version.
pub const SLA_VERSION: u32 = 1;

/// Required hex length of `buyer_nonce` (32 bytes encoded as 64 lower-hex chars).
pub const BUYER_NONCE_HEX_LEN: usize = 64;

/// Inputs to the SLA builder, gathered from the Buy endpoint's query params
/// and catalog lookup.
///
/// All string fields hold the *exact* operator/buyer input. Validation
/// happens in [`Self::validate`] and again implicitly during [`build`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferSlaInputs {
    /// Base58 SPL mint pubkey of the purchased token.
    pub mint: String,
    /// Configured decimals for `mint`. Echoed into the SLA so downstream
    /// consumers can interpret `min_amount` without a separate RPC call.
    pub decimals: u8,
    /// Raw token amount the seller MUST deliver (`price_usdc_ui * 10^decimals`).
    /// Encoded into the SLA as a decimal string under `min_amount`.
    pub price_units: u64,
    /// Base58 pubkey of the destination ATA's owner (the buyer's wallet).
    pub recipient_owner: String,
    /// Buyer-supplied 32-byte nonce, hex-encoded as 64 lowercase chars.
    /// Embedded verbatim in the SLA per Requirement 3.3.
    pub buyer_nonce: String,
    /// On-chain `Payment.payment_uid` hex-encoded as 64 lowercase chars.
    /// REQUIRED by the `x402/oracles/onchain-transfer/v1` profile so the
    /// oracle's `TransferSla` parser can bind the SLA to a single payment.
    /// On the unpaid 402 path the seller does not know this value yet —
    /// the buyer chooses `payment_uid` upfront, signs FundPayment with
    /// it, and the seller reconstructs the same SLA on the paid path
    /// from the bytes pulled out of the FundPayment instruction (see
    /// [`crate::api::buy_handlers`]).
    pub payment_uid: String,
    /// Solana cluster name in kebab-case (`devnet`, `mainnet-beta`,
    /// `testnet`). Mirrors the oracle's `TransferCluster` serde rename
    /// so the document round-trips byte-identically.
    pub cluster: String,
    /// Base58 seller pubkey. Pinned into the SLA as `sender_owner` so the
    /// oracle can defense-in-depth check the source wallet on delivery.
    pub seller_pubkey: String,
    /// Optional Unix-epoch-seconds expiry. Omitted from the canonical JSON
    /// when `None`.
    pub deadline_unix: Option<i64>,
    /// SLA schema version. Use [`SLA_VERSION`] for new SLAs.
    pub version: u32,
}

/// Result of building the canonical SLA: the byte-exact JSON over which the
/// hash was computed and the resulting 32-byte SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltSla {
    /// Byte-exact UTF-8 JSON serialization of the SLA. These bytes — and
    /// only these bytes — are what `sla_hash` covers, and what
    /// `Registry_Client::upload_sla` uploads (Requirement 3.5 / 7.5).
    pub canonical_json: Vec<u8>,
    /// SHA-256 of `canonical_json`.
    pub sla_hash: [u8; 32],
}

impl BuiltSla {
    /// Lowercase hex encoding of [`Self::sla_hash`], suitable for the 402
    /// `accepts[].extra.slaHash` field.
    pub fn sla_hash_hex(&self) -> String {
        hex::encode(self.sla_hash)
    }

    /// View of [`Self::canonical_json`] as a `&str`. Always valid UTF-8
    /// because the canonicalizer only emits ASCII syntax + the JSON-escaped
    /// forms of the input strings.
    pub fn canonical_json_str(&self) -> &str {
        // SAFETY: canonical_json is produced by serde_json's string escaping
        // path plus ASCII syntax characters; both paths produce valid UTF-8.
        std::str::from_utf8(&self.canonical_json).expect("canonical SLA JSON is UTF-8")
    }
}

/// Errors surfaced by the SLA builder. All variants identify the offending
/// field so the Buy endpoint can return a 400 that names the bad parameter
/// (Requirements 3.10 / 3.11 / 3.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlaBuilderError {
    /// A required input field was empty or absent.
    MissingField { field: &'static str },
    /// `recipient_owner` (or `mint` / `seller_pubkey`) is not a valid base58
    /// [`Pubkey`].
    InvalidPubkey {
        field: &'static str,
        value: String,
        reason: String,
    },
    /// `buyer_nonce` failed the 64-lowercase-hex contract.
    InvalidBuyerNonce { value: String, reason: String },
    /// `payment_uid` failed the 64-lowercase-hex contract.
    InvalidPaymentUid { value: String, reason: String },
    /// `cluster` is not one of the supported kebab-case names.
    InvalidCluster { value: String },
    /// `version` is not in the supported set (currently `{1}`).
    UnsupportedVersion { version: u32 },
}

impl fmt::Display for SlaBuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField { field } => {
                write!(f, "required SLA field {:?} is missing or empty", field)
            }
            Self::InvalidPubkey {
                field,
                value,
                reason,
            } => write!(
                f,
                "SLA field {:?} value {:?} is not a valid base58 pubkey ({})",
                field, value, reason
            ),
            Self::InvalidBuyerNonce { value, reason } => write!(
                f,
                "buyer_nonce {:?} is not exactly 64 lowercase hex chars ({})",
                value, reason
            ),
            Self::InvalidPaymentUid { value, reason } => write!(
                f,
                "payment_uid {:?} is not exactly 64 lowercase hex chars ({})",
                value, reason
            ),
            Self::InvalidCluster { value } => write!(
                f,
                "cluster {:?} is not one of {{devnet, mainnet-beta, testnet}}",
                value
            ),
            Self::UnsupportedVersion { version } => {
                write!(f, "TransferSla version {} is not supported", version)
            }
        }
    }
}

impl std::error::Error for SlaBuilderError {}

impl TransferSlaInputs {
    /// Validate inputs without serializing. Pure; cheap to call before
    /// committing to the canonicalization step.
    pub fn validate(&self) -> Result<(), SlaBuilderError> {
        // 1. Required fields non-empty.
        if self.mint.is_empty() {
            return Err(SlaBuilderError::MissingField { field: "mint" });
        }
        if self.recipient_owner.is_empty() {
            return Err(SlaBuilderError::MissingField {
                field: "recipient_owner",
            });
        }
        if self.buyer_nonce.is_empty() {
            return Err(SlaBuilderError::MissingField {
                field: "buyer_nonce",
            });
        }
        if self.seller_pubkey.is_empty() {
            return Err(SlaBuilderError::MissingField {
                field: "seller_pubkey",
            });
        }
        if self.payment_uid.is_empty() {
            return Err(SlaBuilderError::MissingField {
                field: "payment_uid",
            });
        }
        if self.cluster.is_empty() {
            return Err(SlaBuilderError::MissingField { field: "cluster" });
        }

        // 2. Pubkey fields parse as base58.
        parse_pubkey_field("mint", &self.mint)?;
        parse_pubkey_field("recipient_owner", &self.recipient_owner)?;
        parse_pubkey_field("seller_pubkey", &self.seller_pubkey)?;

        // 3. buyer_nonce + payment_uid: exactly 64 lowercase hex chars.
        validate_buyer_nonce(&self.buyer_nonce)?;
        validate_payment_uid(&self.payment_uid)?;

        // 4. cluster: kebab-case in the oracle's accepted set.
        validate_cluster(&self.cluster)?;

        // 5. Version is in the supported set.
        if self.version != SLA_VERSION {
            return Err(SlaBuilderError::UnsupportedVersion {
                version: self.version,
            });
        }

        Ok(())
    }

    /// Build the canonical SLA value tree (without serializing). Exposed
    /// separately so tests can probe the structure before canonicalization.
    pub fn to_value(&self) -> Value {
        let mut transfer = serde_json::Map::new();
        transfer.insert("mint".into(), Value::String(self.mint.clone()));
        transfer.insert(
            "recipient_owner".into(),
            Value::String(self.recipient_owner.clone()),
        );
        transfer.insert(
            "min_amount".into(),
            Value::String(self.price_units.to_string()),
        );
        transfer.insert("direction".into(), Value::String("in".into()));
        transfer.insert(
            "sender_owner".into(),
            Value::String(self.seller_pubkey.clone()),
        );
        transfer.insert("decimals".into(), Value::from(self.decimals));

        let mut sla = serde_json::Map::new();
        sla.insert("version".into(), Value::from(self.version));
        sla.insert("profile_id".into(), Value::String(PROFILE_ID.into()));
        sla.insert(
            "payment_uid".into(),
            Value::String(self.payment_uid.clone()),
        );
        sla.insert(
            "buyer_nonce".into(),
            Value::String(self.buyer_nonce.clone()),
        );
        sla.insert("cluster".into(), Value::String(self.cluster.clone()));
        sla.insert(
            "expected_transfers".into(),
            Value::Array(vec![Value::Object(transfer)]),
        );
        if let Some(deadline) = self.deadline_unix {
            sla.insert("deadline_unix".into(), Value::from(deadline));
        }

        Value::Object(sla)
    }
}

/// Build the canonical SLA bytes and SHA-256 digest from validated inputs.
///
/// On success the returned [`BuiltSla::canonical_json`] are exactly the
/// bytes hashed by `Sla_Hash` and exactly the bytes that should be uploaded
/// to the evidence registry.
pub fn build(inputs: &TransferSlaInputs) -> Result<BuiltSla, SlaBuilderError> {
    inputs.validate()?;
    let value = inputs.to_value();
    let canonical_json = canonicalize_to_bytes(&value);
    let sla_hash: [u8; 32] = Sha256::digest(&canonical_json).into();
    Ok(BuiltSla {
        canonical_json,
        sla_hash,
    })
}

/// Recursively serialize a [`Value`] into a JSON byte string with sorted
/// object keys and no extra whitespace.
///
/// This is intentionally independent of `serde_json::Map`'s internal
/// ordering: even if a downstream feature flag enables `preserve_order`, our
/// canonical output stays sorted because we explicitly collect-and-sort keys
/// at every object boundary.
pub fn canonicalize_to_bytes(value: &Value) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(256);
    write_canonical(value, &mut out).expect("writing to Vec<u8> is infallible");
    out
}

fn write_canonical<W: Write>(value: &Value, w: &mut W) -> std::io::Result<()> {
    match value {
        Value::Null => w.write_all(b"null"),
        Value::Bool(true) => w.write_all(b"true"),
        Value::Bool(false) => w.write_all(b"false"),
        Value::Number(n) => {
            // Numbers are already canonical from the input integer / float;
            // we don't normalize floats further (none appear in our SLAs).
            w.write_all(n.to_string().as_bytes())
        }
        Value::String(s) => {
            // Reuse serde_json's RFC-8259 string escaping rather than rolling
            // our own — that's the part of canonical JSON that is genuinely
            // tricky (control chars, surrogate pairs, etc.).
            let escaped = serde_json::to_string(s).expect("string serialization");
            w.write_all(escaped.as_bytes())
        }
        Value::Array(items) => {
            w.write_all(b"[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    w.write_all(b",")?;
                }
                write_canonical(item, w)?;
            }
            w.write_all(b"]")
        }
        Value::Object(map) => {
            w.write_all(b"{")?;
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    w.write_all(b",")?;
                }
                let key_escaped = serde_json::to_string(k).expect("key serialization");
                w.write_all(key_escaped.as_bytes())?;
                w.write_all(b":")?;
                write_canonical(map.get(*k).expect("present key"), w)?;
            }
            w.write_all(b"}")
        }
    }
}

// --- helpers ---

fn parse_pubkey_field(field: &'static str, value: &str) -> Result<Pubkey, SlaBuilderError> {
    Pubkey::from_str(value).map_err(|e| SlaBuilderError::InvalidPubkey {
        field,
        value: value.to_string(),
        reason: e.to_string(),
    })
}

fn validate_buyer_nonce(value: &str) -> Result<(), SlaBuilderError> {
    if value.len() != BUYER_NONCE_HEX_LEN {
        return Err(SlaBuilderError::InvalidBuyerNonce {
            value: value.to_string(),
            reason: format!(
                "length {} != {} (32 bytes hex-encoded)",
                value.len(),
                BUYER_NONCE_HEX_LEN
            ),
        });
    }
    for (i, ch) in value.chars().enumerate() {
        let is_lower_hex = matches!(ch, '0'..='9' | 'a'..='f');
        if !is_lower_hex {
            return Err(SlaBuilderError::InvalidBuyerNonce {
                value: value.to_string(),
                reason: format!("non-lowercase-hex character {:?} at index {}", ch, i),
            });
        }
    }
    Ok(())
}

/// Same shape contract as `buyer_nonce` (64 lowercase hex chars / 32
/// bytes). Distinct error variant so a misconfigured `payment_uid` is
/// distinguishable in logs.
fn validate_payment_uid(value: &str) -> Result<(), SlaBuilderError> {
    if value.len() != BUYER_NONCE_HEX_LEN {
        return Err(SlaBuilderError::InvalidPaymentUid {
            value: value.to_string(),
            reason: format!(
                "length {} != {} (32 bytes hex-encoded)",
                value.len(),
                BUYER_NONCE_HEX_LEN
            ),
        });
    }
    for (i, ch) in value.chars().enumerate() {
        if !matches!(ch, '0'..='9' | 'a'..='f') {
            return Err(SlaBuilderError::InvalidPaymentUid {
                value: value.to_string(),
                reason: format!("non-lowercase-hex character {:?} at index {}", ch, i),
            });
        }
    }
    Ok(())
}

/// Cluster names mirror the oracle's `TransferCluster` serde rename
/// (`#[serde(rename_all = "kebab-case")]` over `MainnetBeta`,
/// `Devnet`, `Testnet`).
fn validate_cluster(value: &str) -> Result<(), SlaBuilderError> {
    match value {
        "devnet" | "mainnet-beta" | "testnet" => Ok(()),
        _ => Err(SlaBuilderError::InvalidCluster {
            value: value.to_string(),
        }),
    }
}

#[allow(dead_code)] // reserved for future callers; keeps the JSON-builder helper visible
fn _example_marker() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two real Solana pubkeys we can reuse without hitting RPC.
    const VALID_MINT: &str = "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP";
    const VALID_RECIPIENT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
    const VALID_SELLER: &str = "11111111111111111111111111111111";

    fn sample_inputs() -> TransferSlaInputs {
        TransferSlaInputs {
            mint: VALID_MINT.to_string(),
            decimals: 6,
            price_units: 1_000_000,
            recipient_owner: VALID_RECIPIENT.to_string(),
            // 64 lowercase hex chars (`[0-9a-f]`) — the buyer's 32-byte nonce.
            buyer_nonce: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
            payment_uid: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            cluster: "devnet".to_string(),
            seller_pubkey: VALID_SELLER.to_string(),
            deadline_unix: None,
            version: SLA_VERSION,
        }
    }

    // ---- happy path & determinism ----

    #[test]
    fn build_succeeds_on_valid_inputs() {
        let built = build(&sample_inputs()).expect("valid inputs should build");
        // canonical JSON must round-trip as JSON and contain the buyer_nonce verbatim.
        let parsed: Value = serde_json::from_slice(&built.canonical_json).unwrap();
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["profile_id"], PROFILE_ID);
        assert_eq!(
            parsed["buyer_nonce"],
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        assert_eq!(parsed["expected_transfers"][0]["mint"], VALID_MINT);
        assert_eq!(
            parsed["expected_transfers"][0]["recipient_owner"],
            VALID_RECIPIENT
        );
        assert_eq!(parsed["expected_transfers"][0]["min_amount"], "1000000");
        assert_eq!(parsed["expected_transfers"][0]["direction"], "in");
        assert_eq!(
            parsed["expected_transfers"][0]["sender_owner"],
            VALID_SELLER
        );

        // sla_hash_hex is 64 lowercase hex chars.
        let hex = built.sla_hash_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn same_inputs_produce_byte_identical_json_and_hash() {
        // Build twice from identical inputs; bytes must match exactly.
        let a = build(&sample_inputs()).unwrap();
        let b = build(&sample_inputs()).unwrap();
        assert_eq!(a.canonical_json, b.canonical_json);
        assert_eq!(a.sla_hash, b.sla_hash);
    }

    #[test]
    fn key_reordering_in_value_does_not_change_canonical_bytes() {
        // Same logical SLA, but build the inner objects with deliberately
        // different *insertion* orders. The canonicalizer must collapse both
        // into the same byte sequence.
        let inputs = sample_inputs();

        // Path A: build via TransferSlaInputs::to_value (insertion order set
        // by our struct).
        let value_a = inputs.to_value();
        let bytes_a = canonicalize_to_bytes(&value_a);

        // Path B: build the same logical SLA by hand, inserting keys in
        // **reverse** alphabetical order.
        let mut transfer_b = serde_json::Map::new();
        transfer_b.insert(
            "sender_owner".into(),
            Value::String(inputs.seller_pubkey.clone()),
        );
        transfer_b.insert(
            "recipient_owner".into(),
            Value::String(inputs.recipient_owner.clone()),
        );
        transfer_b.insert("mint".into(), Value::String(inputs.mint.clone()));
        transfer_b.insert(
            "min_amount".into(),
            Value::String(inputs.price_units.to_string()),
        );
        transfer_b.insert("direction".into(), Value::String("in".into()));
        transfer_b.insert("decimals".into(), Value::from(inputs.decimals));

        let mut sla_b = serde_json::Map::new();
        sla_b.insert(
            "expected_transfers".into(),
            Value::Array(vec![Value::Object(transfer_b)]),
        );
        sla_b.insert(
            "payment_uid".into(),
            Value::String(inputs.payment_uid.clone()),
        );
        sla_b.insert("cluster".into(), Value::String(inputs.cluster.clone()));
        sla_b.insert(
            "buyer_nonce".into(),
            Value::String(inputs.buyer_nonce.clone()),
        );
        sla_b.insert("profile_id".into(), Value::String(PROFILE_ID.into()));
        sla_b.insert("version".into(), Value::from(inputs.version));

        let value_b = Value::Object(sla_b);
        let bytes_b = canonicalize_to_bytes(&value_b);

        assert_eq!(
            bytes_a, bytes_b,
            "canonical JSON must be invariant under input key order"
        );
        let hash_a: [u8; 32] = Sha256::digest(&bytes_a).into();
        let hash_b: [u8; 32] = Sha256::digest(&bytes_b).into();
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn deadline_is_omitted_when_none_and_present_when_set() {
        let mut inputs = sample_inputs();
        inputs.deadline_unix = None;
        let without = build(&inputs).unwrap();
        assert!(
            !without.canonical_json_str().contains("deadline_unix"),
            "None deadline should not appear in canonical JSON; got {}",
            without.canonical_json_str()
        );

        inputs.deadline_unix = Some(1_735_603_200);
        let with = build(&inputs).unwrap();
        assert!(
            with.canonical_json_str()
                .contains("\"deadline_unix\":1735603200"),
            "expected deadline_unix in canonical JSON, got {}",
            with.canonical_json_str()
        );
        // Different shape ⇒ different hash.
        assert_ne!(without.sla_hash, with.sla_hash);
    }

    #[test]
    fn changing_buyer_nonce_changes_hash() {
        let mut inputs = sample_inputs();
        let a = build(&inputs).unwrap();
        // flip last hex char
        inputs.buyer_nonce =
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef012345678a".into();
        let b = build(&inputs).unwrap();
        assert_ne!(a.sla_hash, b.sla_hash);
        assert_ne!(a.canonical_json, b.canonical_json);
    }

    #[test]
    fn canonical_keys_are_sorted_and_compact() {
        let built = build(&sample_inputs()).unwrap();
        let s = built.canonical_json_str();
        // Compact: no spaces between separators.
        assert!(!s.contains(": "));
        assert!(!s.contains(", "));
        // Top-level keys are emitted in sorted order. Find their positions.
        let buyer_nonce_pos = s.find("\"buyer_nonce\"").unwrap();
        let expected_transfers_pos = s.find("\"expected_transfers\"").unwrap();
        let profile_id_pos = s.find("\"profile_id\"").unwrap();
        let version_pos = s.find("\"version\"").unwrap();
        assert!(buyer_nonce_pos < expected_transfers_pos);
        assert!(expected_transfers_pos < profile_id_pos);
        assert!(profile_id_pos < version_pos);
    }

    // ---- typed validation errors ----

    #[test]
    fn invalid_recipient_owner_surfaces_typed_error() {
        let mut inputs = sample_inputs();
        inputs.recipient_owner = "not-a-real-pubkey".into();
        let err = build(&inputs).unwrap_err();
        match err {
            SlaBuilderError::InvalidPubkey { field, value, .. } => {
                assert_eq!(field, "recipient_owner");
                assert_eq!(value, "not-a-real-pubkey");
            }
            other => panic!("expected InvalidPubkey, got {:?}", other),
        }
    }

    #[test]
    fn invalid_mint_surfaces_typed_error() {
        let mut inputs = sample_inputs();
        inputs.mint = "🚀".into();
        let err = build(&inputs).unwrap_err();
        assert!(matches!(
            err,
            SlaBuilderError::InvalidPubkey { field: "mint", .. }
        ));
    }

    #[test]
    fn invalid_seller_pubkey_surfaces_typed_error() {
        let mut inputs = sample_inputs();
        inputs.seller_pubkey = "0OIl".into(); // base58-illegal characters
        let err = build(&inputs).unwrap_err();
        assert!(matches!(
            err,
            SlaBuilderError::InvalidPubkey {
                field: "seller_pubkey",
                ..
            }
        ));
    }

    #[test]
    fn buyer_nonce_wrong_length_rejected() {
        let mut inputs = sample_inputs();
        inputs.buyer_nonce = "abc".into();
        let err = build(&inputs).unwrap_err();
        match err {
            SlaBuilderError::InvalidBuyerNonce { reason, .. } => {
                assert!(reason.contains("length"), "reason={}", reason);
            }
            other => panic!("expected InvalidBuyerNonce(length), got {:?}", other),
        }

        // 65 chars also rejected.
        inputs.buyer_nonce =
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567890".into();
        let err = build(&inputs).unwrap_err();
        assert!(matches!(err, SlaBuilderError::InvalidBuyerNonce { .. }));
    }

    #[test]
    fn buyer_nonce_uppercase_rejected() {
        let mut inputs = sample_inputs();
        // 64 chars but contains an uppercase 'A'.
        inputs.buyer_nonce =
            "Abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into();
        let err = build(&inputs).unwrap_err();
        match err {
            SlaBuilderError::InvalidBuyerNonce { reason, .. } => {
                assert!(reason.contains("non-lowercase-hex"), "reason={}", reason);
            }
            other => panic!(
                "expected InvalidBuyerNonce(non-lowercase-hex), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn buyer_nonce_non_hex_rejected() {
        let mut inputs = sample_inputs();
        // 64 chars but contains 'g'.
        inputs.buyer_nonce =
            "gbcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into();
        let err = build(&inputs).unwrap_err();
        assert!(matches!(err, SlaBuilderError::InvalidBuyerNonce { .. }));
    }

    #[test]
    fn missing_required_fields_rejected() {
        // Empty mint
        let mut inputs = sample_inputs();
        inputs.mint = "".into();
        let err = build(&inputs).unwrap_err();
        assert!(matches!(
            err,
            SlaBuilderError::MissingField { field: "mint" }
        ));

        // Empty recipient_owner
        let mut inputs = sample_inputs();
        inputs.recipient_owner = "".into();
        let err = build(&inputs).unwrap_err();
        assert!(matches!(
            err,
            SlaBuilderError::MissingField {
                field: "recipient_owner"
            }
        ));

        // Empty buyer_nonce
        let mut inputs = sample_inputs();
        inputs.buyer_nonce = "".into();
        let err = build(&inputs).unwrap_err();
        assert!(matches!(
            err,
            SlaBuilderError::MissingField {
                field: "buyer_nonce"
            }
        ));

        // Empty seller_pubkey
        let mut inputs = sample_inputs();
        inputs.seller_pubkey = "".into();
        let err = build(&inputs).unwrap_err();
        assert!(matches!(
            err,
            SlaBuilderError::MissingField {
                field: "seller_pubkey"
            }
        ));
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut inputs = sample_inputs();
        inputs.version = 99;
        let err = build(&inputs).unwrap_err();
        assert!(matches!(
            err,
            SlaBuilderError::UnsupportedVersion { version: 99 }
        ));
    }

    // ---- canonicalizer unit coverage ----

    #[test]
    fn canonicalize_sorts_object_keys() {
        let v = json!({ "b": 1, "a": 2, "c": 3 });
        let bytes = canonicalize_to_bytes(&v);
        assert_eq!(bytes, br#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn canonicalize_handles_nested_arrays_and_objects() {
        let v = json!({
            "z": [{"y": 1, "x": 2}, {"a": [3, 2, 1]}],
            "m": {"b": "foo", "a": "bar"}
        });
        let bytes = canonicalize_to_bytes(&v);
        let s = std::str::from_utf8(&bytes).unwrap();
        // Outer keys sorted.
        assert!(s.starts_with(r#"{"m":"#));
        // Inner object inside m sorted.
        assert!(s.contains(r#""m":{"a":"bar","b":"foo"}"#));
        // Array order is preserved (not sorted).
        assert!(s.contains(r#""a":[3,2,1]"#));
    }

    #[test]
    fn canonicalize_escapes_strings_via_serde_json() {
        let v = json!({ "k": "line\nbreak\"quote" });
        let bytes = canonicalize_to_bytes(&v);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(s, r#"{"k":"line\nbreak\"quote"}"#);
    }

    #[test]
    fn canonicalize_handles_null_and_booleans() {
        let v = json!({ "a": null, "b": true, "c": false });
        let bytes = canonicalize_to_bytes(&v);
        assert_eq!(bytes, br#"{"a":null,"b":true,"c":false}"#);
    }
}
