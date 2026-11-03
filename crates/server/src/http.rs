use std::convert::Infallible;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::debug;

const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Claude: how long a connection may stay plain HTTP. Upgrading to a
/// WebSocket ends this phase.
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_BUFFER: usize = 32 * 1024;

/// Claude: HTTP/1.1 for at most `max_connections` clients at once, counting
/// upgraded WebSockets until they close.
pub async fn serve(listener: TcpListener, app: Router, max_connections: usize) -> Infallible {
    let slots = Arc::new(Semaphore::new(max_connections));
    loop {
        let slot = slots
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                debug!("cannot accept a connection: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let io = TokioIo::new(Counted {
                stream,
                _slot: slot,
            });
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .max_buf_size(MAX_BUFFER);
            let conn = builder.serve_connection(io, service).with_upgrades();
            match tokio::time::timeout(HTTP_TIMEOUT, conn).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => debug!("HTTP connection failed: {e}"),
                Err(_) => debug!("HTTP connection timed out"),
            }
        });
    }
}

/// Claude: a TCP stream that holds its connection slot, which an upgraded
/// WebSocket takes along.
struct Counted {
    stream: TcpStream,
    _slot: OwnedSemaphorePermit,
}

impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}
