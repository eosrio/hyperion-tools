//! Independent, sparse Wire ABI deployment history from frozen, index-addressed logs.
//!
//! A manifest is published only after every source block, output and identity check succeeds.
//! Its block membership sidecar is 32 bytes per block; no per-block database records are needed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::binio::{FcReader, FileCache};
use crate::catalog::{Catalog, discover, ship_catalog_within};
use crate::provider::{ProtocolProvider, WIRE_SOURCE_REVISION};
use crate::shiplog::{LogKind, ShipLog};

pub const FORMAT: &str = "hyperion-wire-abi-history-v1";
const MAX_PAYLOAD: u64 = 16 * 1024 * 1024;

pub struct ScanOptions {
    pub directory: PathBuf,
    pub schema: String,
    pub chain_id: String,
    pub end: u32,
    pub irreversible_block_id: String,
    pub output: PathBuf,
    pub provider: ProtocolProvider,
}

#[derive(Clone)]
struct AccountState {
    present: bool,
    hex: String,
}

struct Action {
    account: String,
    sequence: u64,
    abi_sequence: Option<u64>,
    ordinal: u32,
    transaction: String,
    deployment: Option<(String, String)>,
}

fn hash(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}
fn hex_id(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid lowercase chain/block id"
    );
    hex::decode(value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid id length"))
}
fn number(value: &Value, field: &str) -> Result<u64> {
    if let Some(n) = value.as_u64() {
        return Ok(n);
    }
    let s = value.as_str().with_context(|| format!("missing {field}"))?;
    let n = s
        .parse::<u64>()
        .with_context(|| format!("invalid {field}"))?;
    ensure!(n.to_string() == s, "noncanonical {field}");
    Ok(n)
}
fn variant(value: &Value) -> Result<(&str, &Value)> {
    let row = value.as_array().context("expected Wire variant")?;
    ensure!(row.len() == 2, "invalid Wire variant length");
    Ok((
        row[0].as_str().context("invalid Wire variant name")?,
        &row[1],
    ))
}
fn suffix(path: &Path, tail: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(tail);
    name.into()
}
fn fresh(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create fresh output {}", path.display()))
}
fn publish(from: &Path, to: &Path) -> Result<()> {
    // Unlike rename on Unix, hard_link cannot overwrite another run's completed artifact.
    fs::hard_link(from, to).with_context(|| format!("publish fresh output {}", to.display()))?;
    fs::remove_file(from)?;
    Ok(())
}
fn file_hash(path: &Path) -> Result<(u64, String)> {
    let mut input = BufReader::new(File::open(path)?);
    let mut state = Sha256::new();
    let mut bytes = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        state.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((bytes, hex::encode(state.finalize())))
}
fn basename(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|v| v.to_str())
        .context("output filename must be UTF-8")
}

fn open_catalog(
    paths: &[crate::catalog::PathPair],
    kind: LogKind,
    cache: &Arc<FileCache>,
    end: u32,
) -> Result<Catalog<ShipLog>> {
    let (catalog, findings) = ship_catalog_within(paths, kind, cache, Some((1, end + 1)))?;
    for finding in findings {
        eprintln!("[wire-disk] {finding}");
    }
    catalog.with_context(|| format!("missing {} logs", kind.label()))
}
fn payload(catalog: &Catalog<ShipLog>, block: u32, out: &mut Vec<u8>) -> Result<[u8; 32]> {
    let log = catalog
        .get(block)
        .with_context(|| format!("missing source block {block}"))?;
    let header = log.header(block)?;
    log.read_inline(&header, MAX_PAYLOAD, out)?;
    Ok(header.id)
}

fn account_rows(
    provider: &ProtocolProvider,
    schema: &Value,
    bytes: &[u8],
) -> Result<Vec<(String, AccountState, u32)>> {
    // Wire emits deltas in a schema-selected order; never assume `account` is the first table.
    let payload = provider.decode_delta_payload(schema, bytes)?;
    let deltas = &payload["deltas"];
    let mut out = Vec::new();
    let mut ordinal = 0u32;
    for delta in deltas.as_array().context("invalid Wire deltas")? {
        let (kind, delta) = variant(delta)?;
        ensure!(kind == "table_delta_v0", "unsupported Wire table delta");
        let stream = delta["name"]
            .as_str()
            .context("missing Wire delta stream")?;
        for row in delta["rows"]
            .as_array()
            .context("invalid Wire delta rows")?
        {
            let current = ordinal;
            ordinal = ordinal.checked_add(1).context("delta ordinal overflow")?;
            if stream != "account" {
                continue;
            }
            let present = number(&row["present"], "delta presence")?;
            ensure!(present <= 1, "invalid delta presence");
            let raw = hex::decode(row["data"].as_str().context("missing account row bytes")?)?;
            let decoded = provider.decode_value(schema, "account", &raw)?;
            let (kind, account) = variant(&decoded)?;
            ensure!(kind == "account_v0", "unsupported Wire account row");
            let name = account["name"]
                .as_str()
                .context("missing account name")?
                .to_owned();
            let raw = account["abi"]
                .as_str()
                .context("missing account ABI bytes")?;
            hex::decode(raw).context("invalid account ABI hex")?;
            out.push((
                name,
                AccountState {
                    present: present == 1,
                    hex: if present == 1 {
                        raw.to_owned()
                    } else {
                        String::new()
                    },
                },
                current,
            ));
        }
    }
    Ok(out)
}

fn setabi(data: &str) -> Result<(String, String)> {
    let bytes = hex::decode(data).context("invalid setabi bytes")?;
    let mut cursor = FcReader::new(&bytes);
    let account = cursor.u64()?;
    let n = cursor.varuint32()? as usize;
    let abi = hex::encode(cursor.bytes(n)?);
    ensure!(cursor.remaining() == 0, "trailing setabi bytes");
    let name = rs_abieos::Abieos::new()
        .name_to_string(account)
        .map_err(|e| anyhow::anyhow!("invalid setabi account: {e:?}"))?;
    Ok((name, abi))
}

fn actions(provider: &ProtocolProvider, schema: &Value, bytes: &[u8]) -> Result<Vec<Action>> {
    let traces = provider.decode_value(schema, "transaction_trace[]", bytes)?;
    let mut out = Vec::new();
    for tx in traces.as_array().context("invalid Wire traces")? {
        let (kind, tx) = variant(tx)?;
        ensure!(
            kind == "transaction_trace_v0",
            "unsupported Wire transaction trace"
        );
        // Match the native observer's proven transaction scope. In particular,
        // soft_fail may contain committed onerror effects: silently skipping it
        // would manufacture complete coverage while losing ABI deployments.
        ensure!(
            number(&tx["status"], "transaction status")? == 0,
            "unsupported_feature: Wire disk history requires executed transaction traces (soft_fail/onerror and other statuses require qualification)"
        );
        ensure!(
            (tx["scheduled"].as_bool() == Some(false) || tx["scheduled"].as_u64() == Some(0))
                && tx["failed_dtrx_trace"].is_null(),
            "unsupported_feature: deferred Wire transaction trace requires qualification"
        );
        // `partial_transaction_v0` is transaction header/signature metadata, not
        // truncated action coverage. It is present in the retained real capture.
        if !tx["partial"].is_null() {
            ensure!(
                variant(&tx["partial"])?.0 == "partial_transaction_v0",
                "unsupported Wire transaction metadata"
            );
        }
        ensure!(
            tx["except"].is_null() && tx["error_code"].is_null(),
            "executed transaction has failure metadata"
        );
        let transaction = tx["id"]
            .as_str()
            .context("missing transaction id")?
            .to_owned();
        hex_id(&transaction)?;
        for action in tx["action_traces"]
            .as_array()
            .context("invalid Wire action traces")?
        {
            let (kind, action) = variant(action)?;
            ensure!(
                matches!(kind, "action_trace_v0" | "action_trace_v1"),
                "unsupported Wire action trace"
            );
            if action["receipt"].is_null() {
                ensure!(
                    action["act"]["account"] != "sysio" || action["act"]["name"] != "setabi",
                    "setabi action lacks a committed receipt"
                );
                continue;
            }
            let (kind, receipt) = variant(&action["receipt"])?;
            ensure!(
                kind == "action_receipt_v0",
                "unsupported Wire action receipt"
            );
            let sequence = number(&receipt["global_sequence"], "global sequence")?;
            let account = action["act"]["account"]
                .as_str()
                .context("missing action account")?
                .to_owned();
            let name = action["act"]["name"]
                .as_str()
                .context("missing action name")?
                .to_owned();
            let primary = action["receiver"] == action["act"]["account"]
                && receipt["receiver"] == action["act"]["account"];
            // A notification uses its original action's ABI; only primary receipts check its revision.
            if !primary {
                out.push(Action {
                    account,
                    sequence,
                    abi_sequence: None,
                    ordinal: 0,
                    transaction: transaction.clone(),
                    deployment: None,
                });
                continue;
            }
            ensure!(
                action["except"].is_null() && action["error_code"].is_null(),
                "receipted action has failure metadata"
            );
            let ordinal = u32::try_from(number(&action["action_ordinal"], "action ordinal")?)?;
            ensure!(ordinal > 0, "zero action ordinal");
            let abi_sequence = number(&receipt["abi_sequence"], "ABI sequence")?;
            let deployment = if account == "sysio" && name == "setabi" {
                Some(setabi(
                    action["act"]["data"]
                        .as_str()
                        .context("missing setabi data")?,
                )?)
            } else {
                None
            };
            out.push(Action {
                account,
                sequence,
                abi_sequence: Some(abi_sequence),
                ordinal,
                transaction: transaction.clone(),
                deployment,
            });
        }
    }
    out.sort_by_key(|a| a.sequence);
    ensure!(
        out.windows(2).all(|w| w[0].sequence < w[1].sequence),
        "duplicate or unordered global action receipts"
    );
    Ok(out)
}

fn abi_fields(record: &mut Value, provider: &ProtocolProvider, state: &AccountState) {
    if !state.present || state.hex.is_empty() {
        return;
    }
    match hex::decode(&state.hex)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| provider.decode_abi(&bytes))
    {
        Ok(abi) => record["abi"] = abi,
        Err(error) => record["abiDecodeError"] = json!(format!("{error:#}")),
    }
}
fn emit(out: &mut impl Write, record: &Value, count: &mut u64) -> Result<()> {
    serde_json::to_writer(&mut *out, record)?;
    out.write_all(b"\n")?;
    *count += 1;
    Ok(())
}

/// Scan the complete genesis-to-irreversible prefix. Inputs must be a stable frozen view.
/// Partial artifacts are retained for diagnosis; only a sealed manifest authorizes consumption.
pub fn scan(options: &ScanOptions) -> Result<Value> {
    options.provider.validate()?;
    let ProtocolProvider::Wire(profile) = &options.provider else {
        bail!("Wire ABI history requires an explicit Wire profile");
    };
    ensure!(
        options.end > 0 && options.end < u32::MAX,
        "invalid Wire history end"
    );
    hex_id(&options.chain_id)?;
    let irreversible = hex_id(&options.irreversible_block_id)?;
    ensure!(
        u32::from_be_bytes(irreversible[..4].try_into()?) == options.end,
        "irreversible id does not identify end block"
    );
    let schema: Value =
        serde_json::from_str(&options.schema).context("invalid Wire SHiP schema")?;
    let cache = FileCache::new(16);
    let discovered = discover(std::slice::from_ref(&options.directory), &[], true)?;
    let states = open_catalog(
        &discovered.chain_state,
        LogKind::ChainState,
        &cache,
        options.end,
    )?;
    let traces = open_catalog(&discovered.trace, LogKind::Trace, &cache, options.end)?;
    ensure!(
        states.range().0 == 1 && states.range().1 > options.end,
        "state history does not cover genesis through end"
    );
    ensure!(
        matches!(traces.range().0, 1 | 2) && traces.range().1 > options.end,
        "trace history does not cover block 2 through end"
    );
    let last = states
        .get(options.end)
        .context("missing last state block")?
        .id(options.end)?;
    ensure!(
        last == irreversible,
        "source end differs from the supplied irreversible boundary"
    );
    let first = states.get(1).context("missing genesis state")?.id(1)?;
    let source_paths = states
        .files()
        .iter()
        .chain(traces.files())
        .flat_map(|log| [log.log_path(), log.index_path()])
        .collect::<BTreeSet<_>>();
    let stamps = source_paths
        .iter()
        .map(|path| {
            let m = fs::metadata(path)?;
            Ok(((*path).to_path_buf(), m.len(), m.modified()?))
        })
        .collect::<Result<Vec<_>>>()?;
    let blocks_path = suffix(&options.output, ".blocks.bin");
    let manifest_path = suffix(&options.output, ".manifest.json");
    for path in [&options.output, &blocks_path, &manifest_path] {
        ensure!(!path.exists(), "output already exists: {}", path.display());
    }
    let records_partial = suffix(&options.output, ".partial");
    let blocks_partial = suffix(&blocks_path, ".partial");
    let manifest_partial = suffix(&manifest_path, ".partial");
    let mut records = BufWriter::new(fresh(&records_partial)?);
    let mut ids = BufWriter::new(fresh(&blocks_partial)?);
    let mut revisions = BTreeMap::<String, u64>::new();
    let mut current = BTreeMap::<String, AccountState>::new();
    let mut record_count = 0u64;
    let mut last_sequence = None;
    let mut state_bytes = Vec::new();
    let mut trace_bytes = Vec::new();
    for block in 1..=options.end {
        let id = payload(&states, block, &mut state_bytes)?;
        let block_id = hex::encode(id);
        let rows = account_rows(&options.provider, &schema, &state_bytes)
            .with_context(|| format!("Wire state block {block}"))?;
        let block_actions = if block >= traces.range().0 {
            let trace_id = payload(&traces, block, &mut trace_bytes)?;
            ensure!(
                trace_id == id,
                "trace/state block identity mismatch at {block}"
            );
            actions(&options.provider, &schema, &trace_bytes)
                .with_context(|| format!("Wire trace block {block}"))?
        } else {
            Vec::new()
        };
        let mut changed = BTreeMap::<String, String>::new();
        for action in block_actions {
            ensure!(
                last_sequence.is_none_or(|prior| action.sequence > prior),
                "global action sequence regressed at block {block}"
            );
            last_sequence = Some(action.sequence);
            if let Some((account, abi_hex)) = action.deployment {
                let revision = revisions.entry(account.clone()).or_default();
                *revision = revision.checked_add(1).context("ABI revision overflow")?;
                let state = AccountState {
                    present: true,
                    hex: abi_hex.clone(),
                };
                let mut record = json!({"account":account,"blockNum":block,"blockId":block_id,"phase":"deployment","present":true,"abiHex":abi_hex,"abiSequence":revision.to_string(),
                    "action":{"transactionId":action.transaction,"actionOrdinal":action.ordinal,"globalSequence":action.sequence.to_string()}});
                abi_fields(&mut record, &options.provider, &state);
                emit(&mut records, &record, &mut record_count)?;
                changed.insert(account, abi_hex);
            } else if let Some(abi_sequence) = action.abi_sequence {
                ensure!(
                    abi_sequence == *revisions.get(&action.account).unwrap_or(&0),
                    "ABI receipt revision disagrees with deployments for {} at block {block}",
                    action.account
                );
            }
        }
        let mut seen = BTreeSet::new();
        for (account, state, ordinal) in rows {
            ensure!(
                seen.insert(account.clone()),
                "duplicate account final state at block {block}"
            );
            if let Some(deployment) = changed.remove(&account) {
                ensure!(
                    state.present && state.hex == deployment,
                    "deployment/final ABI mismatch for {account} at block {block}"
                );
            } else if block != 1 {
                ensure!(
                    current
                        .get(&account)
                        .is_some_and(|old| old.present == state.present && old.hex == state.hex)
                        || !state.present
                        || (!current.contains_key(&account) && state.hex.is_empty()),
                    "ABI state changed without a successful setabi for {account} at block {block}"
                );
            }
            let different = current
                .get(&account)
                .is_none_or(|old| old.present != state.present || old.hex != state.hex);
            if different && (!state.hex.is_empty() || current.contains_key(&account)) {
                let mut record = json!({"account":account,"blockNum":block,"blockId":block_id,"phase":"block-final","present":state.present,"abiHex":state.hex,
                    "abiSequence":revisions.get(&account).unwrap_or(&0).to_string(),"deltaOrdinal":ordinal});
                abi_fields(&mut record, &options.provider, &state);
                emit(&mut records, &record, &mut record_count)?;
            }
            current.insert(account, state);
        }
        // Re-setting the same ABI can leave no changed account row, but still creates a new epoch.
        for (account, value) in changed {
            ensure!(
                current
                    .get(&account)
                    .is_some_and(|old| old.present && old.hex == value),
                "setabi lacks its changed final account row for {account} at block {block}"
            );
        }
        ids.write_all(&id)?;
        if block % 10_000 == 0 {
            eprintln!(
                "[wire-disk] {block}/{} blocks, {record_count} sparse ABI records",
                options.end
            );
        }
    }
    ensure!(
        options.end == 1 || last_sequence.is_some(),
        "history has no committed action receipts"
    );
    for (path, size, modified) in &stamps {
        let m = fs::metadata(path)?;
        ensure!(
            m.len() == *size && m.modified()? == *modified,
            "source changed while scanning: {}",
            path.display()
        );
    }
    records.flush()?;
    records.get_ref().sync_all()?;
    ids.flush()?;
    ids.get_ref().sync_all()?;
    drop(records);
    drop(ids);
    let (record_bytes, record_hash) = file_hash(&records_partial)?;
    let (id_bytes, id_hash) = file_hash(&blocks_partial)?;
    ensure!(
        id_bytes == u64::from(options.end) * 32,
        "block membership length mismatch"
    );
    let sources = states.files().iter().map(|log| ("chain-state",log)).chain(traces.files().iter().map(|log| ("trace",log)))
        .map(|(kind, log)| json!({"kind":kind,"log":log.log_path(),"index":log.index_path(),"begin":log.range().0,"endExclusive":log.range().1,"logBytes":log.size()})).collect::<Vec<_>>();
    let manifest = json!({"format":FORMAT,"chainId":options.chain_id,"nodeRevision":profile.upstream_revision,"producerProfileId":options.provider.id(),
        "producerRevision":WIRE_SOURCE_REVISION,"shipAbiSha256":hash(options.schema.as_bytes()),"startBlock":1,"endBlock":options.end,"firstBlockId":hex::encode(first),"lastBlockId":options.irreversible_block_id,
        "irreversibleBoundary":{"blockNum":options.end,"blockId":options.irreversible_block_id},"complete":true,"lastGlobalSequence":last_sequence.map(|v|v.to_string()),
        "nativeBaseline":{"kind":"wire-native-profile","abiSequence":"0","genesisTraceAbsent":traces.range().0==2},
        "deployments":{"name":basename(&options.output)?,"bytes":record_bytes,"sha256":record_hash,"records":record_count},
        "blockIds":{"name":basename(&blocks_path)?,"bytes":id_bytes,"sha256":id_hash,"count":options.end},"sources":sources});
    let mut manifest_file = fresh(&manifest_partial)?;
    serde_json::to_writer(&mut manifest_file, &manifest)?;
    manifest_file.write_all(b"\n")?;
    manifest_file.sync_all()?;
    drop(manifest_file);
    publish(&records_partial, &options.output)?;
    publish(&blocks_partial, &blocks_path)?;
    publish(&manifest_partial, &manifest_path)?;
    eprintln!(
        "[wire-disk] complete: {} blocks, {record_count} sparse ABI records; {}",
        options.end,
        manifest_path.display()
    );
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_wire_delta_bytes_use_the_evidenced_count_correction() {
        let capture: Value =
            serde_json::from_str(include_str!("../testdata/wire/ship-capture.json")).unwrap();
        let schema: Value =
            serde_json::from_str(capture["handshakeUtf8"].as_str().unwrap()).unwrap();
        let provider: ProtocolProvider = "wire-sysio-7dede884-engineering-v1".parse().unwrap();
        let frame = capture["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "captured-ship-block-result")
            .unwrap();
        let envelope = provider
            .decode_value(
                &schema,
                "result",
                &hex::decode(frame["hex"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
        let raw = hex::decode(envelope[1]["deltas"].as_str().unwrap()).unwrap();
        let traces = provider
            .decode_value(
                &schema,
                "transaction_trace[]",
                &hex::decode(envelope[1]["traces"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
        assert_eq!(traces[0][1]["partial"][0], "partial_transaction_v0");
        assert!(
            !actions(
                &provider,
                &schema,
                &hex::decode(envelope[1]["traces"].as_str().unwrap()).unwrap()
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            provider
                .decode_value(&schema, "table_delta[]", &raw)
                .is_err()
        );
        let rows = account_rows(&provider, &schema, &raw).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "sysio");
        assert!(rows[0].1.present);
        assert!(rows[0].1.hex.starts_with("0e737973696f3a3a6162692f312e"));
        assert_eq!(
            provider
                .decode_abi(&hex::decode(&rows[0].1.hex).unwrap())
                .unwrap()["version"],
            "sysio::abi/1.0"
        );
    }
}
