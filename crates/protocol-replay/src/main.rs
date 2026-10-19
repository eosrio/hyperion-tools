//! One fully validated input operation per NDJSON line; failures never count as success.
use anyhow::{Context, Result, bail};
use clap::Parser;
use hyperion_ship::provider::ProtocolProvider;
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};

#[derive(Parser)]
struct Args {
    /// Explicit source-pinned Wire engineering profile, or the existing Antelope default.
    #[arg(long, default_value = "antelope")]
    protocol: ProtocolProvider,
}

fn operation(provider: &ProtocolProvider, input: &Value) -> Result<Value> {
    let bytes = || -> Result<Vec<u8>> {
        Ok(hex::decode(
            input["hex"].as_str().context("missing input hex")?,
        )?)
    };
    let ty = || input["typeName"].as_str().context("missing typeName");
    Ok(
        match input["operation"].as_str().context("missing operation")? {
            "decodeAbi" => provider.decode_abi(&bytes()?)?,
            "decodeValue" => provider.decode_value(&input["abi"], ty()?, &bytes()?)?,
            "encodeValue" => json!(hex::encode(provider.encode_value(
                &input["abi"],
                ty()?,
                &input["value"]
            )?)),
            "decodeBundle" => provider.decode_bundle(&input["abi"], &bytes()?)?,
            "normalizeKvRow" => provider.normalize_kv_row(
                &input["abi"],
                input["stream"].as_str().context("missing stream")?,
                input["present"].as_bool().context("missing presence")?,
                &bytes()?,
                &input["source"],
            )?,
            "decodeKvRecord" => provider.decode_kv_record(
                &input["deployments"],
                &input["record"],
                &input["source"],
            )?,
            "slotToIso" => json!(provider.timestamp_profile()?.slot_to_iso(u32::try_from(
                input["slot"].as_u64().context("invalid slot")?
            )?)),
            _ => bail!("unsupported_feature: unknown replay operation"),
        },
    )
}

fn main() -> Result<()> {
    let args = Args::parse();
    args.protocol.validate()?;
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let mut failed = false;
    let mut input = std::io::stdin().lock();
    loop {
        let mut raw = Vec::new();
        // Take bounds allocation before reading, including the newline. An oversized
        // record terminates the stream; a partial record is never interpreted.
        let count = input
            .by_ref()
            .take(64 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut raw)?;
        if count == 0 {
            break;
        }
        anyhow::ensure!(
            count <= 64 * 1024 * 1024,
            "limits_exceeded: replay line byte limit"
        );
        let line = String::from_utf8(raw).context("invalid replay UTF-8")?;
        if line.trim().is_empty() {
            continue;
        }
        // Bound text before JSON parsing; codec checks apply separate nesting/bytes limits.
        let result = if line.len() > 64 * 1024 * 1024 {
            Err(anyhow::anyhow!("limits_exceeded: replay line byte limit"))
        } else {
            serde_json::from_str::<Value>(&line)
                .context("invalid replay JSON")
                .and_then(|input| operation(&args.protocol, &input))
        };
        let response = match result {
            Ok(result) => json!({"result":result}),
            Err(error) => {
                failed = true;
                json!({"error":format!("{error:#}")})
            }
        };
        writeln!(out, "{response}")?;
        out.flush()?;
    }
    if failed {
        bail!("protocol replay failed; erroneous inputs did not advance successful progress");
    }
    Ok(())
}
