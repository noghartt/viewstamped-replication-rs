use std::convert::Infallible;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Request, Response};

use crate::replica::Replica;

pub async fn handle_request(req: Request<hyper::body::Incoming>, replica: Replica) -> Result<Response<Full<Bytes>>, Infallible> {
  println!("Received request: {:?} {:?}", req, replica);
  Ok(Response::new(Full::new(Bytes::from("Hello, world!"))))
}
