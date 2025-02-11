use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::UdpSocket;
use tokio::sync::RwLock;

use playit_agent_proto::control_messages::UdpChannelDetails;

use crate::network::udp_clients::UdpClients;
use crate::tunnel::udp_proto::UdpFlow;
use crate::utils::now_milli;

#[derive(Clone)]
pub struct UdpTunnel {
    inner: Arc<Inner>,
}

struct Inner {
    udp4: UdpSocket,
    udp6: Option<UdpSocket>,
    details: RwLock<Option<UdpChannelDetails>>,
    last_confirm: AtomicU64,
    last_send: AtomicU64,
}

impl UdpTunnel {
    pub async fn new() -> std::io::Result<Self> {
        Ok(UdpTunnel {
            inner: Arc::new(Inner {
                udp4: UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).await?,
                udp6: UdpSocket::bind(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)).await.ok(),
                details: RwLock::new(None),
                last_confirm: AtomicU64::new(0),
                last_send: AtomicU64::new(0),
            })
        })
    }

    pub async fn is_setup(&self) -> bool {
        self.inner.details.read().await.is_some()
    }

    pub fn requires_resend(&self) -> bool {
        let last_confirm = self.inner.last_confirm.load(Ordering::SeqCst);
        /* send token every 10 seconds */
        10_000 < now_milli() - last_confirm
    }

    pub fn requires_auth(&self) -> bool {
        let last_confirm = self.inner.last_confirm.load(Ordering::SeqCst);
        let last_send = self.inner.last_send.load(Ordering::SeqCst);

        /* send is confirmed */
        if last_send < last_confirm {
            return false;
        }

        let now = now_milli();
        5_000 < now - last_send
    }

    pub async fn set_udp_tunnel(&self, details: UdpChannelDetails) -> std::io::Result<()> {
        {
            let mut details_lock = self.inner.details.write().await;

            /* if details haven't changed, exit */
            if let Some(current) = &*details_lock {
                if details.eq(current) {
                    return Ok(());
                }
            }

            details_lock.replace(details.clone());
        }

        self.send_token(&details).await
    }

    pub async fn resend_token(&self) -> std::io::Result<bool> {
        let token = {
            let lock = self.inner.details.read();
            match &*lock.await {
                Some(v) => v.clone(),
                None => return Ok(false),
            }
        };

        self.send_token(&token).await?;
        Ok(true)
    }

    async fn send_token(&self, details: &UdpChannelDetails) -> std::io::Result<()> {
        match details.tunnel_addr {
            SocketAddr::V4(tunnel_addr) => {
                self.inner.udp4.send_to(&details.token, tunnel_addr).await?;
            }
            SocketAddr::V6(tunnel_addr) => {
                let udp = match &self.inner.udp6 {
                    Some(v) => v,
                    None => return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "IPv6 not supported")),
                };

                udp.send_to(&details.token, tunnel_addr).await?;
            }
        }

        self.inner.last_send.store(now_milli(), Ordering::SeqCst);
        Ok(())
    }

    pub async fn send(&self, data: &mut Vec<u8>, flow: UdpFlow) -> std::io::Result<usize> {
        /* append flow to udp packet */
        let og_packet_len = data.len();
        data.resize(flow.len() + og_packet_len, 0);
        flow.write_to(&mut data[og_packet_len..]);

        let (socket, tunnel_addr, _) = self.get_sock().await?;
        socket.send_to(&data, tunnel_addr).await
    }

    async fn get_sock(&self) -> std::io::Result<(&UdpSocket, SocketAddr, Arc<Vec<u8>>)> {
        let lock = self.inner.details.read().await;

        let details = match &*lock {
            Some(v) => v,
            None => return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "udp tunnel not connected")),
        };

        Ok(if details.tunnel_addr.is_ipv4() {
            (&self.inner.udp4, details.tunnel_addr, details.token.clone())
        } else {
            (self.inner.udp6.as_ref().unwrap(), details.tunnel_addr, details.token.clone())
        })
    }

    pub async fn receive_from(&self, buffer: &mut [u8]) -> std::io::Result<UdpTunnelRx> {
        let (udp, tunnel_addr, token) = self.get_sock().await?;
        let (bytes, remote) = udp.recv_from(buffer).await?;

        if tunnel_addr != remote {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "got data from other source"));
        }

        if buffer[..bytes].eq(&token[..]) {
            self.inner.last_confirm.store(now_milli(), Ordering::SeqCst);
            return Ok(UdpTunnelRx::ConfirmedConnection);
        }

        if buffer.len() + UdpFlow::len_v4().max(UdpFlow::len_v6()) < bytes {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "receive buffer too small"));
        }

        let footer = match UdpFlow::from_tail(&buffer[..bytes]) {
            Some(v) => v,
            None => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "failed to extract udp footer")),
        };

        Ok(UdpTunnelRx::ReceivedPacket {
            bytes: bytes - footer.len(),
            flow: footer,
        })
    }
}

pub enum UdpTunnelRx {
    ReceivedPacket { bytes: usize, flow: UdpFlow },
    ConfirmedConnection,
}
