use std::net::SocketAddr;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use facetty_proto::signal::MediaServer;
use facetty_sfu::Sfu;
use facetty_signal::{Config, Signal};
use tracing::info;

/// Call server: call codes, rosters and chat over HTTP, video and audio over QUIC.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// TCP address for HTTP and WebSocket.
    #[arg(long, env = "FACETTY_LISTEN", default_value = "0.0.0.0:8740")]
    listen: SocketAddr,
    /// UDP address for media.
    #[arg(long, env = "FACETTY_MEDIA_LISTEN", default_value = "0.0.0.0:8741")]
    media_listen: SocketAddr,
    /// Media address clients should dial, as `host:port`. Defaults to the
    /// media port on whichever host clients used to reach this server.
    #[arg(long, env = "FACETTY_MEDIA_ADDR")]
    media_addr: Option<String>,
    /// Key required to create calls. Generated at startup when unset.
    #[arg(long, env = "FACETTY_HOST_KEY", hide_env_values = true)]
    host_key: Option<String>,
    /// Let anyone create calls, without a host key.
    #[arg(long, env = "FACETTY_OPEN", value_parser = clap::builder::BoolishValueParser::new())]
    open: bool,
    /// Most calls held at once. When full, the oldest call nobody is in is
    /// dropped to make room.
    #[arg(long, env = "FACETTY_MAX_CALLS", default_value_t = 1000,
          value_parser = clap::value_parser!(u32).range(1..))]
    max_calls: u32,
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
    let host_key = match (args.open, args.host_key) {
        (true, _) => None,
        (false, Some(key)) => Some(key),
        (false, None) => {
            let key = random_hex()?;
            eprintln!("host key for creating calls: {key}");
            Some(key)
        }
    };
    ensure!(
        host_key.as_ref().is_none_or(|k| !k.is_empty()),
        "FACETTY_HOST_KEY cannot be empty"
    );
    let secret = random_hex()?;

    let sfu = Sfu::bind(args.media_listen, secret.as_bytes())?;
    let media = MediaServer {
        addr: args
            .media_addr
            .unwrap_or_else(|| format!(":{}", args.media_listen.port())),
        cert_sha256: sfu.cert_sha256().to_string(),
    };
    info!(listen = %sfu.local_addr()?, addr = %media.addr, "media listening");
    let signal = Signal::new(
        secret.as_bytes(),
        Config {
            host_key: host_key.map(String::into_bytes),
            max_calls: args.max_calls as usize,
            media,
        },
    );
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    info!(listen = %listener.local_addr()?, open = args.open, max_calls = args.max_calls, "server up");

    tokio::select! {
        () = sfu.run() => bail!("media endpoint closed"),
        served = axum::serve(listener, facetty_signal::router(signal)) => served.context("serving HTTP"),
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
