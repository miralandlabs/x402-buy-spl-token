//! Session quote: seller-computed payment + deliverable totals for one purchase.
//!
//! # x402 compliance (v0.3)
//!
//! x402 requires the unpaid 402 to advertise the **whole** USDC amount due at
//! `payTo`. Buyers must **not** multiply unit catalog prices client-side.
//!
//! Flow:
//!
//! 1. Buyer requests optional `quantity` (default `1`).
//! 2. Seller scales catalog **unit list** prices server-side into a
//!    [`SessionQuote`].
//! 3. `accepts[].amount` and `commitMaterial.paymentAmountRaw` both equal
//!    `quote.payment_amount_raw` — the session total, authoritative for escrow.
//! 4. Buyer builds SLA / signs `FundPayment` against `commitMaterial` session
//!    totals only.
//!
//! Catalog `price_usdc_ui` / `deliver_amount_ui` are **unit list** values
//! (operator configuration). They never appear directly in `accepts[].amount`.

use {
    crate::{
        amounts::{self, AmountParseError},
        catalog::CatalogEntry,
    },
    std::fmt,
};

/// Default line quantity when the buyer omits `quantity`.
pub const DEFAULT_QUANTITY: u32 = 1;

/// Upper bound on `quantity` per request (overflow guard + abuse limit).
pub const MAX_QUANTITY: u32 = 10_000;

/// Seller-computed totals for one payment session at a given `quantity`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionQuote {
    /// Line quantity the buyer requested (≥ 1).
    pub quantity: u32,
    /// Unit USDC raw from catalog `price_usdc_ui`.
    pub unit_payment_raw: u64,
    /// Unit SPL raw from catalog `deliver_amount_ui`.
    pub unit_deliver_raw: u64,
    /// Session USDC raw — **x402 `accepts[].amount`** and escrow funding total.
    pub payment_amount_raw: u64,
    /// Session SPL raw — SLA `min_amount` and `TransferChecked` amount.
    pub deliver_amount_raw: u64,
    /// Session SPL human amount at mint `decimals` (derived from `deliver_amount_raw`).
    pub deliver_amount_ui: String,
}

/// Failures building a [`SessionQuote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuoteError {
    InvalidQuantity { reason: String },
    UnitPayment(AmountParseError),
    UnitDeliver(AmountParseError),
    ScaleOverflow { field: &'static str },
}

impl fmt::Display for QuoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuantity { reason } => write!(f, "invalid quantity: {}", reason),
            Self::UnitPayment(e) => write!(f, "unit payment: {}", e),
            Self::UnitDeliver(e) => write!(f, "unit deliverable: {}", e),
            Self::ScaleOverflow { field } => write!(f, "{} total overflows u64", field),
        }
    }
}

impl std::error::Error for QuoteError {}

impl CatalogEntry {
    /// Build a seller-quoted session total from this catalog unit list row.
    pub fn session_quote(&self, quantity: u32) -> Result<SessionQuote, QuoteError> {
        validate_quantity(quantity)?;

        let unit_payment_raw = self.price_usdc_raw().map_err(QuoteError::UnitPayment)?;
        let unit_deliver_raw = self.deliver_amount_raw().map_err(QuoteError::UnitDeliver)?;

        let payment_amount_raw = unit_payment_raw
            .checked_mul(quantity as u64)
            .ok_or(QuoteError::ScaleOverflow { field: "payment" })?;
        let deliver_amount_raw =
            unit_deliver_raw
                .checked_mul(quantity as u64)
                .ok_or(QuoteError::ScaleOverflow {
                    field: "deliverable",
                })?;

        let deliver_amount_ui = amounts::raw_to_ui_units(deliver_amount_raw, self.decimals as u32);

        Ok(SessionQuote {
            quantity,
            unit_payment_raw,
            unit_deliver_raw,
            payment_amount_raw,
            deliver_amount_raw,
            deliver_amount_ui,
        })
    }
}

/// Parse optional `quantity` query value. Omitted → [`DEFAULT_QUANTITY`].
pub fn parse_quantity(raw: Option<&str>) -> Result<u32, QuoteError> {
    match raw {
        None => Ok(DEFAULT_QUANTITY),
        Some("") => Ok(DEFAULT_QUANTITY),
        Some(s) => {
            let q: u32 = s.parse().map_err(|_| QuoteError::InvalidQuantity {
                reason: format!("{:?} is not a positive integer", s),
            })?;
            validate_quantity(q)
        }
    }
}

fn validate_quantity(quantity: u32) -> Result<u32, QuoteError> {
    if quantity == 0 {
        return Err(QuoteError::InvalidQuantity {
            reason: "must be at least 1".into(),
        });
    }
    if quantity > MAX_QUANTITY {
        return Err(QuoteError::InvalidQuantity {
            reason: format!("must not exceed {}", MAX_QUANTITY),
        });
    }
    Ok(quantity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::parse_catalog_json;

    const VALID_MINT: &str = "5bpyckh5YBVG5fB63PSm4BGPjD5sw1TwBtU5GGd9VRRP";

    fn entry() -> CatalogEntry {
        let json = format!(
            r#"[{{"mint":"{VALID_MINT}","decimals":6,"price_usdc_ui":"0.42","deliver_amount_ui":"1000","name":"X"}}]"#
        );
        parse_catalog_json(&json).unwrap().entries()[0].clone()
    }

    #[test]
    fn default_quantity_one_matches_unit_list() {
        let q = entry().session_quote(1).unwrap();
        assert_eq!(q.quantity, 1);
        assert_eq!(q.payment_amount_raw, 420_000);
        assert_eq!(q.deliver_amount_raw, 1_000_000_000);
        assert_eq!(q.deliver_amount_ui, "1000");
    }

    #[test]
    fn quantity_three_scales_both_axes() {
        let q = entry().session_quote(3).unwrap();
        assert_eq!(q.payment_amount_raw, 1_260_000);
        assert_eq!(q.deliver_amount_raw, 3_000_000_000);
        assert_eq!(q.deliver_amount_ui, "3000");
        assert_eq!(q.unit_payment_raw, 420_000);
        assert_eq!(q.unit_deliver_raw, 1_000_000_000);
    }

    #[test]
    fn parse_quantity_defaults_and_rejects_zero() {
        assert_eq!(parse_quantity(None).unwrap(), 1);
        assert_eq!(parse_quantity(Some("")).unwrap(), 1);
        assert_eq!(parse_quantity(Some("2")).unwrap(), 2);
        assert!(matches!(
            parse_quantity(Some("0")).unwrap_err(),
            QuoteError::InvalidQuantity { .. }
        ));
        assert!(matches!(
            parse_quantity(Some("abc")).unwrap_err(),
            QuoteError::InvalidQuantity { .. }
        ));
    }

    #[test]
    fn overflow_rejected_on_deliverable_scale() {
        let json = format!(
            r#"[{{"mint":"{VALID_MINT}","decimals":0,"price_usdc_ui":"1","deliver_amount_ui":"1844674407370956","name":"big"}}]"#
        );
        let entry = parse_catalog_json(&json).unwrap().entries()[0].clone();
        let err = entry.session_quote(MAX_QUANTITY).unwrap_err();
        assert!(
            matches!(
                err,
                QuoteError::ScaleOverflow {
                    field: "deliverable"
                }
            ),
            "expected deliverable ScaleOverflow, got {:?}",
            err
        );
    }
}
