use bytes::{Buf, Bytes};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use hyper::{Method, Request};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use uuid::Uuid;

pub struct Proxy {
    /// A sorted array containing the IP addresses of the replicas in the system.
    pub configuration: Vec<String>,
    /// The current view number. The primary replica is the one with the index `current_view` in the `configuration` array.
    pub current_view: usize,
    /// The unique identifier of this client.
    pub id: String,
    /// The current request number. For future requests, it should ensure to be greater than the previous request number.
    pub request_number: u64,
    /// The current epoch number of the replica group.
    pub epoch: usize,
}

impl Proxy {
    pub async fn new(addr: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let data = connect_to_replica(addr).await?;
        Ok(Self {
            configuration: data.configuration,
            current_view: data.current_view,
            id: Uuid::new_v4().to_string(),
            request_number: 0,
            epoch: data.epoch,
        })
    }

    pub async fn send_write_request(&mut self, key: String, value: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let primary_replica_addr = self.configuration.get(self.current_view).unwrap();

        let stream = TcpStream::connect(&primary_replica_addr).await?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Full<Bytes>>(io).await?;

        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                eprintln!("Connection failed: {:?}", err);
            }
        });

        let request_number = self.request_number;
        self.request_number += 1;

        let op = vec!["SET".to_string(), key, value];
        println!("Sending request: {:?}", op);

        #[derive(Debug, Serialize)]
        struct RequestData {
            r#type: String,
            client_id: String,
            op: Vec<String>,
            request_number: u64,
        }

        let req = Request::builder()
        .uri(format!("http://{}/", primary_replica_addr))
        .method(Method::POST)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(serde_json::to_string(&RequestData {
            r#type: "request".to_string(),
            client_id: self.id.clone(),
            op,
            request_number,
        })?)))?;

        let Ok(res) = sender.send_request(req).await else {
            println!("Failed to send request");
            return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "Failed to send request")));
        };

        let body = res.collect().await?.to_bytes();
        let data = String::from_utf8(body.to_vec()).unwrap();

        println!("Response data: {:?}", data);

        Ok(())
    }
}

async fn connect_to_replica(addr: &str) -> Result<ResponseClientData, Box<dyn std::error::Error + Send + Sync>> {
    let stream = TcpStream::connect(addr).await?;
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Full<Bytes>>(io).await?;

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
    Ok(data)
}

#[derive(Debug, Deserialize)]
struct ResponseClientData {
    configuration: Vec<String>,
    current_view: usize,
    epoch: usize,
}
