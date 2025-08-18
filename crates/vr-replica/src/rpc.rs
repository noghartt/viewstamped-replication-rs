use std::{collections::HashMap, sync::{Arc, RwLock}};
use std::fmt::Debug;

use axum::http;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use hyper::{client::conn::http1 as c1, server::conn::http1 as s1};

type Request = http::Request<Vec<u8>>;
type Response = http::Response<Vec<u8>>;

#[async_trait::async_trait]
pub trait Rpc: Send + Sync + Debug {
    async fn send_request(
        &self,
        to: &str,
        req: Request,
    ) -> anyhow::Result<Response>;
}

#[derive(Debug, Clone)]
pub struct ReqwestRpc {
    client: reqwest::Client,
}

impl ReqwestRpc {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
            .build()
            .unwrap(),
        }
    }
}

unsafe impl Send for ReqwestRpc {}
unsafe impl Sync for ReqwestRpc {}

#[async_trait::async_trait]
impl Rpc for ReqwestRpc {
    async fn send_request(
        &self,
        to: &str,
        req: Request,
    ) -> anyhow::Result<Response> {
        use http::Method;
        
        let (parts, body) = req.into_parts();
        
        // Build absolute URL (Axum routes are usually relative like "/prepare").
        let path = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let url = format!("{to}{path}");
        
        // Map method + headers
        let mut rb = self.client.request(
            match parts.method {
                Method::GET    => reqwest::Method::GET,
                Method::POST   => reqwest::Method::POST,
                Method::PUT    => reqwest::Method::PUT,
                Method::DELETE => reqwest::Method::DELETE,
                Method::PATCH  => reqwest::Method::PATCH,
                Method::HEAD   => reqwest::Method::HEAD,
                Method::OPTIONS=> reqwest::Method::OPTIONS,
                _ => reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?,
            },
            &url,
        );
        
        for (k, v) in parts.headers.iter() {
            rb = rb.header(k, v);
        }
        
        let resp = rb.body(body).send().await?;
        let status = resp.status();
        let mut builder = http::Response::builder().status(status);
        for (k, v) in resp.headers().iter() {
            builder = builder.header(k, v);
        }
        let bytes = resp.bytes().await?.to_vec();
        Ok(builder.body(bytes)?)
    }
}

#[derive(Debug, Clone)]
pub struct InMemoryHyperRpc {
    routers: Arc<RwLock<HashMap<String, axum::Router>>>
}

impl InMemoryHyperRpc {
    pub fn new(_: HashMap<String, axum::Router>) -> Self {
        Self {
            routers: Arc::new(RwLock::new(HashMap::new()))
        }
    }

    pub fn register(&self, id: impl Into<String>, router: axum::Router) {
        self.routers.write().unwrap().insert(id.into(), router);
    }

    pub fn add_router(&mut self, id: &str, router: axum::Router) {
        self.routers.write().unwrap().insert(id.to_string(), router);
    }

    // Method to get a shared reference for passing to replicas
    pub fn get_shared(&self) -> Arc<InMemoryHyperRpc> {
        Arc::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Rpc for InMemoryHyperRpc {
    async fn send_request(
        &self,
        to: &str,                       // logical node name like "node-A"
        req: Request,
    ) -> anyhow::Result<Response> {
        // clone the router under read lock so we drop the lock before awaits
        let router = {
            let guard = self.routers.read().unwrap();
            println!("guard: {:?}", guard);
            guard.get(to).cloned().expect("unknown node")
        };

        println!("router: {:?}", router);
        
        // Real HTTP over an in-memory pipe
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        
        // Serve the target router
        tokio::spawn(async move {
            let svc = TowerToHyperService::new(router);
            let _ = s1::Builder::new()
            .serve_connection(TokioIo::new(server_io), svc)
            .await;
        });
        
        // Hyper client on the client end
        let (mut client, conn) = c1::handshake::<_, Full<Bytes>>(TokioIo::new(client_io)).await?;
        tokio::spawn(conn);

        let (parts, body) = req.into_parts();
        let req = http::Request::from_parts(parts, Full::from(Bytes::from(body)));
        let res = client.send_request(req).await?;

        let (parts, body) = res.into_parts();
        let bytes = body.collect().await?.to_bytes().to_vec();
        Ok(http::Response::from_parts(parts, bytes))
    }
}

unsafe impl Send for InMemoryHyperRpc {}
unsafe impl Sync for InMemoryHyperRpc {}