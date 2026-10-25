use std::net::SocketAddr;

fn address(name: &str, default: &str) -> SocketAddr {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_owned())
        .parse()
        .unwrap_or_else(|error| panic!("{name} is not an address: {error}"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let http = address("LEDGER_HTTP_ADDR", "127.0.0.1:8080");
    let grpc = address("LEDGER_GRPC_ADDR", "127.0.0.1:50051");
    ledger::serve(http, grpc, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await
}
