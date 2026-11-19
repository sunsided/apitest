//! Runs the demo billing service standalone, for the CLI and the Python bindings.
//!
//! cargo run -p apitest --example `demo_server` -- [port] [1.0|1.1|2.0]

/// The billing service.
#[path = "../tests/support/demo_server.rs"]
mod demo;

/// Serves the billing service on `127.0.0.1:[port]` (default 8080) until killed.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().map(|p| p.parse()).transpose()?.unwrap_or(8080);
    let variant: demo::Variant = args
        .next()
        .as_deref()
        .unwrap_or("1.0")
        .parse()
        .map_err(anyhow::Error::msg)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!(
        "demo billing service {variant:?} on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, demo::app(variant)).await?;
    Ok(())
}
