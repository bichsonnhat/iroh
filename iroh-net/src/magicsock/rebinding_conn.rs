use std::{
    fmt::Debug,
    io,
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{bail, Context as _};
use futures::ready;
use quinn::AsyncUdpSocket;
use tokio::io::Interest;
use tracing::{debug, trace, warn};

use crate::net::IpFamily;
use crate::net::UdpSocket;

use super::CurrentPortFate;

/// A UDP socket that can be re-bound. Unix has no notion of re-binding a socket, so we swap it out for a new one.
#[derive(Clone, Debug)]
pub struct RebindingUdpConn {
    io: Arc<UdpSocket>,
    state: Arc<quinn_udp::UdpSocketState>,
}

impl RebindingUdpConn {
    pub(super) fn as_socket(&self) -> Arc<UdpSocket> {
        self.io.clone()
    }

    pub(super) fn rebind(
        &mut self,
        port: u16,
        network: IpFamily,
        cur_port_fate: CurrentPortFate,
    ) -> anyhow::Result<()> {
        trace!(
            "rebinding from {} to {} ({:?})",
            self.port(),
            port,
            cur_port_fate
        );

        // Do not bother rebinding if we are keeping the port.
        if self.port() == port && cur_port_fate == CurrentPortFate::Keep {
            return Ok(());
        }

        let sock = bind(Some(&self.io), port, network, cur_port_fate)?;
        self.io = Arc::new(sock);
        self.state = Default::default();

        Ok(())
    }

    pub(super) fn bind(port: u16, network: IpFamily) -> anyhow::Result<Self> {
        let sock = bind(None, port, network, CurrentPortFate::Keep)?;
        Ok(Self {
            io: Arc::new(sock),
            state: Default::default(),
        })
    }

    pub fn port(&self) -> u16 {
        self.local_addr().map(|p| p.port()).unwrap_or_default()
    }

    #[allow(clippy::unused_async)]
    pub async fn close(&self) -> Result<(), io::Error> {
        // Nothing to do atm
        Ok(())
    }
}

impl AsyncUdpSocket for RebindingUdpConn {
    fn poll_send(
        &self,
        state: &quinn_udp::UdpState,
        cx: &mut Context,
        transmits: &[quinn_udp::Transmit],
    ) -> Poll<io::Result<usize>> {
        let inner = &self.state;
        let io = &self.io;
        loop {
            ready!(io.poll_send_ready(cx))?;
            if let Ok(res) = io.try_io(Interest::WRITABLE, || {
                inner.send(Arc::as_ref(io).into(), state, transmits)
            }) {
                for t in transmits.iter().take(res) {
                    trace!(
                        dst = %t.destination,
                        len = t.contents.len(),
                        count = t.segment_size.map(|ss| t.contents.len() / ss).unwrap_or(1),
                        src = %t.src_ip.map(|x| x.to_string()).unwrap_or_default(),
                        "UDP send"
                    );
                }

                return Poll::Ready(Ok(res));
            }
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [quinn_udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            ready!(self.io.poll_recv_ready(cx))?;
            if let Ok(res) = self.io.try_io(Interest::READABLE, || {
                self.state.recv(Arc::as_ref(&self.io).into(), bufs, meta)
            }) {
                for meta in meta.iter().take(res) {
                    trace!(
                        src = %meta.addr,
                        len = meta.len,
                        count = meta.len / meta.stride,
                        dst = %meta.dst_ip.map(|x| x.to_string()).unwrap_or_default(),
                        "UDP recv"
                    );
                }

                return Poll::Ready(Ok(res));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }
}

fn bind(
    inner: Option<&UdpSocket>,
    port: u16,
    network: IpFamily,
    cur_port_fate: CurrentPortFate,
) -> anyhow::Result<UdpSocket> {
    debug!(?network, %port, ?cur_port_fate, "binding");

    // Build a list of preferred ports.
    // - Best is the port that the user requested.
    // - Second best is the port that is currently in use.
    // - If those fail, fall back to 0.

    let mut ports = Vec::new();
    if port != 0 {
        ports.push(port);
    }
    if cur_port_fate == CurrentPortFate::Keep {
        if let Some(cur_addr) = inner.as_ref().and_then(|i| i.local_addr().ok()) {
            ports.push(cur_addr.port());
        }
    }
    // Backup port
    ports.push(0);
    // Remove duplicates. (All duplicates are consecutive.)
    ports.dedup();
    debug!(?ports, "candidate ports");

    for port in &ports {
        // Close the existing conn, in case it is sitting on the port we want.
        if let Some(_inner) = inner {
            // TODO: inner.close()
        }
        // Open a new one with the desired port.
        match UdpSocket::bind(network, *port) {
            Ok(pconn) => {
                let local_addr = pconn.local_addr().context("UDP socket not bound")?;
                debug!(?network, %local_addr, "successfully bound");
                return Ok(pconn);
            }
            Err(err) => {
                warn!(?network, %port, "failed to bind: {:#?}", err);
                continue;
            }
        }
    }

    // Failed to bind, including on port 0 (!).
    bail!(
        "failed to bind any ports on {:?} (tried {:?})",
        network,
        ports
    );
}

#[cfg(test)]
mod tests {
    use crate::{key, tls};

    use super::*;
    use anyhow::Result;

    const ALPN: &[u8] = b"n0/test/1";

    fn wrap_socket(conn: impl AsyncUdpSocket) -> Result<(quinn::Endpoint, key::SecretKey)> {
        let key = key::SecretKey::generate();
        let tls_server_config = tls::make_server_config(&key, vec![ALPN.to_vec()], false)?;
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_server_config));
        let mut quic_ep = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(server_config),
            conn,
            Arc::new(quinn::TokioRuntime),
        )?;

        let tls_client_config = tls::make_client_config(&key, None, vec![ALPN.to_vec()], false)?;
        let client_config = quinn::ClientConfig::new(Arc::new(tls_client_config));
        quic_ep.set_default_client_config(client_config);
        Ok((quic_ep, key))
    }

    #[tokio::test]
    async fn test_rebinding_conn_send_recv_ipv4() -> Result<()> {
        rebinding_conn_send_recv(IpFamily::V4).await
    }

    #[tokio::test]
    async fn test_rebinding_conn_send_recv_ipv6() -> Result<()> {
        if !crate::netcheck::os_has_ipv6() {
            return Ok(());
        }
        rebinding_conn_send_recv(IpFamily::V6).await
    }

    async fn rebinding_conn_send_recv(network: IpFamily) -> Result<()> {
        let m1 = RebindingUdpConn::bind(0, network)?;
        let (m1, _m1_key) = wrap_socket(m1)?;

        let m2 = RebindingUdpConn::bind(0, network)?;
        let (m2, _m2_key) = wrap_socket(m2)?;

        let m1_addr = SocketAddr::new(network.local_addr(), m1.local_addr()?.port());
        let (m1_send, m1_recv) = flume::bounded(8);

        let m1_task = tokio::task::spawn(async move {
            if let Some(conn) = m1.accept().await {
                let conn = conn.await?;
                let (mut send_bi, mut recv_bi) = conn.accept_bi().await?;

                let val = recv_bi.read_to_end(usize::MAX).await?;
                m1_send.send_async(val).await?;
                send_bi.finish().await?;
            }

            Ok::<_, anyhow::Error>(())
        });

        let conn = m2.connect(m1_addr, "localhost")?.await?;

        let (mut send_bi, mut recv_bi) = conn.open_bi().await?;
        send_bi.write_all(b"hello").await?;
        send_bi.finish().await?;

        let _ = recv_bi.read_to_end(usize::MAX).await?;
        conn.close(0u32.into(), b"done");
        m2.wait_idle().await;

        drop(send_bi);

        // make sure the right values arrived
        let val = m1_recv.recv_async().await?;
        assert_eq!(val, b"hello");

        m1_task.await??;

        Ok(())
    }
}
