//! Contract-table walkers (the producer side of the pipeline).
//!
//! Each walker reads the framing sequentially, enforces the consumption/count invariants, and emits
//! every *selected* primary `key_value` row (owned) to `sink`. Decoding happens downstream in the
//! parallel workers — these functions never touch an ABI.

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::model::{ProducerStats, RawRow, Targets};
use crate::reader::{Section, Snap, READ_SKIP_MAX, SECONDARY_ROW_SIZES};

/// v6: one commingled `contract_tables` section. Per table: `table_id_object` row, then for each of
/// the 6 index types a `[varuint count][rows]` group. `table_id_object.count` must equal the sum of
/// all 6 group counts (tripwire for a framing/skip-size desync), and the walk must consume the
/// section to its exact byte boundary.
pub fn walk_v6(
    s: &mut Snap,
    sec: &Section,
    t: &Targets,
    limit: Option<u64>,
    sink: &mut dyn FnMut(RawRow) -> Result<()>,
) -> Result<ProducerStats> {
    s.seek_to(sec.payload_off)?;
    let end = sec.payload_off + sec.payload_len;
    let mut ps = ProducerStats::default();
    let mut scratch: Vec<u8> = Vec::new();

    while s.pos < end {
        let code = s.u64()?;
        let scope = s.u64()?;
        let table = s.u64()?;
        let _payer = s.u64()?;
        let count = s.u32()? as u64;
        ps.tables += 1;
        let selected = t.selected(code, scope, table);

        let mut table_total = 0u64;
        for idx in 0u8..6 {
            let n = s.varuint()?;
            table_total += n;
            if idx == 0 {
                for _ in 0..n {
                    let pk = s.u64()?;
                    let payer = s.u64()?;
                    let vlen = s.varuint()? as usize;
                    ps.kv_rows += 1;
                    if selected && !ps.limited {
                        let mut value = vec![0u8; vlen];
                        s.read_buf(&mut value)?;
                        sink(RawRow {
                            code,
                            scope,
                            table,
                            pk,
                            payer,
                            value,
                        })?;
                        ps.emitted += 1;
                        if matches!(limit, Some(l) if ps.emitted >= l) {
                            ps.limited = true;
                        }
                    } else {
                        s.read_into(vlen, &mut scratch)?;
                    }
                }
            } else {
                let bytes = n * SECONDARY_ROW_SIZES[(idx - 1) as usize];
                if bytes <= READ_SKIP_MAX {
                    s.read_into(bytes as usize, &mut scratch)?;
                } else {
                    s.skip(bytes)?;
                }
            }
        }
        if table_total != count {
            ps.count_mismatches += 1;
        }
        if ps.limited {
            break;
        }
    }
    if !ps.limited && s.pos != end {
        bail!(
            "contract_tables walk DESYNC: consumed to {} but section ends at {} (delta {})",
            s.pos,
            end,
            end as i64 - s.pos as i64
        );
    }
    Ok(ps)
}

/// v8: parse the standalone `table_id_object` section → `flattened ordinal -> (code,scope,table)`
/// for our target tables. The 0-based row index is the flattened `t_id` the row sections reference.
pub fn load_table_ids_v8(
    s: &mut Snap,
    sec: &Section,
    t: &Targets,
) -> Result<HashMap<u64, (u64, u64, u64)>> {
    s.seek_to(sec.payload_off)?;
    let end = sec.payload_off + sec.payload_len;
    let mut interesting = HashMap::new();
    for i in 0..sec.rows {
        let code = s.u64()?;
        let scope = s.u64()?;
        let table = s.u64()?;
        let _payer = s.u64()?;
        let _count = s.u32()?;
        if t.selected(code, scope, table) {
            interesting.insert(i, (code, scope, table));
        }
    }
    if s.pos != end {
        bail!(
            "table_id_object walk desync: consumed to {} but section ends at {}",
            s.pos,
            end
        );
    }
    Ok(interesting)
}

/// v8: walk the `key_value_object` section — repeated `[t_id: int64 LE][varuint count][rows]`. `t_id`
/// is the flattened ordinal; in the key_value section every table has primary rows, so t_ids are
/// strictly increasing (a wrong t_id width breaks that and the consumption check).
pub fn walk_v8(
    s: &mut Snap,
    kv_sec: &Section,
    interesting: &HashMap<u64, (u64, u64, u64)>,
    limit: Option<u64>,
    sink: &mut dyn FnMut(RawRow) -> Result<()>,
) -> Result<ProducerStats> {
    s.seek_to(kv_sec.payload_off)?;
    let end = kv_sec.payload_off + kv_sec.payload_len;
    let mut ps = ProducerStats::default();
    let mut scratch: Vec<u8> = Vec::new();
    let mut prev_tid: i128 = -1;

    while s.pos < end {
        let t_id = s.u64()?;
        let count = s.varuint()?;
        if (t_id as i128) <= prev_tid {
            bail!("v8 key_value t_id not strictly increasing: {t_id} after {prev_tid} — framing error");
        }
        prev_tid = t_id as i128;
        ps.tables += 1;
        let sel = interesting.get(&t_id).copied();
        for _ in 0..count {
            let pk = s.u64()?;
            let payer = s.u64()?;
            let vlen = s.varuint()? as usize;
            ps.kv_rows += 1;
            if let Some((code, scope, table)) = sel {
                if !ps.limited {
                    let mut value = vec![0u8; vlen];
                    s.read_buf(&mut value)?;
                    sink(RawRow {
                        code,
                        scope,
                        table,
                        pk,
                        payer,
                        value,
                    })?;
                    ps.emitted += 1;
                    if matches!(limit, Some(l) if ps.emitted >= l) {
                        ps.limited = true;
                    }
                    continue;
                }
            }
            s.read_into(vlen, &mut scratch)?;
        }
        if ps.limited {
            break;
        }
    }
    if !ps.limited && s.pos != end {
        bail!(
            "v8 key_value walk DESYNC: consumed to {} but section ends at {} (delta {})",
            s.pos,
            end,
            end as i64 - s.pos as i64
        );
    }
    Ok(ps)
}
