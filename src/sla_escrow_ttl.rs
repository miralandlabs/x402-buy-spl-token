//! SLA-Escrow `FundPayment` TTL rules (`x402/sla-escrow-fund-payment-ttl/v1`).
//!
//! Mirrors [`pr402::sla_escrow_ttl`] semantics for resource providers that do not depend on pr402.

use std::fmt;

/// Normative identifier for integrators.
pub const RULE_ID: &str = "x402/sla-escrow-fund-payment-ttl/v1";

/// Align with `sla-escrow-api::consts::DEFAULT_DELIVERY_CUTOFF_SECONDS`.
pub const DEFAULT_DELIVERY_CUTOFF_SECONDS: i64 = 300;

/// Default post-funding work budget (verify/settle + delivery + registry + SubmitDelivery).
pub const DEFAULT_DELIVERY_BUDGET_SECONDS: i64 = 300;

/// Program-enforced floor (`sla-escrow-api::consts::MIN_TTL_SECONDS`).
pub const MIN_TTL_SECONDS: i64 = 60;

const ENV_DELIVERY_CUTOFF: &str = "SLA_ESCROW_DELIVERY_CUTOFF_SECONDS";
const ENV_DELIVERY_BUDGET: &str = "SLA_ESCROW_DELIVERY_BUDGET_SECONDS";

pub fn min_fund_payment_ttl_seconds(
    delivery_cutoff_seconds: i64,
    delivery_budget_seconds: i64,
) -> u64 {
    let min = delivery_cutoff_seconds
        .saturating_add(delivery_budget_seconds)
        .max(MIN_TTL_SECONDS);
    min as u64
}

pub fn resolve_delivery_cutoff_seconds() -> i64 {
    std::env::var(ENV_DELIVERY_CUTOFF)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DELIVERY_CUTOFF_SECONDS)
}

pub fn resolve_delivery_budget_seconds() -> i64 {
    std::env::var(ENV_DELIVERY_BUDGET)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DELIVERY_BUDGET_SECONDS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FundPaymentTtlError {
    TtlMismatch {
        quoted_max_timeout_seconds: u64,
        fund_payment_ttl_seconds: u64,
    },
    TtlTooShort {
        fund_payment_ttl_seconds: u64,
        minimum_required_seconds: u64,
        delivery_cutoff_seconds: i64,
        delivery_budget_seconds: i64,
    },
}

impl fmt::Display for FundPaymentTtlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TtlMismatch {
                quoted_max_timeout_seconds,
                fund_payment_ttl_seconds,
            } => write!(
                f,
                "FundPayment.ttl_seconds ({fund_payment_ttl_seconds}) must equal seller-quoted maxTimeoutSeconds ({quoted_max_timeout_seconds})"
            ),
            Self::TtlTooShort {
                fund_payment_ttl_seconds,
                minimum_required_seconds,
                delivery_cutoff_seconds,
                delivery_budget_seconds,
            } => write!(
                f,
                "FundPayment.ttl_seconds ({fund_payment_ttl_seconds}) is below minimum {minimum_required_seconds} \
                 (delivery_cutoff_seconds={delivery_cutoff_seconds} + delivery_budget_seconds={delivery_budget_seconds})"
            ),
        }
    }
}

impl std::error::Error for FundPaymentTtlError {}

pub fn validate_fund_payment_ttl(
    fund_payment_ttl_seconds: u64,
    quoted_max_timeout_seconds: u64,
    delivery_cutoff_seconds: i64,
    delivery_budget_seconds: i64,
) -> Result<(), FundPaymentTtlError> {
    if fund_payment_ttl_seconds != quoted_max_timeout_seconds {
        return Err(FundPaymentTtlError::TtlMismatch {
            quoted_max_timeout_seconds,
            fund_payment_ttl_seconds,
        });
    }
    let minimum = min_fund_payment_ttl_seconds(delivery_cutoff_seconds, delivery_budget_seconds);
    if fund_payment_ttl_seconds < minimum {
        return Err(FundPaymentTtlError::TtlTooShort {
            fund_payment_ttl_seconds,
            minimum_required_seconds: minimum,
            delivery_cutoff_seconds,
            delivery_budget_seconds,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ttl_equal_to_cutoff_without_budget() {
        assert!(validate_fund_payment_ttl(300, 300, 300, 300).is_err());
    }

    #[test]
    fn accepts_quoted_ttl_with_headroom() {
        assert!(validate_fund_payment_ttl(3600, 3600, 300, 300).is_ok());
    }
}
