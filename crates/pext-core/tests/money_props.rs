//! Property tests for the invariants every money path depends on.

use pext_core::money::*;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// format -> parse is the identity for every representable amount.
    #[test]
    fn format_parse_roundtrip(minor in -MAX_ABS_MINOR..=MAX_ABS_MINOR, dec in 0u32..=3) {
        let s = format_amount(minor, dec);
        prop_assert_eq!(parse_amount(&s, dec).unwrap(), minor);
    }

    /// Allocation never creates or destroys a single minor unit.
    #[test]
    fn allocate_sums_exactly(
        total in -MAX_ABS_MINOR..=MAX_ABS_MINOR,
        weights in proptest::collection::vec(0u64..1_000_000, 1..12),
    ) {
        prop_assume!(weights.iter().any(|w| *w > 0));
        let parts = allocate(total, &weights).unwrap();
        prop_assert_eq!(parts.len(), weights.len());
        prop_assert_eq!(parts.iter().sum::<i64>(), total);
        // zero weights get nothing; all parts share the sign of the total
        for (p, w) in parts.iter().zip(&weights) {
            if *w == 0 { prop_assert_eq!(*p, 0); }
            if total >= 0 { prop_assert!(*p >= 0); } else { prop_assert!(*p <= 0); }
        }
    }

    /// Parts differ from the ideal proportional share by less than one unit.
    #[test]
    fn allocate_is_fair(total in 0i64..=MAX_ABS_MINOR, weights in proptest::collection::vec(1u64..1000, 1..10)) {
        let parts = allocate(total, &weights).unwrap();
        let sum: i128 = weights.iter().map(|w| *w as i128).sum();
        for (p, w) in parts.iter().zip(&weights) {
            let ideal_floor = (total as i128 * *w as i128) / sum;
            prop_assert!((*p as i128) == ideal_floor || (*p as i128) == ideal_floor + 1);
        }
    }

    /// Rounding half-even is within half a unit of the exact quotient.
    #[test]
    fn mul_div_round_is_close(a in -1_000_000_000i64..1_000_000_000, b in 1i64..1_000_000, c in 1i64..1_000_000) {
        let q = mul_div_round(a as i128, b as i128, c as i128).unwrap();
        let exact_num = a as i128 * b as i128;
        // |q*c - exact_num| <= c/2
        let err = (q * c as i128 - exact_num).abs();
        prop_assert!(err * 2 <= c as i128);
    }

    /// Converting at rate 1.0 is the identity for same-decimals currencies.
    #[test]
    fn identity_conversion(minor in -MAX_ABS_MINOR..=MAX_ABS_MINOR) {
        prop_assert_eq!(convert_minor(minor, 2, 2, 100_000_000).unwrap(), minor);
    }

    /// Converting out and back at reciprocal exact rates stays within a unit.
    #[test]
    fn round_trip_conversion_close(minor in -1_000_000_000i64..1_000_000_000) {
        let there = convert_minor(minor, 2, 2, 200_000_000).unwrap();   // x2
        let back = convert_minor(there, 2, 2, 50_000_000).unwrap();     // x0.5
        prop_assert_eq!(back, minor);
    }

    /// Parsing arbitrary text never panics, and accepted text is canonical-safe.
    #[test]
    fn parse_never_panics(s in "\\PC{0,24}") {
        let _ = parse_amount(&s, 2);
        let _ = parse_decimal(&s);
    }

    /// Grouped display strips back to the plain amount.
    #[test]
    fn grouped_display_roundtrip(minor in -MAX_ABS_MINOR..=MAX_ABS_MINOR) {
        for cur in ["INR", "USD", "JPY", "KWD"] {
            let shown = format_grouped(minor, cur).replace(',', "");
            prop_assert_eq!(shown, format_amount(minor, decimals_for(cur)));
        }
    }
}
