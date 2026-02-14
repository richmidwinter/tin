use std::net::SocketAddr;
use tracing::info;

mod cache;
mod server;
mod thumbnail;

use crate::server::create_app;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("thumbnail_service=info,tower_http=debug")
        .init();

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9142);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    
    info!("Starting thumbnail service on {}", addr);
    
    let app = create_app().await?;
    
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
