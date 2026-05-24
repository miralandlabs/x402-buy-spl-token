//! Human-decimal ↔ on-chain raw unit conversion (exact integer arithmetic).
//!
//! Used for two independent catalog axes:
//!
//! - **Payment (x402 / USDC):** `price_usdc_ui` × 10^[`USDC_DECIMALS`] → escrow amount.
//! - **Deliverable (SPL mint):** `deliver_amount_ui` × 10^`mint.decimals` → SLA + transfer amount.

/// USDC always uses 6 decimal places on Solana (mainnet + devnet mints we support).
pub const USDC_DECIMALS: u32 = 6;

/// Failure modes of [`ui_to_raw_units`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmountParseError {
    Empty,
    InvalidChar(char),
    MultipleDots,
    NoDigits,
    TooManyFractionalDigits { digits: usize, allowed: u32 },
    Overflow,
    NotPositive,
}

impl std::fmt::Display for AmountParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty amount"),
            Self::InvalidChar(c) => write!(f, "invalid character {:?}", c),
            Self::MultipleDots => f.write_str("multiple decimal points"),
            Self::NoDigits => f.write_str("no digits"),
            Self::TooManyFractionalDigits { digits, allowed } => write!(
                f,
                "{} fractional digits exceeds the allowed maximum of {}",
                digits, allowed
            ),
            Self::Overflow => f.write_str("amount overflows u64 raw units"),
            Self::NotPositive => f.write_str("amount must be strictly greater than zero"),
        }
    }
}

impl std::error::Error for AmountParseError {}

/// Convert a human decimal string into raw on-chain units (× 10^`decimals`).
///
/// Accepts `"1"`, `"0.5"`, `"42.42"`. Rejects signs, exponents, whitespace,
/// multiple decimal points, zero, and fractional digit counts above `decimals`.
pub fn ui_to_raw_units(amount_ui: &str, decimals: u32) -> Result<u64, AmountParseError> {
    if amount_ui.is_empty() {
        return Err(AmountParseError::Empty);
    }
    if matches!(amount_ui.as_bytes()[0], b'+' | b'-') {
        return Err(AmountParseError::InvalidChar(
            amount_ui.chars().next().unwrap(),
        ));
    }

    let mut parts = amount_ui.splitn(2, '.');
    let integer_part = parts.next().unwrap_or("");
    let fractional_part = parts.next().unwrap_or("");
    if amount_ui.matches('.').count() > 1 {
        return Err(AmountParseError::MultipleDots);
    }
    if integer_part.is_empty() && fractional_part.is_empty() {
        return Err(AmountParseError::NoDigits);
    }

    for ch in integer_part.chars().chain(fractional_part.chars()) {
        if !ch.is_ascii_digit() {
            return Err(AmountParseError::InvalidChar(ch));
        }
    }

    if (fractional_part.len() as u32) > decimals {
        return Err(AmountParseError::TooManyFractionalDigits {
            digits: fractional_part.len(),
            allowed: decimals,
        });
    }

    let pad = (decimals as usize).saturating_sub(fractional_part.len());
    let mut combined = String::with_capacity(integer_part.len() + decimals as usize);
    if integer_part.is_empty() {
        combined.push('0');
    } else {
        combined.push_str(integer_part);
    }
    combined.push_str(fractional_part);
    for _ in 0..pad {
        combined.push('0');
    }

    let raw: u64 = combined.parse().map_err(|_| AmountParseError::Overflow)?;
    if raw == 0 {
        return Err(AmountParseError::NotPositive);
    }
    Ok(raw)
}

/// Format raw on-chain units as a minimal human decimal string (no trailing zeros).
pub fn raw_to_ui_units(raw: u64, decimals: u32) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let scale = 10u128.pow(decimals);
    let int_part = raw as u128 / scale;
    let frac_part = raw as u128 % scale;
    if frac_part == 0 {
        int_part.to_string()
    } else {
        let frac_str = format!("{:0width$}", frac_part, width = decimals as usize);
        let trimmed = frac_str.trim_end_matches('0');
        format!("{}.{}", int_part, trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_to_ui_round_trip_integer() {
        assert_eq!(raw_to_ui_units(1_000_000_000, 6), "1000");
        assert_eq!(raw_to_ui_units(420_000, USDC_DECIMALS), "0.42");
    }

    #[test]
    fn usdc_six_decimals() {
        assert_eq!(ui_to_raw_units("1", USDC_DECIMALS).unwrap(), 1_000_000);
        assert_eq!(ui_to_raw_units("0.5", USDC_DECIMALS).unwrap(), 500_000);
        assert_eq!(ui_to_raw_units("42.42", USDC_DECIMALS).unwrap(), 42_420_000);
    }

    #[test]
    fn rejects_too_many_usdc_fractional_digits() {
        let err = ui_to_raw_units("0.1234567", USDC_DECIMALS).unwrap_err();
        assert!(matches!(
            err,
            AmountParseError::TooManyFractionalDigits { .. }
        ));
    }

    #[test]
    fn token_mint_decimals() {
        assert_eq!(ui_to_raw_units("0.42", 6).unwrap(), 420_000);
        assert_eq!(ui_to_raw_units("1000", 6).unwrap(), 1_000_000_000);
        assert_eq!(ui_to_raw_units("3", 0).unwrap(), 3);
    }

    #[test]
    fn rejects_zero_and_malformed() {
        assert!(matches!(
            ui_to_raw_units("0", USDC_DECIMALS).unwrap_err(),
            AmountParseError::NotPositive
        ));
        assert!(matches!(
            ui_to_raw_units("abc", 6).unwrap_err(),
            AmountParseError::InvalidChar(_)
        ));
    }
}
