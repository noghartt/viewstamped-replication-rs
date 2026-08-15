use std::collections::BTreeMap;

use vr_replica::message::Message;

use crate::types::NodeId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Set(String, u64),
    Get(String, Option<u64>),
    Del(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequest {
    pub request_number: usize,
    pub op: Op,
    pub retry_generation: u64,
}

#[derive(Debug, Clone)]
pub struct Client {
    pub id: NodeId,
    pub state: BTreeMap<String, u64>,
    pub replies_received: u64,

    /// A sorted array containing the IP addresses of the replicas in the system.
    pub configuration: Vec<u64>,
    /// The current view number. The primary replica is the one with the index `current_view` in the `configuration` array.
    pub current_view: u64,
    /// The current request number. For future requests, it should ensure to be greater than the previous request number.
    pub request_number: usize,
    /// The current epoch number of the replica group.
    pub epoch: usize,

    pending_request: Option<PendingRequest>,
    next_retry_generation: u64,
}

impl Client {
    pub fn new(id: NodeId, configuration: Vec<u64>) -> Self {
        Self {
            id,
            state: BTreeMap::new(),
            replies_received: 0,
            configuration,
            current_view: 0,
            request_number: 0,
            epoch: 0,
            pending_request: None,
            next_retry_generation: 0,
        }
    }

    pub fn believed_primary(&self) -> u64 {
        self.configuration[(self.current_view as usize) % self.configuration.len()]
    }

    pub fn try_begin_request(&mut self, op: Op) -> Option<PendingRequest> {
        if self.pending_request.is_some() {
            return None;
        }

        let request_number = self
            .request_number
            .checked_add(1)
            .expect("client request number overflow");
        let pending = PendingRequest {
            request_number,
            op,
            retry_generation: self.next_retry_generation,
        };

        self.next_retry_generation = self
            .next_retry_generation
            .checked_add(1)
            .expect("client retry generation overflow");

        self.pending_request = Some(pending.clone());
        Some(pending)
    }

    pub fn on_message<I: std::fmt::Debug>(
        &mut self,
        message: Message<I, Op>,
    ) -> Option<(usize, Op)> {
        let Message::Reply {
            result,
            client_id,
            request_id,
            ..
        } = message
        else {
            // VR's rule for unexpected messages is ignore-and-drop; a panic
            // here would kill an entire seed campaign on one stray message.
            tracing::debug!(?message, "client ignoring unexpected message");
            return None;
        };

        let pending = self.pending_request.as_ref()?;
        if client_id != self.id.0 || request_id != pending.request_number {
            return None;
        }

        let result = result?;
        let request_number = pending.request_number;

        self.request_number = request_number;
        self.pending_request = None;
        self.replies_received += 1;
        self.apply_op(result.clone());

        Some((request_number, result))
    }

    pub fn pending_retry(&self, request_number: usize, generation: u64) -> Option<PendingRequest> {
        self.pending_request
            .as_ref()
            .filter(|pending| {
                pending.request_number == request_number && pending.retry_generation == generation
            })
            .cloned()
    }

    pub fn has_pending_request(&self) -> bool {
        self.pending_request.is_some()
    }

    fn apply_op(&mut self, op: Op) {
        match op {
            Op::Set(key, value) => {
                self.state.insert(key, value);
            }
            Op::Get(_, _) => {}
            Op::Del(key) => {
                self.state.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(client: &mut Client, op: Op) -> PendingRequest {
        client.try_begin_request(op).expect("request should begin")
    }

    fn reply(client_id: u64, request_id: usize, result: Option<Op>) -> Message<Op, Op> {
        Message::Reply {
            client_id,
            view_number: 0,
            request_id,
            result,
        }
    }

    #[test]
    fn begins_request_when_none_is_pending() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let op = Op::Set("k".into(), 1);

        let pending = begin(&mut client, op.clone());

        assert_eq!(pending.request_number, 1);
        assert_eq!(pending.op, op);
        assert_eq!(pending.retry_generation, 0);
        assert_eq!(client.pending_request, Some(pending));
        assert_eq!(client.request_number, 0);
    }

    #[test]
    fn does_not_begin_second_request_while_pending() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let pending = begin(&mut client, Op::Set("first".into(), 1));

        assert!(
            client
                .try_begin_request(Op::Set("second".into(), 2))
                .is_none()
        );
        assert_eq!(client.pending_request, Some(pending));
        assert_eq!(client.request_number, 0);
    }

    #[test]
    fn accepts_matching_reply_and_clears_pending_request() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let pending = begin(&mut client, Op::Set("k".into(), 1));

        assert_eq!(
            client.on_message::<Op>(reply(
                0,
                pending.request_number,
                Some(Op::Set("k".into(), 1)),
            )),
            Some((1, Op::Set("k".into(), 1)))
        );

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.replies_received, 1);
        assert_eq!(client.state.get("k"), Some(&1));
    }

    #[test]
    fn ignores_duplicate_reply() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let pending = begin(&mut client, Op::Set("k".into(), 1));
        let reply = reply(0, pending.request_number, Some(Op::Set("k".into(), 1)));

        assert!(client.on_message::<Op>(reply.clone()).is_some());
        assert!(client.on_message::<Op>(reply).is_none());

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.replies_received, 1);
    }

    #[test]
    fn ignores_reply_for_different_request() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let pending = begin(&mut client, Op::Set("k".into(), 1));

        assert!(
            client
                .on_message::<Op>(reply(0, 2, Some(Op::Set("k".into(), 1))))
                .is_none()
        );

        assert_eq!(client.pending_request, Some(pending));
        assert_eq!(client.request_number, 0);
        assert!(client.state.is_empty());
    }

    #[test]
    fn ignores_reply_for_different_client() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let pending = begin(&mut client, Op::Set("k".into(), 1));

        assert!(
            client
                .on_message::<Op>(reply(1, 1, Some(Op::Set("k".into(), 1))))
                .is_none()
        );

        assert_eq!(client.pending_request, Some(pending));
        assert_eq!(client.request_number, 0);
        assert!(client.state.is_empty());
    }

    #[test]
    fn ignores_matching_reply_without_result() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let pending = begin(&mut client, Op::Set("k".into(), 1));

        assert!(
            client
                .on_message::<Op>(reply(0, pending.request_number, None))
                .is_none()
        );
        assert_eq!(client.pending_request, Some(pending));
        assert_eq!(client.request_number, 0);
        assert_eq!(client.replies_received, 0);
        assert!(client.state.is_empty());
    }

    #[test]
    fn allows_next_request_after_completion() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let first = begin(&mut client, Op::Set("k".into(), 1));

        assert!(
            client
                .on_message::<Op>(reply(0, first.request_number, Some(first.op)))
                .is_some()
        );

        let second = begin(&mut client, Op::Set("k".into(), 2));
        assert_eq!(second.request_number, 2);
        assert_eq!(second.retry_generation, 1);
        assert!(
            client
                .on_message::<Op>(reply(0, second.request_number, Some(second.op)))
                .is_some()
        );

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 2);
        assert_eq!(client.replies_received, 2)
    }

    #[test]
    fn retry_lookup_requires_matching_request_and_generation() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);
        let pending = begin(&mut client, Op::Set("k".into(), 1));

        assert_eq!(
            client.pending_retry(pending.request_number, pending.retry_generation),
            Some(pending.clone())
        );
        assert!(
            client
                .pending_retry(pending.request_number + 1, pending.retry_generation)
                .is_none()
        );
        assert!(
            client
                .pending_retry(pending.request_number, pending.retry_generation + 1)
                .is_none()
        );
    }
}
