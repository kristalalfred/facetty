use anyhow::{Context, Result, bail};
use facetty_proto::media::{self, ClientControl, MAX_CONTROL_MESSAGE, ServerControl};
use facetty_proto::signal::MediaServer;
use web_transport_wasm::{ClientBuilder, CongestionControl, RecvStream, SendStream, Session};

pub async fn connect(server: &MediaServer) -> Result<Session> {
    let hash = hex::decode(&server.cert_sha256).context("bad certificate hash")?;
    let url = url::Url::parse(&format!("https://{}/", server.addr))?;
    ClientBuilder::new()
        .with_congestion_control(CongestionControl::LowLatency)
        .with_server_certificate_hashes(vec![hash])
        .connect(url)
        .await
        .context("connecting to the media server")
}

pub async fn hello(session: &Session, token: String) -> Result<(SendStream, ControlReader)> {
    let (mut send, recv) = session.open_bi().await?;
    write_control(&mut send, &ClientControl::Hello { token }).await?;
    let mut reader = ControlReader {
        recv,
        buf: Vec::new(),
    };
    match reader.next().await? {
        Some(ServerControl::Welcome { .. }) => Ok((send, reader)),
        Some(ServerControl::Error { message }) => bail!("media server: {message}"),
        _ => bail!("unexpected reply from the media server"),
    }
}

pub async fn write_control(send: &mut SendStream, msg: &ClientControl) -> Result<()> {
    let body = media::encode(msg);
    let mut framed = (body.len() as u32).to_le_bytes().to_vec();
    framed.extend(body);
    send.write(&framed).await?;
    Ok(())
}

pub struct ControlReader {
    recv: RecvStream,
    buf: Vec<u8>,
}

impl ControlReader {
    pub async fn next(&mut self) -> Result<Option<ServerControl>> {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_le_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
                if len > MAX_CONTROL_MESSAGE {
                    bail!("control message of {len} bytes");
                }
                if self.buf.len() >= 4 + len {
                    let msg = media::decode(&self.buf[4..4 + len])?;
                    self.buf.drain(..4 + len);
                    return Ok(Some(msg));
                }
            }
            match self.recv.read(MAX_CONTROL_MESSAGE).await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None => return Ok(None),
            }
        }
    }
}

pub async fn read_to_end(recv: &mut RecvStream, limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = recv.read(limit).await? {
        out.extend_from_slice(&chunk);
        if out.len() > limit {
            bail!("stream longer than {limit} bytes");
        }
    }
    Ok(out)
}
