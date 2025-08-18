use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::OnceCell;

use crate::request::handle_request;
use crate::rpc::Rpc;

#[derive(Clone, Debug)]
pub struct ReplicaRequest {
    pub client_id: String,
    pub request_id: usize,
    pub op: Vec<String>,
    pub result: Option<Vec<String>>,
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
struct ClientRequest {    client_id: String,
    request_id: usize,
    // TODO: Check how exactly do we plan to save/structure the request result. It need to be changed here.
    result: Option<()>,
}

#[derive(Clone, Debug)]
pub struct Replica {
    pub address: String,
    /// The RPC instance for the replica. It's used to send requests to other replicas. We need to pass this to let us be able to test the replica in isolation.
    pub rpc: Arc<OnceCell<Arc<dyn Rpc>>>,
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

#[derive(Clone, Debug)]
pub struct ReplicaState {
    pub replica: Replica,
}

impl Replica {
    pub fn new(configuration: Vec<String>, replica_number: usize) -> Self {
        let mut configuration = configuration.clone();
        configuration.sort();
        let address = configuration.get(replica_number).unwrap().clone();
        Replica {
            address,
            rpc: Arc::new(OnceCell::new()),
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

    pub fn set_rpc(&self, rpc: Arc<dyn Rpc>) {
        self.rpc.set(rpc).unwrap();
    }
}


pub fn create_router_for_replica(replica: Replica) -> Router {
    let state = ReplicaState {
        replica: replica.clone(),
    };

    Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/", post(handle_request))
        .with_state(state)
}

// Helper function to set up replicas with shared RPC
pub fn setup_replicas_with_rpc(
    configurations: Vec<Vec<String>>, 
    rpc: &crate::rpc::InMemoryHyperRpc
) -> Vec<(Replica, Router)> {
    let shared_rpc = Arc::new(rpc.clone()) as Arc<dyn Rpc>;
    
    configurations.into_iter().enumerate().map(|(index, config)| {
        let mut replica = Replica::new(config, index);
        replica.set_rpc(shared_rpc.clone());
        let router = create_router_for_replica(replica.clone());
        
        // Register the router with the RPC
        rpc.register(&replica.address, router.clone());
        
        (replica, router)
    }).collect()
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper_util::{rt::TokioIo, service::TowerToHyperService};
    use hyper::client::conn::http1 as client_http1;
    use hyper::server::conn::http1 as server_http1;
    use tokio::task::JoinHandle;

    use crate::{message::Message, rpc};

    use super::*;

    #[test]
    fn test_accepts_one_connection() {
        let configuration = vec!["127.0.0.1:8081".to_string(), "127.0.0.1:8080".to_string()];
        
        // Create the shared RPC registry
        let rpc = rpc::InMemoryHyperRpc::new(HashMap::new());
        
        // Set up replica with the shared RPC
        let replicas_and_routers = setup_replicas_with_rpc(vec![configuration.clone()], &rpc);
        let (replica, _router) = &replicas_and_routers[0];

        assert_eq!(replica.configuration, vec!["127.0.0.1:8080".to_string(), "127.0.0.1:8081".to_string()]);
    }


    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_client_connect() {
        let configuration = vec!["127.0.0.1:8081".to_string(), "127.0.0.1:8080".to_string(), "127.0.0.1:8082".to_string()];

        // Create the shared RPC registry
        let rpc = rpc::InMemoryHyperRpc::new(HashMap::new());
        
        // Set up all replicas with the shared RPC
        let configurations = vec![
            configuration.clone(),
            configuration.clone(), 
            configuration.clone()
        ];
        
        let replicas_and_routers = setup_replicas_with_rpc(configurations, &rpc);
        let (_replica_1, router_1) = &replicas_and_routers[0];
        let (_replica_2, _router_2) = &replicas_and_routers[1]; 
        let (_replica_3, _router_3) = &replicas_and_routers[2];

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        let svc = TowerToHyperService::new(router_1.clone());

        let _ = tokio::spawn(async move {
            server_http1::Builder::new()
                .serve_connection(TokioIo::new(server_io), svc)
                .await
                .unwrap();
        });

        let (mut client, conn) = client_http1::handshake::<_, Full<Bytes>>(TokioIo::new(client_io)).await.unwrap();
        tokio::spawn(conn);

        let body = Message::Connect {
            configuration: configuration.clone(),
            current_view: 0,
            epoch: 0,
        };

        let req = Request::post("http://127.0.0.1:8080/")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(serde_json::to_string(&body).unwrap())))
        .unwrap();

        let res = client.send_request(req).await.unwrap();

        let body = res.into_body().collect().await.unwrap();
        let body = String::from_utf8(body.to_bytes().to_vec()).unwrap();

        println!("{}", body);

    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_prepare_request() {
        let configuration = vec!["127.0.0.1:8081".to_string(), "127.0.0.1:8080".to_string(), "127.0.0.1:8082".to_string()];

        // Create the shared RPC registry
        let rpc = rpc::InMemoryHyperRpc::new(HashMap::new());
        
        // Set up all replicas with the shared RPC
        let configurations = vec![
            configuration.clone(),
            configuration.clone(), 
            configuration.clone()
        ];
        
        let replicas_and_routers = setup_replicas_with_rpc(configurations, &rpc);
        let (replica_1, router_1) = &replicas_and_routers[0];
        let (replica_2, router_2) = &replicas_and_routers[1]; 
        let (replica_3, router_3) = &replicas_and_routers[2];

        let (mut client_1, server_1) = create_inmem_node(router_1.clone()).await.unwrap();
        let (mut client_2, server_2) = create_inmem_node(router_2.clone()).await.unwrap();
        let (mut client_3, server_3) = create_inmem_node(router_3.clone()).await.unwrap();

        let body = Message::Request {
            op: vec!["set".to_string(), "key".to_string(), "value".to_string()],
            client_id: "1".to_string(),
            request_number: 0,
        };

        let req = Request::post("http://127.0.0.1:8080/")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(serde_json::to_string(&body).unwrap())))
        .unwrap();

        let res = client_1.send_request(req).await.unwrap();

        let body = res.into_body().collect().await.unwrap();
        let body = String::from_utf8(body.to_bytes().to_vec()).unwrap();

        println!("{}", body);

    }

    type Client = client_http1::SendRequest<Full<Bytes>>;
    async fn create_inmem_node(router: Router) -> anyhow::Result<(Client, JoinHandle<()>)> {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);

        // Serve the router on the "server" end
        let svc = TowerToHyperService::new(router);
        let server = tokio::spawn(async move {
            let _ = server_http1::Builder::new()
                .serve_connection(TokioIo::new(server_io), svc)
                .await;
        });

        // Hyper client on the "client" end
        let (client, conn) =
            client_http1::handshake::<_, Full<Bytes>>(TokioIo::new(client_io)).await?;

        tokio::spawn(conn);

        Ok((client, server))
    }
}
