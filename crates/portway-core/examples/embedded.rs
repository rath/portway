//! Run with an explicit upstream URL. The embedding application owns the runtime.
use portway_core::{ForwarderConfig, Router, Telemetry, server};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: embedded <upstream-url>")?;
    let telemetry = Arc::new(Telemetry::new(|event| eprintln!("{event:?}")));
    let config = ForwarderConfig {
        telemetry,
        ..ForwarderConfig::default()
    };
    let router = Router::single(&config, &url, None)?;
    router.negotiate_all().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8787").await?;
    server::serve(listener, router).await;
    Ok(())
}
