use std::collections::HashMap;
use std::ops::Deref;

use bytes::Bytes;
use eth_trie_utils::nibbles::Nibbles;
use eth_trie_utils::partial_trie::{HashedPartialTrie, PartialTrie};
use ethereum_types::{Address, BigEndianHash, H256, U256, U512};
use keccak_hash::keccak;
use rlp::PayloadInfo;
use rlp_derive::{RlpDecodable, RlpEncodable};

use crate::cpu::kernel::constants::trie_type::PartialTrieType;
use crate::generation::TrieInputs;
use crate::Node;

#[derive(RlpEncodable, RlpDecodable, Debug)]
pub struct AccountRlp {
    pub nonce: U256,
    pub balance: U256,
    pub storage_root: H256,
    pub code_hash: H256,
}

impl Default for AccountRlp {
    fn default() -> Self {
        Self {
            nonce: U256::zero(),
            balance: U256::zero(),
            storage_root: HashedPartialTrie::from(Node::Empty).hash(),
            code_hash: keccak([]),
        }
    }
}

#[derive(RlpEncodable, RlpDecodable, Debug)]
pub struct LegacyTransactionRlp {
    pub nonce: U256,
    pub gas_price: U256,
    pub gas: U256,
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
    pub v: U256,
    pub r: U256,
    pub s: U256,
}

#[derive(RlpEncodable, RlpDecodable, Debug)]
pub struct LogRlp {
    pub address: Address,
    pub topics: Vec<H256>,
    pub data: Bytes,
}

#[derive(RlpEncodable, RlpDecodable, Debug)]
pub struct LegacyReceiptRlp {
    pub status: bool,
    pub cum_gas_used: U256,
    pub bloom: Bytes,
    pub logs: Vec<LogRlp>,
}

pub(crate) fn all_mpt_prover_inputs_reversed(trie_inputs: &TrieInputs) -> Vec<U256> {
    let mut inputs = all_mpt_prover_inputs(trie_inputs);
    inputs.reverse();
    inputs
}

pub(crate) fn parse_receipts(rlp: &[u8]) -> Vec<U256> {
    let payload_info = PayloadInfo::from(rlp).unwrap();
    let decoded_receipt: LegacyReceiptRlp = rlp::decode(rlp).unwrap();
    let mut parsed_receipt = Vec::new();

    parsed_receipt.push(payload_info.value_len.into()); // payload_len of the entire receipt
    parsed_receipt.push((decoded_receipt.status as u8).into());
    parsed_receipt.push(decoded_receipt.cum_gas_used);
    parsed_receipt.extend(decoded_receipt.bloom.iter().map(|byte| U256::from(*byte)));
    let encoded_logs = rlp::encode_list(&decoded_receipt.logs);
    let logs_payload_info = PayloadInfo::from(&encoded_logs).unwrap();
    parsed_receipt.push(logs_payload_info.value_len.into()); // payload_len of all the logs
    parsed_receipt.push(decoded_receipt.logs.len().into());

    for log in decoded_receipt.logs {
        let encoded_log = rlp::encode(&log);
        let log_payload_info = PayloadInfo::from(&encoded_log).unwrap();
        parsed_receipt.push(log_payload_info.value_len.into()); // payload of one log
        parsed_receipt.push(U256::from_big_endian(&log.address.to_fixed_bytes()));
        parsed_receipt.push(log.topics.len().into());
        parsed_receipt.extend(log.topics.iter().map(|topic| U256::from(topic.as_bytes())));
        parsed_receipt.push(log.data.len().into());
        parsed_receipt.extend(log.data.iter().map(|byte| U256::from(*byte)));
    }

    parsed_receipt
}
/// Generate prover inputs for the initial MPT data, in the format expected by `mpt/load.asm`.
pub(crate) fn all_mpt_prover_inputs(trie_inputs: &TrieInputs) -> Vec<U256> {
    let mut prover_inputs = vec![];

    let storage_tries_by_state_key = trie_inputs
        .storage_tries
        .iter()
        .map(|(hashed_address, storage_trie)| {
            let key = Nibbles::from_bytes_be(hashed_address.as_bytes()).unwrap();
            (key, storage_trie)
        })
        .collect();

    mpt_prover_inputs_state_trie(
        &trie_inputs.state_trie,
        empty_nibbles(),
        &mut prover_inputs,
        &storage_tries_by_state_key,
    );

    mpt_prover_inputs(&trie_inputs.transactions_trie, &mut prover_inputs, &|rlp| {
        rlp::decode_list(rlp)
    });

    mpt_prover_inputs(
        &trie_inputs.receipts_trie,
        &mut prover_inputs,
        &parse_receipts,
    );

    // Temporary! The actual number of transactions in the trie cannot be known if the trie
    // contains hash nodes.
    let num_transactions = trie_inputs
        .transactions_trie
        .values()
        .collect::<Vec<_>>()
        .len();
    prover_inputs.push(num_transactions.into());

    prover_inputs
}

/// Given a trie, generate the prover input data for that trie. In essence, this serializes a trie
/// into a `U256` array, in a simple format which the kernel understands. For example, a leaf node
/// is serialized as `(TYPE_LEAF, key, value)`, where key is a `(nibbles, depth)` pair and `value`
/// is a variable-length structure which depends on which trie we're dealing with.
pub(crate) fn mpt_prover_inputs<F>(
    trie: &HashedPartialTrie,
    prover_inputs: &mut Vec<U256>,
    parse_value: &F,
) where
    F: Fn(&[u8]) -> Vec<U256>,
{
    prover_inputs.push((PartialTrieType::of(trie) as u32).into());

    match trie.deref() {
        Node::Empty => {}
        Node::Hash(h) => prover_inputs.push(U256::from_big_endian(h.as_bytes())),
        Node::Branch { children, value } => {
            if value.is_empty() {
                prover_inputs.push(U256::zero()); // value_present = 0
            } else {
                let parsed_value = parse_value(value);
                prover_inputs.push(U256::one()); // value_present = 1
                prover_inputs.extend(parsed_value);
            }
            for child in children {
                mpt_prover_inputs(child, prover_inputs, parse_value);
            }
        }
        Node::Extension { nibbles, child } => {
            prover_inputs.push(nibbles.count.into());
            prover_inputs.push(nibbles.try_into_u256().unwrap());
            mpt_prover_inputs(child, prover_inputs, parse_value);
        }
        Node::Leaf { nibbles, value } => {
            prover_inputs.push(nibbles.count.into());
            prover_inputs.push(nibbles.try_into_u256().unwrap());
            let leaf = parse_value(value);
            prover_inputs.extend(leaf);
        }
    }
}

/// Like `mpt_prover_inputs`, but for the state trie, which is a bit unique since each value
/// leads to a storage trie which we recursively traverse.
pub(crate) fn mpt_prover_inputs_state_trie(
    trie: &HashedPartialTrie,
    key: Nibbles,
    prover_inputs: &mut Vec<U256>,
    storage_tries_by_state_key: &HashMap<Nibbles, &HashedPartialTrie>,
) {
    prover_inputs.push((PartialTrieType::of(trie) as u32).into());
    match trie.deref() {
        Node::Empty => {}
        Node::Hash(h) => prover_inputs.push(U256::from_big_endian(h.as_bytes())),
        Node::Branch { children, value } => {
            assert!(value.is_empty(), "State trie should not have branch values");
            prover_inputs.push(U256::zero()); // value_present = 0

            for (i, child) in children.iter().enumerate() {
                let extended_key = key.merge_nibbles(&Nibbles {
                    count: 1,
                    packed: i.into(),
                });
                mpt_prover_inputs_state_trie(
                    child,
                    extended_key,
                    prover_inputs,
                    storage_tries_by_state_key,
                );
            }
        }
        Node::Extension { nibbles, child } => {
            prover_inputs.push(nibbles.count.into());
            prover_inputs.push(nibbles.try_into_u256().unwrap());
            let extended_key = key.merge_nibbles(nibbles);
            mpt_prover_inputs_state_trie(
                child,
                extended_key,
                prover_inputs,
                storage_tries_by_state_key,
            );
        }
        Node::Leaf { nibbles, value } => {
            let account: AccountRlp = rlp::decode(value).expect("Decoding failed");
            let AccountRlp {
                nonce,
                balance,
                storage_root,
                code_hash,
            } = account;

            let storage_hash_only = HashedPartialTrie::new(Node::Hash(storage_root));
            let merged_key = key.merge_nibbles(nibbles);
            let storage_trie: &HashedPartialTrie = storage_tries_by_state_key
                .get(&merged_key)
                .copied()
                .unwrap_or(&storage_hash_only);

            assert_eq!(storage_trie.hash(), storage_root,
                       "In TrieInputs, an account's storage_root didn't match the associated storage trie hash");

            prover_inputs.push(nibbles.count.into());
            prover_inputs.push(nibbles.try_into_u256().unwrap());
            prover_inputs.push(nonce);
            prover_inputs.push(balance);
            mpt_prover_inputs(storage_trie, prover_inputs, &parse_storage_value);
            prover_inputs.push(code_hash.into_uint());
        }
    }
}

fn parse_storage_value(value_rlp: &[u8]) -> Vec<U256> {
    let value: U256 = rlp::decode(value_rlp).expect("Decoding failed");
    vec![value]
}

fn empty_nibbles() -> Nibbles {
    Nibbles {
        count: 0,
        packed: U512::zero(),
    }
}
