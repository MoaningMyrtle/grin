// Copyright 2017 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The primary module containing the implementations of the transaction pool
//! and its top-level members.

use std::vec::Vec;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

use secp::pedersen::Commitment;

pub use graph;

use time;

use core::core::transaction;
use core::core::block;
use core::core::hash;



/// Placeholder: the data representing where we heard about a tx from.
///
/// Used to make decisions based on transaction acceptance priority from 
/// various sources. For example, a node may want to bypass pool size
/// restrictions when accepting a transaction from a local wallet.
///
/// Most likely this will evolve to contain some sort of network identifier, 
/// once we get a better sense of what transaction building might look like.
pub struct TxSource {
    /// Human-readable name used for logging and errors.
    pub debug_name: String,
    /// Unique identifier used to distinguish this peer from others.
    pub identifier: String,
}

/// This enum describes the parent for a given input of a transaction.
#[derive(Clone)]
pub enum Parent {
    Unknown,
    BlockTransaction,
    PoolTransaction{tx_ref: hash::Hash},
    AlreadySpent{other_tx: hash::Hash},
}

impl fmt::Debug for Parent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Parent::Unknown => write!(f, "Parent: Unknown"),
            &Parent::BlockTransaction => write!(f, "Parent: Block Transaction"),
            &Parent::PoolTransaction{tx_ref: x} => write!(f,
                "Parent: Pool Transaction ({:?})", x),
            &Parent::AlreadySpent{other_tx: x} => write!(f,
                "Parent: Already Spent By {:?}", x),
        }
    }
}

#[derive(Debug)]
pub enum PoolError {
    Invalid,
    AlreadyInPool,
    DuplicateOutput{other_tx: Option<hash::Hash>, in_chain: bool,
        output: Commitment},
    DoubleSpend{other_tx: hash::Hash, spent_output: Commitment},
    // An orphan successfully added to the orphans set
    OrphanTransaction,
}

/// Pool contains the elements of the graph that are connected, in full, to
/// the blockchain.
/// Reservations of outputs by orphan transactions (not fully connected) are
/// not respected.
/// Spending references (input -> output) exist in two structures: internal 
/// graph references are contained in the pool edge sets, while references 
/// sourced from the blockchain's UTXO set are contained in the 
/// blockchain_connections set.
/// Spent by references (output-> input) exist in two structures: pool-pool
/// connections are in the pool edge set, while unspent (dangling) references
/// exist in the available_outputs set.
pub struct Pool {
    graph : graph::DirectedGraph,

    // available_outputs are unspent outputs of the current pool set, 
    // maintained as edges with empty destinations, keyed by the 
    // output's hash.
    available_outputs: HashMap<Commitment, graph::Edge>,

    // Consumed blockchain utxo's are kept in a separate map. 
    consumed_blockchain_outputs: HashMap<Commitment, graph::Edge>
}

impl Pool {
    pub fn has_available_output(&self, c: &Commitment) -> bool {
        self.available_outputs.contains_key(c)
    }

    /// Given an output, return the transaction hash generating the 
    /// available (unspent) output commitment, if one exists.
    pub fn search_for_available_output(&self, c: &Commitment) -> Option<hash::Hash> {
        match self.available_outputs.get(c) {
            Some(e) => e.source_hash(),
            None => None
        }
    }

    /// Given an output, check if a spending reference (input -> output)
    /// already exists in the pool.
    /// Returns the transaction (kernel) hash corresponding to the conflicting
    /// transaction
    pub fn check_double_spend(&self, o: &transaction::Output) -> Option<hash::Hash> {
        self.graph.get_edge_by_commitment(&o.commitment()).or(self.consumed_blockchain_outputs.get(&o.commitment())).map(|x| x.destination_hash().unwrap())
    }


    pub fn get_blockchain_spent(&self, c: &Commitment) -> Option<&graph::Edge> {
        self.consumed_blockchain_outputs.get(c)
    }

    pub fn add_pool_transaction(&mut self, pool_entry: graph::PoolEntry,
        blockchain_refs: Vec<graph::Edge>, pool_refs: Vec<graph::Edge>,
        new_unspents: Vec<graph::Edge>) {

        // Removing consumed available_outputs
        for new_edge in &pool_refs {
            // All of these should correspond to an existing unspent
            assert!(self.available_outputs.remove(&new_edge.output_commitment()).is_some());
        }

        // Accounting for consumed blockchain outputs
        for new_blockchain_edge in blockchain_refs.drain(..) {
            self.consumed_blockchain_outputs.insert(
                new_blockchain_edge.output_commitment(),
                new_blockchain_edge);
        }

        // Adding the transaction to the vertices list along with internal
        // pool edges
        self.graph.add_entry(pool_entry, pool_refs);

        // Adding the new unspents to the unspent map
        for unspent_output in new_unspents.drain(..) {
            self.available_outputs.insert(
                unspent_output.output_commitment(), unspent_output);
        }
    }
}

impl TransactionGraphContainer for Pool { 
    fn get_available_output(&self, output: &Commitment) -> Option<graph::Edge> {
        self.available_outputs.get(output)
    }
    fn get_external_spent_output(&self, output: &Commitment) -> Option<graph::Edge> {
        self.blockchain_connections.get(output)
    }
    fn get_internal_spent_output(&self, output: &Commitment) -> Option<graph::Edge> {
        self.graph.get_edge_by_commitment(output)
    }
}

/// Orphans contains the elements of the transaction graph that have not been
/// connected in full to the blockchain. 
pub struct Orphans {
    graph : graph::DirectedGraph,

    // available_outputs are unspent outputs of the current orphan set, 
    // maintained as edges with empty destinations.
    available_outputs: HashMap<Commitment, graph::Edge>,

    // missing_outputs are spending references (inputs) with missing 
    // corresponding outputs, maintained as edges with empty sources.
    missing_outputs: HashMap<Commitment, graph::Edge>,

    // pool_connections are bidirectional edges which connect to the pool
    // graph. They should map one-to-one to pool graph available_outputs. 
    // pool_connections should not be viewed authoritatively, they are 
    // merely informational until the transaction is officially connected to
    // the pool.
    pool_connections: HashMap<Commitment, graph::Edge>,
}

impl Orphans {
    /// Checks for a double spent output, given the hash of the output, 
    /// ONLY in the data maintained by the orphans set. This includes links
    /// to the pool as well as links internal to orphan transactions.
    /// Returns the transaction hash corresponding to the conflicting
    /// transaction.
    fn check_double_spend(&self, o: transaction::Output) -> Option<hash::Hash> {
        self.graph.get_edge_by_commitment(&o.commitment()).or(self.pool_connections.get(&o.commitment())).map(|x| x.destination_hash().unwrap())
    }

    pub fn get_unknown_output(&self, output: &Commitment) -> Option<graph::Edge> {
        self.missing_outputs.get(output)
    }

    /// Add an orphan transaction to the orphans set.
    ///
    /// This method adds a given transaction (represented by the PoolEntry at
    /// orphan_entry) to the orphans set.
    ///
    /// This method has no failure modes. All checks should be passed before
    /// entry.
    ///
    /// Expects a HashMap at is_missing describing the indices of orphan_refs
    /// which correspond to missing (vs orphan-to-orphan) links.
    pub fn add_orphan_transaction(&mut self, orphan_entry: graph::PoolEntry,
        pool_refs: Vec<graph::Edge>, orphan_refs: Vec<graph::Edge>,
        is_missing: HashMap<usize, ()>, new_unspents: Vec<graph::Edge>) {

        // Removing consumed available_outputs
        for (i, new_edge) in orphan_refs.drain(..).enumerate() {
            if is_missing.contains_key(&i) {
                self.missing_outputs.insert(new_edge.output_commitment(),
                    new_edge);
            } else {
                assert!(self.available_outputs.remove(new_edge.output_commitment()).is_some());
                self.graph.add_edge_only(new_edge);
            }
        }

        // Accounting for consumed blockchain and pool outputs
        for external_edge in pool_refs.drain(..) {
            self.pool_connections.insert(
                external_edge.output_commitment(), external_edge);
        }

        // if missing_refs is the same length as orphan_refs, we have
        // no orphan-orphan links for this transaction and it is a
        // root transaction of the orphans set
        self.graph.add_vertex_only(orphan_entry,
            missing_refs.len() == orphan_refs.len());


        // Adding the new unspents to the unspent map
        for unspent_output in new_unspents.drain(..) {
            self.available_outputs.insert(
                unspent_output.output_commitment(), unspent_output);
        }
    }
}

impl TransactionGraphContainer for Orphans {
    fn get_available_output(&self, output: &Commitment) -> Option<graph::Edge> {
        self.available_outputs.get(output)
    }
    fn get_external_spent_output(&self, output: &Commitment) -> Option<graph::Edge> {
        self.pool_connections.get(output)
    }
    fn get_internal_spent_output(&self, output: &Commitment) -> Option<graph::Edge> {
        self.graph.get_edge_by_commitment(output)
    }
}

/// Trait for types that embed a graph and connect to external state.
///
/// The types implementing this trait consist of a graph with nodes and edges
/// representing transactions and outputs, respectively. Outputs fall into one
/// of three categories:
/// 1) External spent: An output sourced externally consumed by a transaction
///     in this graph,
/// 2) Internal spent: An output produced by a transaction in this graph and
///     consumed by another transaction in this graph,
/// 3) [External] Unspent: An output produced by a transaction in this graph
///     that is not yet spent.
/// 
/// There is no concept of an external "spent by" reference (output produced by
/// a transaction in the graph spent by a transaction in another source), as 
/// these references are expected to be maintained by descendent graph. Outputs
/// follow a heirarchy (Blockchain -> Pool -> Orphans) where each descendent 
/// exists at a lower priority than their parent. An output consumed by a 
/// child graph is marked as unspent in the parent graph and an external spent
/// in the child. This ensures that no descendent set must modify state in a 
/// set of higher priority.
pub trait TransactionGraphContainer {
    /// Accessor for internal spents
    fn get_internal_spent_output(&self, output: &Commitment) -> Option<graph::Edge>;
    /// Accessor for external unspents
    fn get_available_output(&self, output: &Commitment) -> Option<graph::Edge>;
    /// Accessor for external spents
    fn get_external_spent_output(&self, output: &Commitment) -> Option<graph::Edge>;

    /// Checks if the available_output set has the output at the given
    /// commitment
    fn has_available_output(&self, c: &Commitment) -> bool {
        self.get_available_output(c).is_some()
    }

    /// Checks if the pool has anything by this output already, between 
    /// available outputs and internal ones.
    fn find_output(&self, c: &Commitment) -> Option<hash::Hash> {
        self.get_available_output(c).
            or(self.get_internal_spent_output(c)).
            map(|x| x.source_hash().unwrap())
    }

    /// Search for a spent reference internal to the graph
    fn get_internal_spent(&self, c: &Commitment) -> Option<&graph::Edge> {
        self.get_internal_spent_output(c)
    }

}