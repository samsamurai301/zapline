#[tokio::main]
async fn main() -> std::io::Result<()> {
    let addr = std::env::var("ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, axum_minimal::app()).await
}
