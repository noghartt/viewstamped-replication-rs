use http_body_util::{BodyExt, Full};
use hyper::body::{Buf, Bytes};
use hyper::client::conn::http1::SendRequest;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;

use crate::replica::{Replica, ReplicaRequest};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Op = Vec<String>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RequestType {
    Connect,
    Request { op: Op, client_id: String, request_number: usize },
    Prepare(PrepareData),
}

#[derive(Debug, Serialize, Deserialize)]
struct PrepareData {
    op: Op,
    view_number: usize,
    op_number: usize,
    commit_number: usize,
}

// TODO: Validate if the `i` in the PrepareOk response mentioned in the paper means the replica number.
#[derive(Debug, Serialize, Deserialize)]
struct PrepareOkData {
    r#type: String,
    view_number: usize,
    op_number: usize,
    replica_number: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReplyData {
    view_number: usize,
    request_number: usize,
    result: Vec<String>,
}

pub async fn handle_request(req: Request<hyper::body::Incoming>, replica: Replica) -> Result<Response<Full<Bytes>>> {
    let body = req.collect().await?.aggregate();
    let data = match serde_json::from_reader::<_, RequestType>(body.reader()) {
        Ok(data) => data,
        Err(e) => { 
            let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Full::new(Bytes::from(format!("Invalid request: {:?}", e))))?;
            
            return Ok(response);
        }
    };
    
    match data {
        RequestType::Connect => handle_client_connection(replica),
        RequestType::Request { client_id, op, request_number } => handle_client_request(replica, client_id, request_number, op).await,
        RequestType::Prepare(prepare_data) => on_prepare(replica, prepare_data).await,
    }
}

fn handle_client_connection(replica: Replica) -> Result<Response<Full<Bytes>>> {
    if replica.replica_number != replica.view_number {
        let response = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Full::new(Bytes::from("Replica is not the primary replica in the current view")))
        .unwrap();
        
        return Ok(response);
    }
    
    #[derive(Serialize)]
    struct Data {
        configuration: Vec<String>,
        current_view: usize,
        epoch: usize,
    }
    
    let bytes = Bytes::from(serde_json::to_string(&Data {
        current_view: replica.view_number,
        configuration: replica.configuration,
        epoch: replica.epoch,
    })?);
    
    let response = Response::builder()
    .status(StatusCode::OK)
    .body(Full::new(bytes))?;
    
    Ok(response)
}

async fn handle_client_request(mut replica: Replica, client_id: String, request_number: usize, op: Vec<String>) -> Result<Response<Full<Bytes>>> {
    println!("handle_client_request: {:?}", request_number);
    println!("replica.op_number: {:?}", replica.op_number);
    println!("replica.log: {:?}", replica.log);
    println!("replica.client_table: {:?}", replica.client_table);
    
    // TODO: Check the `epoch` to validate if it's an old client which requests to be reconfigured.
    if replica.replica_number != replica.view_number {
        let response = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Full::new(Bytes::from("Replica is not the primary replica in the current view")))
        .unwrap();
        
        return Ok(response);
    }
    
    if request_number < replica.op_number {
        let response = Response::builder()
        .status(StatusCode::OK)
        .body(Full::new(Bytes::from("Request not processed")))
        .unwrap();
        
        return Ok(response);
    }
    
    
    let cached_response = if request_number == replica.op_number {
        if let Some(request) = replica.log.get(request_number) {
            if let Some(result) = &request.result {
                Some(Response::builder()
                    .status(StatusCode::OK)
                    .body(Full::new(Bytes::from(result.clone())))
                    .unwrap())
            } else {
                Some(Response::builder()
                    .status(StatusCode::OK)
                    .body(Full::new(Bytes::from("Request not processeed")))
                    .unwrap())
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some(response) = cached_response {
        return Ok(response);
    }
    
    let request = ReplicaRequest {
        client_id,
        request_id: request_number,
        op,
        result: None,
    };
    
    replica.op_number += 1;
    replica.log.push(request.clone());
    replica.client_table.entry(request.client_id.clone()).or_insert(request.clone());
    
    prepare_request_for_replicas(replica, request).await
}

async fn prepare_request_for_replicas(replica: Replica, request: ReplicaRequest) -> Result<Response<Full<Bytes>>> {
    let replicas_addresses = replica.configuration
    .iter()
    .enumerate()
    .filter(|(i, _)| *i != replica.replica_number)
    .map(|(_, addr)| addr)
    .collect::<Vec<_>>();
    
    let mut senders = Vec::new();
    for addr in replicas_addresses.clone() {
        let sender = get_sender_by_addr(addr).await?;
        senders.push((addr, sender))
    }
    
    let mut prepare_acks = Vec::new();
    for (addr, mut sender) in senders {
        let prepare_data = PrepareData {
            op: request.op.clone(),
            view_number: replica.view_number,
            op_number: replica.op_number,
            commit_number: replica.commit_number,
        };

        let request_data = RequestType::Prepare(prepare_data);

        let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{}/", addr))
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(serde_json::to_string(&request_data)?)))?;

        let res = sender.send_request(req).await?;

        let body = res.collect().await?.aggregate();
        let data = serde_json::from_reader::<_, PrepareOkData>(body.reader());

        prepare_acks.push(data);
    }
    
    let prepare_ack_success = prepare_acks.iter().filter(|ack| ack.is_ok()).collect::<Vec<_>>();
    // TODO: Check if the `quorum` is correct. I do believe that the calculation should be: `(replica.configuration.len() as usize / 2) + 1`.
    // Specifically including this replica already in the quorum, so maybe we should be expecting adding an extra replica to the quorum which is not needed in this implementation.
    let quorum: usize = replicas_addresses.len() / 2 + 1;

    // TODO: Should we rollback the request if the prepare request fails?
    if prepare_ack_success.len() < quorum {
        let response = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Full::new(Bytes::from("Prepare request failed: quorum not reached")))
        .unwrap();
        
        return Ok(response);
    }
    
    let reply_data = ReplyData {
        view_number: replica.view_number,
        request_number: replica.op_number,
        result: request.op.clone(),
    };
    
    let bytes = Bytes::from(serde_json::to_string(&reply_data)?);
    let response = Response::builder()
    .status(StatusCode::OK)
    .body(Full::new(bytes))?;

    Ok(response)
}

// TODO: Implement the rest of the flow.
async fn on_prepare(replica: Replica, data: PrepareData) -> Result<Response<Full<Bytes>>> {
    println!("on_prepare: {:?}", data);
    if data.view_number != replica.view_number {
        let response = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Full::new(Bytes::from("View number mismatch")))
        .unwrap();
        
        return Ok(response);
    }
    
    let ok = PrepareOkData {
        r#type: "prepare_ok".to_string(),
        view_number: replica.view_number,
        op_number: replica.op_number,
        replica_number: replica.replica_number,
    };

    println!("ok: {:?}", ok);
    
    let response = Response::builder()
    .status(StatusCode::OK)
    .body(Full::new(Bytes::from(serde_json::to_string(&ok)?)))
    .unwrap();
    
    Ok(response)
}

async fn get_sender_by_addr(addr: &str) -> Result<SendRequest<Full<Bytes>>> {
    let stream = TcpStream::connect(addr).await?;
    let io = TokioIo::new(stream);
    let (sender, conn) = hyper::client::conn::http1::handshake::<_, Full<Bytes>>(io).await?;
    
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            eprintln!("Connection failed: {:?}", err);
        }
    });
    
    Ok(sender)
}
