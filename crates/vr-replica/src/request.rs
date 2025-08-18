use axum::extract::{Request, State};
use axum::{Json, http::{Method, StatusCode}};

use crate::message::{Message, PrepareData, PrepareOkData};
use crate::replica::{Replica, ReplicaRequest, ReplicaState};

pub async fn handle_request(State(state): State<ReplicaState>, data: Json<Message>) -> (StatusCode, Json<Message>) {
    let replica = state.replica;

    match data.0 {
        Message::Connect { .. } => handle_client_connection(replica),
        Message::Request { client_id, op, request_number } => handle_client_request(replica, client_id, request_number, op).await,
        Message::Prepare(prepare_data) => on_prepare(replica, prepare_data).await,
        _ => (StatusCode::BAD_REQUEST, Json(Message::Error {
            message: "Invalid request".to_string(),
        })),
    }
}

fn handle_client_connection(replica: Replica) -> (StatusCode, Json<Message>) {
    if replica.replica_number != replica.view_number {
        return (StatusCode::BAD_REQUEST, Json(Message::Error {
            message: "Replica is not the primary replica in the current view".to_string(),
        }));
    }
    
    let response = Message::Connect {
        configuration: replica.configuration,
        current_view: replica.view_number,
        epoch: replica.epoch,
    };
    
    (StatusCode::OK, Json(response))
}

async fn handle_client_request(mut replica: Replica, client_id: String, request_number: usize, op: Vec<String>) -> (StatusCode, Json<Message>) {
    // TODO: Check the `epoch` to validate if it's an old client which requests to be reconfigured.
    if replica.replica_number != replica.view_number {
        return (StatusCode::BAD_REQUEST, Json(Message::Error {
            message: "Replica is not the primary replica in the current view".to_string(),
        }));
    }
    
    if request_number < replica.op_number {
        return (StatusCode::BAD_REQUEST, Json(Message::Error {
            message: "Request number is less than the current op number".to_string(),
        }));
    }
    
    
    let cached_response = if request_number == replica.op_number {
        if let Some(request) = replica.log.get(request_number) {
            if let Some(result) = &request.result {
                Some(Json(Message::Reply {
                    view_number: replica.view_number,
                    request_id: request_number,
                    result: Some(result.clone()),
                }))
            } else {
                Some(Json(Message::Error {
                    message: "Request not processed".to_string(),
                }))
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some(response) = cached_response {
        return (StatusCode::OK, response);
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

    match prepare_request_for_replicas(replica, request).await {
        Ok(response) => response,
        Err(_) => (StatusCode::BAD_REQUEST, Json(Message::Error {
            message: "Prepare request failed".to_string(),
        })),
    }   
}

async fn prepare_request_for_replicas(replica: Replica, request: ReplicaRequest) -> Result<(StatusCode, Json<Message>), ()> {
    let replicas_addresses = replica.configuration
    .iter()
    .enumerate()
    .filter(|(i, _)| *i != replica.replica_number)
    .map(|(_, addr)| addr)
    .collect::<Vec<_>>();

    println!("Replicas addresses: {:?}", replicas_addresses);
    
    let mut prepare_acks: Vec<Result<PrepareOkData, ()>> = Vec::new();
    for addr in replicas_addresses.clone() {
        let prepare_data = Message::Prepare(PrepareData {
            op: request.op.clone(),
            view_number: replica.view_number,
            op_number: replica.op_number,
            commit_number: replica.commit_number,
        });

        // TODO: It needs to check for timeout and guarantee the response struct of the replica response.
        let request_data_json = serde_json::to_string(&prepare_data).unwrap();
        println!("Data: {}, addr: {}", request_data_json, addr);

        let request = Request::builder()
            .method(Method::POST)
            .header("Content-Type", "application/json")
            .uri(format!("http://{}/", addr))
            .body(request_data_json.into())
            .unwrap();

        let result = replica.rpc.get().unwrap().send_request(addr, request).await.unwrap();
        let data = serde_json::from_slice::<PrepareOkData>(&result.body()).unwrap();

        prepare_acks.push(Ok(data));
    }
    
    let prepare_ack_success = prepare_acks.iter().filter(|ack| ack.is_ok()).collect::<Vec<_>>();
    // TODO: Check if the `quorum` is correct. I do believe that the calculation should be: `(replica.configuration.len() as usize / 2) + 1`.
    // Specifically including this replica already in the quorum, so maybe we should be expecting adding an extra replica to the quorum which is not needed in this implementation.
    let quorum: usize = replicas_addresses.len() / 2 + 1;

    // TODO: Should we rollback the request if the prepare request fails?
    if prepare_ack_success.len() < quorum {
        return Ok((StatusCode::BAD_REQUEST, Json(Message::Error {
            message: "Prepare request failed: quorum not reached".to_string(),
        })));
    }
    
    let reply_data = Message::Reply {
        view_number: replica.view_number,
        request_id: replica.op_number,
        result: Some(request.op.clone()),
    };

    Ok((StatusCode::OK, Json(reply_data)))
}

// TODO: Implement the rest of the flow.
async fn on_prepare(replica: Replica, data: PrepareData) -> (StatusCode, Json<Message>) {
    println!("on_prepare: {:?} {:?}", data, replica);
    if data.view_number != replica.view_number {
        return (StatusCode::BAD_REQUEST, Json(Message::Error {
            message: "View number mismatch".to_string(),
        }));
    }
    
    let ok = Message::PrepareOk(PrepareOkData {
        view_number: replica.view_number,
        op_number: replica.op_number,
        commit_number: replica.commit_number,
    });

    (StatusCode::OK, Json(ok))
}
