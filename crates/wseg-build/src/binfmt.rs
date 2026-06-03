//! Binary accinfo record — the compact on-segment encoding the procedure renders to cc32d9 JSON at
//! request time. Replaces ~572 B/account of pre-rendered JSON with ~130 B of binary (WAX accinfo
//! 12.4 GB → ~3.5 GB). The big win is keys: a `{"pubkey":"EOS…53","public_key":"PUB_K1_…57","weight":1}`
//! entry (~150 B) becomes 33-byte point + two 4-byte checksums + weight = 43 B, base58-re-encoded in
//! the procedure. Antelope `name` fields (perm/actor/permission/link) store as u64, decoded server-side.
//!
//! Layout (little-endian). A record starts with byte 0x00 — the JSON overlay starts with `"` (0x22),
//! so the procedure picks binary-decode vs JSON-splice by the first byte (back-compatible).
//!
//!   u8  marker = 0x00
//!   u8  flags            bit0 has_resources, bit1 has_code
//!   [has_resources] i64 net, i64 cpu, i64 ram                       (24)
//!   [has_code]      [32] code_hash (raw; rendered as 64-hex)
//!   u16 nperms
//!     per perm: u64 name | u32 threshold | u16 nkeys
//!       per key: u8 ktype
//!         0 (K1): [33] point | [4] eos_ck | [4] k1_ck | u16 weight
//!         1 (raw): u16 legacy_len | legacy | u16 modern_len | modern | u16 weight
//!       u16 naccts  per: u64 actor | u64 permission | u16 weight
//!       u16 nlinks  per: u64 code | u64 type
//!   u16 ndeleg_to    per: u64 to   | i64 cpu | i64 net
//!   u16 ndeleg_from  per: u64 from | i64 cpu | i64 net

use crate::name;

pub const MARKER: u8 = 0x00;
pub const FLAG_RESOURCES: u8 = 0x01;
pub const FLAG_CODE: u8 = 0x02;
pub const KEY_K1: u8 = 0;
pub const KEY_RAW: u8 = 1;

pub type Resources = (i64, i64, i64); // net, cpu, ram
pub type Deleg = (String, i64, i64); // peer, cpu, net

/// A permission in the form the Builder accumulates it.
#[derive(Default, Clone)]
pub struct Perm {
    pub perm_name: String,
    pub threshold: i64,
    pub keys: Vec<(String, String, i64)>, // legacy(EOS), modern(PUB_K1), weight
    pub accounts: Vec<(String, String, i64)>, // actor, permission, weight
    pub linked: Vec<(String, String)>,    // code(account), type(action)
}

fn put_u16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(o: &mut Vec<u8>, v: u64) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_i64(o: &mut Vec<u8>, v: i64) {
    o.extend_from_slice(&v.to_le_bytes());
}

/// Decode an EOS/PUB_K1 string pair to (point33, eos_ck4, k1_ck4) if both are well-formed K1 keys
/// sharing the same point. Returns None for non-K1 / malformed (caller stores the raw strings).
fn k1_parts(legacy: &str, modern: &str) -> Option<([u8; 33], [u8; 4], [u8; 4])> {
    let lbody = legacy.strip_prefix("EOS")?;
    let mbody = modern.strip_prefix("PUB_K1_")?;
    let ld = bs58::decode(lbody).into_vec().ok()?;
    let md = bs58::decode(mbody).into_vec().ok()?;
    if ld.len() != 37 || md.len() != 37 || ld[..33] != md[..33] {
        return None;
    }
    let mut point = [0u8; 33];
    point.copy_from_slice(&ld[..33]);
    let mut eos_ck = [0u8; 4];
    eos_ck.copy_from_slice(&ld[33..37]);
    let mut k1_ck = [0u8; 4];
    k1_ck.copy_from_slice(&md[33..37]);
    Some((point, eos_ck, k1_ck))
}

/// Encode one account's accinfo record into `out` (cleared first).
pub fn encode(
    out: &mut Vec<u8>,
    resources: Option<&Resources>,
    perms: &[Perm], // must be sorted by perm_name (matches light-api)
    deleg_to: Option<&Vec<Deleg>>,
    deleg_from: Option<&Vec<Deleg>>,
    code_hash_hex: Option<&String>,
) {
    out.clear();
    out.push(MARKER);
    let mut flags = 0u8;
    if resources.is_some() {
        flags |= FLAG_RESOURCES;
    }
    // code_hash is stored hex (64 chars); decode to 32 raw bytes (fallback: skip if not 32 bytes).
    let code_raw: Option<[u8; 32]> = code_hash_hex.and_then(|h| {
        let v = hex_decode(h)?;
        if v.len() == 32 {
            let mut a = [0u8; 32];
            a.copy_from_slice(&v);
            Some(a)
        } else {
            None
        }
    });
    if code_raw.is_some() {
        flags |= FLAG_CODE;
    }
    out.push(flags);

    if let Some((net, cpu, ram)) = resources {
        put_i64(out, *net);
        put_i64(out, *cpu);
        put_i64(out, *ram);
    }
    if let Some(c) = &code_raw {
        out.extend_from_slice(c);
    }

    put_u16(out, perms.len() as u16);
    for p in perms {
        put_u64(out, name::encode(&p.perm_name));
        put_u32(out, p.threshold as u32);
        put_u16(out, p.keys.len() as u16);
        for (legacy, modern, w) in &p.keys {
            match k1_parts(legacy, modern) {
                Some((point, eck, kck)) => {
                    out.push(KEY_K1);
                    out.extend_from_slice(&point);
                    out.extend_from_slice(&eck);
                    out.extend_from_slice(&kck);
                    put_u16(out, *w as u16);
                }
                None => {
                    out.push(KEY_RAW);
                    put_u16(out, legacy.len() as u16);
                    out.extend_from_slice(legacy.as_bytes());
                    put_u16(out, modern.len() as u16);
                    out.extend_from_slice(modern.as_bytes());
                    put_u16(out, *w as u16);
                }
            }
        }
        put_u16(out, p.accounts.len() as u16);
        for (actor, permission, w) in &p.accounts {
            put_u64(out, name::encode(actor));
            put_u64(out, name::encode(permission));
            put_u16(out, *w as u16);
        }
        put_u16(out, p.linked.len() as u16);
        for (code, typ) in &p.linked {
            put_u64(out, name::encode(code));
            put_u64(out, name::encode(typ));
        }
    }

    let dt = deleg_to.map(|v| v.as_slice()).unwrap_or(&[]);
    put_u16(out, dt.len() as u16);
    for (to, cpu, net) in dt {
        put_u64(out, name::encode(to));
        put_i64(out, *cpu);
        put_i64(out, *net);
    }
    let df = deleg_from.map(|v| v.as_slice()).unwrap_or(&[]);
    put_u16(out, df.len() as u16);
    for (from, cpu, net) in df {
        put_u64(out, name::encode(from));
        put_i64(out, *cpu);
        put_i64(out, *net);
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut v = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < b.len() {
        v.push((nib(b[i])? << 4) | nib(b[i + 1])?);
        i += 2;
    }
    Some(v)
}
