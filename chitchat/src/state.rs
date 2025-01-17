use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BinaryHeap, HashSet};
use std::net::SocketAddr;
use std::time::Instant;

use rand::prelude::SliceRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::delta::{Delta, DeltaWriter};
use crate::digest::Digest;
use crate::{NodeId, Version, VersionedValue};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct NodeState {
    pub key_values: BTreeMap<String, VersionedValue>,
    #[serde(skip)]
    #[serde(default = "Instant::now")]
    last_heartbeat: Instant,
    pub max_version: u64,
}

impl Default for NodeState {
    fn default() -> Self {
        Self {
            last_heartbeat: Instant::now(),
            max_version: Default::default(),
            key_values: Default::default(),
        }
    }
}

impl NodeState {
    /// Returns an iterator over keys matching the given predicate.
    /// Keys marked for deletion are not returned.
    pub fn iter_key_values(
        &self,
        predicate: impl Fn(&String, &VersionedValue) -> bool,
    ) -> impl Iterator<Item = (&str, &VersionedValue)> {
        self.internal_iter_key_values(predicate)
            .filter(|&(_, versioned_value)| !versioned_value.marked_for_deletion)
    }

    /// Returns an iterator over keys matching the given predicate.
    /// Not public as it returns also keys marked for deletion.
    fn internal_iter_key_values(
        &self,
        predicate: impl Fn(&String, &VersionedValue) -> bool,
    ) -> impl Iterator<Item = (&str, &VersionedValue)> {
        self.key_values
            .iter()
            .filter(move |&(key, versioned_value)| predicate(key, versioned_value))
            .map(|(key, record)| (key.as_str(), record))
    }

    /// Returns an iterator over the version values that are older than `floor_version`.
    fn iter_stale_key_values(
        &self,
        floor_version: u64,
    ) -> impl Iterator<Item = (&str, &VersionedValue)> {
        // TODO optimize by checking the max version.
        self.internal_iter_key_values(move |_key, versioned_value| {
            versioned_value.version > floor_version
        })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.get_versioned(key)
            .map(|versioned_value| versioned_value.value.as_str())
    }

    pub fn get_versioned(&self, key: &str) -> Option<&VersionedValue> {
        self.key_values.get(key)
    }

    /// Sets a new value for a given key.
    ///
    /// Setting a new value automatically increments the
    /// version of the entire NodeState regardless of whether the
    /// value is really changed or not.
    pub fn set<K: ToString, V: ToString>(&mut self, key: K, value: V) {
        let new_version = self.max_version + 1;
        self.set_with_version(key.to_string(), value.to_string(), new_version);
    }

    pub fn mark_for_deletion(&mut self, key: &str) {
        let new_version = self.max_version + 1;
        self.max_version = new_version;
        if let Some(versioned_value) = self.key_values.get_mut(key) {
            versioned_value.marked_for_deletion = true;
            versioned_value.version = new_version;
        }
    }

    // Remove keys marked for deletion and with `version + grace_period < max_version`.
    pub fn gc_keys_marked_for_deletion(&mut self, grace_period: usize) {
        self.key_values.retain(|_, versioned_value| {
            !(versioned_value.marked_for_deletion
                && versioned_value.version + (grace_period as u64) < self.max_version)
        });
    }

    fn set_with_version(&mut self, key: String, value: String, version: Version) {
        assert!(version > self.max_version);
        self.max_version = version;
        self.key_values.insert(
            key,
            VersionedValue {
                version,
                value,
                marked_for_deletion: false,
            },
        );
    }
}

#[derive(Debug)]
pub struct ClusterState {
    pub node_states: BTreeMap<NodeId, NodeState>,
    seed_addrs: watch::Receiver<HashSet<SocketAddr>>,
}

#[cfg(test)]
impl Default for ClusterState {
    fn default() -> Self {
        let (_seed_addrs_tx, seed_addrs_rx) = watch::channel(Default::default());
        Self {
            node_states: Default::default(),
            seed_addrs: seed_addrs_rx,
        }
    }
}

impl ClusterState {
    pub fn with_seed_addrs(seed_addrs: watch::Receiver<HashSet<SocketAddr>>) -> ClusterState {
        ClusterState {
            seed_addrs,
            node_states: BTreeMap::new(),
        }
    }

    pub(crate) fn node_state_mut(&mut self, node_id: &NodeId) -> &mut NodeState {
        // TODO use the `hash_raw_entry` feature once it gets stabilized.
        self.node_states.entry(node_id.clone()).or_default()
    }

    pub fn node_state(&self, node_id: &NodeId) -> Option<&NodeState> {
        self.node_states.get(node_id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &NodeId> {
        self.node_states.keys()
    }

    pub fn seed_addrs(&self) -> HashSet<SocketAddr> {
        self.seed_addrs.borrow().clone()
    }

    pub(crate) fn remove_node(&mut self, node_id: &NodeId) {
        self.node_states.remove(node_id);
    }

    pub(crate) fn apply_delta(&mut self, delta: Delta) {
        // Remove nodes to reset.
        self.node_states
            .retain(|node_id, _| !delta.nodes_to_reset.contains(node_id));
        // And apply delta.
        for (node_id, node_delta) in delta.node_deltas {
            let mut node_state_map = self
                .node_states
                .entry(node_id)
                .or_insert_with(NodeState::default);

            for (key, versioned_value) in node_delta.key_values {
                node_state_map.max_version =
                    node_state_map.max_version.max(versioned_value.version);
                let entry = node_state_map.key_values.entry(key);
                match entry {
                    Entry::Occupied(mut record) => {
                        if record.get().version >= versioned_value.version {
                            // Due to the message passing being totally asynchronous, it is not an
                            // error to receive updates that are already obsolete.
                            continue;
                        }
                        record.insert(versioned_value);
                    }
                    Entry::Vacant(vacant) => {
                        vacant.insert(versioned_value);
                    }
                }
            }

            node_state_map.last_heartbeat = Instant::now();
        }
    }

    pub fn compute_digest(&self, dead_nodes: &HashSet<&NodeId>) -> Digest {
        Digest {
            node_max_version: self
                .node_states
                .iter()
                .filter(|(node_id, _)| !dead_nodes.contains(node_id))
                .map(|(node_id, node_state)| (node_id.clone(), node_state.max_version))
                .collect(),
        }
    }

    pub fn gc_keys_marked_for_deletion(
        &mut self,
        marked_for_deletion_grace_period: usize,
        dead_nodes: &HashSet<NodeId>,
    ) {
        for (node_id, node_state_map) in &mut self.node_states {
            if dead_nodes.contains(node_id) {
                continue;
            }
            node_state_map.gc_keys_marked_for_deletion(marked_for_deletion_grace_period);
        }
    }

    /// Implements the scuttlebutt reconciliation with the scuttle-depth ordering.
    pub fn compute_delta(
        &self,
        digest: &Digest,
        mtu: usize,
        dead_nodes: HashSet<&NodeId>,
        marked_for_deletion_grace_period: usize,
    ) -> Delta {
        let mut delta_writer = DeltaWriter::with_mtu(mtu);

        let mut node_sorted_by_stale_length = NodeSortedByStaleLength::default();
        for (node_id, node_state_map) in &self.node_states {
            if dead_nodes.contains(node_id) {
                continue;
            }
            let mut floor_version = digest.node_max_version.get(node_id).cloned().unwrap_or(0);
            // Node needs to be reset if `digest.node_max_version +
            // marked_for_deletion_grace_period` is inferior to
            // `node_state_map.max_version`.
            // Note that there is no need to reset if floor_version = 0 (new node).
            if floor_version > 0
                && floor_version + (marked_for_deletion_grace_period as u64)
                    < node_state_map.max_version
            {
                // `floor_version` is set to 0 so the delta is populated with all keys and values.
                floor_version = 0;
                delta_writer.add_node_to_reset(node_id.clone());
            }
            let stale_kv_count = node_state_map.iter_stale_key_values(floor_version).count();
            if stale_kv_count > 0 {
                node_sorted_by_stale_length.insert(node_id, stale_kv_count);
            }
        }

        for node_id in node_sorted_by_stale_length.into_iter() {
            if !delta_writer.add_node(node_id.clone()) {
                break;
            }
            let node_state_map = self.node_states.get(node_id).unwrap();
            let mut floor_version = digest.node_max_version.get(node_id).cloned().unwrap_or(0);
            if node_state_map.max_version
                > floor_version + (marked_for_deletion_grace_period as u64)
            {
                floor_version = 0;
            }
            let mut stale_kvs: Vec<(&str, &VersionedValue)> = node_state_map
                .iter_stale_key_values(floor_version)
                .collect();

            assert!(!stale_kvs.is_empty());
            stale_kvs.sort_unstable_by_key(|(_, record)| record.version);
            for (key, versioned_value) in stale_kvs {
                if !delta_writer.add_kv(key, versioned_value.clone()) {
                    let delta: Delta = delta_writer.into();
                    return delta;
                }
            }
        }
        delta_writer.into()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ClusterStateSnapshot {
    pub seed_addrs: HashSet<SocketAddr>,
    pub node_states: BTreeMap<String, NodeState>,
}

impl<'a> From<&'a ClusterState> for ClusterStateSnapshot {
    fn from(state: &'a ClusterState) -> Self {
        ClusterStateSnapshot {
            seed_addrs: state.seed_addrs(),
            node_states: state
                .node_states
                .iter()
                .map(|(node_id, node_state)| (node_id.id.clone(), node_state.clone()))
                .collect(),
        }
    }
}

#[derive(Default)]
struct NodeSortedByStaleLength<'a> {
    node_per_stale_length: BTreeMap<usize, Vec<&'a NodeId>>,
    stale_lengths: BinaryHeap<usize>,
}

impl<'a> NodeSortedByStaleLength<'a> {
    fn insert(&mut self, node_id: &'a NodeId, stale_length: usize) {
        self.node_per_stale_length
            .entry(stale_length)
            .or_insert_with(|| {
                self.stale_lengths.push(stale_length);
                Vec::new()
            })
            .push(node_id);
    }

    fn into_iter(mut self) -> impl Iterator<Item = &'a NodeId> {
        let mut rng = random_generator();
        std::iter::from_fn(move || self.stale_lengths.pop()).flat_map(move |length| {
            let mut nodes = self.node_per_stale_length.remove(&length).unwrap();
            nodes.shuffle(&mut rng);
            nodes.into_iter()
        })
    }
}

#[cfg(not(test))]
fn random_generator() -> impl Rng {
    rand::thread_rng()
}

// We use a deterministic random generator in tests.
#[cfg(test)]
fn random_generator() -> impl Rng {
    use rand::prelude::StdRng;
    use rand::SeedableRng;
    StdRng::seed_from_u64(9u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialize::Serializable;
    use crate::MAX_UDP_DATAGRAM_PAYLOAD_SIZE;

    #[test]
    fn test_node_sorted_by_stale_length_empty() {
        let node_sorted_by_stale_length = NodeSortedByStaleLength::default();
        assert!(node_sorted_by_stale_length.into_iter().next().is_none());
    }

    #[test]
    fn test_node_sorted_by_stale_length_simple() {
        let mut node_sorted_by_stale_length = NodeSortedByStaleLength::default();
        let node_ids = vec![(10_001, 1), (10_002, 2), (10_003, 3)]
            .into_iter()
            .map(|(port, state_length)| (NodeId::for_test_localhost(port), state_length))
            .collect::<Vec<_>>();
        for (node_id, state_length) in node_ids.iter() {
            node_sorted_by_stale_length.insert(node_id, *state_length);
        }
        let nodes: Vec<&NodeId> = node_sorted_by_stale_length.into_iter().collect();
        let expected_nodes: Vec<NodeId> = [10_003, 10_002, 10_001]
            .into_iter()
            .map(NodeId::for_test_localhost)
            .collect();
        assert_eq!(nodes, expected_nodes.iter().collect::<Vec<_>>());
    }

    #[test]
    fn test_node_sorted_by_stale_length_doubles() {
        let mut node_sorted_by_stale_length = NodeSortedByStaleLength::default();
        let node_ids = vec![(10_001, 1), (20_001, 2), (20_002, 2), (20_003, 2)]
            .into_iter()
            .map(|(port, state_length)| (NodeId::for_test_localhost(port), state_length))
            .collect::<Vec<_>>();

        for (node_id, state_length) in node_ids.iter() {
            node_sorted_by_stale_length.insert(node_id, *state_length);
        }

        let nodes: Vec<NodeId> = node_sorted_by_stale_length.into_iter().cloned().collect();
        let expected_nodes: Vec<NodeId> = vec![20_002, 20_001, 20_003, 10_001]
            .into_iter()
            .map(NodeId::for_test_localhost)
            .collect();
        assert_eq!(nodes, expected_nodes);
    }

    #[test]
    fn test_cluster_state_missing_node() {
        let cluster_state = ClusterState::default();
        let node_state = cluster_state.node_state(&NodeId::for_test_localhost(10_001));
        assert!(node_state.is_none());
    }

    #[test]
    fn test_cluster_state_first_version_is_one() {
        let mut cluster_state = ClusterState::default();
        let node_state = cluster_state.node_state_mut(&NodeId::for_test_localhost(10_001));
        node_state.set("key_a", "");
        assert_eq!(
            node_state.get_versioned("key_a").unwrap(),
            &VersionedValue {
                value: "".to_string(),
                version: 1,
                marked_for_deletion: false,
            }
        );
    }

    #[test]
    fn test_cluster_state_set() {
        let mut cluster_state = ClusterState::default();
        let node_state = cluster_state.node_state_mut(&NodeId::for_test_localhost(10_001));
        node_state.set("key_a", "1");
        assert_eq!(
            node_state.get_versioned("key_a").unwrap(),
            &VersionedValue {
                value: "1".to_string(),
                version: 1,
                marked_for_deletion: false,
            }
        );
        node_state.set("key_b", "2");
        assert_eq!(
            node_state.get_versioned("key_a").unwrap(),
            &VersionedValue {
                value: "1".to_string(),
                version: 1,
                marked_for_deletion: false,
            }
        );
        assert_eq!(
            node_state.get_versioned("key_b").unwrap(),
            &VersionedValue {
                value: "2".to_string(),
                version: 2,
                marked_for_deletion: false,
            }
        );
        node_state.set("key_a", "3");
        assert_eq!(
            node_state.get_versioned("key_a").unwrap(),
            &VersionedValue {
                value: "3".to_string(),
                version: 3,
                marked_for_deletion: false,
            }
        );
    }

    #[test]
    fn test_cluster_state_set_with_same_value_updates_version() {
        let mut cluster_state = ClusterState::default();
        let node_state = cluster_state.node_state_mut(&NodeId::for_test_localhost(10_001));
        node_state.set("key", "1");
        assert_eq!(
            node_state.get_versioned("key").unwrap(),
            &VersionedValue {
                value: "1".to_string(),
                version: 1,
                marked_for_deletion: false,
            }
        );
        node_state.set("key", "1");
        assert_eq!(
            node_state.get_versioned("key").unwrap(),
            &VersionedValue {
                value: "1".to_string(),
                version: 2,
                marked_for_deletion: false,
            }
        );
    }

    #[test]
    fn test_cluster_state_set_and_mark_for_deletion() {
        let mut cluster_state = ClusterState::default();
        let node_state = cluster_state.node_state_mut(&NodeId::for_test_localhost(10_001));
        node_state.set("key", "1");
        node_state.mark_for_deletion("key");
        assert_eq!(
            node_state.get_versioned("key").unwrap(),
            &VersionedValue {
                value: "1".to_string(),
                version: 2,
                marked_for_deletion: true,
            }
        );
        node_state.set("key", "2");
        assert_eq!(
            node_state.get_versioned("key").unwrap(),
            &VersionedValue {
                value: "2".to_string(),
                version: 3,
                marked_for_deletion: false,
            }
        );
    }

    #[test]
    fn test_cluster_state_compute_digest() {
        let mut cluster_state = ClusterState::default();
        let node1 = NodeId::for_test_localhost(10_001);
        let node1_state = cluster_state.node_state_mut(&node1);
        node1_state.set("key_a", "");
        node1_state.set("key_b", "");

        let node2 = NodeId::for_test_localhost(10_002);
        let node2_state = cluster_state.node_state_mut(&node2);
        node2_state.set("key_a", "");

        let dead_nodes = HashSet::new();
        let digest = cluster_state.compute_digest(&dead_nodes);
        let mut node_max_version_map = BTreeMap::default();
        node_max_version_map.insert(node1.clone(), 2);
        node_max_version_map.insert(node2.clone(), 1);
        assert_eq!(&digest.node_max_version, &node_max_version_map);

        // exclude node1
        let dead_nodes = HashSet::from_iter([&node1]);
        let digest = cluster_state.compute_digest(&dead_nodes);
        let mut node_max_version_map = BTreeMap::default();
        node_max_version_map.insert(node2, 1);
        assert_eq!(&digest.node_max_version, &node_max_version_map);
    }

    #[test]
    fn test_cluster_state_gc_keys_marked_for_deletion() {
        let mut cluster_state = ClusterState::default();
        let node1 = NodeId::for_test_localhost(10_001);
        let node1_state = cluster_state.node_state_mut(&node1);
        node1_state.set_with_version("key_a".to_string(), "1".to_string(), 1); // 1
        node1_state.mark_for_deletion("key_a"); // 2
        node1_state.set_with_version("key_b".to_string(), "3".to_string(), 13); // 3

        // No gc.
        cluster_state.gc_keys_marked_for_deletion(11, &HashSet::new());
        assert!(cluster_state
            .node_state(&node1)
            .unwrap()
            .key_values
            .get_key_value("key_a")
            .is_some());
        assert!(cluster_state
            .node_state(&node1)
            .unwrap()
            .key_values
            .get_key_value("key_b")
            .is_some());
        // Gc.
        cluster_state.gc_keys_marked_for_deletion(10, &HashSet::new());
        assert!(cluster_state
            .node_state(&node1)
            .unwrap()
            .key_values
            .get_key_value("key_a")
            .is_none());
        assert!(cluster_state
            .node_state(&node1)
            .unwrap()
            .key_values
            .get_key_value("key_b")
            .is_some());
    }

    #[test]
    fn test_cluster_state_apply_delta() {
        let mut cluster_state = ClusterState::default();

        let node1 = NodeId::for_test_localhost(10_001);
        let node1_state = cluster_state.node_state_mut(&node1);
        node1_state.set_with_version("key_a".to_string(), "1".to_string(), 1); // 1
        node1_state.set_with_version("key_b".to_string(), "3".to_string(), 3); // 2
        let node2 = NodeId::for_test_localhost(10_002);
        let node2_state = cluster_state.node_state_mut(&node2);
        node2_state.set_with_version("key_c".to_string(), "3".to_string(), 1); // 1

        let mut delta = Delta::default();
        delta.add_node_delta(node1.clone(), "key_a", "4", 4, false);
        delta.add_node_delta(node1.clone(), "key_b", "2", 2, false);
        // Node 2 is reset.
        delta.add_node_to_reset(node2.clone());
        delta.add_node_delta(node2.clone(), "key_d", "4", 4, false);
        cluster_state.apply_delta(delta);

        let node1_state = cluster_state.node_state(&node1).unwrap();
        assert_eq!(
            node1_state.get_versioned("key_a").unwrap(),
            &VersionedValue {
                value: "4".to_string(),
                version: 4,
                marked_for_deletion: false,
            }
        );
        // We ignore stale values.
        assert_eq!(
            node1_state.get_versioned("key_b").unwrap(),
            &VersionedValue {
                value: "3".to_string(),
                version: 3,
                marked_for_deletion: false,
            }
        );
        // Check node 2 is reset and is only populated with the new `key_d`.
        let node2_state = cluster_state.node_state(&node2).unwrap();
        assert_eq!(node2_state.key_values.len(), 1);
        assert_eq!(
            node2_state.get_versioned("key_d").unwrap(),
            &VersionedValue {
                value: "4".to_string(),
                version: 4,
                marked_for_deletion: false,
            }
        );
    }

    // This helper test function will test all possible mtu version, and check that the resulting
    // delta matches the expectation.
    fn test_with_varying_max_transmitted_kv_helper(
        cluster_state: &ClusterState,
        digest: &Digest,
        exclude_node_ids: HashSet<&NodeId>,
        expected_delta_atoms: &[(&NodeId, &str, &str, Version, bool)],
    ) {
        let max_delta =
            cluster_state.compute_delta(digest, usize::MAX, exclude_node_ids.clone(), 10_000);
        let mut buf = Vec::new();
        max_delta.serialize(&mut buf);
        let mut mtu_per_num_entries = Vec::new();
        for mtu in 2..buf.len() {
            let delta = cluster_state.compute_delta(digest, mtu, exclude_node_ids.clone(), 10_000);
            let num_tuples = delta.num_tuples();
            if mtu_per_num_entries.len() == num_tuples + 1 {
                continue;
            }
            buf.clear();
            delta.serialize(&mut buf);
            mtu_per_num_entries.push(buf.len());
        }
        for (num_entries, &mtu) in mtu_per_num_entries.iter().enumerate() {
            let mut expected_delta = Delta::default();
            for &(node, key, val, version, marked_for_deletion) in
                &expected_delta_atoms[..num_entries]
            {
                expected_delta.add_node_delta(node.clone(), key, val, version, marked_for_deletion);
            }
            {
                let delta =
                    cluster_state.compute_delta(digest, mtu, exclude_node_ids.clone(), 10_000);
                assert_eq!(&delta, &expected_delta);
            }
            {
                let delta =
                    cluster_state.compute_delta(digest, mtu + 1, exclude_node_ids.clone(), 10_000);
                assert_eq!(&delta, &expected_delta);
            }
        }
    }

    fn test_cluster_state() -> ClusterState {
        let mut cluster_state = ClusterState::default();

        let node1 = NodeId::for_test_localhost(10_001);
        let node1_state = cluster_state.node_state_mut(&node1);
        node1_state.set_with_version("key_a".to_string(), "1".to_string(), 1); // 1
        node1_state.set_with_version("key_b".to_string(), "2".to_string(), 2); // 3

        let node2 = NodeId::for_test_localhost(10_002);
        let node2_state = cluster_state.node_state_mut(&node2);
        node2_state.set_with_version("key_a".to_string(), "1".to_string(), 1); // 1
        node2_state.set_with_version("key_b".to_string(), "2".to_string(), 2); // 2
        node2_state.set_with_version("key_c".to_string(), "3".to_string(), 3); // 3
        node2_state.set_with_version("key_d".to_string(), "4".to_string(), 4); // 4
        node2_state.mark_for_deletion("key_d"); // 5

        cluster_state
    }

    #[test]
    fn test_cluster_state_compute_delta_depth_first_single_node() {
        let cluster_state = test_cluster_state();
        let mut digest = Digest::default();
        let node1 = NodeId::for_test_localhost(10_001);
        let node2 = NodeId::for_test_localhost(10_002);
        digest.add_node(node1.clone(), 1);
        digest.add_node(node2.clone(), 2);
        test_with_varying_max_transmitted_kv_helper(
            &cluster_state,
            &digest,
            HashSet::new(),
            &[
                (&node2, "key_c", "3", 3, false),
                (&node2, "key_d", "4", 5, true),
                (&node1, "key_b", "2", 2, false),
            ],
        );
    }

    #[test]
    fn test_cluster_state_compute_delta_depth_first_chitchat() {
        let cluster_state = test_cluster_state();
        let mut digest = Digest::default();
        let node1 = NodeId::for_test_localhost(10_001);
        let node2 = NodeId::for_test_localhost(10_002);
        digest.add_node(node1.clone(), 1);
        digest.add_node(node2.clone(), 2);
        test_with_varying_max_transmitted_kv_helper(
            &cluster_state,
            &digest,
            HashSet::new(),
            &[
                (&node2, "key_c", "3", 3, false),
                (&node2, "key_d", "4", 5, true),
                (&node1, "key_b", "2", 2, false),
            ],
        );
    }

    #[test]
    fn test_cluster_state_compute_delta_missing_node() {
        let cluster_state = test_cluster_state();
        let mut digest = Digest::default();
        let node1 = NodeId::for_test_localhost(10_001);
        let node2 = NodeId::for_test_localhost(10_002);
        digest.add_node(node2.clone(), 3);
        test_with_varying_max_transmitted_kv_helper(
            &cluster_state,
            &digest,
            HashSet::new(),
            &[
                (&node1, "key_a", "1", 1, false),
                (&node1, "key_b", "2", 2, false),
                (&node2, "key_d", "4", 4, false),
            ],
        );
    }

    #[test]
    fn test_cluster_state_compute_delta_should_ignore_dead_nodes() {
        let cluster_state = test_cluster_state();
        let digest = Digest::default();
        let node1 = NodeId::for_test_localhost(10_001);
        let node2 = NodeId::for_test_localhost(10_002);
        let dead_nodes = vec![node2];
        test_with_varying_max_transmitted_kv_helper(
            &cluster_state,
            &digest,
            dead_nodes.iter().collect(),
            &[
                (&node1, "key_a", "1", 1, false),
                (&node1, "key_b", "2", 2, false),
            ],
        );
    }

    #[test]
    fn test_cluster_state_compute_delta_with_old_node_state_that_needs_reset() {
        let mut cluster_state = ClusterState::default();

        let node1 = NodeId::for_test_localhost(10_001);
        let node1_state = cluster_state.node_state_mut(&node1);
        node1_state.set_with_version("key_a".to_string(), "1".to_string(), 1); // 1
        node1_state.set_with_version("key_b".to_string(), "2".to_string(), 10_003); // 10_003

        let node2 = NodeId::for_test_localhost(10_002);
        let node2_state = cluster_state.node_state_mut(&node2);
        node2_state.set_with_version("key_c".to_string(), "3".to_string(), 2); // 2

        let mut digest = Digest::default();
        let node1 = NodeId::for_test_localhost(10_001);
        digest.add_node(node1.clone(), 1);
        {
            let delta = cluster_state.compute_delta(
                &digest,
                MAX_UDP_DATAGRAM_PAYLOAD_SIZE,
                HashSet::new(),
                10_002,
            );
            assert!(delta.nodes_to_reset.is_empty());
            let mut expected_delta = Delta::default();
            expected_delta.add_node_delta(node1.clone(), "key_b", "2", 10_003, false);
            expected_delta.add_node_delta(node2.clone(), "key_c", "3", 2, false);
            assert_eq!(delta, expected_delta);
        }
        {
            // Node 1 max_version in digest + grace period (10_000) is inferior to the
            // node1's max_version in the cluster state. Thus we expect the cluster to compute a
            // delta that will reset node 1.
            let delta = cluster_state.compute_delta(
                &digest,
                MAX_UDP_DATAGRAM_PAYLOAD_SIZE,
                HashSet::new(),
                10_000,
            );
            let mut expected_delta = Delta::default();
            expected_delta.add_node_to_reset(node1.clone());
            expected_delta.add_node_delta(node1.clone(), "key_a", "1", 1, false);
            expected_delta.add_node_delta(node1, "key_b", "2", 10_003, false);
            expected_delta.add_node_delta(node2.clone(), "key_c", "3", 2, false);
            assert_eq!(delta, expected_delta);
        }
    }
}
