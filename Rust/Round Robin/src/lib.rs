//! A round-robin HTTP load balancer.
//!
//! # Architecture
//!
//! ```text
//!   Client
//!     │
//!     ▼
//! ┌──────────────────────────────────┐
//! │          LoadBalancer            │
//! │  round-robin + health filtering  │
//! └──────────┬───────────────────────┘
//!            │  picks next alive server
//!     ┌──────┼──────┐
//!     ▼      ▼      ▼
//!   srv0   srv1   srv2   (SimpleServer instances)
//! ```

use async_trait::async_trait;
use axum::body::Body;
use hyper::{
    header::{HOST},
    Request, Response, StatusCode,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Server trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Server: Send + Sync {
    /// The base URL / address of this backend (e.g. `http://127.0.0.1:8001`).
    fn address(&self) -> &str;

    /// Checks whether the backend is currently healthy.
    async fn is_alive(&self) -> bool;

    /// Reverse-proxies `req` to this backend and returns its response.
    async fn serve(&self, req: Request<Body>, client_addr: Option<SocketAddr>) -> Response<Body>;
}

// ---------------------------------------------------------------------------
// SimpleServer
// ---------------------------------------------------------------------------

pub struct SimpleServer {
    addr: String,
    /// Cached liveness flag – updated by the background health-checker.
    alive: Arc<AtomicBool>,
}

impl SimpleServer {
    pub fn new(addr: impl Into<String>) -> Self {
        SimpleServer {
            addr: addr.into(),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Returns a clone of the shared liveness flag so the health-checker can
    /// write to it without holding a reference to the whole struct.
    pub fn alive_flag(&self) -> Arc<AtomicBool> {
        self.alive.clone()
    }
}

#[async_trait]
impl Server for SimpleServer {
    fn address(&self) -> &str {
        &self.addr
    }

    /// Active health-check: sends a GET / and considers 2xx as alive.
    async fn is_alive(&self) -> bool {
        let result = reqwest::get(&self.addr)
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);

        self.alive.store(result, Ordering::Relaxed);
        result
    }

    /// Reverse-proxy with header forwarding.
    async fn serve(&self, req: Request<Body>, client_addr: Option<SocketAddr>) -> Response<Body> {
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
            .to_owned();

        let target = format!("{}{}", self.addr, path_and_query);
        let client = reqwest::Client::new();
        let method =
            reqwest::Method::from_bytes(req.method().as_str().as_bytes()).unwrap_or_default();

        // ---- Build forwarded headers ----------------------------------------
        let mut headers = reqwest::header::HeaderMap::new();

        // Forward all original headers (except Host – we set that explicitly).
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

        // X-Forwarded-For
        if let Some(addr) = client_addr {
            let ip = addr.ip().to_string();
            let existing = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .map(|s| format!("{}, {}", s, ip))
                .unwrap_or(ip);
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&existing) {
                headers.insert("x-forwarded-for", v);
            }
        }

        // X-Real-IP (first untrusted IP in the chain)
        if let Some(addr) = client_addr {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&addr.ip().to_string()) {
                headers.entry("x-real-ip").or_insert(v);
            }
        }

        // X-Forwarded-Proto
        headers
            .entry("x-forwarded-proto")
            .or_insert_with(|| reqwest::header::HeaderValue::from_static("http"));

        // ---- Collect body & forward ----------------------------------------
        let body_bytes = hyper::body::to_bytes(req.into_body()).await.unwrap_or_default();

        debug!(target = %target, "forwarding request");

        match client
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

// ---------------------------------------------------------------------------
// LoadBalancer
// ---------------------------------------------------------------------------

pub struct LoadBalancer {
    pub port: String,
    round_robin_counter: AtomicUsize,
    servers: Vec<Arc<dyn Server>>,
}

impl LoadBalancer {
    pub fn new(port: impl Into<String>, servers: Vec<Arc<dyn Server>>) -> Arc<Self> {
        Arc::new(LoadBalancer {
            port: port.into(),
            round_robin_counter: AtomicUsize::new(0),
            servers,
        })
    }

    /// Round-robin walk; returns the first alive server or `None`.
    pub async fn get_next_available_server(&self) -> Option<Arc<dyn Server>> {
        let n = self.servers.len();
        for _ in 0..n {
            let idx = self.round_robin_counter.fetch_add(1, Ordering::SeqCst) % n;
            if self.servers[idx].is_alive().await {
                return Some(self.servers[idx].clone());
            }
        }
        None
    }

    /// Reverse-proxies `req` through the next available backend.
    pub async fn serve_proxy(
        &self,
        req: Request<Body>,
        client_addr: Option<SocketAddr>,
    ) -> Response<Body> {
        match self.get_next_available_server().await {
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

    /// Spawns a background task that polls every backend on `interval`.
    /// Logs changes in server health.
    pub fn start_health_checks(self: &Arc<Self>, interval: Duration) {
        let lb = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                for server in &lb.servers {
                    let was_alive = server.is_alive().await; // also updates the AtomicBool
                    if was_alive {
                        debug!(backend = %server.address(), "health check OK");
                    } else {
                        warn!(backend = %server.address(), "health check FAILED");
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// Author: Morteza Farrokhnejad
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
    };
    
    use std::net::SocketAddr;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Spins up a minimal Axum server on a random port that always responds
    /// with `status`. Returns the server's base URL.
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

    /// Starts a server that echoes specific request headers back in the response.
    async fn start_header_echo_server() -> String {
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

        format!("http://{}", addr)
    }

    // -----------------------------------------------------------------------
    // Mirror of TestSimpleServerIsAlive
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_simple_server_is_alive() {
        let url = start_test_server(200).await;
        let server = SimpleServer::new(&url);
        assert!(server.is_alive().await, "Expected server to be alive");
    }

    // -----------------------------------------------------------------------
    // 5xx means not alive
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_simple_server_is_not_alive_on_500() {
        let url = start_test_server(500).await;
        let server = SimpleServer::new(&url);
        assert!(!server.is_alive().await, "Expected server to be dead on 500");
    }

    // -----------------------------------------------------------------------
    // Mirror of TestLoadBalancer_getNextAvailableServer
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_load_balancer_get_next_available_server() {
        let alive_url = start_test_server(200).await;
        let unavailable_url = start_test_server(500).await;

        let servers: Vec<Arc<dyn Server>> = vec![
            Arc::new(SimpleServer::new(&unavailable_url)),
            Arc::new(SimpleServer::new(&alive_url)),
        ];

        let lb = LoadBalancer::new("8000", servers);

        let selected = lb
            .get_next_available_server()
            .await
            .expect("Expected an alive server to be selected");

        assert_eq!(
            selected.address(),
            alive_url,
            "Expected the alive server, got {}",
            selected.address()
        );
    }

    // -----------------------------------------------------------------------
    // Mirror of TestLoadBalancer_ServeProxy
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_load_balancer_serve_proxy() {
        let backend_url = start_test_server(200).await;
        let servers: Vec<Arc<dyn Server>> = vec![Arc::new(SimpleServer::new(&backend_url))];
        let lb = LoadBalancer::new("8000", servers);

        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let response = lb.serve_proxy(req, None).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    // -----------------------------------------------------------------------
    // 503 when no alive servers
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_load_balancer_serve_proxy_no_alive_servers() {
        let down_url = start_test_server(500).await;
        let servers: Vec<Arc<dyn Server>> = vec![Arc::new(SimpleServer::new(&down_url))];
        let lb = LoadBalancer::new("8000", servers);

        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let response = lb.serve_proxy(req, None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // -----------------------------------------------------------------------
    // X-Forwarded-For / X-Real-IP headers are injected
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_forwarded_headers_injected() {
        let backend_url = start_header_echo_server().await;
        let servers: Vec<Arc<dyn Server>> = vec![Arc::new(SimpleServer::new(&backend_url))];
        let lb = LoadBalancer::new("8000", servers);

        let client_addr: SocketAddr = "192.0.2.1:1234".parse().unwrap();

        let req = Request::builder()
            .method("GET")
            .uri("/")
            .body(Body::empty())
            .unwrap();

        let response = lb.serve_proxy(req, Some(client_addr)).await;
        assert_eq!(response.status(), StatusCode::OK);

        // The echo server reflects headers back as `x-echo-<name>`.
        let xff = response
            .headers()
            .get("x-echo-x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        assert!(
            xff.contains("192.0.2.1"),
            "Expected X-Forwarded-For to contain client IP, got: {}",
            xff
        );
    }
    // Author: Morteza Farrokhnejad

    // -----------------------------------------------------------------------
    // Round-robin distributes load across alive servers
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_round_robin_distribution() {
        let url1 = start_test_server(200).await;
        let url2 = start_test_server(200).await;

        let servers: Vec<Arc<dyn Server>> = vec![
            Arc::new(SimpleServer::new(&url1)),
            Arc::new(SimpleServer::new(&url2)),
        ];

        let lb = LoadBalancer::new("8000", servers);

        let first = lb.get_next_available_server().await.unwrap();
        let second = lb.get_next_available_server().await.unwrap();

        assert_ne!(
            first.address(),
            second.address(),
            "Round-robin should alternate between servers"
        );
    }

    // -----------------------------------------------------------------------
    // Health-check background task marks servers alive/dead
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_health_check_background_task() {
        let alive_url = start_test_server(200).await;
        let servers: Vec<Arc<dyn Server>> = vec![Arc::new(SimpleServer::new(&alive_url))];

        let lb = LoadBalancer::new("8000", servers);
        lb.start_health_checks(Duration::from_millis(50));

        // Give the background task time to run at least once.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // After health checks ran the server should still appear alive.
        let selected = lb.get_next_available_server().await;
        assert!(selected.is_some(), "Server should still be alive after health checks");
    }
}
