use async_trait::async_trait;
use axum::{body::Body, extract::ConnectInfo, Extension, Router};
use hyper::{
    header::HOST,
    Request, Response, StatusCode,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tracing::{debug, info, warn};

const REPLICAS: usize = 100;

fn hash32(key: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() & 0xffff_ffff) as u32
}

fn client_ip(addr: Option<SocketAddr>, req: &Request<Body>) -> String {
    if let Some(xff) = req.headers().get("x-forwarded-for") {
        if let Ok(s) = xff.to_str() {
            if let Some(first) = s.split(',').next() {
                return first.trim().to_string();
            }
        }
    }

    addr.map(|a| a.ip().to_string())
        .unwrap_or_else(|| "0.0.0.0".to_string())
}

#[async_trait]
pub trait Server: Send + Sync {
    fn address(&self) -> &str;
    async fn is_alive(&self) -> bool;
    async fn serve(&self, req: Request<Body>, client_addr: Option<SocketAddr>) -> Response<Body>;
}

pub struct SimpleServer {
    addr: String,
    alive: Arc<AtomicBool>,
    client: reqwest::Client,
}

impl SimpleServer {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            alive: Arc::new(AtomicBool::new(true)),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("failed to build reqwest client"),
        }
    }

    pub fn alive_flag(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }
}

// Author: Morteza Farrokhnejad

#[async_trait]
impl Server for SimpleServer {
    fn address(&self) -> &str {
        &self.addr
    }

    async fn is_alive(&self) -> bool {
        let result = self
            .client
            .get(&self.addr)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        self.alive.store(result, Ordering::Relaxed);
        result
    }

    async fn serve(&self, req: Request<Body>, client_addr: Option<SocketAddr>) -> Response<Body> {
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
            .to_owned();

        let target = format!("{}{}", self.addr, path_and_query);
        let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
            .unwrap_or(reqwest::Method::GET);

        let mut headers = reqwest::header::HeaderMap::new();

        for (name, value) in req.headers().iter() {
            if name == HOST {
                continue;
            }

            if let (Ok(n), Ok(v)) = (
                reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
                reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
            ) {
                headers.insert(n, v);
            }
        }

        if let Some(addr) = client_addr {
            let ip = addr.ip().to_string();

            let xff = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .map(|existing| format!("{}, {}", existing, ip))
                .unwrap_or(ip);

            if let Ok(v) = reqwest::header::HeaderValue::from_str(&xff) {
                headers.insert("x-forwarded-for", v);
            }

            if let Ok(v) = reqwest::header::HeaderValue::from_str(&addr.ip().to_string()) {
                headers.entry("x-real-ip").or_insert(v);
            }
        }

        headers
            .entry("x-forwarded-proto")
            .or_insert_with(|| reqwest::header::HeaderValue::from_static("http"));

        let body_bytes = hyper::body::to_bytes(req.into_body())
            .await
            .unwrap_or_default();

        debug!(target = %target, "forwarding request");

        match self
            .client
            .request(method, &target)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let resp_headers = resp.headers().clone();
                let bytes = resp.bytes().await.unwrap_or_default();

                let mut builder = Response::builder().status(status);
                for (name, value) in &resp_headers {
                    builder = builder.header(name.as_str(), value.as_bytes());
                }

                builder.body(Body::from(bytes)).unwrap()
            }
            Err(e) => {
                warn!(error = %e, backend = %self.addr, "upstream error");
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::empty())
                    .unwrap()
            }
        }
    }
}

#[derive(Clone)]
struct RingNode {
    hash: u32,
    idx: usize,
}

pub struct LoadBalancer {
    pub port: String,
    servers: Vec<Arc<dyn Server>>,
    ring: Mutex<Vec<RingNode>>,
}

impl LoadBalancer {
    pub fn new(port: impl Into<String>, servers: Vec<Arc<dyn Server>>) -> Arc<Self> {
        let lb = Arc::new(Self {
            port: port.into(),
            servers,
            ring: Mutex::new(Vec::new()),
        });
        lb.build_ring();
        lb
    }

    fn build_ring(&self) {
        let mut ring = Vec::new();

        for (idx, server) in self.servers.iter().enumerate() {
            for replica in 0..REPLICAS {
                let key = format!("{}#{}", server.address(), replica);
                ring.push(RingNode {
                    hash: hash32(&key),
                    idx,
                });
            }
        }

        ring.sort_by_key(|n| n.hash);
        *self.ring.lock().unwrap() = ring;
    }

    pub async fn get_server_for_client(
        &self,
        client_addr: Option<SocketAddr>,
        req: &Request<Body>,
    ) -> Option<Arc<dyn Server>> {
        let ip = client_ip(client_addr, req);
        let key = hash32(&ip);
        let ring = self.ring.lock().unwrap().clone();

        if ring.is_empty() {
            return None;
        }

        let mut left = 0usize;
        let mut right = ring.len();
        while left < right {
            let mid = (left + right) / 2;
            if ring[mid].hash < key {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        let start = if left == ring.len() { 0 } else { left };

        for i in 0..ring.len() {
            let node = &ring[(start + i) % ring.len()];
            let server = self.servers[node.idx].clone();
            if server.is_alive().await {
                return Some(server);
            }
        }

        None
    }

    pub async fn serve_proxy(
        &self,
        req: Request<Body>,
        client_addr: Option<SocketAddr>,
    ) -> Response<Body> {
        match self.get_server_for_client(client_addr, &req).await {
            Some(server) => {
                info!(backend = %server.address(), "routing request");
                server.serve(req, client_addr).await
            }
            None => {
                warn!("no healthy backends available");
                Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .body(Body::from("no healthy backends available"))
                    .unwrap()
            }
        }
    }

    pub fn start_health_checks(self: &Arc<Self>, interval: Duration) {
        let lb = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                for server in &lb.servers {
                    let alive = server.is_alive().await;
                    if alive {
                        debug!(backend = %server.address(), "health check OK");
                    } else {
                        warn!(backend = %server.address(), "health check FAILED");
                    }
                }
            }
        });
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let servers: Vec<Arc<dyn Server>> = vec![
        Arc::new(SimpleServer::new("http://localhost:8001")),
        Arc::new(SimpleServer::new("http://localhost:8002")),
        Arc::new(SimpleServer::new("http://localhost:8003")),
    ];

    let lb = LoadBalancer::new("8000", servers);
    lb.start_health_checks(Duration::from_secs(10));

    let app = Router::new()
        .fallback(
            |Extension(lb): Extension<Arc<LoadBalancer>>,
             ConnectInfo(addr): ConnectInfo<SocketAddr>,
             req: Request<Body>| async move { lb.serve_proxy(req, Some(addr)).await },
        )
        .layer(Extension(lb.clone()));

    let bind_addr = format!("0.0.0.0:{}", lb.port);
    info!("Load balancer listening on {}", bind_addr);

    axum::Server::bind(&bind_addr.parse().unwrap())
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}