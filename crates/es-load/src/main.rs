//! es-load — fast parallel NDJSON -> Elasticsearch `_bulk` loader (Rust, no GIL).
//!
//! For LOCAL ES write-ceiling benchmarking + tuning: reads decoded action/delta NDJSON (produced
//! by action-proto / delta-proto) and POSTs it to a LOCAL Elasticsearch with N parallel posters,
//! applying the Hyperion `_id`/`_index` rules. Co-locate this with the ES under test (same box /
//! LAN) so the measured docs/s reflects ES, not the loader or a WAN link. The Python `bulk-load.py`
//! is GIL-bound (~137k docs/s single-process); this saturates ES instead.
//!
//! SAFETY: refuses a non-loopback ES host unless BENCH_ALLOW_EXTERNAL_ES=1 — local benchmarking only.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use hyperion_ship::sequence::sequence_from_json;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use serde_json::value::{RawValue, to_raw_value};

#[derive(Parser, Debug)]
#[command(about = "Fast parallel NDJSON -> Elasticsearch _bulk loader (local benchmarking).")]
struct Args {
    /// NDJSON file of decoded docs (action or delta).
    #[arg(long)]
    file: String,
    /// Elasticsearch base URL (loopback only unless BENCH_ALLOW_EXTERNAL_ES=1).
    #[arg(long, default_value = "http://localhost:9200")]
    es: String,
    /// "action" (_id = global_sequence) or "delta" (_id = block-code-scope-table-pk).
    #[arg(long, default_value = "action")]
    mode: String,
    #[arg(long, default_value = "wax")]
    chain: String,
    #[arg(long, default_value = "v1")]
    index_version: String,
    #[arg(long, default_value_t = 10_000_000)]
    partition_size: u64,
    /// docs per `_bulk` request.
    #[arg(long, default_value_t = 4000)]
    batch: usize,
    /// concurrent poster threads.
    #[arg(long, default_value_t = 8)]
    workers: usize,
}

fn partition(block: u64, size: u64) -> String {
    format!("{:06}", block.div_ceil(size.max(1)).max(1))
}

/// A metadata object that refuses duplicate keys instead of letting `_id` and body interpreters
/// disagree. Values are raw JSON: contract data is never parsed or reserialized by normalization.
struct MetadataObject(BTreeMap<String, Box<RawValue>>);

impl<'de> Deserialize<'de> for MetadataObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = MetadataObject;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a metadata object with unique keys")
            }

            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate metadata key"));
                    }
                }
                Ok(MetadataObject(fields))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

fn normalize_sequence(fields: &mut BTreeMap<String, Box<RawValue>>, key: &str) -> Result<u64> {
    let raw = fields.get(key).with_context(|| format!("missing {key}"))?;
    let value: Value = serde_json::from_str(raw.get())?;
    let sequence = sequence_from_json(&value)
        .with_context(|| format!("{key} needs canonical uint64 text or a safe legacy integer"))?;
    fields.insert(key.to_owned(), to_raw_value(&sequence.to_string())?);
    Ok(sequence)
}

fn normalize_receipt(raw: &RawValue) -> Result<Box<RawValue>> {
    let MetadataObject(mut fields) = serde_json::from_str(raw.get())?;
    normalize_sequence(&mut fields, "global_sequence")?;
    normalize_sequence(&mut fields, "recv_sequence")?;
    if let Some(auth) = fields.get("auth_sequence") {
        let entries: Vec<Box<RawValue>> = serde_json::from_str(auth.get())?;
        let mut normalized = Vec::with_capacity(entries.len());
        for entry in entries {
            let MetadataObject(mut item) = serde_json::from_str(entry.get())?;
            normalize_sequence(&mut item, "sequence")?;
            normalized.push(to_raw_value(&item)?);
        }
        fields.insert("auth_sequence".into(), to_raw_value(&normalized)?);
    }
    to_raw_value(&fields).map_err(Into::into)
}

/// Validate the exact ID and rewrite only known sequence metadata. All unrelated raw values,
/// including arbitrary contract integer/float/exponent tokens, are forwarded byte-for-byte.
fn prepare_line(
    mode: &str,
    line: &str,
    chain: &str,
    ver: &str,
    size: u64,
) -> Result<(String, String, String)> {
    let MetadataObject(mut fields) = serde_json::from_str(line)?;
    let block: Value =
        serde_json::from_str(fields.get("block_num").context("missing block_num")?.get())?;
    let block = block.as_u64().context("block_num must be numeric uint64")?;
    let part = partition(block, size);
    if mode == "delta" {
        let get = |key: &str| -> Result<String> {
            let raw = fields.get(key).with_context(|| format!("missing {key}"))?;
            serde_json::from_str(raw.get()).map_err(Into::into)
        };
        let id = format!(
            "{}-{}-{}-{}-{}",
            block,
            get("code")?,
            get("scope")?,
            get("table")?,
            get("primary_key")?
        );
        Ok((format!("{chain}-delta-{ver}-{part}"), id, line.to_owned()))
    } else {
        let id = normalize_sequence(&mut fields, "global_sequence")?.to_string();
        if let Some(receipts) = fields.get("receipts") {
            let receipts: Vec<Box<RawValue>> = serde_json::from_str(receipts.get())?;
            let normalized: Result<Vec<_>> =
                receipts.iter().map(|r| normalize_receipt(r)).collect();
            fields.insert("receipts".into(), to_raw_value(&normalized?)?);
        }
        if let Some(receipt) = fields.get("receipt") {
            let normalized = normalize_receipt(receipt)?;
            fields.insert("receipt".into(), normalized);
        }
        Ok((
            format!("{chain}-action-{ver}-{part}"),
            id,
            serde_json::to_string(&fields)?,
        ))
    }
}

fn guard_local(es: &str) -> Result<()> {
    let host = es
        .split("://")
        .last()
        .unwrap_or(es)
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1" | "");
    if !local && std::env::var("BENCH_ALLOW_EXTERNAL_ES").as_deref() != Ok("1") {
        bail!(
            "REFUSING: ES host '{host}' is not loopback — local benchmarking only (set BENCH_ALLOW_EXTERNAL_ES=1 to override)."
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    guard_local(&args.es)?;
    let url = format!("{}/_bulk", args.es.trim_end_matches('/'));
    let reader = Arc::new(Mutex::new(BufReader::with_capacity(
        8 << 20,
        File::open(&args.file)?,
    )));
    let (docs, bytes, reqs, errors, rejected) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    );
    eprintln!(
        "[es-load] {} -> {} | mode={} index={}-{}-{}-<part> | batch={} workers={}",
        args.file,
        url,
        args.mode,
        args.chain,
        args.mode,
        args.index_version,
        args.batch,
        args.workers
    );
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..args.workers.max(1) {
            let (reader, docs, bytes, reqs, errors, rejected, url, args) = (
                reader.clone(),
                docs.clone(),
                bytes.clone(),
                reqs.clone(),
                errors.clone(),
                rejected.clone(),
                url.clone(),
                &args,
            );
            s.spawn(move || {
                let mut body = String::with_capacity(args.batch * 256);
                loop {
                    // Drain a batch of raw lines under the lock (fast in-memory reads), parse+post outside it.
                    let mut lines: Vec<String> = Vec::with_capacity(args.batch);
                    {
                        let mut r = reader.lock().unwrap();
                        let mut buf = String::new();
                        while lines.len() < args.batch {
                            buf.clear();
                            match r.read_line(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    if !buf.trim().is_empty() {
                                        lines.push(std::mem::take(&mut buf));
                                    }
                                }
                            }
                        }
                    }
                    if lines.is_empty() {
                        break;
                    }
                    body.clear();
                    let mut n = 0u64;
                    for line in &lines {
                        let line = line.trim_end();
                        let (idx, id, doc) = match prepare_line(
                            &args.mode,
                            line,
                            &args.chain,
                            &args.index_version,
                            args.partition_size,
                        ) {
                            Ok(prepared) => prepared,
                            Err(error) => {
                                if rejected.fetch_add(1, Relaxed) < 16 {
                                    eprintln!("[es-load] rejected input document: {error}");
                                }
                                continue;
                            }
                        };
                        body.push_str("{\"index\":{\"_index\":\"");
                        body.push_str(&idx);
                        body.push_str("\",\"_id\":\"");
                        body.push_str(&id);
                        body.push_str("\"}}\n");
                        body.push_str(&doc);
                        body.push('\n');
                        n += 1;
                    }
                    if n == 0 {
                        continue;
                    }
                    bytes.fetch_add(body.len() as u64, Relaxed);
                    let ok = match minreq::post(&url)
                        .with_header("Content-Type", "application/x-ndjson")
                        .with_body(body.as_str())
                        .send()
                    {
                        Ok(resp) => {
                            (200..300).contains(&resp.status_code)
                                && resp
                                    .as_str()
                                    .map(|s| !s.contains("\"errors\":true"))
                                    .unwrap_or(false)
                        }
                        Err(_) => false,
                    };
                    if !ok {
                        errors.fetch_add(1, Relaxed);
                    }
                    docs.fetch_add(n, Relaxed);
                    reqs.fetch_add(1, Relaxed);
                }
            });
        }
    });
    let dt = t0.elapsed().as_secs_f64().max(1e-9);
    let (d, mb) = (docs.load(Relaxed), bytes.load(Relaxed) as f64 / 1e6);
    eprintln!(
        "[es-load] {d} docs in {dt:.1}s -> {:.0} docs/s | {mb:.1} MB ({:.1} MB/s) | {} bulk reqs | {} workers | errors={}",
        d as f64 / dt,
        mb / dt,
        reqs.load(Relaxed),
        args.workers,
        errors.load(Relaxed),
    );
    let rejected = rejected.load(Relaxed);
    if rejected != 0 {
        bail!("{rejected} input document(s) were rejected; no rejected document was posted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_literal_sequence_ids_and_unsigned_boundaries() {
        for text in [
            "0",
            "1",
            "9007199254740991",
            "9007199254740992",
            "9007199254740993",
            "9223372036854775807",
            "9223372036854775808",
            "9223372036854775809",
            "18446744073709551615",
        ] {
            let line = format!(
                r#"{{"block_num":42,"global_sequence":"{text}","receipts":[{{"global_sequence":"{text}","recv_sequence":"{text}","auth_sequence":[{{"account":"alice","sequence":"{text}"}}]}}]}}"#
            );
            let (index, id, doc) = prepare_line("action", &line, "wax", "v1", 10_000_000).unwrap();
            assert_eq!(index, "wax-action-v1-000001");
            assert_eq!(id, text);
            let doc: Value = serde_json::from_str(&doc).unwrap();
            assert_eq!(doc["global_sequence"].as_str(), Some(text));
            assert_eq!(doc["receipts"][0]["global_sequence"].as_str(), Some(text));
            assert_eq!(doc["receipts"][0]["recv_sequence"].as_str(), Some(text));
            assert_eq!(
                doc["receipts"][0]["auth_sequence"][0]["sequence"].as_str(),
                Some(text)
            );
        }
    }

    #[test]
    fn safe_legacy_numbers_normalize_without_rewriting_opaque_contract_values() {
        // Even tokens beyond serde Value's float range are valid opaque JSON and remain untouched.
        let act = r#"{"data" : {"global_sequence":18446744073709551615,"float":1.23456789012345678901234567890,"exponent":1e309,"padding":1.0000,"dup":1,"dup":2}}"#;
        let line = format!(
            r#"{{"block_num":42,"global_sequence":9007199254740991,"action_ordinal":7,"code_sequence":4294967295,"abi_sequence":4294967295,"act":{act},"receipts":[{{"global_sequence":9007199254740991,"recv_sequence":1,"auth_sequence":[{{"account":"alice","sequence":0}}]}}]}}"#
        );
        let (_, id, output) = prepare_line("action", &line, "wax", "v1", 100).unwrap();
        assert_eq!(id, "9007199254740991");
        let MetadataObject(fields) = serde_json::from_str(&output).unwrap();
        assert_eq!(fields["act"].get(), act);
        assert_eq!(fields["global_sequence"].get(), "\"9007199254740991\"");
        assert_eq!(fields["action_ordinal"].get(), "7");
        assert_eq!(fields["code_sequence"].get(), "4294967295");
        assert_eq!(fields["abi_sequence"].get(), "4294967295");
        let receipts: Value = serde_json::from_str(fields["receipts"].get()).unwrap();
        assert_eq!(receipts[0]["recv_sequence"], "1");
        assert_eq!(receipts[0]["auth_sequence"][0]["sequence"], "0");
    }

    #[test]
    fn rejects_unsafe_or_malformed_sequence_tokens_at_every_owned_path() {
        for raw in [
            "9007199254740992",
            "9007199254740993",
            "18446744073709551615",
            "-1",
            "-0",
            "1.0",
            "1e3",
            "true",
            "null",
            "[]",
            "{}",
            "\"01\"",
            "\"+1\"",
            "\" 1\"",
            "\"1 \"",
            "\"1e3\"",
            "\"18446744073709551616\"",
        ] {
            for line in [
                format!(r#"{{"block_num":42,"global_sequence":{raw}}}"#),
                format!(
                    r#"{{"block_num":42,"global_sequence":"1","receipts":[{{"global_sequence":{raw},"recv_sequence":"1"}}]}}"#
                ),
                format!(
                    r#"{{"block_num":42,"global_sequence":"1","receipt":{{"global_sequence":"1","recv_sequence":{raw}}}}}"#
                ),
                format!(
                    r#"{{"block_num":42,"global_sequence":"1","receipts":[{{"global_sequence":"1","recv_sequence":"1","auth_sequence":[{{"account":"alice","sequence":{raw}}}]}}]}}"#
                ),
            ] {
                assert!(
                    prepare_line("action", &line, "wax", "v1", 100).is_err(),
                    "accepted {line}"
                );
            }
        }
    }

    #[test]
    fn rejects_ambiguous_duplicate_metadata_keys() {
        for line in [
            r#"{"block_num":42,"global_sequence":"1","global_sequence":"2"}"#,
            r#"{"block_num":42,"global_sequence":"1","global_\u0073equence":"2"}"#,
            r#"{"block_num":42,"global_sequence":"1","receipts":[{"global_sequence":"1","global_sequence":"2","recv_sequence":"1"}]}"#,
            r#"{"block_num":42,"global_sequence":"1","receipts":[{"global_sequence":"1","recv_sequence":"1","auth_sequence":[{"sequence":"1","sequence":"2"}]}]}"#,
        ] {
            assert!(
                prepare_line("action", line, "wax", "v1", 100).is_err(),
                "accepted {line}"
            );
        }
    }

    #[test]
    fn delta_document_and_key_semantics_are_unchanged() {
        let line = r#"{ "block_num":42,"code":"eosio.token","scope":"alice","table":"accounts","primary_key":"18446744073709551615","data":{"global_sequence":18446744073709551615,"amount":1.00000}}"#;
        let (index, id, output) = prepare_line("delta", line, "wax", "v1", 100).unwrap();
        assert_eq!(index, "wax-delta-v1-000001");
        assert_eq!(id, "42-eosio.token-alice-accounts-18446744073709551615");
        assert_eq!(output, line);
    }
}
