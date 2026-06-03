//! Antelope asset-string handling: `"<amount> <SYMBOL>"` ↔ integer units.
//!
//! An asset like `"1.0000 EOS"` has precision 4 and integer-unit value 10000. The cc32d9 API returns
//! stake/resource weights as integer units (`net_weight: 10000`) and balances as the decimal string
//! plus a separate `decimals`. These helpers parse and format both.

/// A parsed asset: integer-unit `amount`, decimal `precision`, and `symbol` code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// Value in integer units (amount × 10^precision). `i128` to tolerate large supplies safely.
    pub units: i128,
    pub precision: u32,
    pub symbol: String,
}

/// Parse `"<amount> <SYMBOL>"`. Returns `None` on any malformation.
pub fn parse(s: &str) -> Option<Asset> {
    let (num, sym) = s.trim().split_once(' ')?;
    if sym.is_empty() || sym.len() > 7 || !sym.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let (neg, num) = match num.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, num),
    };
    let (int_part, frac_part) = match num.split_once('.') {
        Some((i, f)) => (i, f),
        None => (num, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits: String = format!("{int_part}{frac_part}");
    let mut units: i128 = digits.parse().ok()?;
    if neg {
        units = -units;
    }
    Some(Asset {
        units,
        precision: frac_part.len() as u32,
        symbol: sym.to_string(),
    })
}

/// Integer-unit value of an asset string, ignoring symbol/precision (for weight sums). `None` if
/// unparseable.
pub fn units(s: &str) -> Option<i128> {
    parse(s).map(|a| a.units)
}

/// Format integer `units` at `precision` back into a decimal string (no symbol), preserving trailing
/// zeros — e.g. `(10000, 4)` → `"1.0000"`, `(5, 4)` → `"0.0005"`.
pub fn format_units(units: i128, precision: u32) -> String {
    if precision == 0 {
        return units.to_string();
    }
    let neg = units < 0;
    let mag = units.unsigned_abs();
    let scale = 10u128.pow(precision);
    let int = mag / scale;
    let frac = mag % scale;
    let sign = if neg { "-" } else { "" };
    format!("{sign}{int}.{frac:0width$}", width = precision as usize)
}

/// Format an integer-unit value with its symbol — e.g. `(10000, 4, "EOS")` → `"1.0000 EOS"`.
pub fn format_asset(units: i128, precision: u32, symbol: &str) -> String {
    format!("{} {}", format_units(units, precision), symbol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical() {
        let a = parse("1.0000 EOS").unwrap();
        assert_eq!(a.units, 10000);
        assert_eq!(a.precision, 4);
        assert_eq!(a.symbol, "EOS");
    }

    #[test]
    fn parses_big_supply_precisely() {
        // 454_751_989.2767 EOS — would lose precision as f64; i128 keeps it exact.
        let a = parse("454751989.2767 EOS").unwrap();
        assert_eq!(a.units, 4_547_519_892_767);
        assert_eq!(a.precision, 4);
    }

    #[test]
    fn parses_zero_precision_and_negative() {
        assert_eq!(parse("1 WRAM").unwrap().units, 1);
        assert_eq!(parse("1 WRAM").unwrap().precision, 0);
        assert_eq!(parse("-5.0 TLOS").unwrap().units, -50);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_none());
        assert!(parse("1.0000").is_none()); // no symbol
        assert!(parse("abc EOS").is_none());
        assert!(parse("1.0 lower").is_none()); // symbol must be uppercase
        assert!(parse("1.0 TOOLONGS").is_none()); // symbol > 7
    }

    #[test]
    fn formats_round_trip() {
        assert_eq!(format_units(10000, 4), "1.0000");
        assert_eq!(format_units(5, 4), "0.0005");
        assert_eq!(format_units(0, 4), "0.0000");
        assert_eq!(format_units(-50, 4), "-0.0050");
        assert_eq!(format_units(7, 0), "7");
        assert_eq!(format_asset(10000, 4, "EOS"), "1.0000 EOS");
    }
}
