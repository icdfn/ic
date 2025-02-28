use crate::{block, proto, types::Height};
use bitcoin::{hashes::Hash, Block, Network, OutPoint, Script, TxOut, Txid};
use core::cell::RefCell;
use ic_protobuf::bitcoin::v1;
use stable_structures::{StableBTreeMap, VectorMemory};
use std::collections::BTreeMap;
use std::rc::Rc;

/// A structure used to maintain the entire state.
pub struct State {
    // The height of the latest block marked as stable.
    pub height: Height,

    // The UTXOs of all stable blocks since genesis.
    pub utxos: UtxoSet,

    // Blocks inserted, but are not considered stable yet.
    pub unstable_blocks: UnstableBlocks,
}

impl State {
    /// Create a new blockchain.
    ///
    /// The `stability_threshold` parameter specifies how many confirmations a
    /// block needs before it is considered stable. Stable blocks are assumed
    /// to be final and are never removed.
    pub fn new(stability_threshold: u64, network: Network, genesis_block: Block) -> Self {
        Self {
            height: 0,
            utxos: UtxoSet::new(true, network),
            unstable_blocks: UnstableBlocks::new(stability_threshold, genesis_block),
        }
    }

    pub fn to_proto(&self) -> proto::State {
        proto::State {
            height: self.height,
            utxos: Some(self.utxos.to_proto()),
            unstable_blocks: Some(self.unstable_blocks.to_proto()),
        }
    }

    pub fn from_proto(proto_state: proto::State) -> Self {
        Self {
            height: proto_state.height,
            utxos: UtxoSet::from_proto(proto_state.utxos.unwrap()),
            unstable_blocks: UnstableBlocks::from_proto(proto_state.unstable_blocks.unwrap()),
        }
    }
}

/// A key-value store for UTXOs (unspent transaction outputs).
///
/// A UTXO is the tuple (OutPoint, TxOut, Height). For ease of access, UTXOs are
/// stored such that the OutPoint is the key, and (TxOut, Height) is the value.
///
/// Ordinarily, a standard `BTreeMap` would suffice for storing UTXOs, but UTXOs
/// have properties that make storing them more complex.
///
///  * Number of entries: As of early 2022, there are tens of millions of UTXOs.
///    Storing them in a standard `BTreeMap` would make checkpointing very
///    inefficient as it would require serializing all the UTXOs. To work
///    around this, `StableBTreeMap` is used instead, where checkpointing grows
///    linearly only with the number of dirty memory pages.
///
///  * A `StableBTreeMap` allocates the maximum size possible for a key/value.
///    Scripts in Bitcoin are bounded to 10k bytes, but allocating 10k for every
///    UTXO wastes a lot of memory and increases the number of memory read/writes.
///
///    Based on a study of mainnet up to height ~705,000, the following is the
///    distribution of script sizes in UTXOs:
///
///    | Script Size           |  # UTXOs     | % of Total |
///    |-----------------------|--------------|------------|
///    | <= 25 bytes           |  74,136,585  |   98.57%   |
///    | > 25 && <= 201 bytes  |   1,074,004  |    1.43%   |
///    | > 201 bytes           |          13  | 0.00002%   |
///
///    Because of the skewness in the sizes of the script, the KV store for
///    UTXOs is split into buckets:
///
///    1) "Small" to store UTXOs with script size <= 25 bytes.
///    2) "Medium" to store UTXOs with script size > 25 bytes && <= 201 bytes.
///    3) "Large" to store UTXOs with script size > 201 bytes.
pub struct Utxos {
    // A map storing the UTXOs that are "small" in size.
    // TODO(EXC-1039): Use pagemap instead of a vec.
    pub small_utxos: StableBTreeMap<VectorMemory>,

    // A map storing the UTXOs that are "medium" in size.
    // TODO(EXC-1039): Use pagemap instead of a vec.
    pub medium_utxos: StableBTreeMap<VectorMemory>,

    // A map storing the UTXOs that are "large" in size.
    // The number of entries stored in this map is tiny (see docs above), so a
    // standard `BTreeMap` suffices.
    pub large_utxos: BTreeMap<OutPoint, (TxOut, Height)>,

    // References to the memory used in stable structures for protobuf conversions.
    small_utxos_memory: VectorMemory,
    medium_utxos_memory: VectorMemory,
}

// The size of an outpoint in bytes.
const OUTPOINT_TX_ID_SIZE: u32 = 32; // The size of the transaction ID.
const OUTPOINT_VOUT_SIZE: u32 = 4; // The size of a transaction's vout.
const OUTPOINT_SIZE: u32 = OUTPOINT_TX_ID_SIZE + OUTPOINT_VOUT_SIZE;

// The maximum size in bytes of a bitcoin script for it to be considered "small".
const TX_OUT_SCRIPT_MAX_SIZE_SMALL: u32 = 25;

// The maximum size in bytes of a bitcoin script for it to be considered "medium".
const TX_OUT_SCRIPT_MAX_SIZE_MEDIUM: u32 = 201;

// A transaction output's value in satoshis is a `u64`, which is 8 bytes.
const TX_OUT_VALUE_SIZE: u32 = 8;

const TX_OUT_MAX_SIZE_SMALL: u32 = TX_OUT_SCRIPT_MAX_SIZE_SMALL + TX_OUT_VALUE_SIZE;

const TX_OUT_MAX_SIZE_MEDIUM: u32 = TX_OUT_SCRIPT_MAX_SIZE_MEDIUM + TX_OUT_VALUE_SIZE;

// The height is a `u32`, which is 4 bytes.
const HEIGHT_SIZE: u32 = 4;

/// The size of a key in the UTXOs map, which is an outpoint.
pub const UTXO_KEY_SIZE: u32 = OUTPOINT_SIZE;

/// The max size of a value in the "small UTXOs" map.
pub const UTXO_VALUE_MAX_SIZE_SMALL: u32 = TX_OUT_MAX_SIZE_SMALL + HEIGHT_SIZE;

/// The max size of a value in the "medium UTXOs" map.
pub const UTXO_VALUE_MAX_SIZE_MEDIUM: u32 = TX_OUT_MAX_SIZE_MEDIUM + HEIGHT_SIZE;

// The longest addresses are bech32 addresses, and a bech32 string can be at most 90 chars.
// See https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki
const MAX_ADDRESS_SIZE: u32 = 90;
const MAX_ADDRESS_OUTPOINT_SIZE: u32 = MAX_ADDRESS_SIZE + OUTPOINT_SIZE;

impl Default for Utxos {
    fn default() -> Self {
        let small_utxos_memory = make_memory();
        let medium_utxos_memory = make_memory();

        Self {
            small_utxos: StableBTreeMap::new(
                small_utxos_memory.clone(),
                UTXO_KEY_SIZE,
                UTXO_VALUE_MAX_SIZE_SMALL,
            ),
            medium_utxos: StableBTreeMap::new(
                medium_utxos_memory.clone(),
                UTXO_KEY_SIZE,
                UTXO_VALUE_MAX_SIZE_MEDIUM,
            ),
            large_utxos: BTreeMap::default(),
            small_utxos_memory,
            medium_utxos_memory,
        }
    }
}

impl Utxos {
    pub fn len(&self) -> u64 {
        self.large_utxos.len() as u64 + self.small_utxos.len() + self.medium_utxos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.large_utxos.is_empty() && self.small_utxos.is_empty() && self.medium_utxos.is_empty()
    }
}

pub struct UtxoSet {
    pub utxos: Utxos,
    pub network: Network,
    // An index for fast retrievals of an address's UTXOs.
    // TODO(EXC-1039): Use pagemap instead of a vec.
    pub address_to_outpoints: StableBTreeMap<VectorMemory>,

    // If true, a transaction's inputs must all be present in the UTXO for it to be accepted.
    pub strict: bool,

    // Reference to the memory used in `address_to_outpoints` for protobuf conversions.
    // This won't be necessary once we migrate to `PageMap`.
    pub address_to_outpoints_memory: VectorMemory,
}

impl UtxoSet {
    pub fn new(strict: bool, network: Network) -> Self {
        let address_to_outpoints_memory = make_memory();

        Self {
            utxos: Utxos::default(),
            address_to_outpoints: StableBTreeMap::new(
                address_to_outpoints_memory.clone(),
                MAX_ADDRESS_OUTPOINT_SIZE,
                0, // No values are stored in the map.
            ),
            strict,
            network,
            address_to_outpoints_memory,
        }
    }

    pub fn to_proto(&self) -> proto::UtxoSet {
        proto::UtxoSet {
            small_utxos: self.utxos.small_utxos_memory.borrow().clone(),
            medium_utxos: self.utxos.medium_utxos_memory.borrow().clone(),
            large_utxos: self
                .utxos
                .large_utxos
                .iter()
                .map(|(outpoint, (txout, height))| proto::Utxo {
                    outpoint: Some(v1::OutPoint {
                        txid: outpoint.txid.to_vec(),
                        vout: outpoint.vout,
                    }),
                    txout: Some(v1::TxOut {
                        value: txout.value,
                        script_pubkey: txout.script_pubkey.to_bytes(),
                    }),
                    height: *height,
                })
                .collect(),
            address_to_outpoints: self.address_to_outpoints_memory.borrow().clone(),
            strict: self.strict,
            network: match self.network {
                Network::Bitcoin => 0,
                Network::Testnet => 1,
                Network::Signet => 2,
                Network::Regtest => 3,
            },
        }
    }

    pub fn from_proto(utxos_proto: proto::UtxoSet) -> Self {
        let small_utxos_memory = Rc::new(RefCell::new(utxos_proto.small_utxos));
        let medium_utxos_memory = Rc::new(RefCell::new(utxos_proto.medium_utxos));
        let address_to_outpoints_memory = Rc::new(RefCell::new(utxos_proto.address_to_outpoints));
        let utxos = Utxos {
            small_utxos: StableBTreeMap::load(small_utxos_memory.clone()),
            medium_utxos: StableBTreeMap::load(medium_utxos_memory.clone()),
            large_utxos: utxos_proto
                .large_utxos
                .into_iter()
                .map(|utxo| {
                    let outpoint = utxo
                        .outpoint
                        .map(|o| {
                            OutPoint::new(
                                Txid::from_hash(Hash::from_slice(&o.txid).unwrap()),
                                o.vout,
                            )
                        })
                        .unwrap();

                    let tx_out = utxo
                        .txout
                        .map(|t| TxOut {
                            value: t.value,
                            script_pubkey: Script::from(t.script_pubkey),
                        })
                        .unwrap();

                    (outpoint, (tx_out, utxo.height))
                })
                .collect(),
            small_utxos_memory,
            medium_utxos_memory,
        };

        Self {
            utxos,
            address_to_outpoints: StableBTreeMap::load(address_to_outpoints_memory.clone()),
            address_to_outpoints_memory,
            strict: utxos_proto.strict,
            network: match utxos_proto.network {
                0 => Network::Bitcoin,
                1 => Network::Testnet,
                2 => Network::Signet,
                3 => Network::Regtest,
                _ => panic!("Invalid network ID"),
            },
        }
    }
}

/// A data structure for maintaining all unstable blocks.
///
/// A block `b` is considered stable if:
///   depth(block) ≥ stability_threshold
///   ∀ b', height(b') = height(b): depth(b) - depth(b’) ≥ stability_threshold
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct UnstableBlocks {
    pub stability_threshold: u64,
    pub tree: BlockTree,
}

impl UnstableBlocks {
    pub fn new(stability_threshold: u64, anchor: Block) -> Self {
        Self {
            stability_threshold,
            tree: BlockTree::new(anchor),
        }
    }

    pub fn to_proto(&self) -> proto::UnstableBlocks {
        proto::UnstableBlocks {
            stability_threshold: self.stability_threshold,
            tree: Some(self.tree.to_proto()),
        }
    }

    pub fn from_proto(block_forest_proto: proto::UnstableBlocks) -> Self {
        Self {
            stability_threshold: block_forest_proto.stability_threshold,
            tree: BlockTree::from_proto(
                block_forest_proto
                    .tree
                    .expect("BlockTree must be present in the proto"),
            ),
        }
    }
}

/// Maintains a tree of connected blocks.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct BlockTree {
    pub root: Block,
    pub children: Vec<BlockTree>,
}

impl BlockTree {
    /// Creates a new `BlockTree` with the given block as its root.
    pub fn new(root: Block) -> Self {
        Self {
            root,
            children: vec![],
        }
    }

    pub fn to_proto(&self) -> proto::BlockTree {
        proto::BlockTree {
            root: Some(block::to_proto(&self.root)),
            children: self.children.iter().map(|t| t.to_proto()).collect(),
        }
    }

    pub fn from_proto(block_tree_proto: proto::BlockTree) -> Self {
        Self {
            root: block::from_proto(&block_tree_proto.root.unwrap()),
            children: block_tree_proto
                .children
                .into_iter()
                .map(BlockTree::from_proto)
                .collect(),
        }
    }
}

fn make_memory() -> VectorMemory {
    Rc::new(RefCell::new(Vec::new()))
}
