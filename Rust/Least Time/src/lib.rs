// Author: Morteza Farrokhnejad
// Scheduling rule:
// - keep a moving average of observed backend latency
// - add a small in-flight penalty
// - pick the healthy backend with the lowest score

use async_trait::async_trait;
use axum::body::Body;
use hyper::{
    header::HOST,
    Request, Response, StatusCode,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

const DEFAULT_EWMA_SECS: f64 = 0.100;
const EWMA_ALPHA: f64 = 0.20;
const INFLIGHT_PENALTY_SECS: f64 = 0.025;

#[async_trait]
pub trait Server: Send + Sync {
    fn address(&self) -> &str;
    async fn is_alive(&self) -> bool;
    fn score(&self) -> f64;
    async fn serve(&self, req: Request<Body>, client_addr: Option<SocketAddr>) -> Response<Body>;
}

pub struct SimpleServer {
    addr: String,
    alive: Arc<AtomicBool>,
    ewma_secs: Arc<Mutex<f64>>,
    inflight: Arc<AtomicUsize>,
    client: reqwest::Client,
}

impl SimpleServer {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            alive: Arc::new(AtomicBool::new(true)),
            ewma_secs: Arc::new(Mutex::new(DEFAULT_EWMA_SECS)),
            inflight: Arc::new(AtomicUsize::new(0)),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("failed to build reqwest client"),
        }
    }

    pub fn alive_flag(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }

    fn observe_latency(&self, elapsed: Duration) {
        let sample = elapsed.as_secs_f64();
        let mut ewma = self.ewma_secs.lock().unwrap();
        *ewma = EWMA_ALPHA * sample + (1.0 - EWMA_ALPHA) * *ewma;
    }
}

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

    fn score(&self) -> f64 {
        let ewma = *self.ewma_secs.lock().unwrap();
        let inflight = self.inflight.load(Ordering::Relaxed) as f64;
        ewma + inflight * INFLIGHT_PENALTY_SECS
    }

    async fn serve(&self, req: Request<Body>, client_addr: Option<SocketAddr>) -> Response<Body> {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        let start = Instant::now();

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

        let response = match self
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
        };

        self.observe_latency(start.elapsed());
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        response
    }
}

pub struct LoadBalancer {
    pub port: String,
    servers: Vec<Arc<dyn Server>>,
}

impl LoadBalancer {
    pub fn new(port: impl Into<String>, servers: Vec<Arc<dyn Server>>) -> Arc<Self> {
        Arc::new(Self {
            port: port.into(),
            servers,
        })
    }

    pub async fn get_best_available_server(&self) -> Option<Arc<dyn Server>> {
        let mut best: Option<(Arc<dyn Server>, f64)> = None;

        for server in &self.servers {
            if !server.is_alive().await {
                continue;
            }

            let score = server.score();
            match &best {
                None => best = Some((server.clone(), score)),
                Some((_, best_score)) if score < *best_score => {
                    best = Some((server.clone(), score))
                }
                _ => {}
            }
        }

        best.map(|(server, _)| server)
    }

    pub async fn serve_proxy(
        &self,
        req: Request<Body>,
        client_addr: Option<SocketAddr>,
    ) -> Response<Body> {
        match self.get_best_available_server().await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router};
    use std::net::SocketAddr;

    async fn start_test_server(status: u16) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let app = Router::new().fallback(move || async move {
            Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap()
        });

        tokio::spawn(async move {
            axum::Server::from_tcp(listener)
                .unwrap()
                .serve(app.into_make_service())
                .await
                .unwrap();
        });

        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn test_simple_server_is_alive() {
        let url = start_test_server(200).await;
        let server = SimpleServer::new(&url);
        assert!(server.is_alive().await);
    }

    #[tokio::test]
    async fn test_least_time_prefers_lower_score() {
        let url1 = start_test_server(200).await;
        let url2 = start_test_server(200).await;

        let s1 = Arc::new(SimpleServer::new(&url1));
        let s2 = Arc::new(SimpleServer::new(&url2));

        {
            let mut a = s1.ewma_secs.lock().unwrap();
            *a = 0.400;
        }
        {
            let mut b = s2.ewma_secs.lock().unwrap();
            *b = 0.050;
        }

        let lb = LoadBalancer::new("8000", vec![s1, s2]);
        let selected = lb.get_best_available_server().await.unwrap();
        assert_eq!(selected.address(), url2);
    }

    #[tokio::test]
    async fn test_serve_proxy_no_alive_servers() {
        let down_url = start_test_server(500).await;
        let lb = LoadBalancer::new("8000", vec![Arc::new(SimpleServer::new(&down_url))]);

        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let response = lb.serve_proxy(req, None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_forwarded_headers_injected() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let app = Router::new().fallback(|req: Request<Body>| async move {
            let mut builder = Response::builder().status(200);
            for (name, value) in req.headers() {
                builder = builder.header(format!("x-echo-{}", name), value);
            }
            builder.body(Body::empty()).unwrap()
        });

        tokio::spawn(async move {
            axum::Server::from_tcp(listener)
                .unwrap()
                .serve(app.into_make_service())
                .await
                .unwrap();
        });

        let backend_url = format!("http://{}", addr);
        let lb = LoadBalancer::new("8000", vec![Arc::new(SimpleServer::new(&backend_url))]);

        let client_addr: SocketAddr = "192.0.2.1:1234".parse().unwrap();

        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let response = lb.serve_proxy(req, Some(client_addr)).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}