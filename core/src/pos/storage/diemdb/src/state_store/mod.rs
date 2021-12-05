// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

// Copyright 2021 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

//! This file defines state store APIs that are related account state Merkle
//! tree.

#[cfg(test)]
mod state_store_test;

use crate::{
    change_set::ChangeSet,
    ledger_counters::LedgerCounter,
    schema::{
        jellyfish_merkle_node::JellyfishMerkleNodeSchema,
        stale_node_index::StaleNodeIndexSchema,
    },
};
use anyhow::Result;
use diem_crypto::HashValue;
use diem_jellyfish_merkle::{
    node_type::NodeKey, JellyfishMerkleTree, TreeReader, TreeWriter,
    ROOT_NIBBLE_HEIGHT,
};
use diem_types::{
    account_address::{AccountAddress, HashAccountAddress},
    account_state_blob::AccountStateBlob,
    proof::{SparseMerkleProof, SparseMerkleRangeProof},
    transaction::Version,
};
use schemadb::{SchemaBatch, DB};
use std::{collections::HashMap, sync::Arc};

type LeafNode = diem_jellyfish_merkle::node_type::LeafNode<AccountStateBlob>;
type Node = diem_jellyfish_merkle::node_type::Node<AccountStateBlob>;
type NodeBatch = diem_jellyfish_merkle::NodeBatch<AccountStateBlob>;

#[derive(Debug)]
pub(crate) struct StateStore {
    db: Arc<DB>,
}

impl StateStore {
    pub fn new(db: Arc<DB>) -> Self { Self { db } }

    /// Get the account state blob given account address and root hash of state
    /// Merkle tree
    pub fn get_account_state_with_proof_by_version(
        &self, address: AccountAddress, version: Version,
    ) -> Result<(
        Option<AccountStateBlob>,
        SparseMerkleProof<AccountStateBlob>,
    )> {
        JellyfishMerkleTree::new(self).get_with_proof(address.hash(), version)
    }

    /// Gets the proof that proves a range of accounts.
    pub fn get_account_state_range_proof(
        &self, rightmost_key: HashValue, version: Version,
    ) -> Result<SparseMerkleRangeProof> {
        JellyfishMerkleTree::new(self).get_range_proof(rightmost_key, version)
    }

    /// Put the results generated by `account_state_sets` to `batch` and return
    /// the result root hashes for each write set.
    pub fn put_account_state_sets(
        &self,
        account_state_sets: Vec<HashMap<AccountAddress, AccountStateBlob>>,
        first_version: Version, cs: &mut ChangeSet,
    ) -> Result<Vec<HashValue>>
    {
        let blob_sets = account_state_sets
            .into_iter()
            .map(|account_states| {
                account_states
                    .into_iter()
                    .map(|(addr, blob)| (addr.hash(), blob))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let (new_root_hash_vec, tree_update_batch) =
            JellyfishMerkleTree::new(self)
                .put_value_sets(blob_sets, first_version)?;

        let num_versions = new_root_hash_vec.len();
        assert_eq!(num_versions, tree_update_batch.node_stats.len());

        tree_update_batch.node_stats.iter().enumerate().for_each(
            |(i, stats)| {
                let counter_bumps = cs.counter_bumps(first_version + i as u64);
                counter_bumps
                    .bump(LedgerCounter::NewStateNodes, stats.new_nodes);
                counter_bumps
                    .bump(LedgerCounter::NewStateLeaves, stats.new_leaves);
                counter_bumps
                    .bump(LedgerCounter::StaleStateNodes, stats.stale_nodes);
                counter_bumps
                    .bump(LedgerCounter::StaleStateLeaves, stats.stale_leaves);
            },
        );
        add_node_batch(&mut cs.batch, &tree_update_batch.node_batch)?;

        tree_update_batch
            .stale_node_index_batch
            .iter()
            .map(|row| cs.batch.put::<StaleNodeIndexSchema>(row, &()))
            .collect::<Result<Vec<()>>>()?;

        Ok(new_root_hash_vec)
    }

    pub fn get_root_hash(&self, version: Version) -> Result<HashValue> {
        JellyfishMerkleTree::new(self).get_root_hash(version)
    }

    pub fn get_root_hash_option(
        &self, version: Version,
    ) -> Result<Option<HashValue>> {
        JellyfishMerkleTree::new(self).get_root_hash_option(version)
    }

    /// Finds the rightmost leaf by scanning the entire DB.
    #[cfg(test)]
    pub fn get_rightmost_leaf_naive(
        &self,
    ) -> Result<Option<(NodeKey, LeafNode)>> {
        let mut ret = None;

        let mut iter = self
            .db
            .iter::<JellyfishMerkleNodeSchema>(Default::default())?;
        iter.seek_to_first();

        while let Some((node_key, node)) = iter.next().transpose()? {
            if let Node::Leaf(leaf_node) = node {
                match ret {
                    None => ret = Some((node_key, leaf_node)),
                    Some(ref other) => {
                        if leaf_node.account_key() > other.1.account_key() {
                            ret = Some((node_key, leaf_node));
                        }
                    }
                }
            }
        }

        Ok(ret)
    }
}

impl TreeReader<AccountStateBlob> for StateStore {
    fn get_node_option(&self, node_key: &NodeKey) -> Result<Option<Node>> {
        self.db.get::<JellyfishMerkleNodeSchema>(node_key)
    }

    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        // Since everything has the same version during restore, we seek to the
        // first node and get its version.
        let mut iter = self
            .db
            .iter::<JellyfishMerkleNodeSchema>(Default::default())?;
        iter.seek_to_first();
        let version = match iter.next().transpose()? {
            Some((node_key, _node)) => node_key.version(),
            None => return Ok(None),
        };

        // The encoding of key and value in DB looks like:
        //
        // | <-------------- key --------------> | <- value -> |
        // | version | num_nibbles | nibble_path |    node     |
        //
        // Here version is fixed. For each num_nibbles, there could be a range
        // of nibble paths of the same length. If one of them is the
        // rightmost leaf R, it must be at the end of this
        // range. Otherwise let's assume the R is in the middle of the range, so
        // we call the node at the end of this range X:
        //   1. If X is leaf, then X.account_key() > R.account_key(), because
        // the nibble path is a      prefix of the account key. So R is
        // not the rightmost leaf.   2. If X is internal node, then X
        // must be on the right side of R, so all its children's
        //      account keys are larger than R.account_key(). So R is not the
        // rightmost leaf.
        //
        // Given that num_nibbles ranges from 0 to ROOT_NIBBLE_HEIGHT, there are
        // only ROOT_NIBBLE_HEIGHT+1 ranges, so we can just find the
        // node at the end of each range and then pick the one with the
        // largest account key.
        let mut ret = None;

        for num_nibbles in 1..=ROOT_NIBBLE_HEIGHT + 1 {
            let mut iter = self
                .db
                .iter::<JellyfishMerkleNodeSchema>(Default::default())?;
            // nibble_path is always non-empty except for the root, so if we use
            // an empty nibble path as the seek key, the iterator
            // will end up pointing to the end of the previous
            // range.
            let seek_key = (version, num_nibbles as u8);
            iter.seek_for_prev(&seek_key)?;

            if let Some((node_key, node)) = iter.next().transpose()? {
                debug_assert_eq!(node_key.version(), version);
                debug_assert!(
                    node_key.nibble_path().num_nibbles() < num_nibbles
                );

                if let Node::Leaf(leaf_node) = node {
                    match ret {
                        None => ret = Some((node_key, leaf_node)),
                        Some(ref other) => {
                            if leaf_node.account_key() > other.1.account_key() {
                                ret = Some((node_key, leaf_node));
                            }
                        }
                    }
                }
            }
        }

        Ok(ret)
    }
}

impl TreeWriter<AccountStateBlob> for StateStore {
    fn write_node_batch(&self, node_batch: &NodeBatch) -> Result<()> {
        let mut batch = SchemaBatch::new();
        add_node_batch(&mut batch, node_batch)?;
        self.db.write_schemas(batch, false)
    }
}

fn add_node_batch(
    batch: &mut SchemaBatch, node_batch: &NodeBatch,
) -> Result<()> {
    node_batch
        .iter()
        .map(|(node_key, node)| {
            batch.put::<JellyfishMerkleNodeSchema>(node_key, node)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(())
}
