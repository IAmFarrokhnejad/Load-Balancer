use axum::{body::Body, extract::ConnectInfo, Extension, Router};
use hyper::Request;
use load_balancer::{LoadBalancer, SimpleServer, Server};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tracing::info;
// Author: Morteza Farrokhnejad


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

    // Start the background health-checker (polls every 10 s).
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
