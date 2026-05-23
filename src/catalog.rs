//! Buy SPL Token catalog: parsing, validation, and on-chain decimals check.
//!
//! The catalog is a list of [`CatalogEntry`] items the Buy endpoint can sell.
//! It is sourced (in priority order) from a Postgres `parameters` row and the
//! `BUY_SPL_TOKEN_CATALOG_JSON` environment variable, then validated for
//! self-consistency before the service starts serving requests.
//!
//! # Validation steps
//!
//! 1. **Static (`parse_catalog_json`)**:
//!    - `mint` parses as a base58 [`Pubkey`].
//!    - `decimals` is in `[0, 18]`.
//!    - `price_usdc_ui` is a positive decimal whose fractional-digit count
//!      does not exceed `decimals`.
//! 2. **On-chain (`TokenCatalog::validate_against_chain`)**:
//!    - The on-chain `Mint` account at `mint` is fetched via the configured
//!      RPC under `RetryPolicy`, and its decimals byte (offset 44) must match
//!      the configured `decimals`.
//!
//! Each error variant of [`CatalogError`] carries enough context (notably the
//! offending entry's `name` and the `mint` it pertains to) for an operator to
//! locate the bad row in their config without grepping logs.
//!
//! # Why string-typed `price_usdc_ui`?
//!
//! Counting *fractional digits* requires preserving the literal the operator
//! wrote. A JSON literal like `0.50` round-trips through `f64` and is emitted
//! as `0.5`, silently dropping a fractional digit. We accept both string
//! (`"0.50"`) and JSON-number forms, but operators who care about
//! trailing-zero precision should pass strings.

use {
    crate::{
        db::ParametersDb,
        parameters,
        rpc_retry::{with_retry, RetryPolicy},
    },
    serde::{Deserialize, Deserializer, Serialize},
    solana_client::nonblocking::rpc_client::RpcClient,
    solana_sdk::pubkey::Pubkey,
    std::{fmt, str::FromStr, sync::Arc},
};

/// Postgres `parameters` key and process env var used to source the catalog.
pub const BUY_SPL_TOKEN_CATALOG_JSON: &str = "BUY_SPL_TOKEN_CATALOG_JSON";

/// One purchasable token entry. Field shape mirrors the JSON the operator
/// configures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Base58-encoded Solana mint pubkey.
    pub mint: String,
    /// Configured decimals for the mint. Validated against on-chain decimals
    /// at cold start.
    pub decimals: u8,
    /// Price expressed as USDC UI units (e.g. `"0.42"` for 0.42 USDC).
    /// Stored as a string to preserve the operator's exact fractional-digit
    /// shape; see module docs.
    #[serde(deserialize_with = "deserialize_decimal_str")]
    pub price_usdc_ui: String,
    /// Operator-facing display name for this token. Echoed back in
    /// validation error messages so a misconfigured row is easy to find.
    pub name: String,
    /// Optional pre-computed sender treasury ATA. When omitted, the buy
    /// endpoint derives the seller signer's ATA at runtime.
    #[serde(default)]
    pub sender_treasury_ata: Option<String>,
}

impl CatalogEntry {
    /// Parse the `mint` field as a [`Pubkey`]. Cheap, but allocates.
    pub fn mint_pubkey(&self) -> Result<Pubkey, CatalogError> {
        Pubkey::from_str(&self.mint).map_err(|e| CatalogError::InvalidMint {
            entry_name: self.name.clone(),
            mint: self.mint.clone(),
            reason: e.to_string(),
        })
    }
}

/// Parsed and statically-validated token catalog.
///
/// Construct via [`parse_catalog_json`] or [`load_from_strings`]; on-chain
/// validation lives on [`TokenCatalog::validate_against_chain`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenCatalog {
    entries: Vec<CatalogEntry>,
}

impl TokenCatalog {
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up an entry by base58-encoded mint string.
    pub fn find_by_mint(&self, mint: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.mint == mint)
    }

    /// Fetch each entry's on-chain Mint account under [`RetryPolicy`] and
    /// fail-fast if any decimals byte disagrees with the configured value.
    ///
    /// This is the cold-start gate that protects production from
    /// off-by-decimal raw-unit transfers.
    pub async fn validate_against_chain(
        &self,
        rpc: &Arc<RpcClient>,
        retry: RetryPolicy,
    ) -> Result<(), CatalogError> {
        for entry in &self.entries {
            let mint_pk = entry.mint_pubkey()?;
            let rpc_for_op = rpc.clone();
            let mint_for_op = mint_pk;
            let fetched = with_retry(retry, "get_account:catalog_mint", None, || {
                let rpc = rpc_for_op.clone();
                async move { rpc.get_account(&mint_for_op).await }
            })
            .await
            .map_err(|e| CatalogError::UnreachableMint {
                entry_name: entry.name.clone(),
                mint: entry.mint.clone(),
                reason: e.to_string(),
            })?;

            // SPL Token / Token-2022 base mint layouts both place the
            // decimals byte at offset 44 (mint_auth_opt(4) + mint_auth(32)
            // + supply(8) = 44). A shorter buffer means the account isn't
            // a Mint at all — surface as unreachable with a clear reason.
            if fetched.data.len() < 45 {
                return Err(CatalogError::UnreachableMint {
                    entry_name: entry.name.clone(),
                    mint: entry.mint.clone(),
                    reason: format!(
                        "account data length {} < 45 (not a Mint account)",
                        fetched.data.len()
                    ),
                });
            }
            let on_chain_decimals = fetched.data[44];
            if on_chain_decimals != entry.decimals {
                return Err(CatalogError::DecimalsMismatch {
                    entry_name: entry.name.clone(),
                    mint: entry.mint.clone(),
                    configured: entry.decimals,
                    on_chain: on_chain_decimals,
                });
            }
        }
        Ok(())
    }
}

/// Fail-fast errors for catalog configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// Neither `BUY_SPL_TOKEN_CATALOG_JSON` env var nor a Postgres
    /// `parameters` row override is present.
    MissingEnv,
    /// The catalog source string is not a valid JSON array of entries.
    ParseError { source: String },
    /// `mint` (or `sender_treasury_ata`) is not a valid base58 [`Pubkey`].
    InvalidMint {
        entry_name: String,
        mint: String,
        reason: String,
    },
    /// `decimals` is outside the inclusive range `[0, 18]`.
    DecimalsOutOfRange { entry_name: String, decimals: u8 },
    /// `price_usdc_ui` is not strictly greater than zero.
    NonPositivePrice { entry_name: String, price: String },
    /// `price_usdc_ui` could not be parsed as a decimal at all.
    InvalidPrice {
        entry_name: String,
        price: String,
        reason: String,
    },
    /// `price_usdc_ui` has more fractional digits than `decimals`.
    FractionalOverflow {
        entry_name: String,
        decimals: u8,
        fractional_digits: usize,
        price: String,
    },
    /// Configured `decimals` does not match on-chain mint decimals.
    DecimalsMismatch {
        entry_name: String,
        mint: String,
        configured: u8,
        on_chain: u8,
    },
    /// The on-chain Mint account could not be fetched after exhausting
    /// [`RetryPolicy`], or returned data shorter than a valid Mint layout.
    UnreachableMint {
        entry_name: String,
        mint: String,
        reason: String,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv => write!(
                f,
                "BUY_SPL_TOKEN_CATALOG_JSON not set and no Postgres parameters override present"
            ),
            Self::ParseError { source } => {
                write!(f, "failed to parse catalog JSON: {}", source)
            }
            Self::InvalidMint {
                entry_name,
                mint,
                reason,
            } => write!(
                f,
                "catalog entry {:?}: mint {:?} is not a valid base58 pubkey ({})",
                entry_name, mint, reason
            ),
            Self::DecimalsOutOfRange {
                entry_name,
                decimals,
            } => write!(
                f,
                "catalog entry {:?}: decimals {} is outside [0, 18]",
                entry_name, decimals
            ),
            Self::NonPositivePrice { entry_name, price } => write!(
                f,
                "catalog entry {:?}: price_usdc_ui {:?} must be a positive decimal",
                entry_name, price
            ),
            Self::InvalidPrice {
                entry_name,
                price,
                reason,
            } => write!(
                f,
                "catalog entry {:?}: price_usdc_ui {:?} is not a valid decimal ({})",
                entry_name, price, reason
            ),
            Self::FractionalOverflow {
                entry_name,
                decimals,
                fractional_digits,
                price,
            } => write!(
                f,
                "catalog entry {:?}: price_usdc_ui {:?} has {} fractional digits, exceeds decimals={}",
                entry_name, price, fractional_digits, decimals
            ),
            Self::DecimalsMismatch {
                entry_name,
                mint,
                configured,
                on_chain,
            } => write!(
                f,
                "catalog entry {:?}: configured decimals {} does not match on-chain mint {} decimals {}",
                entry_name, configured, mint, on_chain
            ),
            Self::UnreachableMint {
                entry_name,
                mint,
                reason,
            } => write!(
                f,
                "catalog entry {:?}: mint {} unreachable on-chain ({})",
                entry_name, mint, reason
            ),
        }
    }
}

impl std::error::Error for CatalogError {}

// --- Loaders ---

/// Parse a JSON array of [`CatalogEntry`] objects and statically validate
/// every entry. Pure; safe to call before any RPC is reachable.
pub fn parse_catalog_json(s: &str) -> Result<TokenCatalog, CatalogError> {
    let entries: Vec<CatalogEntry> =
        serde_json::from_str(s).map_err(|e| CatalogError::ParseError {
            source: e.to_string(),
        })?;
    for entry in &entries {
        validate_entry(entry)?;
    }
    Ok(TokenCatalog { entries })
}

/// Apply the source-priority rule (Postgres beats env), then parse.
///
/// Exposed so the override semantics can be unit-tested without a live DB.
pub fn load_from_strings(
    db_value: Option<&str>,
    env_value: Option<&str>,
) -> Result<TokenCatalog, CatalogError> {
    let chosen = match (
        db_value.filter(|s| !s.is_empty()),
        env_value.filter(|s| !s.is_empty()),
    ) {
        (Some(db), _) => db,
        (None, Some(env)) => env,
        (None, None) => return Err(CatalogError::MissingEnv),
    };
    parse_catalog_json(chosen)
}

/// Production loader: pulls the catalog source via [`parameters::resolve_string`]
/// (Postgres beats env) and parses + validates it.
///
/// `endpoint` is the per-handler endpoint id used by the parameters
/// resolver (e.g. [`crate::parameters::ENDPOINT_BUY_SPL_TOKEN`]). Pass
/// the same endpoint id every catalog reload uses, otherwise wildcard
/// fallback in `resolve_string` will silently load the wrong row.
pub async fn load(db: Option<&ParametersDb>, endpoint: &str) -> Result<TokenCatalog, CatalogError> {
    let raw = parameters::resolve_string(
        db,
        endpoint,
        BUY_SPL_TOKEN_CATALOG_JSON,
        Some(BUY_SPL_TOKEN_CATALOG_JSON),
    )
    .await
    .ok_or(CatalogError::MissingEnv)?;
    parse_catalog_json(&raw)
}

// --- Internals ---

fn validate_entry(entry: &CatalogEntry) -> Result<(), CatalogError> {
    // 1. mint
    Pubkey::from_str(&entry.mint).map_err(|e| CatalogError::InvalidMint {
        entry_name: entry.name.clone(),
        mint: entry.mint.clone(),
        reason: e.to_string(),
    })?;

    // 2. decimals
    if entry.decimals > 18 {
        return Err(CatalogError::DecimalsOutOfRange {
            entry_name: entry.name.clone(),
            decimals: entry.decimals,
        });
    }

    // 3. price_usdc_ui
    let DecimalShape {
        is_positive,
        fractional_digits,
    } = parse_decimal_shape(&entry.price_usdc_ui).map_err(|reason| CatalogError::InvalidPrice {
        entry_name: entry.name.clone(),
        price: entry.price_usdc_ui.clone(),
        reason,
    })?;
    if !is_positive {
        return Err(CatalogError::NonPositivePrice {
            entry_name: entry.name.clone(),
            price: entry.price_usdc_ui.clone(),
        });
    }
    if fractional_digits as u32 > entry.decimals as u32 {
        return Err(CatalogError::FractionalOverflow {
            entry_name: entry.name.clone(),
            decimals: entry.decimals,
            fractional_digits,
            price: entry.price_usdc_ui.clone(),
        });
    }

    // 4. sender_treasury_ata (optional, must still be valid base58 pubkey when present)
    if let Some(ata) = &entry.sender_treasury_ata {
        Pubkey::from_str(ata).map_err(|e| CatalogError::InvalidMint {
            entry_name: entry.name.clone(),
            mint: ata.clone(),
            reason: format!("invalid sender_treasury_ata: {}", e),
        })?;
    }

    Ok(())
}

#[derive(Debug)]
struct DecimalShape {
    is_positive: bool,
    fractional_digits: usize,
}

/// Inspect a decimal literal without converting through floating point.
///
/// Returns the digit-count of the fractional part and whether the value is
/// strictly greater than zero. Returns an error for malformed input.
fn parse_decimal_shape(s: &str) -> Result<DecimalShape, String> {
    if s.is_empty() {
        return Err("empty string".into());
    }
    if s != s.trim() {
        return Err("contains whitespace".into());
    }

    let bytes = s.as_bytes();
    if bytes[0] == b'+' {
        return Err("leading '+' not allowed".into());
    }
    let is_negative = bytes[0] == b'-';
    let body = if is_negative { &s[1..] } else { s };
    if body.is_empty() {
        return Err("missing digits after sign".into());
    }

    let mut saw_digit = false;
    let mut saw_dot = false;
    let mut saw_nonzero = false;
    let mut fractional_digits: usize = 0;

    for ch in body.chars() {
        if ch == '.' {
            if saw_dot {
                return Err("multiple decimal points".into());
            }
            saw_dot = true;
        } else if ch.is_ascii_digit() {
            saw_digit = true;
            if ch != '0' {
                saw_nonzero = true;
            }
            if saw_dot {
                fractional_digits += 1;
            }
        } else {
            return Err(format!("invalid character: {:?}", ch));
        }
    }

    if !saw_digit {
        return Err("no digits".into());
    }

    Ok(DecimalShape {
        is_positive: !is_negative && saw_nonzero,
        fractional_digits,
    })
}

/// Accept either a JSON string or number for `price_usdc_ui`.
///
/// Floats round-trip via `format!("{}", v)`, which uses the shortest
/// representation. That can drop trailing-zero fractional digits relative to
/// the operator's literal — see module docs.
fn deserialize_decimal_str<'de, D>(d: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{Error as DeError, Visitor};

    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = String;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a decimal string or a JSON number")
        }
        fn visit_str<E: DeError>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_string<E: DeError>(self, v: String) -> Result<String, E> {
            Ok(v)
        }
        fn visit_u64<E: DeError>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: DeError>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_f64<E: DeError>(self, v: f64) -> Result<String, E> {
            Ok(format!("{}", v))
        }
    }

    d.deserialize_any(V)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Solana mint pubkey from the spec (Merry Xmas devnet token).
    const VALID_MINT: &str = "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP";
    // USDC devnet mint, used as a second valid pubkey for ATA tests.
    const VALID_PUBKEY_2: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

    fn entry_json(price: &str, decimals: u8) -> String {
        format!(
            r#"[{{"mint":"{mint}","decimals":{decimals},"price_usdc_ui":"{price}","name":"Merry Xmas"}}]"#,
            mint = VALID_MINT,
            decimals = decimals,
            price = price
        )
    }

    // ---- happy path ----

    #[test]
    fn happy_path_parses_and_validates() {
        let json = entry_json("0.42", 6);
        let cat = parse_catalog_json(&json).expect("happy path should parse");
        assert_eq!(cat.len(), 1);
        let entry = &cat.entries()[0];
        assert_eq!(entry.mint, VALID_MINT);
        assert_eq!(entry.decimals, 6);
        assert_eq!(entry.price_usdc_ui, "0.42");
        assert_eq!(entry.name, "Merry Xmas");
        assert_eq!(entry.sender_treasury_ata, None);
        assert_eq!(
            cat.find_by_mint(VALID_MINT).map(|e| &e.name).unwrap(),
            "Merry Xmas"
        );
    }

    #[test]
    fn happy_path_with_optional_sender_treasury_ata() {
        let json = format!(
            r#"[{{"mint":"{mint}","decimals":6,"price_usdc_ui":"1","name":"X","sender_treasury_ata":"{ata}"}}]"#,
            mint = VALID_MINT,
            ata = VALID_PUBKEY_2
        );
        let cat = parse_catalog_json(&json).unwrap();
        assert_eq!(
            cat.entries()[0].sender_treasury_ata.as_deref(),
            Some(VALID_PUBKEY_2)
        );
    }

    #[test]
    fn happy_path_accepts_json_number_for_price() {
        let json = format!(
            r#"[{{"mint":"{}","decimals":2,"price_usdc_ui":1.5,"name":"X"}}]"#,
            VALID_MINT
        );
        let cat = parse_catalog_json(&json).unwrap();
        assert_eq!(cat.entries()[0].price_usdc_ui, "1.5");
    }

    #[test]
    fn happy_path_integer_price_with_zero_decimals() {
        let json = entry_json("3", 0);
        let cat = parse_catalog_json(&json).unwrap();
        assert_eq!(cat.entries()[0].price_usdc_ui, "3");
    }

    // ---- invalid mint ----

    #[test]
    fn invalid_mint_is_rejected_and_names_entry() {
        let json =
            r#"[{"mint":"not-a-real-mint","decimals":6,"price_usdc_ui":"1","name":"Bad Token"}]"#;
        let err = parse_catalog_json(json).unwrap_err();
        match err {
            CatalogError::InvalidMint {
                entry_name, mint, ..
            } => {
                assert_eq!(entry_name, "Bad Token");
                assert_eq!(mint, "not-a-real-mint");
            }
            other => panic!("expected InvalidMint, got {:?}", other),
        }
    }

    #[test]
    fn invalid_sender_treasury_ata_is_rejected() {
        let json = format!(
            r#"[{{"mint":"{}","decimals":6,"price_usdc_ui":"1","name":"X","sender_treasury_ata":"nope"}}]"#,
            VALID_MINT
        );
        let err = parse_catalog_json(&json).unwrap_err();
        assert!(matches!(err, CatalogError::InvalidMint { .. }));
    }

    // ---- decimals out of range ----

    #[test]
    fn decimals_above_18_rejected() {
        let json = entry_json("1", 19);
        let err = parse_catalog_json(&json).unwrap_err();
        match err {
            CatalogError::DecimalsOutOfRange {
                entry_name,
                decimals,
            } => {
                assert_eq!(entry_name, "Merry Xmas");
                assert_eq!(decimals, 19);
            }
            other => panic!("expected DecimalsOutOfRange, got {:?}", other),
        }
    }

    #[test]
    fn decimals_zero_and_eighteen_are_in_range() {
        // boundary inclusive at 0
        parse_catalog_json(&entry_json("1", 0)).expect("decimals=0 ok");
        // boundary inclusive at 18 (price has 18 fractional digits)
        let json = format!(
            r#"[{{"mint":"{}","decimals":18,"price_usdc_ui":"0.000000000000000001","name":"X"}}]"#,
            VALID_MINT
        );
        parse_catalog_json(&json).expect("decimals=18 ok");
    }

    // ---- non-positive price ----

    #[test]
    fn zero_price_is_rejected() {
        let json = entry_json("0", 6);
        let err = parse_catalog_json(&json).unwrap_err();
        assert!(matches!(err, CatalogError::NonPositivePrice { .. }));
    }

    #[test]
    fn zero_with_decimals_is_rejected() {
        let json = entry_json("0.000000", 6);
        let err = parse_catalog_json(&json).unwrap_err();
        assert!(matches!(err, CatalogError::NonPositivePrice { .. }));
    }

    #[test]
    fn negative_price_is_rejected() {
        let json = entry_json("-1.0", 6);
        let err = parse_catalog_json(&json).unwrap_err();
        assert!(matches!(err, CatalogError::NonPositivePrice { .. }));
    }

    #[test]
    fn malformed_price_is_rejected() {
        let json = entry_json("abc", 6);
        let err = parse_catalog_json(&json).unwrap_err();
        assert!(matches!(err, CatalogError::InvalidPrice { .. }));
    }

    // ---- fractional digits exceed decimals ----

    #[test]
    fn fractional_digits_exceeding_decimals_rejected() {
        let json = entry_json("0.123", 2);
        let err = parse_catalog_json(&json).unwrap_err();
        match err {
            CatalogError::FractionalOverflow {
                entry_name,
                decimals,
                fractional_digits,
                ..
            } => {
                assert_eq!(entry_name, "Merry Xmas");
                assert_eq!(decimals, 2);
                assert_eq!(fractional_digits, 3);
            }
            other => panic!("expected FractionalOverflow, got {:?}", other),
        }
    }

    #[test]
    fn equal_fractional_digits_to_decimals_accepted() {
        let cat = parse_catalog_json(&entry_json("0.42", 2)).unwrap();
        assert_eq!(cat.entries()[0].price_usdc_ui, "0.42");
    }

    // ---- missing env / source priority ----

    #[test]
    fn missing_both_sources_returns_missing_env() {
        let err = load_from_strings(None, None).unwrap_err();
        assert!(matches!(err, CatalogError::MissingEnv));
    }

    #[test]
    fn empty_strings_treated_as_missing() {
        let err = load_from_strings(Some(""), Some("")).unwrap_err();
        assert!(matches!(err, CatalogError::MissingEnv));
    }

    #[test]
    fn env_only_loads_when_db_absent() {
        let env = entry_json("0.5", 6);
        let cat = load_from_strings(None, Some(&env)).unwrap();
        assert_eq!(cat.entries()[0].price_usdc_ui, "0.5");
    }

    // ---- Postgres override beats env ----

    #[test]
    fn db_value_overrides_env_value() {
        // Env carries an entry with name "FromEnv", db carries "FromDb".
        // The loader must pick the DB version.
        let env = format!(
            r#"[{{"mint":"{}","decimals":6,"price_usdc_ui":"1","name":"FromEnv"}}]"#,
            VALID_MINT
        );
        let db = format!(
            r#"[{{"mint":"{}","decimals":6,"price_usdc_ui":"2","name":"FromDb"}}]"#,
            VALID_MINT
        );
        let cat = load_from_strings(Some(&db), Some(&env)).unwrap();
        assert_eq!(cat.entries()[0].name, "FromDb");
        assert_eq!(cat.entries()[0].price_usdc_ui, "2");
    }

    #[test]
    fn empty_db_value_falls_back_to_env() {
        let env = entry_json("0.5", 6);
        let cat = load_from_strings(Some(""), Some(&env)).unwrap();
        assert_eq!(cat.entries()[0].name, "Merry Xmas");
    }

    // ---- parse error ----

    #[test]
    fn malformed_json_is_parse_error() {
        let err = parse_catalog_json("not json").unwrap_err();
        assert!(matches!(err, CatalogError::ParseError { .. }));
    }

    #[test]
    fn empty_array_is_valid_but_empty() {
        let cat = parse_catalog_json("[]").unwrap();
        assert!(cat.is_empty());
    }

    // ---- error formatting smoke test ----

    #[test]
    fn error_messages_name_offending_entry() {
        let err = parse_catalog_json(&entry_json("0.123", 2)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Merry Xmas"), "msg={}", msg);
        assert!(msg.contains("3 fractional digits"), "msg={}", msg);
    }

    // ---- decimal shape parser unit coverage ----

    #[test]
    fn parse_decimal_shape_basics() {
        let s = parse_decimal_shape("1.23").unwrap();
        assert!(s.is_positive);
        assert_eq!(s.fractional_digits, 2);

        let s = parse_decimal_shape("0").unwrap();
        assert!(!s.is_positive);
        assert_eq!(s.fractional_digits, 0);

        let s = parse_decimal_shape("0.0").unwrap();
        assert!(!s.is_positive);
        assert_eq!(s.fractional_digits, 1);

        assert!(parse_decimal_shape("").is_err());
        assert!(parse_decimal_shape("1.2.3").is_err());
        assert!(parse_decimal_shape("1e6").is_err());
        assert!(parse_decimal_shape("+1").is_err());
    }
}
