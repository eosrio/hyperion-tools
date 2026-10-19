//! Bounded, whole-entry framing for recovery without a usable index. This walks the FC
//! signed_block layout; it does not verify signatures or transaction/action Merkle roots.
use anyhow::{Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::format::{Header, num, u32le};
use crate::io::MAX_BYTES;

#[derive(Debug)]
pub struct Incomplete;
impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("incomplete entry")
    }
}
impl std::error::Error for Incomplete {}

struct Walk<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Walk<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Incomplete)?;
        ensure!(
            end as u64 <= MAX_BYTES,
            "entry exceeds 256 MiB safety limit"
        );
        if end > self.bytes.len() {
            return Err(Incomplete.into());
        }
        let bytes = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn var(&mut self) -> Result<u32> {
        let mut out = 0;
        for shift in (0..35).step_by(7) {
            let b = self.byte()?;
            ensure!(shift != 28 || b <= 15, "varuint32 overflow");
            out |= u32::from(b & 127) << shift;
            if b & 128 == 0 {
                ensure!(shift == 0 || b != 0, "noncanonical varuint32");
                return Ok(out);
            }
        }
        bail!("varuint32 overflow")
    }
    fn count(&mut self) -> Result<u32> {
        let n = self.var()?;
        ensure!(n <= 1_000_000, "vector exceeds recovery safety limit");
        Ok(n)
    }
    fn blob(&mut self) -> Result<()> {
        let n = self.var()?;
        self.take(n as usize)?;
        Ok(())
    }
    fn key(&mut self) -> Result<()> {
        match self.var()? {
            0 | 1 => {
                self.take(33)?;
            }
            2 => {
                self.take(34)?;
                self.blob()?;
            }
            k => bail!("unsupported public key type {k}"),
        }
        Ok(())
    }
    fn signature(&mut self) -> Result<()> {
        match self.var()? {
            0 | 1 => {
                self.take(65)?;
            }
            2 => {
                self.take(65)?;
                self.blob()?;
                self.blob()?;
            }
            k => bail!("unsupported signature type {k}"),
        }
        Ok(())
    }
    fn extensions(&mut self) -> Result<()> {
        for _ in 0..self.count()? {
            self.take(2)?;
            self.blob()?;
        }
        Ok(())
    }
}

pub struct Preamble {
    pub first: u32,
    pub end: usize,
    pub chain_id: [u8; 32],
}

pub fn preamble(bytes: &[u8]) -> Result<Preamble> {
    let mut c = Walk { bytes, pos: 0 };
    ensure!(
        u32le(c.take(4)?) == 3,
        "only non-pruned blocks.log v3 is supported"
    );
    let first = u32le(c.take(4)?);
    ensure!(first > 0 && first < u32::MAX, "invalid first block");
    let chain_id = if first == 1 {
        let start = c.pos;
        c.take(8)?; // initial timestamp
        c.key()?;
        c.take(68)?; // chain_config_v0
        Sha256::digest(&bytes[start..c.pos]).into()
    } else {
        c.take(32)?.try_into()?
    };
    ensure!(
        c.take(8)? == [255; 8],
        "blocks.log header separator missing"
    );
    Ok(Preamble {
        first,
        end: c.pos,
        chain_id,
    })
}

/// Return the exact end of a signed_block in a possibly longer input. Reading the position
/// trailer is the caller's job. Never search payload bytes for a plausible trailer.
pub fn signed_block(bytes: &[u8]) -> Result<(Header, usize)> {
    let mut c = Walk { bytes, pos: 0 };
    c.take(14)?;
    let previous: [u8; 32] = c.take(32)?.try_into()?;
    c.take(68)?;
    match c.byte()? {
        0 => (),
        1 => {
            c.take(4)?;
            for _ in 0..c.count()? {
                c.take(8)?;
                c.key()?;
            }
        }
        _ => bail!("invalid producer optional flag"),
    }
    c.extensions()?;
    let number = num(&previous).checked_add(1).ok_or(Incomplete)?;
    ensure!(number < u32::MAX, "block number overflow");
    let mut id: [u8; 32] = Sha256::digest(&bytes[..c.pos]).into();
    id[..4].copy_from_slice(&number.to_be_bytes());
    c.signature()?;
    for _ in 0..c.count()? {
        ensure!(c.byte()? <= 4, "invalid transaction receipt status");
        c.take(4)?; // cpu_usage_us
        c.var()?; // net_usage_words
        match c.var()? {
            0 => {
                c.take(32)?;
            }
            1 => {
                for _ in 0..c.count()? {
                    c.signature()?;
                }
                ensure!(c.byte()? <= 1, "unsupported transaction compression");
                c.blob()?; // packed_context_free_data
                c.blob()?; // packed_trx
            }
            k => bail!("unsupported transaction receipt variant {k}"),
        }
    }
    c.extensions()?;
    Ok((
        Header {
            number,
            id,
            previous,
        },
        c.pos,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_eos_block_is_parsed_to_its_exact_end() {
        let body =
            hex::decode(include_str!("../testdata/eos-mainnet-480385480.hex").trim()).unwrap();
        let (h, end) = signed_block(&body).unwrap();
        assert_eq!(end, body.len());
        assert_eq!(
            hex::encode(h.id),
            "1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4"
        );
        let mut with_trailer = body.clone();
        with_trailer.extend([0; 8]);
        assert_eq!(signed_block(&with_trailer).unwrap().1, body.len());
        for cut in 0..body.len() {
            assert!(signed_block(&body[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn whole_block_walker_handles_webauthn_producers_signatures_and_packed_receipts() {
        let mut body = vec![0; 14];
        body.extend([0; 32]);
        body.extend([0; 68]);
        body.push(1); // new_producers
        body.extend([0; 4]);
        body.push(1); // one producer
        body.extend([0; 8]);
        body.push(2); // WebAuthn public key
        body.extend([0; 34]);
        body.extend([2, b'a', b'b']); // rpid
        body.extend([1, 3, 0, 2, 7, 8]); // header extension
        body.push(2); // WebAuthn producer signature
        body.extend([0; 65]);
        body.extend([2, 1, 2, 2, 3, 4]); // auth_data, client_json
        body.push(2); // receipts
        body.extend([0; 5]); // executed + cpu_usage
        body.extend([0, 0]); // net_usage + id variant
        body.extend([0; 32]);
        body.push(4); // expired
        body.extend([0; 4]);
        body.extend([128, 1, 1, 2]); // net_usage=128, packed variant, two signatures
        for variant in [0, 1] {
            body.push(variant);
            body.extend([0; 65]);
        }
        body.extend([1, 3, 4, 5, 6, 2, 7, 8]); // compression, cfd blob, trx blob
        body.extend([1, 2, 0, 1, 9]); // block extension
        assert_eq!(signed_block(&body).unwrap().1, body.len());
        for cut in 0..body.len() {
            assert!(signed_block(&body[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn genesis_header_is_sized_without_searching_for_a_separator() {
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend(1u32.to_le_bytes());
        let mut genesis = vec![0; 8];
        genesis.push(0); // K1
        genesis.extend([255; 33]); // includes bytes that look like the separator
        genesis.extend([0; 68]);
        bytes.extend(&genesis);
        bytes.extend([255; 8]);
        let p = preamble(&bytes).unwrap();
        assert_eq!(p.end, 126);
        assert_eq!(p.chain_id, <[u8; 32]>::from(Sha256::digest(&genesis)));
        for cut in 0..bytes.len() {
            assert!(preamble(&bytes[..cut]).is_err());
        }
        let mut bad = bytes;
        bad[16] = 7; // unknown initial_key variant
        assert!(preamble(&bad).is_err());
    }
}
