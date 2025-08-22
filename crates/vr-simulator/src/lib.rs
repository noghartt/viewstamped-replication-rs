mod events;
mod simulator;
mod client;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use vr_replica::replica::Replica;
    use vr_replica::state_machine::StateMachine;

    use crate::client::{Client, Op};
    use crate::simulator::{Link, NodeId, NodeKind, Simulator};

    #[test]
    fn test_setup_clients_and_replicas() {
        let mut sim = Simulator::<Op, ()>::new();
        setup_clients_and_replicas(&mut sim, 2, 3);

        let clients = sim.get_clients();
        let replicas = sim.get_replicas();
        let links = sim.get_links();

        assert_eq!(clients.len(), 2);
        assert_eq!(replicas.len(), 3);

        let mut client_links = 0;
        for client in clients {
            let link_from_client = links.0.get(&(NodeKind::Client(client.id), NodeKind::Replica(NodeId(0))));
            if link_from_client.is_some() {
                client_links += 1;
            }

            let link_to_client = links.0.get(&(NodeKind::Replica(NodeId(0)), NodeKind::Client(client.id)));
            if link_to_client.is_some() {
                client_links += 1;
            }
        }

        let mut replica_links = 0;
        for replica in replicas.clone() {
            let node_id = NodeId(replica.replica_number);
            let other_replicas = replicas.iter().filter(|r| r.replica_number != replica.replica_number);
            let has_link_to_other_replicas = other_replicas.clone().all(|r| links.0.get(&(NodeKind::Replica(node_id), NodeKind::Replica(NodeId(r.replica_number)))).is_some());
            if has_link_to_other_replicas {
                replica_links += 1;
            }

            let has_link_from_other_replicas = other_replicas.clone().all(|r| links.0.get(&(NodeKind::Replica(NodeId(r.replica_number)), NodeKind::Replica(node_id))).is_some());
            if has_link_from_other_replicas {
                replica_links += 1;
            }
        }

        assert_eq!(client_links, 4);
        assert_eq!(replica_links, 6);
        assert_eq!(links.0.len(), 10);
    }

    #[test]
    fn test_connect_client_to_replica() {
        let mut sim = Simulator::<Op, ()>::new();
        setup_clients_and_replicas(&mut sim, 2, 3);

        let clients = sim.get_clients();
        let replicas = sim.get_replicas();
    }

    fn setup_clients_and_replicas(sim: &mut Simulator<Op, ()>, client_count: u64, replica_count: u64) {
        let mut clients = Vec::new();
        for i in 0..client_count {
            let client = Client::new(NodeId(i));
            clients.push(client.clone());
            sim.add_client(NodeId(i), client);
        }

        let configuration = (0..replica_count).map(|i| format!("{}", i)).collect::<Vec<_>>();
        let mut replicas = Vec::new();
        for i in 0..replica_count {
            let replica = setup_replica(i, configuration.clone());
            let node_id = NodeId(i);
            replicas.push((node_id.clone(), replica.clone()));
            sim.add_replica(node_id, replica);
        }

        let link = Link {
            base_ms: 100,
            jitter_ms: 10,
            drop_pct: 0,
            dup_pct: 0,
            up: true,
        };

        for i in 0..client_count as usize {
            let client = clients[i].clone();
            let (node_id, _)= replicas.get(0).unwrap();
            sim.set_link(NodeKind::Client(client.id), NodeKind::Replica(node_id.clone()), link.clone());
        }

        set_link_between_replicas(sim, replicas, link);
    }

    fn setup_replica(id: u64, configuration: Vec<String>) -> Replica<Op, ()> {
        let state = Rc::new(RefCell::new(ReplicaState { state: HashMap::new() }));
        Replica::new(configuration, id, state)
    }

    fn set_link_between_replicas(sim: &mut Simulator<Op, ()>, replicas: Vec<(NodeId, Replica<Op, ()>)>, link: Link) {
        replicas.iter().for_each(|(node_id, _)| {
            let other_replicas = replicas.iter().filter(|(other_node_id, _)| other_node_id != node_id);
            other_replicas.for_each(|(other_node_id, _)| {
                sim.set_link(NodeKind::Replica(node_id.clone()), NodeKind::Replica(other_node_id.clone()), link.clone());
            });
        });
    }

    #[derive(Debug)]
    struct ReplicaState {
        state: HashMap<String, u64>,
    }

    impl StateMachine for ReplicaState {
        type Input = Op;
        type Output = ();

        fn apply(&mut self, input: Self::Input) -> Self::Output {
            match input {
                Op::Set(key, value) => {
                    self.state.insert(key, value);
                }
                Op::Get(key) => {
                    self.state.get(&key).cloned();
                    todo!()
                }
                Op::Del(key) => {
                    self.state.remove(&key);
                }
            }
        }
    }
}