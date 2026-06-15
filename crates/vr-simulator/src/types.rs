use std::collections::BTreeMap;

use vr_replica::replica::Replica;

use crate::client::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Client(NodeId),
    Replica(NodeId),
}

pub type Replicas<Input, Output> = BTreeMap<NodeId, Replica<Input, Output>>;
pub type Clients = BTreeMap<NodeId, Client>;
