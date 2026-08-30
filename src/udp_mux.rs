use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicUsize, Ordering},
};
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
    queue: Mutex<EndpointQueue>,
    waker: AtomicWaker,
    dropped_packets: AtomicUsize,
}

struct EndpointQueue {
    datagrams: VecDeque<Datagram>,
    bytes: usize,
}

struct Route {
    endpoint: Arc<EndpointState>,
    remote_addr: Mutex<Option<SocketAddr>>,
}

// Keep media latency bounded when a peer stops reading briefly.  Dropping the
// oldest queued media gives a recovered peer the most current packet instead
// of making it play through a stale backlog.
const MAX_QUEUED_PACKETS: usize = 64;
const MAX_QUEUED_BYTES: usize = 512 * 1024;

pub struct UdpMux {
    socket: Arc<UdpSocket>,
    candidate_addr: RwLock<SocketAddr>,
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
            .field("candidate_addr", &self.mux.candidate_addr().ok())
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
            candidate_addr: RwLock::new(candidate_addr),
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

    pub fn candidate_addr(&self) -> io::Result<SocketAddr> {
        self.candidate_addr
            .read()
            .map(|address| *address)
            .map_err(|_| io::Error::other("media candidate address lock poisoned"))
    }

    pub fn set_candidate_addr(&self, candidate_addr: SocketAddr) -> io::Result<()> {
        let mut address = self
            .candidate_addr
            .write()
            .map_err(|_| io::Error::other("media candidate address lock poisoned"))?;
        *address = candidate_addr;
        Ok(())
    }

    pub fn endpoint(
        self: &Arc<Self>,
        connection_id: Uuid,
        remote_ufrag: String,
    ) -> Arc<dyn AsyncUdpSocket> {
        let route = Arc::new(Route {
            endpoint: Arc::new(EndpointState {
                queue: Mutex::new(EndpointQueue {
                    datagrams: VecDeque::new(),
                    bytes: 0,
                }),
                waker: AtomicWaker::new(),
                dropped_packets: AtomicUsize::new(0),
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
                // Do not let an older route remove a newer route that has
                // since claimed the same address.
                if addresses.get(&remote_addr) == Some(&connection_id) {
                    addresses.remove(&remote_addr);
                }
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
            mux.route_packet(connection_id, packet, remote_addr);
        }
    }

    fn route_packet(&self, connection_id: Uuid, packet: &[u8], remote_addr: SocketAddr) {
        // Keep the route registered until its learned-address update commits.
        // Otherwise unregister could remove it between lookup and insertion,
        // leaving a stale address entry behind.
        let Ok(routes) = self.routes.lock() else {
            return;
        };
        let Some(route) = routes.get(&connection_id) else {
            return;
        };

        let old_addr = route.remote_addr.lock().ok().and_then(|mut address| {
            let previous = *address;
            *address = Some(remote_addr);
            previous
        });
        if let Ok(mut addresses) = self.by_remote_addr.lock() {
            if let Some(old_addr) = old_addr
                && old_addr != remote_addr
                && addresses.get(&old_addr) == Some(&connection_id)
            {
                addresses.remove(&old_addr);
            }
            addresses.insert(remote_addr, connection_id);
        }
        route.endpoint.enqueue(packet, remote_addr);
        drop(routes);
    }

    fn identify_packet(&self, packet: &[u8], remote_addr: SocketAddr) -> Option<Uuid> {
        // A STUN USERNAME is the only authenticated-ish routing hint available
        // to this mux before ICE consumes the packet. Prefer it to a learned
        // address so ICE restarts and NAT rebinding reach the intended peer.
        // Never fall back to an address for malformed or unknown STUN packets.
        if let Some(username) = stun_username(packet) {
            let ufrags = self.by_remote_ufrag.lock().ok()?;
            let connection_id = ufrags.iter().find_map(|(ufrag, connection_id)| {
                username
                    .split(':')
                    .any(|part| part == ufrag)
                    .then_some(*connection_id)
            });
            // A request with an unknown username must never inherit an older
            // address route. It belongs to another ICE generation or peer.
            return connection_id;
        }
        // Binding success/error responses do not carry USERNAME. Once an
        // authenticated request has learned the peer's address, route those
        // responses by that exact address so ICE connectivity checks can
        // complete. Requests and malformed STUN packets remain fail-closed.
        if is_stun_packet(packet) && !is_stun_response(packet) {
            return None;
        }
        if let Ok(addresses) = self.by_remote_addr.lock()
            && let Some(connection_id) = addresses.get(&remote_addr)
        {
            return Some(*connection_id);
        }
        None
    }
}

impl EndpointState {
    fn enqueue(&self, packet: &[u8], remote_addr: SocketAddr) {
        let packet_len = packet.len();
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        if packet_len > MAX_QUEUED_BYTES {
            self.dropped_packets.fetch_add(1, Ordering::Relaxed);
            return;
        }
        while queue.datagrams.len() >= MAX_QUEUED_PACKETS
            || queue.bytes.saturating_add(packet_len) > MAX_QUEUED_BYTES
        {
            let Some(dropped) = queue.datagrams.pop_front() else {
                break;
            };
            queue.bytes -= dropped.data.len();
            self.dropped_packets.fetch_add(1, Ordering::Relaxed);
        }
        queue.bytes += packet_len;
        queue.datagrams.push_back(Datagram {
            data: Bytes::copy_from_slice(packet),
            remote_addr,
        });
        drop(queue);
        self.waker.wake();
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
        self.mux.candidate_addr()
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
        if let Some(datagram) = queue.datagrams.pop_front() {
            queue.bytes -= datagram.data.len();
            let length = datagram.data.len().min(bufs[0].len());
            bufs[0][..length].copy_from_slice(&datagram.data[..length]);
            meta[0].addr = datagram.remote_addr;
            meta[0].len = length;
            meta[0].stride = length.max(1);
            return Poll::Ready(Ok(1));
        }
        self.state.waker.register(cx.waker());
        if queue.datagrams.is_empty() {
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
    if !is_stun_packet(packet) {
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

fn is_stun_packet(packet: &[u8]) -> bool {
    packet.len() >= 20 && packet[4..8] == [0x21, 0x12, 0xa4, 0x42]
}

fn is_stun_response(packet: &[u8]) -> bool {
    if !is_stun_packet(packet) {
        return false;
    }
    let message_length = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if 20_usize.saturating_add(message_length) > packet.len() {
        return false;
    }
    let message_type = u16::from_be_bytes([packet[0], packet[1]]);
    let class = ((message_type >> 4) & 0x1) | ((message_type >> 7) & 0x2);
    class >= 2
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

    #[tokio::test]
    async fn new_endpoints_use_the_updated_public_candidate() {
        let bind_address = "127.0.0.1:0".parse().unwrap();
        let initial_candidate = "192.168.1.10:8475".parse().unwrap();
        let public_candidate = "203.0.113.7:8475".parse().unwrap();
        let mux = UdpMux::bind(bind_address, initial_candidate).unwrap();

        mux.set_candidate_addr(public_candidate).unwrap();
        let endpoint = mux.endpoint(Uuid::new_v4(), "viewer".to_owned());

        assert_eq!(endpoint.local_addr().unwrap(), public_candidate);
    }

    fn stun_packet(username: &str) -> Vec<u8> {
        let username = username.as_bytes();
        let padded_len = (username.len() + 3) & !3;
        let mut packet = vec![0_u8; 20];
        packet[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        packet[2..4].copy_from_slice(&((4 + padded_len) as u16).to_be_bytes());
        packet.extend_from_slice(&0x0006_u16.to_be_bytes());
        packet.extend_from_slice(&(username.len() as u16).to_be_bytes());
        packet.extend_from_slice(username);
        packet.resize(20 + 4 + padded_len, 0);
        packet
    }

    fn stun_success_response() -> Vec<u8> {
        let mut packet = vec![0_u8; 20];
        packet[..2].copy_from_slice(&0x0101_u16.to_be_bytes());
        packet[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        packet
    }

    #[test]
    fn endpoint_queue_is_bounded_and_drops_oldest_packets() {
        let state = EndpointState {
            queue: Mutex::new(EndpointQueue {
                datagrams: VecDeque::new(),
                bytes: 0,
            }),
            waker: AtomicWaker::new(),
            dropped_packets: AtomicUsize::new(0),
        };
        let remote_addr = "127.0.0.1:9000".parse().unwrap();
        for value in 0..=MAX_QUEUED_PACKETS {
            state.enqueue(&[value as u8], remote_addr);
        }
        let queue = state.queue.lock().unwrap();
        assert_eq!(queue.datagrams.len(), MAX_QUEUED_PACKETS);
        assert_eq!(queue.datagrams.front().unwrap().data[0], 1);
        assert_eq!(state.dropped_packets.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn endpoint_queue_is_bounded_by_bytes() {
        let state = EndpointState {
            queue: Mutex::new(EndpointQueue {
                datagrams: VecDeque::new(),
                bytes: 0,
            }),
            waker: AtomicWaker::new(),
            dropped_packets: AtomicUsize::new(0),
        };
        let remote_addr = "127.0.0.1:9000".parse().unwrap();
        let packet = vec![0_u8; MAX_QUEUED_BYTES / 2];
        state.enqueue(&packet, remote_addr);
        state.enqueue(&packet, remote_addr);
        state.enqueue(&packet, remote_addr);

        let queue = state.queue.lock().unwrap();
        assert_eq!(queue.datagrams.len(), 2);
        assert_eq!(queue.bytes, MAX_QUEUED_BYTES);
        assert_eq!(state.dropped_packets.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn stun_route_replaces_a_changed_remote_address() {
        let bind_address = "127.0.0.1:0".parse().unwrap();
        let mux = UdpMux::bind(bind_address, bind_address).unwrap();
        let id = Uuid::new_v4();
        let _endpoint = mux.endpoint(id, "viewer".to_owned());
        let old_addr = "127.0.0.1:9001".parse().unwrap();
        let new_addr = "127.0.0.1:9002".parse().unwrap();

        mux.route_packet(id, &stun_packet("server:viewer"), old_addr);
        mux.route_packet(id, &stun_packet("server:viewer"), new_addr);

        let addresses = mux.by_remote_addr.lock().unwrap();
        assert_eq!(addresses.get(&new_addr), Some(&id));
        assert!(!addresses.contains_key(&old_addr));
    }

    #[tokio::test]
    async fn unregistering_an_old_route_keeps_a_new_same_address_route() {
        let bind_address = "127.0.0.1:0".parse().unwrap();
        let mux = UdpMux::bind(bind_address, bind_address).unwrap();
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();
        let _old_endpoint = mux.endpoint(old_id, "old".to_owned());
        let _new_endpoint = mux.endpoint(new_id, "new".to_owned());
        let addr = "127.0.0.1:9003".parse().unwrap();

        mux.route_packet(old_id, &stun_packet("server:old"), addr);
        mux.route_packet(new_id, &stun_packet("server:new"), addr);
        mux.unregister(old_id);

        assert_eq!(mux.by_remote_addr.lock().unwrap().get(&addr), Some(&new_id));
    }

    #[tokio::test]
    async fn stun_routing_overrides_a_stale_address_mapping() {
        let bind_address = "127.0.0.1:0".parse().unwrap();
        let mux = UdpMux::bind(bind_address, bind_address).unwrap();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let _first_endpoint = mux.endpoint(first_id, "first".to_owned());
        let _second_endpoint = mux.endpoint(second_id, "second".to_owned());
        let addr = "127.0.0.1:9004".parse().unwrap();

        mux.route_packet(first_id, &stun_packet("server:first"), addr);
        assert_eq!(
            mux.identify_packet(&stun_packet("server:second"), addr),
            Some(second_id)
        );
        assert_eq!(mux.identify_packet(&[0x80, 0x60], addr), Some(first_id));
    }

    #[tokio::test]
    async fn stun_responses_use_the_address_learned_from_a_valid_request() {
        let bind_address = "127.0.0.1:0".parse().unwrap();
        let mux = UdpMux::bind(bind_address, bind_address).unwrap();
        let id = Uuid::new_v4();
        let _endpoint = mux.endpoint(id, "viewer".to_owned());
        let addr = "127.0.0.1:9005".parse().unwrap();

        let request = stun_packet("server:viewer");
        assert_eq!(mux.identify_packet(&request, addr), Some(id));
        mux.route_packet(id, &request, addr);
        assert_eq!(
            mux.identify_packet(&stun_success_response(), addr),
            Some(id)
        );

        // An unknown request cannot reuse the learned route.
        assert_eq!(
            mux.identify_packet(&stun_packet("server:other"), addr),
            None
        );
    }
}
