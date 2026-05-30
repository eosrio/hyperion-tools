//! State-history / SHiP binary parsing primitives.
//!
//! Everything here works identically for the SHiP and from-disk paths: the
//! `deltas` bytes are the same on the wire and on disk (once inflated).

use anyhow::{anyhow, Result};
use rs_abieos::Abieos;

/// Read a LEB128 varuint32. Returns (value, bytes_consumed).
pub fn read_varuint(buf: &[u8]) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    let mut i = 0usize;
    loop {
        let byte = *buf.get(i)?;
        i += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value as usize, i));
        }
        shift += 7;
        if shift > 35 {
            return None;
        }
    }
}

/// Parse a get_blocks_result_v0 envelope (zero-copy). Returns (block_num, deltas_bytes).
/// Layout: variant(1) head(36) lib(36) this_block(opt 36) prev_block(opt 36)
///         block(opt bytes) traces(opt bytes) deltas(opt bytes).
///
/// The variant index of get_blocks_result_v0 is `1` in every Leap/Spring SHiP
/// ABI, so this parse is version-independent.
pub fn parse_result(bin: &[u8]) -> Option<(u32, &[u8])> {
    if bin.first().copied() != Some(1) {
        return None; // not get_blocks_result_v0
    }
    let mut off = 1usize + 36 + 36; // variant + head + last_irreversible
    let this_present = *bin.get(off)?;
    off += 1;
    let block_num;
    if this_present == 1 {
        block_num = u32::from_le_bytes(bin.get(off..off + 4)?.try_into().ok()?);
        off += 36;
    } else {
        return None; // idle / no block in this message
    }
    let prev_present = *bin.get(off)?;
    off += 1;
    if prev_present == 1 {
        off += 36;
    }
    for present_optional_bytes in 0..2 {
        // block, traces — skip if present
        let present = *bin.get(off)?;
        off += 1;
        if present == 1 {
            let (len, k) = read_varuint(bin.get(off..)?)?;
            off += k + len;
        }
        let _ = present_optional_bytes;
    }
    // deltas (optional bytes)
    let deltas_present = *bin.get(off)?;
    off += 1;
    if deltas_present == 1 {
        let (len, k) = read_varuint(bin.get(off..)?)?;
        off += k;
        return Some((block_num, bin.get(off..off + len)?));
    }
    Some((block_num, &[]))
}

/// Walk table_delta[] and call `f` on each `account` table row (raw bytes),
/// skipping all other tables (e.g. the dense contract_row) by length.
pub fn for_each_account_row<F: FnMut(&[u8]) -> Result<()>>(deltas: &[u8], mut f: F) -> Result<()> {
    let mut off = 0usize;
    let (n_tables, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad table count"))?;
    off += k;
    for _ in 0..n_tables {
        let (_variant, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad variant"))?;
        off += k;
        let (name_len, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad name len"))?;
        off += k;
        let name = deltas
            .get(off..off + name_len)
            .ok_or_else(|| anyhow!("name oob"))?;
        off += name_len;
        let (rows, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad rows count"))?;
        off += k;
        let is_account = name == b"account";
        for _ in 0..rows {
            off += 1; // present byte
            let (data_len, k) =
                read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad data len"))?;
            off += k;
            if is_account {
                let data = deltas
                    .get(off..off + data_len)
                    .ok_or_else(|| anyhow!("data oob"))?;
                f(data)?;
            }
            off += data_len;
        }
    }
    Ok(())
}

/// Decode a SHiP `account` table row **manually** (no SHiP ABI required):
///   [varuint variant=0][name u64][creation_date u32][abi: varuint len + bytes]
/// Returns (account_name, abi_hex) when the row carries a non-empty ABI (a setabi).
/// Works identically for the SHiP and from-disk paths.
pub fn account_setabi(abieos: &Abieos, row: &[u8]) -> Result<Option<(String, String)>> {
    let (_variant, k) = read_varuint(row).ok_or_else(|| anyhow!("account variant"))?;
    let mut off = k;
    if off + 12 > row.len() {
        return Ok(None);
    }
    let name_u64 = u64::from_le_bytes(row[off..off + 8].try_into().unwrap());
    off += 8;
    off += 4; // creation_date (block_timestamp_type, u32)
    let (abi_len, k) = read_varuint(&row[off..]).ok_or_else(|| anyhow!("account abi len"))?;
    off += k;
    if abi_len == 0 {
        return Ok(None);
    }
    let abi_bytes = row
        .get(off..off + abi_len)
        .ok_or_else(|| anyhow!("account abi oob"))?;
    let name = abieos
        .name_to_string(name_u64)
        .map_err(|e| anyhow!("name_to_string: {e:?}"))?;
    Ok(Some((name, hex::encode(abi_bytes))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varuint() {
        assert_eq!(read_varuint(&[0x05]), Some((5, 1)));
        assert_eq!(read_varuint(&[0xd4, 0x10]), Some((2132, 2))); // eosio abi length @ WAX block 2
        assert_eq!(read_varuint(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(read_varuint(&[]), None);
        assert_eq!(read_varuint(&[0x80]), None); // truncated
    }

    #[test]
    fn account_row_parse() {
        let abieos = Abieos::new();
        // account_v0 row from WAX block 2: [variant 0][name "eosio"][creation_date][abi bytes]
        let mut row = vec![0x00];
        row.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0xea, 0x30, 0x55]); // "eosio"
        row.extend_from_slice(&[0x80, 0xd6, 0x14, 0x49]); // creation_date
        row.extend_from_slice(&[0x02, 0x0e, 0x65]); // abi: varuint len 2, bytes 0e 65
        let (name, abi_hex) = account_setabi(&abieos, &row).unwrap().unwrap();
        assert_eq!(name, "eosio");
        assert_eq!(abi_hex, "0e65");

        // empty abi -> not a setabi
        let mut empty = row[..13].to_vec();
        empty.push(0x00); // abi len 0
        assert!(account_setabi(&abieos, &empty).unwrap().is_none());
    }
}
