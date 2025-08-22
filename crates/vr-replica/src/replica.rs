use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Debug;
use std::rc::Rc;

use crate::clock::TimerKind;
use crate::effect::Effect;
use crate::message::{ClientRequest, Message};
use crate::state_machine::StateMachine;
use crate::types::{OpNumber, ReplicaId};

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Normal,
    ViewChange,
    Recovering,
    Transitioning,
}

#[derive(Debug, Clone)]
pub struct Replica<Input, Output> 
where 
    Input: Clone + 'static,
    Output: Clone + 'static,
{
    configuration: Vec<String>,
    pub replica_number: ReplicaId,

    pub epoch: u64,
    pub view_number: ReplicaId,
    pub status: Status,

    pub op_number: usize,
    pub commit_number: usize,
    log: Vec<(OpNumber, ClientRequest<Input, Output>)>,
    client_table: HashMap<String, ClientRequest<Input, Output>>,

    pub op_ack_table: HashMap<OpNumber, Vec<ReplicaId>>,

    state_machine: Rc<RefCell<dyn StateMachine<Input = Input, Output = Output>>>,

    // Timers
    timeout_primary_idle_commit: u64,
    next_primary_idle_commit: Option<u64>,
    timeout_backup_watchdog: u64,
    next_backup_watchdog: Option<u64>,
}

impl<Input, Output> Replica<Input, Output>
where 
    Input: Clone,
    Output: Clone,
{
    pub fn new(
        configuration: Vec<String>,
        replica_number: ReplicaId,
        state_machine: Rc<RefCell<dyn StateMachine<Input = Input, Output = Output>>>,
    ) -> Self {
        let mut configuration = configuration.clone();
        configuration.sort();
        Replica {
            state_machine,
            configuration,
            replica_number,
            view_number: 0,
            op_number: 0,
            commit_number: 0,
            epoch: 0,
            status: Status::Normal,
            log: Vec::new(),
            client_table: HashMap::new(),
            op_ack_table: HashMap::new(),
            timeout_primary_idle_commit: 1000,
            next_primary_idle_commit: None,
            timeout_backup_watchdog: 5000,
            next_backup_watchdog: None,
        }
    }

    pub fn tick(&mut self, now: u64) -> Vec<Effect<Input, Output>> {
        let mut effects = vec![];
        if self.is_primary() && self.status == Status::Normal {
            if self.next_primary_idle_commit.is_some_and(|t| now >= t) {
                let commit = Message::Commit {
                    op_number: self.op_number,
                    commit_number: self.commit_number,
                    view_number: self.view_number,
                };

                effects.push(Effect::Broadcast { to: self.configuration.clone(), message: commit });
                let at = now + self.timeout_primary_idle_commit;
                self.next_primary_idle_commit = Some(at);
                effects.push(Effect::SetTimer { kind: TimerKind::PrimaryIdleCommit, at });
            }
        }

        if !self.is_primary() && self.status == Status::Normal {
            if self.next_backup_watchdog.is_some_and(|t| now >= t) {
                // TODO: Implement View Change Protocol here
                todo!()
            }
        }

        effects
    }

    pub fn on_message(&mut self, message: Message<Input, Output>, now: u64) -> Vec<Effect<Input, Output>> {
        match message {
            Message::Request { 0: request } => self.on_request(request, now),
            Message::Prepare { op: _, view_number, op_number, commit_number , request } =>
                self.on_prepare(request, view_number, op_number, commit_number, now),
            Message::PrepareOk { view_number, replica_number, op_number, commit_number } =>
                self.on_prepare_ok(view_number, replica_number, op_number, commit_number),
            _ => todo!() 
        }
    }

    fn on_request(&mut self, request: ClientRequest<Input, Output>, now: u64) -> Vec<Effect<Input, Output>> {
        if !self.is_primary() {
            return vec![];
        }

        if let Some(last_request) = self.get_last_request_from_client(request.client_id.clone()) {
            if request.request_number < last_request.request_number {
                return vec![];
            }

            if request.request_number == last_request.request_number {
                let reply = Message::Reply {
                    client_id: request.client_id.clone(),
                    view_number: self.view_number,
                    request_id: request.request_number,
                    result: last_request.result.clone(),
                };

                return vec![Effect::Reply { client_id: request.client_id.clone(), message: reply }];
            }
        };

        self.op_number += 1;

        let mut effects = vec![];

        let prepare = Message::Prepare {
            op: request.op.clone(),
            view_number: self.view_number,
            op_number: self.op_number,
            commit_number: self.commit_number,
            request: Box::new(request.clone()),
        };

        effects.push(Effect::Broadcast { to: self.configuration.clone(), message: prepare });

        let at = now + self.timeout_primary_idle_commit;
        self.next_primary_idle_commit = Some(at);
        effects.push(Effect::SetTimer { kind: TimerKind::PrimaryIdleCommit, at });

        effects
    }

    fn on_prepare(
        &mut self,
        request: Box<ClientRequest<Input, Output>>,
        view_number: ReplicaId,
        op_number: usize,
        commit_number: usize,
        now: u64
    ) -> Vec<Effect<Input, Output>> {
        if !self.is_same_view(view_number) {
            return vec![];
        }

        let mut effects = vec![];

        if self.log.len() + 1 == op_number {
            self.log.push((op_number, *request));
        }

        self.commit_number = commit_number;

        let prepare_ok = Message::PrepareOk {
            view_number: self.view_number,
            replica_number: self.replica_number,
            op_number,
            commit_number,
        };

        effects.push(Effect::Send { to: self.view_number, message: prepare_ok });

        let at = now + self.timeout_backup_watchdog;
        self.next_backup_watchdog = Some(at);
        effects.push(Effect::SetTimer { kind: TimerKind::BackupWatchdog, at });
        effects.push(Effect::ApplyCommited { op_number: self.op_number });

        effects
    }

    fn on_prepare_ok(&mut self, view_number: ReplicaId, replica_number: ReplicaId, op_number: usize, commit_number: usize) -> Vec<Effect<Input, Output>> {
        if !self.is_same_view(view_number) || !self.is_primary() {
            return vec![];
        }

        if op_number <= self.commit_number {
            return vec![];
        }

        self.op_ack_table.entry(op_number).or_insert(vec![]).push(replica_number);

        let quorum = self.get_quorum();
        if self.op_ack_table.get(&op_number).unwrap_or(&vec![]).len() < quorum {
            return vec![];
        }

        self.commit_op(op_number)
    }

    #[inline]
    fn is_primary(&self) -> bool {
        self.view_number == self.replica_number
    }

    fn is_same_view(&self, view_number: ReplicaId) -> bool {
        self.view_number == view_number
    }

    fn get_last_request_from_client(&self, client_id: String) -> Option<ClientRequest<Input, Output>> {
        self.client_table.get(&client_id).cloned()
    }

    fn get_quorum(&self) -> usize {
        self.configuration.len() / 2 + 1
    }

    // TODO: Validate if it needs to do more operations here
    fn commit_op(&mut self, op_number: OpNumber) -> Vec<Effect<Input, Output>> {
        let (_op_number, request) = self.log.get(op_number - 1).unwrap();
        let sm = self.state_machine.clone();
        let result = sm.borrow_mut().apply(request.op.clone());

        let mut effects = vec![];

        let mut request = request.clone();
        request.result = Some(result.clone());

        self.client_table.insert(request.client_id.clone(), request.clone());

        let reply = Message::Reply {
            client_id: request.client_id.clone(),
            view_number: self.view_number,
            request_id: request.request_number,
            result: Some(result),
        };

        effects.push(Effect::Reply { client_id: request.client_id.clone(), message: reply });

        effects
    }
}
