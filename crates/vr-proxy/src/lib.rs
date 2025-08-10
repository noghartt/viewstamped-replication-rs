use bytes::{Buf, Bytes};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use hyper::{Method, Request};
use serde::Deserialize;
use tokio::net::TcpStream;

pub struct Proxy {
    /// A sorted array containing the IP addresses of the replicas in the system.
    configuration: Vec<String>,
    /// The current view number. The primary replica is the one with the index `current_view` in the `configuration` array.
    current_view: usize,
    /// The unique identifier of this client.
    id: String,
    /// The current request number. For future requests, it should ensure to be greater than the previous request number.
    request_number: u64,
}

impl Proxy {
    pub async fn new(addr: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let stream = TcpStream::connect(addr).await?;
        let io = TokioIo::new(stream);

        // TODO: Improve type of the Empty<Bytes>. Ensure to use the correct type.
        let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Full<Bytes>>(io).await?;
        
        // Spawn the connection task
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                eprintln!("Connection failed: {:?}", err);
            }
        });

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{}/connect", addr))
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(r#"{"type": "connect"}"#)))?;

        let Ok(res) = sender.send_request(req).await else {
            return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "Failed to send request")));
        };

        let body = res.collect().await?.aggregate();
        let data: ResponseClientData = serde_json::from_reader(body.reader())?;

        println!("Received response: {:?}", data);

        Ok(Self {
            configuration: data.configuration,
            current_view: data.current_view,
            id: data.id,
            request_number: 0,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ResponseClientData {
    configuration: Vec<String>,
    current_view: usize,
    id: String,
}
