use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use tracing::{info, warn};

/// Signaling server: rooms, roster, chat, and handing out media server tokens.
#[derive(Parser)]
struct Args {
    #[arg(long, env = "BITS_SIGNAL_LISTEN", default_value = "0.0.0.0:8740")]
    listen: SocketAddr,
    #[arg(long, env = "BITS_SECRET", hide_env_values = true)]
    secret: String,
    /// Key required to create calls. Generated at startup when unset.
    #[arg(long, env = "BITS_HOST_KEY", hide_env_values = true)]
    host_key: Option<String>,
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
    ensure!(!args.secret.is_empty(), "BITS_SECRET cannot be empty");
    let host_key = match args.host_key {
        Some(key) => key,
        None => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).context("generating host key")?;
            let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            eprintln!("host key for creating calls: {key}");
            key
        }
    };
    ensure!(!host_key.is_empty(), "BITS_HOST_KEY cannot be empty");
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
        host_key.as_bytes(),
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
