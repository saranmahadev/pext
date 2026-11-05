//! Exact money arithmetic. Amounts are integers in the currency's minor unit.
//! Nothing in this module touches floating point.

use crate::error::{Error, Result};
use std::cmp::Ordering;

/// Largest absolute amount (in minor units) accepted anywhere in the system.
/// Keeps sums of millions of transactions inside i64.
pub const MAX_ABS_MINOR: i64 = 10_000_000_000_000;

/// Number of minor-unit decimal places for an ISO 4217 code.
pub fn decimals_for(code: &str) -> u32 {
    match code {
        "JPY" | "KRW" | "VND" | "CLP" | "ISK" | "UGX" | "PYG" | "RWF" | "XAF" | "XOF" => 0,
        "BHD" | "KWD" | "OMR" | "JOD" | "TND" | "IQD" | "LYD" => 3,
        _ => 2,
    }
}

pub fn validate_currency(s: &str) -> Result<String> {
    let u = s.trim().to_ascii_uppercase();
    if u.len() == 3 && u.bytes().all(|b| b.is_ascii_uppercase()) {
        Ok(u)
    } else {
        Err(Error::validation(
            "invalid_currency",
            format!("'{s}' is not a 3-letter currency code"),
        )
        .with_field("currency")
        .with_hint("use an ISO 4217 code such as INR, USD or EUR"))
    }
}

pub fn pow10(n: u32) -> i128 {
    10i128.pow(n.min(30))
}

pub fn check_amount(minor: i64) -> Result<i64> {
    if minor.unsigned_abs() > MAX_ABS_MINOR as u64 {
        return Err(Error::validation(
            "amount_out_of_range",
            format!("amount {minor} exceeds the supported maximum of {MAX_ABS_MINOR} minor units"),
        )
        .with_hint("check for a misplaced decimal point"));
    }
    Ok(minor)
}

pub fn checked_add(a: i64, b: i64) -> Result<i64> {
    a.checked_add(b)
        .ok_or_else(|| Error::validation("amount_overflow", "amount arithmetic overflowed"))
}

/// An exact decimal number: `units / 10^scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    pub units: i128,
    pub scale: u32,
}

impl Decimal {
    /// Convert to minor units exactly; fails if precision would be lost.
    pub fn to_minor(&self, decimals: u32) -> Result<i64> {
        let v = if self.scale <= decimals {
            self.units
                .checked_mul(pow10(decimals - self.scale))
                .ok_or_else(|| Error::validation("amount_out_of_range", "amount is too large"))?
        } else {
            let div = pow10(self.scale - decimals);
            if self.units % div != 0 {
                return Err(Error::validation(
                    "amount_precision",
                    format!("amount has more than {decimals} decimal places"),
                )
                .with_hint(format!("this currency uses {decimals} decimal places")));
            }
            self.units / div
        };
        let v = i64::try_from(v)
            .map_err(|_| Error::validation("amount_out_of_range", "amount is too large"))?;
        check_amount(v)
    }

    /// Compare a minor-unit amount (with `decimals`) against this decimal.
    pub fn cmp_minor(&self, minor: i64, decimals: u32) -> Ordering {
        let s = decimals.max(self.scale);
        let a = (minor as i128).checked_mul(pow10(s - decimals));
        let b = self.units.checked_mul(pow10(s - self.scale));
        match (a, b) {
            (Some(a), Some(b)) => a.cmp(&b),
            (None, Some(_)) => {
                if minor >= 0 {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Some(_), None) => {
                if self.units >= 0 {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (None, None) => Ordering::Equal,
        }
    }
}

pub fn parse_decimal(s: &str) -> Result<Decimal> {
    let t = s.trim();
    let err = || {
        Error::validation("invalid_amount", format!("'{s}' is not a valid decimal number"))
            .with_hint("use plain digits with an optional decimal point, e.g. -450.50 (no commas or currency symbols)")
    };
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if rest.is_empty() {
        return Err(err());
    }
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, f),
        None => (rest, ""),
    };
    let has_point = rest.contains('.');
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || (has_point && frac_part.is_empty())
        || int_part.len() + frac_part.len() > 30
    {
        return Err(err());
    }
    let digits = format!("{int_part}{frac_part}");
    let mut units: i128 = digits.parse().map_err(|_| err())?;
    if neg {
        units = -units;
    }
    Ok(Decimal {
        units,
        scale: frac_part.len() as u32,
    })
}

/// Parse a decimal string in major units into minor units, exactly.
pub fn parse_amount(s: &str, decimals: u32) -> Result<i64> {
    parse_decimal(s)?.to_minor(decimals)
}

/// Format minor units as a plain decimal string ("-1234.50").
pub fn format_amount(minor: i64, decimals: u32) -> String {
    let neg = minor < 0;
    let abs = (minor as i128).unsigned_abs();
    let body = if decimals == 0 {
        abs.to_string()
    } else {
        let p = 10u128.pow(decimals);
        format!("{}.{:0width$}", abs / p, abs % p, width = decimals as usize)
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

fn group_digits(int_part: &str, indian: bool) -> String {
    let n = int_part.len();
    if n <= 3 {
        return int_part.to_string();
    }
    let head = &int_part[..n - 3];
    let tail = &int_part[n - 3..];
    let step = if indian { 2 } else { 3 };
    let mut groups: Vec<&str> = Vec::new();
    let mut end = head.len();
    while end > 0 {
        let start = end.saturating_sub(step);
        groups.push(&head[start..end]);
        end = start;
    }
    groups.reverse();
    groups.push(tail);
    groups.join(",")
}

/// Human display with thousands separators (Indian grouping for INR).
pub fn format_grouped(minor: i64, currency: &str) -> String {
    let plain = format_amount(minor, decimals_for(currency));
    let (neg, rest) = match plain.strip_prefix('-') {
        Some(r) => (true, r.to_string()),
        None => (false, plain),
    };
    let (int_part, frac) = match rest.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f.to_string())),
        None => (rest, None),
    };
    let mut s = group_digits(&int_part, currency == "INR");
    if let Some(f) = frac {
        s.push('.');
        s.push_str(&f);
    }
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

/// (a * b) / c with banker's rounding (half to even), overflow-checked.
pub fn mul_div_round(a: i128, b: i128, c: i128) -> Result<i128> {
    if c == 0 {
        return Err(Error::internal("division_by_zero", "division by zero"));
    }
    let n = a
        .checked_mul(b)
        .ok_or_else(|| Error::validation("amount_overflow", "amount arithmetic overflowed"))?;
    let neg = (n < 0) != (c < 0);
    let n = n.unsigned_abs();
    let d = c.unsigned_abs();
    let mut q = n / d;
    let r = n % d;
    let twice = r
        .checked_mul(2)
        .ok_or_else(|| Error::validation("amount_overflow", "amount arithmetic overflowed"))?;
    if twice > d || (twice == d && q % 2 == 1) {
        q += 1;
    }
    let q = i128::try_from(q)
        .map_err(|_| Error::validation("amount_overflow", "amount arithmetic overflowed"))?;
    Ok(if neg { -q } else { q })
}

/// Convert `amount` (minor units of the source currency) using `rate_e8`
/// (units of target currency per 1 unit of source, scaled by 1e8).
pub fn convert_minor(amount: i64, from_dec: u32, to_dec: u32, rate_e8: i64) -> Result<i64> {
    if rate_e8 <= 0 {
        return Err(Error::validation(
            "invalid_rate",
            "exchange rate must be positive",
        ));
    }
    let num = pow10(to_dec);
    let den = pow10(from_dec)
        .checked_mul(100_000_000)
        .ok_or_else(|| Error::internal("overflow", "overflow"))?;
    let scaled = (amount as i128)
        .checked_mul(rate_e8 as i128)
        .ok_or_else(|| Error::validation("amount_overflow", "amount arithmetic overflowed"))?;
    let v = mul_div_round(scaled, num, den)?;
    i64::try_from(v)
        .map_err(|_| Error::validation("amount_overflow", "converted amount is too large"))
}

/// Split `total` across `weights` so the parts always sum to exactly `total`
/// (largest-remainder method; ties go to the earlier index).
pub fn allocate(total: i64, weights: &[u64]) -> Result<Vec<i64>> {
    let sum: u128 = weights.iter().map(|w| *w as u128).sum();
    if weights.is_empty() || sum == 0 {
        return Err(Error::validation(
            "invalid_allocation",
            "allocation needs at least one positive weight",
        ));
    }
    let neg = total < 0;
    let t = (total as i128).unsigned_abs();
    let mut parts: Vec<u128> = Vec::with_capacity(weights.len());
    let mut rems: Vec<(u128, usize)> = Vec::with_capacity(weights.len());
    for (i, w) in weights.iter().enumerate() {
        let prod = t * (*w as u128);
        parts.push(prod / sum);
        rems.push((prod % sum, i));
    }
    let assigned: u128 = parts.iter().sum();
    let mut left = (t - assigned) as usize;
    rems.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, i) in rems {
        if left == 0 {
            break;
        }
        parts[i] += 1;
        left -= 1;
    }
    parts
        .into_iter()
        .map(|p| {
            let v = if neg { -(p as i128) } else { p as i128 };
            i64::try_from(v)
                .map_err(|_| Error::validation("amount_overflow", "allocated amount is too large"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats() {
        assert_eq!(parse_amount("450.50", 2).unwrap(), 45050);
        assert_eq!(parse_amount("-0.05", 2).unwrap(), -5);
        assert_eq!(parse_amount("+12", 2).unwrap(), 1200);
        assert_eq!(parse_amount(".5", 2).unwrap(), 50);
        assert_eq!(parse_amount("100", 0).unwrap(), 100);
        assert_eq!(parse_amount("10.500", 2).unwrap(), 1050);
        assert_eq!(parse_amount("1.234", 3).unwrap(), 1234);
        assert_eq!(format_amount(-5, 2), "-0.05");
        assert_eq!(format_amount(123456, 2), "1234.56");
        assert_eq!(format_amount(7, 0), "7");
        assert_eq!(format_amount(i64::MIN, 2), "-92233720368547758.08");
    }

    #[test]
    fn rejects_bad_amounts() {
        for bad in [
            "", "-", "1,000", "1e3", "5.", "abc", "1.2.3", " ", "₹5", "0x10", "--5", "1 000",
        ] {
            assert!(parse_amount(bad, 2).is_err(), "should reject {bad:?}");
        }
        assert_eq!(
            parse_amount("1.005", 2).unwrap_err().code,
            "amount_precision"
        );
        assert_eq!(
            parse_amount("99999999999999999999", 2).unwrap_err().code,
            "amount_out_of_range"
        );
        assert_eq!(
            parse_amount("100000000000.01", 2).unwrap_err().code,
            "amount_out_of_range"
        );
    }

    #[test]
    fn grouping() {
        assert_eq!(format_grouped(123456789, "INR"), "12,34,567.89");
        assert_eq!(format_grouped(123456789, "USD"), "1,234,567.89");
        assert_eq!(format_grouped(-99, "USD"), "-0.99");
        assert_eq!(format_grouped(100000, "INR"), "1,000.00");
        assert_eq!(format_grouped(10000000, "INR"), "1,00,000.00");
        assert_eq!(format_grouped(5, "JPY"), "5");
        assert_eq!(format_grouped(1234567, "JPY"), "1,234,567");
    }

    #[test]
    fn rounding_is_bankers() {
        assert_eq!(mul_div_round(5, 1, 2).unwrap(), 2);
        assert_eq!(mul_div_round(7, 1, 2).unwrap(), 4);
        assert_eq!(mul_div_round(-5, 1, 2).unwrap(), -2);
        assert_eq!(mul_div_round(-7, 1, 2).unwrap(), -4);
        assert_eq!(mul_div_round(10, 1, 3).unwrap(), 3);
        assert!(mul_div_round(1, 1, 0).is_err());
    }

    #[test]
    fn converts() {
        // 100.00 USD at 83.25 INR/USD = 8325.00 INR
        assert_eq!(convert_minor(10000, 2, 2, 8_325_000_000).unwrap(), 832500);
        // 1000 JPY at 0.55 INR/JPY = 550.00 INR
        assert_eq!(convert_minor(1000, 0, 2, 55_000_000).unwrap(), 55000);
        assert!(convert_minor(1, 2, 2, 0).is_err());
    }

    #[test]
    fn allocation_examples() {
        assert_eq!(allocate(100, &[1, 1, 1]).unwrap(), vec![34, 33, 33]);
        assert_eq!(allocate(-100, &[1, 1, 1]).unwrap(), vec![-34, -33, -33]);
        assert_eq!(allocate(5, &[1, 0, 1]).unwrap(), vec![3, 0, 2]);
        assert!(allocate(5, &[]).is_err());
        assert!(allocate(5, &[0, 0]).is_err());
        assert_eq!(allocate(i64::MIN, &[1]).unwrap_or_default().len(), 1);
    }

    #[test]
    fn decimal_compare() {
        let d = parse_decimal("500").unwrap();
        assert_eq!(d.cmp_minor(50000, 2), Ordering::Equal);
        assert_eq!(d.cmp_minor(50001, 2), Ordering::Greater);
        let d = parse_decimal("0.5").unwrap();
        assert_eq!(d.cmp_minor(49, 2), Ordering::Less);
    }
}
