use std::collections::BTreeMap;

use vr_replica::message::Message;

use crate::types::NodeId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Set(String, u64),
    Get(String, Option<u64>),
    Del(String),
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
    pending_request: Option<usize>,
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
        }
    }

    pub fn believed_primary(&self) -> u64 {
        self.configuration[(self.current_view as usize) % self.configuration.len()]
    }

    pub fn on_message<I: std::fmt::Debug>(&mut self, message: Message<I, Op>) {
        match message {
            Message::Reply {
                result,
                client_id,
                request_id,
                ..
            } => {
                if client_id != self.id.0 {
                    return;
                }

                let Some(pending_request) = self.pending_request else {
                    return;
                };

                if request_id != pending_request {
                    return;
                }

                self.request_number = pending_request;
                self.pending_request = None;
                self.replies_received += 1;

                if let Some(op) = result {
                    self.apply_op(op);
                }
            }
            // VR's rule for unexpected messages is ignore-and-drop; a panic
            // here would kill an entire seed campaign on one stray message.
            other => tracing::debug!(?other, "client ignoring unexpected message"),
        }
    }

    pub fn lock_request_number(&mut self) -> usize {
        if let Some(request_number) = self.pending_request {
            return request_number;
        }

        let request_number: usize = self.request_number + 1;
        self.pending_request = Some(request_number);
        request_number
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

    #[test]
    fn reserves_request_number_when_no_request_is_pending() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 0);

        client.lock_request_number();

        assert_eq!(client.pending_request, Some(1));
        assert_eq!(client.request_number, 0);
    }

    #[test]
    fn does_not_reserve_second_request_while_pending() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 0);

        let next_request_number = client.lock_request_number();

        assert_eq!(client.pending_request, Some(1));
        assert_eq!(next_request_number, 1);
        assert_eq!(client.request_number, 0);

        let next_request_number = client.lock_request_number();

        assert_eq!(client.pending_request, Some(1));
        assert_eq!(next_request_number, 1);
        assert_eq!(client.request_number, 0);
    }

    #[test]
    fn accepts_matching_reply_and_clears_pending_request() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 0);

        let next_request_number = client.lock_request_number();

        assert_eq!(client.pending_request, Some(1));
        assert_eq!(next_request_number, 1);
        assert_eq!(client.request_number, 0);

        let reply = Message::<Op, Op>::Reply {
            client_id: 0,
            view_number: 0,
            request_id: next_request_number,
            result: Some(Op::Set("k".into(), 1)),
        };

        client.on_message::<Op>(reply);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.replies_received, 1);
        assert_eq!(client.state.get("k"), Some(&1));
    }

    #[test]
    fn ignores_duplicate_reply() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 0);

        let next_request_number = client.lock_request_number();

        let reply = Message::<Op, Op>::Reply {
            client_id: 0,
            view_number: 0,
            request_id: next_request_number,
            result: Some(Op::Set("k".into(), 1)),
        };

        client.on_message::<Op>(reply.clone());

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.replies_received, 1);

        client.on_message::<Op>(reply);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.replies_received, 1);
    }

    #[test]
    fn ignores_reply_for_different_request() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 0);
        assert_eq!(client.replies_received, 0);

        client.pending_request = Some(2);
        client.request_number = 1;

        let reply = Message::<Op, Op>::Reply {
            client_id: 0,
            view_number: 0,
            request_id: 1,
            result: Some(Op::Set("k".into(), 1)),
        };

        client.on_message::<Op>(reply.clone());

        assert_eq!(client.pending_request, Some(2));
        assert_eq!(client.request_number, 1);
        assert!(client.state.is_empty());
    }

    #[test]
    fn ignores_reply_for_different_client() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 0);

        let next_request_number = client.lock_request_number();

        let reply = Message::<Op, Op>::Reply {
            client_id: 1,
            view_number: 0,
            request_id: next_request_number,
            result: Some(Op::Set("k".into(), 1)),
        };

        client.on_message::<Op>(reply.clone());

        assert_eq!(client.pending_request, Some(1));
        assert_eq!(client.request_number, 0);
        assert!(client.state.is_empty());
    }

    #[test]
    fn allows_next_request_after_completion() {
        let mut client = Client::new(NodeId(0), vec![0, 1, 2]);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 0);

        let next_request_number = client.lock_request_number();

        assert_eq!(client.pending_request, Some(1));
        assert_eq!(next_request_number, 1);
        assert_eq!(client.request_number, 0);

        let reply = Message::<Op, Op>::Reply {
            client_id: 0,
            view_number: 0,
            request_id: next_request_number,
            result: Some(Op::Set("k".into(), 1)),
        };

        client.on_message::<Op>(reply);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.replies_received, 1);

        let next_request_number = client.lock_request_number();

        assert_eq!(client.pending_request, Some(2));
        assert_eq!(next_request_number, 2);
        assert_eq!(client.request_number, 1);

        let reply = Message::<Op, Op>::Reply {
            client_id: 0,
            view_number: 0,
            request_id: next_request_number,
            result: Some(Op::Set("k".into(), 1)),
        };

        client.on_message::<Op>(reply);

        assert_eq!(client.pending_request, None);
        assert_eq!(client.request_number, 2);
        assert_eq!(client.replies_received, 2)
    }
}
