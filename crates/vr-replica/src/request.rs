use http_body_util::{BodyExt, Full};
use hyper::body::{Buf, Bytes};
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::replica::Replica;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RequestType {
  /// The client is connecting to the replicas.
  Connect,
  Request { op: Vec<String>, client_id: String, request_number: u64 },
}

pub async fn handle_request(req: Request<hyper::body::Incoming>, replica: Replica) -> Result<Response<Full<Bytes>>> {
  let body = req.collect().await?.aggregate();
  let data = match serde_json::from_reader::<_, RequestType>(body.reader()) {
    Ok(data) => data,
    Err(e) => { 
      println!("Error: {:?}", e);
      let response = Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Full::new(Bytes::from(format!("Invalid request: {:?}", e))))?;

      return Ok(response);
    }
  };

  match data {
    RequestType::Connect => handle_request_connect(replica),
    RequestType::Request { .. } => todo!(),
  }
}

fn handle_request_connect(replica: Replica) -> Result<Response<Full<Bytes>>> {
  #[derive(Serialize)]
  struct Data {
    configuration: Vec<String>,
    current_view: usize,
    id: String,
  }

  let bytes = Bytes::from(serde_json::to_string(&Data {
    current_view: replica.view_number,
    configuration: replica.configuration,
    id: Uuid::new_v4().to_string(),
  })?);

  let response = Response::builder()
    .status(StatusCode::OK)
    .body(Full::new(bytes))?;

  Ok(response)
}
