//! Hyperion 4.5 Wire action occurrences: input origins and schema-3 occurrence metadata.
//!
//! Hyperion 4.5 groups a Wire transaction's actions by their input action, not by `act_digest`.
//! Each occurrence's input is found by walking its creators while its receiver is not its account:
//! every step must replay the creator's exact action (account, name, authorization, data and
//! context-free flag) at a strictly later global sequence, within 64 steps. The walk ends at a
//! first receiver, whose input is itself. One document is written per input; it is the input
//! action's own occurrence and carries every member's receipt.
//!
//! With a chain id, that document also carries schema-3 occurrence metadata. Its selection is the
//! deterministic disk selection of the block's catalog position:
//!
//! ```text
//! {"schemaVersion":3,
//!  "source":{"chainId":..,"blockNum":..,"blockId":..},
//!  "selectionId":"h45abidiskselection1:" + sha256(canonical {blockId, blockNum, chainId, previousBlockId}),
//!  "input":{"transactionId","actionOrdinal","receiver","account","name","rawHex",
//!           "globalSequence","receiptAbiSequence","inputActionOrdinal"}}
//! ```
//!
//! The 4.5 API resolves it from the node's logs into exactly the schema-1 or schema-2 metadata the
//! indexer would have stored, then applies its unchanged checks.

use std::collections::HashMap;

use hyperion_ship::trace::{Act, name_to_string};
use sha2::{Digest, Sha256};

/// Longest notification ancestry an occurrence may have.
const MAX_ANCESTRY: usize = 64;

/// The input action ordinal of each of `actions` (same order), or why their ancestry is invalid.
/// `payload` is the inflated trace payload the actions' data ranges point into.
pub fn input_origins(actions: &[Act], payload: &[u8]) -> Result<Vec<u32>, String> {
    let mut index: HashMap<u32, usize> = HashMap::with_capacity(actions.len());
    for (i, act) in actions.iter().enumerate() {
        if act.action_ordinal == 0 {
            return Err("zero action ordinal".into());
        }
        if index.insert(act.action_ordinal, i).is_some() {
            return Err(format!("repeated action ordinal {}", act.action_ordinal));
        }
    }
    let sequence = |act: &Act| act.receipt.as_ref().map(|r| r.global_sequence);
    let data = |act: &Act| &payload[act.data.0..act.data.0 + act.data.1];
    let replays = |parent: &Act, child: &Act| {
        parent.account == child.account
            && parent.name == child.name
            && parent.authorization == child.authorization
            && parent.context_free == child.context_free
            && data(parent) == data(child)
    };
    let mut origins: Vec<Option<u32>> = vec![None; actions.len()];
    for (i, act) in actions.iter().enumerate() {
        if act.creator_action_ordinal > 0 {
            let parent = index.get(&act.creator_action_ordinal).map(|&p| &actions[p]);
            match (parent.and_then(sequence), sequence(act)) {
                (Some(before), Some(after)) if before < after => {}
                _ => {
                    return Err(format!(
                        "invalid creator ancestry at action {}",
                        act.action_ordinal
                    ));
                }
            }
        }
        let mut origin = i;
        let mut path = Vec::new();
        while origins[origin].is_none() && actions[origin].receiver != actions[origin].account {
            if path.len() >= MAX_ANCESTRY {
                return Err(format!(
                    "notification ancestry of action {} exceeds {MAX_ANCESTRY} steps",
                    act.action_ordinal
                ));
            }
            path.push(origin);
            let child = &actions[origin];
            let parent = index.get(&child.creator_action_ordinal).copied();
            let valid = parent.is_some_and(|p| {
                replays(&actions[p], child)
                    && matches!((sequence(&actions[p]), sequence(child)),
                        (Some(before), Some(after)) if before < after)
            });
            if !valid {
                return Err(format!(
                    "invalid notification ancestry at action {}",
                    child.action_ordinal
                ));
            }
            origin = parent.unwrap();
        }
        let input = origins[origin].unwrap_or(actions[origin].action_ordinal);
        origins[origin] = Some(input);
        for step in path {
            origins[step] = Some(input);
        }
    }
    Ok(origins
        .into_iter()
        .map(|origin| origin.expect("every occurrence resolves an input"))
        .collect())
}

/// The deterministic disk selection id of a catalog position: the sha256 of its canonical JSON
/// (keys sorted, no whitespace).
pub fn disk_selection_id(
    chain_id: &str,
    block_num: u32,
    block_id: &str,
    previous_block_id: &str,
) -> String {
    let position = format!(
        "{{\"blockId\":\"{block_id}\",\"blockNum\":{block_num},\"chainId\":\"{chain_id}\",\"previousBlockId\":\"{previous_block_id}\"}}"
    );
    format!(
        "h45abidiskselection1:{}",
        hex::encode(Sha256::digest(position.as_bytes()))
    )
}

/// Everything of schema-3 metadata up to its `input`, shared by one block's occurrences.
pub fn block_prefix(
    chain_id: &str,
    block_num: u32,
    block_id: &str,
    previous_block_id: &str,
) -> String {
    format!(
        "{{\"schemaVersion\":3,\"source\":{{\"chainId\":\"{chain_id}\",\"blockNum\":{block_num},\"blockId\":\"{block_id}\"}},\"selectionId\":\"{}\",\"input\":",
        disk_selection_id(chain_id, block_num, block_id, previous_block_id)
    )
}

/// One occurrence's complete schema-3 metadata. `data` is the action's original bytes.
pub fn metadata(
    prefix: &str,
    transaction_id: &str,
    act: &Act,
    data: &[u8],
    input_ordinal: u32,
) -> Option<String> {
    let receipt = act.receipt.as_ref()?;
    let mut s = String::with_capacity(prefix.len() + 2 * data.len() + 320);
    s.push_str(prefix);
    s.push_str("{\"transactionId\":\"");
    s.push_str(transaction_id);
    s.push_str("\",\"actionOrdinal\":");
    s.push_str(&act.action_ordinal.to_string());
    for (key, value) in [
        ("receiver", act.receiver),
        ("account", act.account),
        ("name", act.name),
    ] {
        s.push_str(",\"");
        s.push_str(key);
        s.push_str("\":");
        s.push_str(&serde_json::to_string(&name_to_string(value)).ok()?);
    }
    s.push_str(",\"rawHex\":\"");
    s.push_str(&hex::encode(data));
    s.push_str("\",\"globalSequence\":\"");
    s.push_str(&receipt.global_sequence.to_string());
    s.push_str("\",\"receiptAbiSequence\":\"");
    s.push_str(&receipt.abi_sequence.to_string());
    s.push_str("\",\"inputActionOrdinal\":");
    s.push_str(&input_ordinal.to_string());
    s.push_str("}}");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperion_ship::trace::Receipt;
    use serde_json::{Value, json};

    const ALICE: u64 = 0x345c_8500_0000_0000;
    const BOB: u64 = 0x3d0e_0000_0000_0000;
    const TOKEN: u64 = 0x5530_ea03_3482_a600;
    const TRANSFER: u64 = 0xcdcd_3c2d_5700_0000;
    const ACTIVE: u64 = 0x3232_eda8_0000_0000;

    /// An action at `ordinal` created by `creator`, received by `receiver`, with data `data` in
    /// a payload laid out as consecutive ranges.
    fn act(
        ordinal: u32,
        creator: u32,
        receiver: u64,
        account: u64,
        sequence: u64,
        data: (usize, usize),
    ) -> Act {
        Act {
            action_ordinal: ordinal,
            creator_action_ordinal: creator,
            receipt: Some(Receipt {
                receiver,
                act_digest: [7; 32],
                global_sequence: sequence,
                recv_sequence: 1,
                auth_sequence: vec![(ALICE, 1)],
                code_sequence: 1,
                abi_sequence: 2,
            }),
            receiver,
            account,
            name: TRANSFER,
            authorization: vec![(ALICE, ACTIVE)],
            data,
            context_free: false,
            elapsed: 0,
            account_ram_deltas: vec![],
            return_value: None,
            except: false,
        }
    }

    #[test]
    fn notifications_inherit_their_first_receivers_input() {
        let payload = [1, 2, 3, 9, 9, 9];
        // 1: token transfer; 2, 3: its notifications to alice and bob; 4: bob re-notifies the
        // same action (a notification of a notification); 5: an inline transfer from bob.
        let actions = vec![
            act(1, 0, TOKEN, TOKEN, 10, (0, 3)),
            act(2, 1, ALICE, TOKEN, 11, (0, 3)),
            act(3, 1, BOB, TOKEN, 12, (0, 3)),
            act(4, 3, ALICE, TOKEN, 13, (0, 3)),
            act(5, 3, TOKEN, TOKEN, 14, (3, 3)),
        ];
        assert_eq!(
            input_origins(&actions, &payload).unwrap(),
            vec![1, 1, 1, 1, 5]
        );
    }

    #[test]
    fn identical_inline_actions_to_the_same_contract_are_their_own_inputs() {
        // Same act_digest as the creator, but a first receiver: grouping by digest would merge it.
        let payload = [1, 2, 3];
        let actions = vec![
            act(1, 0, TOKEN, TOKEN, 10, (0, 3)),
            act(2, 1, TOKEN, TOKEN, 11, (0, 3)),
        ];
        assert_eq!(input_origins(&actions, &payload).unwrap(), vec![1, 2]);
    }

    #[test]
    fn invalid_ancestry_is_refused() {
        let payload = [1, 2, 3, 4];
        let changed = vec![
            act(1, 0, TOKEN, TOKEN, 10, (0, 3)),
            act(2, 1, ALICE, TOKEN, 11, (1, 3)),
        ];
        assert!(
            input_origins(&changed, &payload)
                .unwrap_err()
                .contains("notification ancestry")
        );
        let earlier = vec![
            act(1, 0, TOKEN, TOKEN, 10, (0, 3)),
            act(2, 1, ALICE, TOKEN, 9, (0, 3)),
        ];
        assert!(
            input_origins(&earlier, &payload)
                .unwrap_err()
                .contains("creator ancestry")
        );
        let orphan = vec![act(1, 0, ALICE, TOKEN, 10, (0, 3))];
        assert!(
            input_origins(&orphan, &payload)
                .unwrap_err()
                .contains("notification ancestry")
        );
        let repeated = vec![
            act(1, 0, TOKEN, TOKEN, 10, (0, 3)),
            act(1, 0, TOKEN, TOKEN, 11, (0, 3)),
        ];
        assert!(
            input_origins(&repeated, &payload)
                .unwrap_err()
                .contains("repeated")
        );
        let mut missing = act(2, 1, ALICE, TOKEN, 11, (0, 3));
        missing.receipt = None;
        let unreceipted = vec![act(1, 0, TOKEN, TOKEN, 10, (0, 3)), missing];
        assert!(input_origins(&unreceipted, &payload).is_err());
        // Deepest first, so one walk crosses the whole chain before any step is resolved.
        let mut chain: Vec<Act> = (2..=66)
            .rev()
            .map(|n| act(n, n - 1, ALICE, TOKEN, 9 + u64::from(n), (0, 3)))
            .collect();
        chain.push(act(1, 0, TOKEN, TOKEN, 10, (0, 3)));
        assert!(
            input_origins(&chain, &payload)
                .unwrap_err()
                .contains("exceeds 64")
        );
        assert_eq!(input_origins(&chain[1..], &payload).unwrap(), vec![1; 65]);
    }

    #[test]
    fn metadata_is_the_schema_3_occurrence_the_api_expects() {
        let payload = [0xab, 0xcd];
        let action = act(2, 1, ALICE, TOKEN, 11, (0, 2));
        let prefix = block_prefix(&"9".repeat(64), 7, &"b".repeat(64), &"a".repeat(64));
        let text = metadata(&prefix, &"f".repeat(64), &action, &payload, 1).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        // The selection id the API's `protocolDiskSelectionId` gives for this position.
        let selection =
            "h45abidiskselection1:d2099170c199b38636f18d254929b131df138a9fc5448b3eeb0d336548d4e10e";
        assert_eq!(
            disk_selection_id(&"9".repeat(64), 7, &"b".repeat(64), &"a".repeat(64)),
            selection
        );
        assert_eq!(
            value,
            json!({"schemaVersion": 3, "source": {"chainId": "9".repeat(64), "blockNum": 7, "blockId": "b".repeat(64)},
                "selectionId": selection,
                "input": {"transactionId": "f".repeat(64), "actionOrdinal": 2, "receiver": "alice", "account": "eosio.token",
                    "name": "transfer", "rawHex": "abcd", "globalSequence": "11", "receiptAbiSequence": "2", "inputActionOrdinal": 1}})
        );
    }
}
