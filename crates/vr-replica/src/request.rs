use std::io::{BufReader, Read};

use http_body_util::{BodyExt, Full};
use hyper::body::{Buf, Bytes};
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;

use crate::replica::Replica;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RequestType {
  Request { op: Vec<String>, client_id: String, request_number: u64 },
}

pub async fn handle_request(req: Request<hyper::body::Incoming>, replica: Replica) -> Result<Response<Full<Bytes>>, Box<dyn std::error::Error + Send + Sync>> {
  println!("Received request: {:?}", req);
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

  println!("Received request: {:?}", data);

  Ok(Response::new(Full::new(Bytes::from("Hello, world!"))))
}
