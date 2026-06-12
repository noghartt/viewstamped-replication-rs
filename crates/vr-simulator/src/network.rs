use std::collections::BTreeMap;

use rand::RngExt;
use rand_chacha::ChaCha8Rng;

use crate::types::NodeKind;

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
}
