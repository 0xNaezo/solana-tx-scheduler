//! Minimal TPU client: one QUIC connection per leader TPU address, one
//! unidirectional stream per transaction, sent strictly in our order.
//! (`solana-tpu-client-next` fans out to the next leaders and owns timing;
//! here the experiment needs both under control.)

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use quinn::{
    ClientConfig, Connection, Endpoint, IdleTimeout, TransportConfig,
    crypto::rustls::QuicClientConfig,
};
use solana_tls_utils::{
    QuicClientCertificate, socket_addr_to_quic_server_name, tls_client_config_builder,
};

const ALPN_TPU: &[u8] = b"solana-tpu";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Tpu {
    endpoint: Endpoint,
    connections: HashMap<SocketAddr, Connection>,
}

impl Tpu {
    /// Unstaked client with an ephemeral certificate: we are subject to the
    /// unstaked SWQoS tier like any ordinary user.
    pub fn new() -> Result<Self> {
        let cert = QuicClientCertificate::new(None);
        let mut crypto = tls_client_config_builder()
            .with_client_auth_cert(vec![cert.certificate], cert.key)
            .context("QUIC client certificate")?;
        crypto.enable_early_data = true;
        crypto.alpn_protocols = vec![ALPN_TPU.to_vec()];

        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(Some(IdleTimeout::try_from(Duration::from_secs(10))?));
        transport.keep_alive_interval(Some(Duration::from_secs(1)));

        let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
        config.transport_config(Arc::new(transport));

        let mut endpoint = Endpoint::client(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
        endpoint.set_default_client_config(config);
        Ok(Self {
            endpoint,
            connections: HashMap::new(),
        })
    }

    /// Opens (or reuses) a connection; returns the handshake time if a new one was made.
    pub async fn connect(&mut self, addr: SocketAddr) -> Result<Option<Duration>> {
        if let Some(conn) = self.connections.get(&addr) {
            if conn.close_reason().is_none() {
                return Ok(None);
            }
            self.connections.remove(&addr);
        }
        let started = Instant::now();
        let connecting = self
            .endpoint
            .connect(addr, &socket_addr_to_quic_server_name(addr))?;
        let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
            .await
            .map_err(|_| anyhow!("handshake timeout"))??;
        let took = started.elapsed();
        self.connections.insert(addr, conn);
        Ok(Some(took))
    }

    /// Writes one transaction on a fresh stream. Does not wait for the peer.
    pub async fn send(&mut self, addr: SocketAddr, wire_tx: &[u8]) -> Result<()> {
        let conn = self
            .connections
            .get(&addr)
            .ok_or_else(|| anyhow!("not connected to {addr}"))?;
        let mut stream = conn.open_uni().await?;
        stream.write_all(wire_tx).await?;
        stream.finish()?;
        Ok(())
    }

    pub async fn close(self) {
        for conn in self.connections.values() {
            conn.close(0u32.into(), b"done");
        }
        self.endpoint.wait_idle().await;
    }
}
