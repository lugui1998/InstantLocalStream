use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::task::AtomicWaker;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use uuid::Uuid;
use webrtc::runtime::{AsyncUdpSocket, RecvMeta, Transmit};

struct Datagram {
    data: Bytes,
    remote_addr: SocketAddr,
}

struct EndpointState {
    queue: Mutex<VecDeque<Datagram>>,
    waker: AtomicWaker,
}

struct Route {
    endpoint: Arc<EndpointState>,
    remote_addr: Mutex<Option<SocketAddr>>,
}

pub struct UdpMux {
    socket: Arc<UdpSocket>,
    candidate_addr: SocketAddr,
    routes: Mutex<HashMap<Uuid, Arc<Route>>>,
    by_remote_ufrag: Mutex<HashMap<String, Uuid>>,
    by_remote_addr: Mutex<HashMap<SocketAddr, Uuid>>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

pub struct UdpMuxEndpoint {
    mux: Arc<UdpMux>,
    connection_id: Uuid,
    state: Arc<EndpointState>,
}

pub fn ice_ufrag(sdp: &str) -> Option<String> {
    sdp.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("a=ice-ufrag:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

impl fmt::Debug for UdpMuxEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpMuxEndpoint")
            .field("connection_id", &self.connection_id)
            .field("candidate_addr", &self.mux.candidate_addr)
            .finish()
    }
}

impl UdpMux {
    pub fn bind(bind_addr: SocketAddr, candidate_addr: SocketAddr) -> io::Result<Arc<Self>> {
        let std_socket = std::net::UdpSocket::bind(bind_addr)?;
        std_socket.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std_socket)?);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mux = Arc::new(Self {
            socket,
            candidate_addr,
            routes: Mutex::new(HashMap::new()),
            by_remote_ufrag: Mutex::new(HashMap::new()),
            by_remote_addr: Mutex::new(HashMap::new()),
            shutdown: Mutex::new(Some(shutdown_tx)),
        });
        let receiver = Arc::downgrade(&mux);
        let socket = Arc::clone(&mux.socket);
        tokio::spawn(async move {
            UdpMux::receive_loop(receiver, socket, shutdown_rx).await;
        });
        Ok(mux)
    }

    pub fn endpoint(
        self: &Arc<Self>,
        connection_id: Uuid,
        remote_ufrag: String,
    ) -> Arc<dyn AsyncUdpSocket> {
        let route = Arc::new(Route {
            endpoint: Arc::new(EndpointState {
                queue: Mutex::new(VecDeque::new()),
                waker: AtomicWaker::new(),
            }),
            remote_addr: Mutex::new(None),
        });
        if let Ok(mut routes) = self.routes.lock() {
            routes.insert(connection_id, Arc::clone(&route));
        }
        if let Ok(mut ufrags) = self.by_remote_ufrag.lock() {
            ufrags.insert(remote_ufrag, connection_id);
        }
        Arc::new(UdpMuxEndpoint {
            mux: Arc::clone(self),
            connection_id,
            state: route.endpoint.clone(),
        })
    }

    pub fn unregister(&self, connection_id: Uuid) {
        let route = self
            .routes
            .lock()
            .ok()
            .and_then(|mut routes| routes.remove(&connection_id));
        if let Some(route) = route {
            if let Ok(mut ufrags) = self.by_remote_ufrag.lock() {
                ufrags.retain(|_, id| *id != connection_id);
            }
            if let Some(remote_addr) = route.remote_addr.lock().ok().and_then(|addr| *addr)
                && let Ok(mut addresses) = self.by_remote_addr.lock()
            {
                addresses.remove(&remote_addr);
            }
            route.endpoint.waker.wake();
        }
    }

    async fn receive_loop(
        weak_mux: std::sync::Weak<Self>,
        socket: Arc<UdpSocket>,
        mut shutdown: oneshot::Receiver<()>,
    ) {
        let mut buffer = vec![0_u8; 65_536];
        loop {
            let received = tokio::select! {
                _ = &mut shutdown => return,
                received = socket.recv_from(&mut buffer) => received,
            };
            let (size, remote_addr) = match received {
                Ok(value) => value,
                Err(error) => {
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::Interrupted
                    ) {
                        tracing::debug!(%error, "transient shared UDP receive error");
                        continue;
                    }
                    tracing::error!(%error, "shared UDP socket receive loop stopped");
                    return;
                }
            };
            let Some(mux) = weak_mux.upgrade() else {
                return;
            };
            let packet = &buffer[..size];
            let connection_id = mux.identify_packet(packet, remote_addr);
            let Some(connection_id) = connection_id else {
                tracing::debug!(
                    %remote_addr,
                    size,
                    username = ?stun_username(packet),
                    "dropping UDP packet without a mux route"
                );
                continue;
            };
            let route = mux
                .routes
                .lock()
                .ok()
                .and_then(|routes| routes.get(&connection_id).cloned());
            let Some(route) = route else {
                continue;
            };
            if let Ok(mut address) = route.remote_addr.lock() {
                *address = Some(remote_addr);
            }
            if let Ok(mut addresses) = mux.by_remote_addr.lock() {
                addresses.insert(remote_addr, connection_id);
            }
            if let Ok(mut queue) = route.endpoint.queue.lock() {
                queue.push_back(Datagram {
                    data: Bytes::copy_from_slice(packet),
                    remote_addr,
                });
            }
            route.endpoint.waker.wake();
        }
    }

    fn identify_packet(&self, packet: &[u8], remote_addr: SocketAddr) -> Option<Uuid> {
        if let Ok(addresses) = self.by_remote_addr.lock()
            && let Some(connection_id) = addresses.get(&remote_addr)
        {
            return Some(*connection_id);
        }
        let username = stun_username(packet)?;
        let ufrags = self.by_remote_ufrag.lock().ok()?;
        ufrags.iter().find_map(|(ufrag, connection_id)| {
            username
                .split(':')
                .any(|part| part == ufrag)
                .then_some(*connection_id)
        })
    }
}

impl Drop for UdpMux {
    fn drop(&mut self) {
        if let Ok(mut shutdown) = self.shutdown.lock()
            && let Some(sender) = shutdown.take()
        {
            let _ = sender.send(());
        }
    }
}

impl AsyncUdpSocket for UdpMuxEndpoint {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.mux.candidate_addr)
    }

    fn poll_send(&self, cx: &mut Context<'_>, transmit: &Transmit<'_>) -> Poll<io::Result<usize>> {
        if transmit.segment_size.is_some() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "UDP mux does not support GSO segments",
            )));
        }
        match self.mux.socket.poll_send_ready(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                Poll::Ready(self.mux.socket.try_io(tokio::io::Interest::WRITABLE, || {
                    self.mux
                        .socket
                        .try_send_to(transmit.contents, transmit.destination)
                }))
            }
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() || meta.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let mut queue = match self.state.queue.lock() {
            Ok(queue) => queue,
            Err(_) => {
                return Poll::Ready(Err(io::Error::other("UDP mux queue lock poisoned")));
            }
        };
        if let Some(datagram) = queue.pop_front() {
            let length = datagram.data.len().min(bufs[0].len());
            bufs[0][..length].copy_from_slice(&datagram.data[..length]);
            meta[0].addr = datagram.remote_addr;
            meta[0].len = length;
            meta[0].stride = length.max(1);
            return Poll::Ready(Ok(1));
        }
        self.state.waker.register(cx.waker());
        if queue.is_empty() {
            Poll::Pending
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    fn max_gso_segments(&self) -> usize {
        1
    }

    fn max_gro_segments(&self) -> usize {
        1
    }
}

fn stun_username(packet: &[u8]) -> Option<String> {
    if packet.len() < 20 || packet[4..8] != [0x21, 0x12, 0xa4, 0x42] {
        return None;
    }
    let message_length = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let end = (20 + message_length).min(packet.len());
    let mut offset = 20;
    while offset + 4 <= end {
        let attribute_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let attribute_length =
            u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start.saturating_add(attribute_length);
        if value_end > end {
            return None;
        }
        if attribute_type == 0x0006 {
            return std::str::from_utf8(&packet[value_start..value_end])
                .ok()
                .map(str::to_owned);
        }
        offset = value_start + ((attribute_length + 3) & !3);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ice_username_fragment_from_offer_sdp() {
        let sdp = "v=0\r\na=ice-ufrag:browser123\r\na=ice-pwd:secret\r\n";
        assert_eq!(ice_ufrag(sdp).as_deref(), Some("browser123"));
    }

    #[test]
    fn extracts_stun_username_attribute() {
        let username = b"server:browser";
        let mut packet = vec![0_u8; 20];
        packet[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        packet[2..4].copy_from_slice(&((4 + username.len()) as u16).to_be_bytes());
        packet.extend_from_slice(&0x0006_u16.to_be_bytes());
        packet.extend_from_slice(&(username.len() as u16).to_be_bytes());
        packet.extend_from_slice(username);
        assert_eq!(stun_username(&packet).as_deref(), Some("server:browser"));
    }

    #[tokio::test]
    async fn dropping_mux_releases_bound_port() {
        let address = "127.0.0.1:0".parse().unwrap();
        let mux = UdpMux::bind(address, address).unwrap();
        let bound_port = mux.socket.local_addr().unwrap().port();
        drop(mux);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(std::net::UdpSocket::bind(("127.0.0.1", bound_port)).is_ok());
    }
}
