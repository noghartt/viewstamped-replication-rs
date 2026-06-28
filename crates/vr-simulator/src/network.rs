use std::collections::BTreeMap;

use rand::RngExt;
use rand_chacha::ChaCha8Rng;

use crate::types::{Clients, NodeId, NodeKind, Replicas};

#[derive(Debug, Clone)]
pub struct Link {
    pub partitioned: bool,
    pub base_ms: u64,
    pub jitter_ms: u64,

    pub drop_probability: u8,
    pub duplication_probability: u8,
}

impl Default for Link {
    fn default() -> Self {
        Self {
            partitioned: false,
            base_ms: 1,
            jitter_ms: 0,
            drop_probability: 0,
            duplication_probability: 0,
        }
    }
}

#[derive(Debug)]
pub enum NetworkSendOutcome {
    Dropped,
    Delivered { at: u64 },
    Duplicated { at: u64, duplicated_at: u64 },
}

impl Link {
    fn perfect_link() -> Link {
        Self { partitioned: false, base_ms: 1, jitter_ms: 0, drop_probability: 0, duplication_probability: 0 }
    }
}

// (2026-06-15) NOTE: I do think there's a specific wrong modelling with this Network structure.
// In case, from what I thought: I'm not sure if the `drop_probability` or `duplication_probability`
// should be part of the network structure per se.
//
// Why? Because not all payloads and events coming through the Network means to be duplicated.
// It can happen once, for a single, exclusive event. So ideally, I think we should have some
// way to model the network to represent scenarios like this one too. Not a static scenario,
// where the probability always still the same.
#[derive(Debug)]
pub struct Network {
    /// Links of communication nodes. This represents the properties of a given
    /// message bus between two connected nodes.
    links: BTreeMap<(NodeKind, NodeKind), Link>,
}

impl Default for Network {
    fn default() -> Self {
        Self::new()
    }
}

impl Network {
    pub fn new() -> Self {
        Self {
            links: BTreeMap::new(),
        }
    }

    pub fn full_mesh<Input: std::fmt::Debug + Clone, Output: std::fmt::Debug + Clone>(
        rng: &mut ChaCha8Rng,
        replicas: Replicas<Input, Output>,
        clients: Clients,
    ) -> Self {
        let mut network = Self::new();

        replicas.iter().for_each(|replica| {
            let replica_id = replica.0;
            let other_replicas: Vec<&NodeId> = replicas
                .iter()
                .filter(|r| r.0 != replica_id)
                .map(|r| r.0)
                .collect();

            other_replicas.iter().for_each(|r| {
                let key_a = NodeKind::Replica(*replica_id);
                let key_b = NodeKind::Replica(**r);

                let has_network = network.links.get(&(key_a, key_b));
                if has_network.is_some() {
                    return;
                }

                network.set_link(key_a, key_b, Network::create_rng_link(rng));
                network.set_link(key_b, key_a, Network::create_rng_link(rng));
            })
        });

        // (2026-06-05) NOTE: Right now, I've built the network considering that
        // every client does have a "wire" access into every replica. I'm still
        // not sure if ideally the link should be considering only the primary replica
        // instead.
        clients.iter().for_each(|c| {
            replicas.iter().for_each(|r| {
                let client_id = NodeKind::Client(*c.0);
                let replica_id = NodeKind::Replica(*r.0);

                network.set_link(client_id, replica_id, Network::create_rng_link(rng));
                network.set_link(replica_id, client_id, Network::create_rng_link(rng));
            })
        });

        network
    }

    pub fn full_mesh_perfect<Input: std::fmt::Debug + Clone, Output: std::fmt::Debug + Clone>(
        replicas: Replicas<Input, Output>,
        clients: Clients,
    ) -> Self {
        let mut network = Self::new();

        replicas.iter().for_each(|replica| {
            let replica_id = replica.0;
            let other_replicas: Vec<&NodeId> = replicas
                .iter()
                .filter(|r| r.0 != replica_id)
                .map(|r| r.0)
                .collect();

            other_replicas.iter().for_each(|r| {
                let key_a = NodeKind::Replica(*replica_id);
                let key_b = NodeKind::Replica(**r);

                let has_network = network.links.get(&(key_a, key_b));
                if has_network.is_some() {
                    return;
                }

                network.set_link(key_a, key_b, Link::perfect_link());
                network.set_link(key_b, key_a, Link::perfect_link());
            })
        });

        clients.iter().for_each(|c| {
            replicas.iter().for_each(|r| {
                let client_id = NodeKind::Client(*c.0);
                let replica_id = NodeKind::Replica(*r.0);

                network.set_link(client_id, replica_id, Link::perfect_link());
                network.set_link(replica_id, client_id, Link::perfect_link());
            })
        });

        network
    }

    pub fn set_link(&mut self, from: NodeKind, to: NodeKind, link: Link) {
        self.links.insert((from, to), link);
    }

    pub fn partition_link(&mut self, from: NodeKind, to: NodeKind) {
        self.links.entry((from, to)).or_default().partitioned = true;
    }

    pub fn heal_link(&mut self, from: NodeKind, to: NodeKind) {
        self.links.entry((from, to)).or_default().partitioned = false;
    }

    /// Resolves what the network does to a single message, rolling all
    /// stochastic decisions from the simulator's seeded RNG.
    ///
    /// Roll order is fixed and load-bearing for determinism: partition check,
    /// then drop, then delay, then duplication. Drop is rolled before dup on
    /// purpose — a message cannot be both dropped and duplicated, and pinning
    /// the order keeps the RNG consumption (and therefore every downstream
    /// event) identical across refactors of this function.
    pub fn resolve_send(
        &self,
        from: NodeKind,
        to: NodeKind,
        now: u64,
        rng: &mut ChaCha8Rng,
    ) -> NetworkSendOutcome {
        // Unconfigured pairs get a default healthy link: without this fallback
        // a forgotten `set_link` makes messages vanish silently, which reads
        // as a protocol liveness bug instead of a harness setup bug.
        let Some(link) = self.links.get(&(from, to)).cloned() else {
            panic!("There's no link between from/to")
        };

        if link.partitioned {
            return NetworkSendOutcome::Dropped;
        }

        if rng.random_range(0..100) < link.drop_probability {
            return NetworkSendOutcome::Dropped;
        }

        let at = now + link.base_ms + Self::jitter(&link, rng);

        // TODO: Duplication is capped at exactly 2 copies — an artifact of a
        // single roll, not a modeling decision. Make it recursive instead: each
        // delivered copy re-rolls "spawn another?" (re-rolling duplication +
        // fresh jitter only, NOT drop), giving a geometric distribution of copy
        // count. This collapses `NetworkSendOutcome::Duplicated` away — a
        // duplicate becomes just another `Delivered` — and turns the dup branch
        // in `Simulator::send` into a loop with no copy-pasted scheduling.
        // Add a safety cap (~8 copies) so a high-probability seed can't flood.
        // Determinism is preserved: variable draw *count* is fine, only the
        // draw *sequence* must stay deterministic.
        if rng.random_range(0..100) < link.duplication_probability {
            let duplicated_at = now + link.base_ms + Self::jitter(&link, rng);
            return NetworkSendOutcome::Duplicated { at, duplicated_at };
        }

        NetworkSendOutcome::Delivered { at }
    }

    fn jitter(link: &Link, rng: &mut ChaCha8Rng) -> u64 {
        if link.jitter_ms == 0 {
            return 0;
        }
        rng.random_range(0..=link.jitter_ms)
    }

    // TODO: See a better way to handle the magic numbers.
    fn create_rng_link(rng: &mut ChaCha8Rng) -> Link {
        Link {
            partitioned: false,
            base_ms: rng.random_range(10..120),
            jitter_ms: rng.random_range(0..80),
            drop_probability: rng.random_range(0..30),
            duplication_probability: rng.random_range(0..50),
        }
    }
}
