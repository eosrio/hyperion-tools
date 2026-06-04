//! Validate the atomicdata decoder against real on-chain cases.
//!
//! Reads a JSON array of cases from stdin and decodes each, comparing to the expected output:
//!   [{ "label": "...", "format": [{"name":..,"type":..},...],
//!      "serialized_hex": "ab12..", "expected": { ...decoded data... } }, ...]
//!
//! Cases are assembled from a live AtomicAssets deployment: `format` + `expected` from the
//! eosio-contract-api Postgres (its decoded `immutable_data`/`mutable_data`), `serialized_hex` from the
//! chain (`get_table_rows` raw `*_serialized_data`). A mismatch means our decoder diverges from
//! atomicassets-js on real data.
//!
//!   cargo run -p atomicdata --example validate < cases.json

use atomicdata::{deserialize, Field};
use serde_json::{Map, Value};
use std::io::Read;

fn main() -> anyhow::Result<()> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    let cases: Vec<Value> = serde_json::from_str(&s)?;

    let (mut pass, mut fail) = (0usize, 0usize);
    for (i, c) in cases.iter().enumerate() {
        let label = c.get("label").and_then(Value::as_str).unwrap_or("");
        let format = match Field::from_format_json(c.get("format").unwrap_or(&Value::Null)) {
            Ok(f) => f,
            Err(e) => {
                fail += 1;
                eprintln!("BAD FORMAT #{i} {label}: {e}");
                continue;
            }
        };
        let bytes = hex_decode(
            c.get("serialized_hex")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let expected = c.get("expected").cloned().unwrap_or(Value::Null);

        match deserialize(&bytes, &format) {
            Ok(attrs) => {
                let got = Value::Object(attrs.into_iter().collect::<Map<String, Value>>());
                if got == expected {
                    pass += 1;
                } else {
                    fail += 1;
                    eprintln!("MISMATCH #{i} {label}");
                    eprintln!("  got = {}", trunc(&got));
                    eprintln!("  exp = {}", trunc(&expected));
                    diff_keys(&got, &expected);
                }
            }
            Err(e) => {
                fail += 1;
                eprintln!("ERROR #{i} {label}: {e}");
            }
        }
    }

    println!(
        "\nvalidate: {pass} pass / {fail} fail  ({} cases)",
        cases.len()
    );
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Show only the keys that differ, to make a mismatch readable.
fn diff_keys(got: &Value, exp: &Value) {
    if let (Some(g), Some(e)) = (got.as_object(), exp.as_object()) {
        for (k, ev) in e {
            match g.get(k) {
                Some(gv) if gv == ev => {}
                Some(gv) => eprintln!("    key '{k}': got {} != exp {}", trunc(gv), trunc(ev)),
                None => eprintln!("    key '{k}': MISSING (exp {})", trunc(ev)),
            }
        }
        for k in g.keys() {
            if !e.contains_key(k) {
                eprintln!("    key '{k}': EXTRA (got {})", trunc(g.get(k).unwrap()));
            }
        }
    }
}

fn trunc(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 200 {
        format!("{}…", &s[..200])
    } else {
        s
    }
}

fn hex_decode(s: &str) -> Vec<u8> {
    let h: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    h.chunks(2)
        .filter_map(|p| {
            if p.len() < 2 {
                return None;
            }
            let hi = (p[0] as char).to_digit(16)?;
            let lo = (p[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}
