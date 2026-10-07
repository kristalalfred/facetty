use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};

/// Signaling server: rooms, roster, chat, and handing out media server tokens.
#[derive(Parser)]
struct Args {
    #[arg(long, env = "BITS_SIGNAL_LISTEN", default_value = "0.0.0.0:8740")]
    listen: SocketAddr,
    #[arg(long, env = "BITS_SECRET", default_value = bits_proto::token::DEV_SECRET, hide_default_value = true)]
    secret: String,
    /// Client binary to hand out at /install. Defaults to the `bits` binary
    /// next to this one, if there is one.
    #[arg(long, env = "BITS_CLIENT_BINARY")]
    client_binary: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    if args.secret == bits_proto::token::DEV_SECRET {
        warn!("BITS_SECRET is not set; using the insecure development secret");
    }
    let client_binary = args
        .client_binary
        .or_else(sibling_client)
        .filter(|p| p.is_file());
    match &client_binary {
        Some(path) => info!(path = %path.display(), "serving the client at /install"),
        None => warn!("no client binary found; /install is disabled"),
    }
    let app = bits_signal::router(bits_signal::Signal::new(
        args.secret.as_bytes(),
        client_binary,
    ));
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    info!(listen = %listener.local_addr()?, "signaling server up");
    axum::serve(listener, app).await?;
    Ok(())
}

fn sibling_client() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.with_file_name(format!("bits{}", std::env::consts::EXE_SUFFIX)))
}
