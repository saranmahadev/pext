//! Calendar helpers. All dates are `chrono::NaiveDate` (no time zones).

use crate::error::{Error, Result};
use chrono::{Datelike, Duration, NaiveDate};

pub fn parse_date(s: &str) -> Result<NaiveDate> {
    let t = s.trim();
    let bad = || {
        Error::validation("invalid_date", format!("'{s}' is not a valid date"))
            .with_hint("use YYYY-MM-DD, e.g. 2026-11-05")
    };
    let b = t.as_bytes();
    if t.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d").map_err(|_| bad())
}

/// Parse using an explicit chrono format string (for bank files).
pub fn parse_date_with(s: &str, fmt: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), fmt).map_err(|_| {
        Error::validation(
            "invalid_date",
            format!("'{s}' does not match date format '{fmt}'"),
        )
        .with_hint("pass --date-format using chrono syntax, e.g. %d/%m/%Y")
    })
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    match (
        NaiveDate::from_ymd_opt(year, month, 1),
        NaiveDate::from_ymd_opt(ny, nm, 1),
    ) {
        (Some(a), Some(b)) => (b - a).num_days() as u32,
        _ => 30,
    }
}

pub fn month_start(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

pub fn month_end(d: NaiveDate) -> NaiveDate {
    d.with_day(days_in_month(d.year(), d.month())).unwrap_or(d)
}

pub fn add_months(d: NaiveDate, n: i32) -> Result<NaiveDate> {
    let total = d.year() * 12 + (d.month() as i32 - 1) + n;
    let ny = total.div_euclid(12);
    let nm = (total.rem_euclid(12) + 1) as u32;
    let day = d.day().min(days_in_month(ny, nm));
    NaiveDate::from_ymd_opt(ny, nm, day).ok_or_else(|| {
        Error::validation("date_out_of_range", "date is outside the supported range")
    })
}

pub fn add_days(d: NaiveDate, n: i64) -> Result<NaiveDate> {
    d.checked_add_signed(Duration::days(n)).ok_or_else(|| {
        Error::validation("date_out_of_range", "date is outside the supported range")
    })
}

/// Monday of the week containing `d`.
pub fn week_start(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

pub fn quarter_start(d: NaiveDate) -> NaiveDate {
    let m = ((d.month() - 1) / 3) * 3 + 1;
    NaiveDate::from_ymd_opt(d.year(), m, 1).unwrap_or(d)
}

/// "2026-10"
pub fn ym(d: NaiveDate) -> String {
    format!("{:04}-{:02}", d.year(), d.month())
}

/// Parse "YYYY-MM" into (first day, last day).
pub fn parse_month(s: &str) -> Result<(NaiveDate, NaiveDate)> {
    let t = s.trim();
    let bad = || {
        Error::validation("invalid_month", format!("'{s}' is not a valid month"))
            .with_hint("use YYYY-MM, e.g. 2026-11")
    };
    if t.len() != 7 || t.as_bytes()[4] != b'-' {
        return Err(bad());
    }
    let y: i32 = t[..4].parse().map_err(|_| bad())?;
    let m: u32 = t[5..].parse().map_err(|_| bad())?;
    let first = NaiveDate::from_ymd_opt(y, m, 1).ok_or_else(bad)?;
    Ok((first, month_end(first)))
}

/// Accept ISO dates plus the words `today` and `yesterday`.
pub fn parse_date_arg(s: &str, today: NaiveDate) -> Result<NaiveDate> {
    match s.trim().to_ascii_lowercase().as_str() {
        "today" => Ok(today),
        "yesterday" => add_days(today, -1),
        _ => parse_date(s),
    }
}

pub fn min_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or(NaiveDate::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    #[test]
    fn strict_parsing() {
        assert!(parse_date("2026-11-05").is_ok());
        for bad in [
            "2026-1-5",
            "2026/11/05",
            "05-11-2026",
            "2026-13-01",
            "2026-02-30",
            "",
            "abc",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn month_math() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 12), 31);
        assert_eq!(add_months(d("2026-01-31"), 1).unwrap(), d("2026-02-28"));
        assert_eq!(add_months(d("2026-03-31"), -1).unwrap(), d("2026-02-28"));
        assert_eq!(add_months(d("2026-11-15"), 14).unwrap(), d("2028-01-15"));
        assert_eq!(add_months(d("2026-01-15"), -13).unwrap(), d("2024-12-15"));
        assert_eq!(month_end(d("2026-02-10")), d("2026-02-28"));
        assert_eq!(week_start(d("2026-11-05")), d("2026-11-02"));
        assert_eq!(quarter_start(d("2026-11-05")), d("2026-10-01"));
        assert_eq!(ym(d("2026-03-09")), "2026-03");
    }

    #[test]
    fn months_and_args() {
        assert_eq!(
            parse_month("2026-02").unwrap(),
            (d("2026-02-01"), d("2026-02-28"))
        );
        assert!(parse_month("2026-13").is_err());
        assert!(parse_month("2026-2").is_err());
        let t = d("2026-11-05");
        assert_eq!(parse_date_arg("today", t).unwrap(), t);
        assert_eq!(parse_date_arg("Yesterday", t).unwrap(), d("2026-11-04"));
    }
}
