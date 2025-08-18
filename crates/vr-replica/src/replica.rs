use std::collections::HashMap;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::request::handle_request;

#[derive(Clone, Debug)]
pub struct ReplicaRequest {
    pub client_id: String,
    pub request_id: usize,
    pub op: Vec<String>,
    pub result: Option<String>,
}

#[derive(Clone, Debug)]
enum ReplicaMessage {
    Request(ReplicaRequest),
    Prepare {
        view_number: usize,
        message: Box<ReplicaRequest>,
        op_number: usize,
        commit_number: usize,
    },
    PrepareOk {
        view_number: usize,
        op_number: usize,
        replica_number: usize,
    },
    Reply {
        view_number: usize,
        request_id: usize,
        // TODO: Check how exactly do we plan to save/structure the request result. It need to be changed here.
        result: Option<()>,
    }
}

#[derive(Clone, Debug)]
enum ReplicaStatus {
    Normal,
    ViewChange,
    Recovering,
    Transitioning,
}

#[derive(Clone, Debug)]
struct ClientRequest {
    client_id: String,
    request_id: usize,
    // TODO: Check how exactly do we plan to save/structure the request result. It need to be changed here.
    result: Option<()>,
}

#[derive(Clone, Debug)]
pub struct Replica {
    /// A sorted array containing the IP addresses of the replicas in the system.
    pub configuration: Vec<String>,
    /// The unique identifier of the replica in the system. The index of the replica in the `configuration` array.
    pub replica_number: usize,
    /// The view number determines who is the primary replica in the current view.
    pub view_number: usize,
    /// The current epoch number of the replica. Which means the number of times the replica group has been reconfigured.
    pub epoch: usize,
    pub status: ReplicaStatus,
    /// The most recently received request. By default, this is set to 0.
    pub op_number: usize,
    /// The number of the most recently committed request.
    pub commit_number: usize,
    /// An array containing `op_number` entries. The entries contain the requests that have been received so far in their assigned order.
    pub log: Vec<ReplicaRequest>,
    /// The client table is a map containing the last client request for each client which has been processed by the replica.
    pub client_table: HashMap<String, ReplicaRequest>,
}

impl Replica {
    pub fn new(configuration: Vec<String>, replica_number: usize) -> Self {
        let mut configuration = configuration.clone();
        configuration.sort();
        Replica {
            configuration,
            replica_number,
            view_number: 0,
            op_number: 0,
            commit_number: 0,
            epoch: 0,
            status: ReplicaStatus::Normal,
            log: Vec::new(),
            client_table: HashMap::new(),
        }
    }

    #[tokio::main(flavor = "multi_thread")]
    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let address = self.configuration.get(self.replica_number).unwrap();
        let listener = TcpListener::bind(address).await?;

        loop {
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);

            let state = self.clone();

            tokio::task::spawn(async move {
                let service = service_fn(move |req| {
                    handle_request(req, state.clone())
                });

                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    eprintln!("Error serving connection: {}", e);
                }
            });
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accepts_one_connection() {
        let configuration = vec!["127.0.0.1:8081".to_string(), "127.0.0.1:8080".to_string()];
        let replica = Replica::new(configuration, 0);

        assert_eq!(replica.configuration, vec!["127.0.0.1:8080".to_string(), "127.0.0.1:8081".to_string()]);
    }
}