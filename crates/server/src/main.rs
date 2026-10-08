use std::net::SocketAddr;

use anyhow::{Context, Result, bail, ensure};
use bits_proto::signal::MediaServer;
use bits_sfu::Sfu;
use bits_signal::Signal;
use clap::Parser;
use tracing::info;

/// Call server: call codes, rosters and chat over HTTP, video and audio over QUIC.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// TCP address for HTTP and WebSocket.
    #[arg(long, env = "BITS_LISTEN", default_value = "0.0.0.0:8740")]
    listen: SocketAddr,
    /// UDP address for media.
    #[arg(long, env = "BITS_MEDIA_LISTEN", default_value = "0.0.0.0:8741")]
    media_listen: SocketAddr,
    /// Media address clients should dial, as `host:port`. Defaults to the
    /// media port on whichever host clients used to reach this server.
    #[arg(long, env = "BITS_MEDIA_ADDR")]
    media_addr: Option<String>,
    /// Key required to create calls. Generated at startup when unset.
    #[arg(long, env = "BITS_HOST_KEY", hide_env_values = true)]
    host_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();
    let host_key = match args.host_key {
        Some(key) => key,
        None => {
            let key = random_hex()?;
            eprintln!("host key for creating calls: {key}");
            key
        }
    };
    ensure!(!host_key.is_empty(), "BITS_HOST_KEY cannot be empty");
    let secret = random_hex()?;

    let sfu = Sfu::bind(args.media_listen, secret.as_bytes())?;
    let media = MediaServer {
        addr: args
            .media_addr
            .unwrap_or_else(|| format!(":{}", args.media_listen.port())),
        cert_sha256: sfu.cert_sha256().to_string(),
    };
    info!(listen = %sfu.local_addr()?, addr = %media.addr, "media listening");
    let signal = Signal::new(secret.as_bytes(), host_key.as_bytes(), media);
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    info!(listen = %listener.local_addr()?, "server up");

    tokio::select! {
        () = sfu.run() => bail!("media endpoint closed"),
        served = axum::serve(listener, bits_signal::router(signal)) => served.context("serving HTTP"),
        stop = shutdown() => stop.context("waiting for a shutdown signal"),
    }
}

fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).context("generating a random key")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Ctrl-C or SIGTERM. As PID 1 in a container, the process ignores SIGTERM
/// unless it handles it, so `docker stop` would wait and then kill it.
async fn shutdown() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            ctrl_c = tokio::signal::ctrl_c() => ctrl_c,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
