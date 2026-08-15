use std::ops::Deref;

use crate::effect::Effect;
use crate::types::ReplicaId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageSender {
    Replica(ReplicaId),
    Client(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolObservation {
    PrimaryActivityAccepted {
        replica: ReplicaId,
        primary: ReplicaId,
        view_number: ReplicaId,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Transition<Input, Output> {
    pub effects: Vec<Effect<Input, Output>>,
    pub observations: Vec<ProtocolObservation>,
}

impl<Input, Output> Transition<Input, Output> {
    pub fn new(
        effects: Vec<Effect<Input, Output>>,
        observations: Vec<ProtocolObservation>,
    ) -> Self {
        Self {
            effects,
            observations,
        }
    }

    pub fn from_effects(effects: Vec<Effect<Input, Output>>) -> Self {
        Self::new(effects, Vec::new())
    }

    pub fn as_slice(&self) -> &[Effect<Input, Output>] {
        &self.effects
    }
}

impl<Input, Output> Deref for Transition<Input, Output> {
    type Target = [Effect<Input, Output>];

    fn deref(&self) -> &Self::Target {
        &self.effects
    }
}

impl<Input: PartialEq, Output: PartialEq> PartialEq<Vec<Effect<Input, Output>>>
    for Transition<Input, Output>
{
    fn eq(&self, other: &Vec<Effect<Input, Output>>) -> bool {
        self.effects == *other
    }
}
