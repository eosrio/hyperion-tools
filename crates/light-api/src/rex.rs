//! REX balance math: convert an account's REX position (`eosio-rexbal` + `eosio-rexfund`) into the
//! cc32d9 `{fund, matured, maturing, savings}` token amounts using the global `eosio-rexpool` rate.
//!
//! REX → token rate is `total_lendable / total_rex` (both from the rexpool singleton). An account's
//! REX is split across maturity buckets:
//!   - `matured_rex` plus any non-savings bucket whose time ≤ now → **matured**
//!   - non-savings buckets whose time > now → **maturing**
//!   - the far-future "savings" sentinel bucket → **savings**
//!
//! NOTE: the savings-sentinel detection (a maturity year ≥ 2100) follows eosio.system's convention of
//! parking saved REX at a maximal timestamp; verify against a live cc32d9 instance for the target
//! chain, as some forks tweak this.

use crate::asset::{self, Asset};

/// Global REX pool rate inputs (from the `eosio-rexpool` singleton).
#[derive(Debug, Clone)]
pub struct RexPool {
    pub total_lendable: Asset,
    pub total_rex: Asset,
}

/// One account's REX position.
#[derive(Debug, Clone, Default)]
pub struct RexInput {
    /// `eosio-rexfund.balance` (already in the system token).
    pub fund: Option<Asset>,
    /// `rexbal.matured_rex`, in REX integer units.
    pub matured_rex: i128,
    /// `rexbal.rex_maturities`: `(unix_seconds, rex_units)` pairs.
    pub maturities: Vec<(i64, i128)>,
    pub pool: Option<RexPool>,
}

/// The four cc32d9 REX figures, as formatted asset strings in the system token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RexOut {
    pub fund: String,
    pub matured: String,
    pub maturing: String,
    pub savings: String,
}

/// Maturities at or beyond this instant are treated as the savings sentinel.
fn savings_sentinel() -> i64 {
    // 2100-01-01T00:00:00Z
    4_102_444_800
}

impl RexInput {
    /// Convert a REX integer-unit amount into token integer units via the pool rate (0 if no pool).
    fn rex_to_token(&self, rex_units: i128) -> i128 {
        match &self.pool {
            Some(p) if p.total_rex.units > 0 => {
                rex_units.saturating_mul(p.total_lendable.units) / p.total_rex.units
            }
            _ => 0,
        }
    }
}

/// Compute `{fund, matured, maturing, savings}` for `now` (unix seconds), formatted in the system
/// token (`symbol`/`precision`).
pub fn compute(input: &RexInput, now: i64, symbol: &str, precision: u32) -> RexOut {
    let sentinel = savings_sentinel();
    let mut matured_rex = input.matured_rex;
    let mut maturing_rex: i128 = 0;
    let mut savings_rex: i128 = 0;

    for &(secs, units) in &input.maturities {
        if secs >= sentinel {
            savings_rex += units;
        } else if secs <= now {
            matured_rex += units;
        } else {
            maturing_rex += units;
        }
    }

    let fmt = |units: i128| asset::format_asset(input.rex_to_token(units), precision, symbol);
    let fund = input
        .fund
        .as_ref()
        .map(|a| asset::format_asset(a.units, a.precision, &a.symbol))
        .unwrap_or_else(|| asset::format_asset(0, precision, symbol));

    RexOut {
        fund,
        matured: fmt(matured_rex),
        maturing: fmt(maturing_rex),
        savings: fmt(savings_rex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(lendable: i128, rex: i128) -> RexPool {
        RexPool {
            total_lendable: Asset {
                units: lendable,
                precision: 4,
                symbol: "EOS".into(),
            },
            total_rex: Asset {
                units: rex,
                precision: 4,
                symbol: "REX".into(),
            },
        }
    }

    #[test]
    fn empty_position_is_all_zero() {
        let out = compute(&RexInput::default(), 1000, "EOS", 4);
        assert_eq!(out.matured, "0.0000 EOS");
        assert_eq!(out.maturing, "0.0000 EOS");
        assert_eq!(out.savings, "0.0000 EOS");
        assert_eq!(out.fund, "0.0000 EOS");
    }

    #[test]
    fn splits_matured_maturing_savings() {
        // Rate 1:1 (lendable == total_rex) keeps the arithmetic obvious.
        let input = RexInput {
            fund: Some(Asset {
                units: 12345,
                precision: 4,
                symbol: "EOS".into(),
            }),
            matured_rex: 10000,
            maturities: vec![
                (500, 5000),                // <= now → matured
                (2000, 7000),               // > now  → maturing
                (savings_sentinel(), 9000), // sentinel → savings
            ],
            pool: Some(pool(1_000_000, 1_000_000)),
        };
        let out = compute(&input, 1000, "EOS", 4);
        assert_eq!(out.fund, "1.2345 EOS");
        assert_eq!(out.matured, "1.5000 EOS"); // 10000 + 5000
        assert_eq!(out.maturing, "0.7000 EOS"); // 7000
        assert_eq!(out.savings, "0.9000 EOS"); // 9000
    }

    #[test]
    fn applies_pool_rate() {
        // 2 EOS lendable per 1 REX → doubles token value.
        let input = RexInput {
            matured_rex: 10000,
            pool: Some(pool(2_000_000, 1_000_000)),
            ..Default::default()
        };
        let out = compute(&input, 0, "EOS", 4);
        assert_eq!(out.matured, "2.0000 EOS");
    }
}
