//! Bounded IPv4 NFSv3 transports for RustNFS.
//!
//! A single VFS represents a virtual root. `exports` contains allowed paths
//! below that root; MOUNT does not strip an export name before VFS lookup.
//! Network admission is by IP, not by AUTH_SYS identity. AUTH_SYS is client
//! supplied metadata and is deliberately not treated as authentication.
//!
//! `shutdown` closes listeners, interrupts idle reads, and drains accepted
//! filesystem operations before returning. It does not cancel an operation in
//! the middle of a write. The backend must ensure its operations terminate.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::{self, Cursor};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nfs3_types::{mount, nfs3, portmap, rpc};
use nfs3_types::rpc::{accept_stat_data, auth_flavor, auth_stat, auth_unix};
use nfs3_types::xdr_codec::{List, Opaque, Pack, Unpack, Void};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, warn};

use crate::context::RPCContext;
use crate::rpcwire::messages::{CompleteRpcMessage, HandleResult, IncomingRpcMessage};
use crate::transaction_tracker::TransactionTracker;
use crate::vfs::NfsFileSystem;
use crate::vfs::handle::FileHandleConverter;

pub const MAX_RPC_RECORD_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_UDP_BYTES: usize = 60 * 1024;
const MAX_MOUNTS: usize = 128;
const MAX_MOUNT_BYTES: usize = 48 * 1024;

pub type ClientFilter = Arc<dyn Fn(IpAddr) -> bool + Send + Sync>;

/// Limits apply across all three listeners, rather than per port.
#[derive(Clone)]
pub struct ServerConfig {
    pub bind_ip: Ipv4Addr,
    pub nfs_port: u16,
    pub mount_port: u16,
    pub portmap_port: u16,
    pub exports: Vec<String>,
    pub allow_client: ClientFilter,
    pub enable_tcp: bool,
    pub enable_udp: bool,
    pub max_connections: usize,
    pub max_concurrent_requests: usize,
    pub idle_timeout: Duration,
    pub max_rpc_record: usize,
    pub max_rpc_fragments: usize,
    pub max_io_bytes: u32,
    pub udp_cache_entries: usize,
    pub udp_cache_bytes: usize,
    pub udp_cache_ttl: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_ip: Ipv4Addr::LOCALHOST,
            nfs_port: 2049,
            mount_port: 20048,
            portmap_port: 111,
            exports: vec!["/share".into()],
            allow_client: Arc::new(|ip| ip.is_loopback()),
            enable_tcp: true,
            enable_udp: true,
            max_connections: 128,
            max_concurrent_requests: 32,
            idle_timeout: Duration::from_secs(60),
            max_rpc_record: MAX_RPC_RECORD_BYTES,
            max_rpc_fragments: 256,
            max_io_bytes: 8192,
            udp_cache_entries: 256,
            udp_cache_bytes: 16 * 1024 * 1024,
            udp_cache_ttl: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ServerPorts {
    pub portmap_port: u16,
    pub mount_port: u16,
    pub nfs_port: u16,
}

pub struct ServerHandle {
    ports: ServerPorts,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl ServerHandle {
    pub const fn ports(&self) -> ServerPorts { self.ports }

    pub fn request_shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Stops admission and waits for accepted filesystem operations to finish.
    pub async fn shutdown(mut self) -> io::Result<()> {
        self.request_shutdown();
        if let Some(task) = self.task.take() {
            task.await.map_err(io::Error::other)?
        } else {
            Ok(())
        }
    }

    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) { self.request_shutdown(); }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Service { Portmap, Mount, Nfs }

impl Service {
    const fn label(self) -> &'static str {
        match self {
            Self::Portmap => "Portmapper",
            Self::Mount => "MOUNT",
            Self::Nfs => "NFS",
        }
    }

    const fn program(self) -> u32 {
        match self {
            Self::Portmap => portmap::PROGRAM,
            Self::Mount => mount::PROGRAM,
            Self::Nfs => nfs3::PROGRAM,
        }
    }

    const fn port(self, ports: ServerPorts) -> u16 {
        match self {
            Self::Portmap => ports.portmap_port,
            Self::Mount => ports.mount_port,
            Self::Nfs => ports.nfs_port,
        }
    }
}

fn bind_error(service: Service, protocol: &str, address: SocketAddr, error: io::Error) -> io::Error {
    let reason = if error.kind() == io::ErrorKind::AddrInUse { "端口已被占用" } else { "绑定失败" };
    io::Error::new(error.kind(), format!("{} {protocol} {address} {reason}：{error}", service.label()))
}

struct BoundEndpoint {
    service: Service,
    tcp: Option<TcpListener>,
    udp: Option<UdpSocket>,
}

struct Shared<T: NfsFileSystem> {
    fs: Arc<T>,
    config: ServerConfig,
    ports: ServerPorts,
    converter: FileHandleConverter,
    tracker: Arc<TransactionTracker>,
    connections: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    replay: Arc<Mutex<ReplayCache>>,
    mounts: Mutex<BTreeSet<(IpAddr, String)>>,
}

/// Binds every requested socket before starting any service. Port zero is
/// supported for tests; TCP and UDP for one service receive the same port.
pub async fn start<T: NfsFileSystem + 'static>(
    fs: Arc<T>,
    mut config: ServerConfig,
) -> io::Result<ServerHandle> {
    validate_config(&mut config)?;
    let mut ports = ServerPorts {
        portmap_port: config.portmap_port,
        mount_port: config.mount_port,
        nfs_port: config.nfs_port,
    };
    let mut endpoints = Vec::new();
    for service in [Service::Portmap, Service::Mount, Service::Nfs] {
        let mut port = service.port(ports);
        let tcp = if config.enable_tcp {
            let address = SocketAddr::from((config.bind_ip, port));
            let socket = TcpListener::bind(address).await
                .map_err(|error| bind_error(service, "TCP", address, error))?;
            port = socket.local_addr()?.port();
            Some(socket)
        } else { None };
        let udp = if config.enable_udp {
            let address = SocketAddr::from((config.bind_ip, port));
            let socket = UdpSocket::bind(address).await
                .map_err(|error| bind_error(service, "UDP", address, error))?;
            port = socket.local_addr()?.port();
            Some(socket)
        } else { None };
        match service {
            Service::Portmap => ports.portmap_port = port,
            Service::Mount => ports.mount_port = port,
            Service::Nfs => ports.nfs_port = port,
        }
        endpoints.push(BoundEndpoint { service, tcp, udp });
    }

    let shared = Arc::new(Shared {
        fs,
        ports,
        converter: FileHandleConverter::new(),
        tracker: Arc::new(TransactionTracker::new(Duration::from_secs(60), 1, 2)),
        connections: Arc::new(Semaphore::new(config.max_connections)),
        requests: Arc::new(Semaphore::new(config.max_concurrent_requests)),
        replay: Arc::new(Mutex::new(ReplayCache::new(&config))),
        mounts: Mutex::new(BTreeSet::new()),
        config,
    });
    let (stop, receiver) = watch::channel(false);
    let supervisor_stop = stop.clone();
    let task = tokio::spawn(async move {
        let mut listeners = JoinSet::new();
        for endpoint in endpoints {
            if let Some(listener) = endpoint.tcp {
                listeners.spawn(tcp_loop(listener, endpoint.service, shared.clone(), receiver.clone()));
            }
            if let Some(socket) = endpoint.udp {
                listeners.spawn(udp_loop(socket, endpoint.service, shared.clone(), receiver.clone()));
            }
        }
        let mut failure = None;
        while let Some(result) = listeners.join_next().await {
            match result {
                Ok(Ok(())) => {},
                Ok(Err(error)) => {
                    warn!(%error, "NFS listener failed; stopping all listeners");
                    if failure.is_none() { failure = Some(error); }
                }
                Err(error) => {
                    if failure.is_none() { failure = Some(io::Error::other(error)); }
                }
            }
            supervisor_stop.send_replace(true);
        }
        failure.map_or(Ok(()), Err)
    });
    info!(?ports, "NFSv3 listeners ready");
    Ok(ServerHandle { ports, stop, task: Some(task) })
}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidInput, message) }

fn validate_config(config: &mut ServerConfig) -> io::Result<()> {
    if !config.enable_tcp && !config.enable_udp { return Err(invalid("Enable TCP or UDP")); }
    if config.max_connections == 0 || config.max_connections > 4096
        || config.max_concurrent_requests == 0 || config.max_concurrent_requests > 256 {
        return Err(invalid("Connection/request limits are outside supported bounds"));
    }
    if !(1024..=MAX_RPC_RECORD_BYTES).contains(&config.max_rpc_record)
        || !(1..=256).contains(&config.max_rpc_fragments)
        || !(512..=32768).contains(&config.max_io_bytes)
        || config.max_io_bytes as usize + 1024 > config.max_rpc_record {
        return Err(invalid("Invalid RPC record/fragment/I/O limits"));
    }
    if config.idle_timeout.is_zero() || config.idle_timeout > Duration::from_secs(3600) {
        return Err(invalid("Idle timeout must be 1 ms to 1 hour"));
    }
    if !(1..=4096).contains(&config.udp_cache_entries)
        || !(2 * MAX_UDP_BYTES..=64 * 1024 * 1024).contains(&config.udp_cache_bytes)
        || config.udp_cache_ttl.is_zero() || config.udp_cache_ttl > Duration::from_secs(3600) {
        return Err(invalid("Invalid UDP replay cache limits"));
    }
    if config.exports.is_empty() || config.exports.len() > 64 {
        return Err(invalid("Configure 1 to 64 exports"));
    }
    for name in &mut config.exports {
        *name = normalize_mount_path(name).ok_or_else(|| invalid("Invalid export path"))?;
    }
    config.exports.sort();
    config.exports.dedup();
    if config.exports.iter().map(String::len).sum::<usize>() > 16 * 1024 {
        return Err(invalid("Export list is too large"));
    }
    let ports = [config.portmap_port, config.mount_port, config.nfs_port];
    for (index, port) in ports.iter().enumerate() {
        if *port != 0 && ports[index + 1..].contains(port) {
            return Err(invalid("NFS, MOUNT, and portmap require distinct ports"));
        }
    }
    Ok(())
}

fn normalize_mount_path(path: &str) -> Option<String> {
    if !path.starts_with('/') || path.len() > mount::MNTPATHLEN
        || path.contains('\\') || path.contains(':') || path.chars().any(char::is_control) {
        return None;
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.iter().any(|part| *part == "." || *part == ".." || part.len() > 255) {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

fn is_exported(path: &str, exports: &[String]) -> bool {
    exports.iter().any(|export| export == "/" || path == export
        || path.strip_prefix(export).is_some_and(|suffix| suffix.starts_with('/')))
}

async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow() { return; }
        if stop.changed().await.is_err() { return; }
    }
}

async fn read_record<R: AsyncRead + Unpin>(
    input: &mut R, max_record: usize, max_fragments: usize,
) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    for _ in 0..max_fragments {
        let raw = input.read_u32().await?;
        let length = (raw & 0x7fff_ffff) as usize;
        let previous = data.len();
        let total = previous.checked_add(length).filter(|size| *size <= max_record)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "RPC record exceeds limit"))?;
        data.resize(total, 0);
        input.read_exact(&mut data[previous..]).await?;
        if raw & 0x8000_0000 != 0 { return Ok(data); }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "Too many RPC fragments"))
}

async fn tcp_loop<T: NfsFileSystem + 'static>(
    listener: TcpListener, service: Service, shared: Arc<Shared<T>>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut workers = JoinSet::new();
    let mut failure = None;
    loop {
        tokio::select! {
            biased;
            () = stop_requested(&mut stop) => break,
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                if let Err(error) = result { warn!(%error, "TCP worker failed"); }
            }
            incoming = listener.accept() => {
                let (socket, peer) = match incoming {
                    Ok(pair) => pair,
                    Err(error) => { failure = Some(error); break; }
                };
                if !(shared.config.allow_client)(peer.ip()) { continue; }
                let Ok(permit) = shared.connections.clone().try_acquire_owned() else {
                    debug!(%peer, "TCP connection limit reached");
                    continue;
                };
                let state = shared.clone();
                let receiver = stop.clone();
                workers.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_tcp(socket, peer, service, state, receiver).await {
                        debug!(%peer, %error, "TCP connection ended");
                    }
                });
            }
        }
    }
    drop(listener);
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result { warn!(%error, "TCP worker failed during shutdown"); }
    }
    failure.map_or(Ok(()), Err)
}

async fn serve_tcp<T: NfsFileSystem + 'static>(
    mut socket: TcpStream, peer: SocketAddr, service: Service,
    shared: Arc<Shared<T>>, mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    socket.set_nodelay(true)?;
    loop {
        let record = tokio::select! {
            biased;
            () = stop_requested(&mut stop) => return Ok(()),
            record = tokio::time::timeout(shared.config.idle_timeout,
                read_record(&mut socket, shared.config.max_rpc_record, shared.config.max_rpc_fragments)) => {
                record.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "RPC read timed out"))??
            }
        };
        let permit = tokio::select! {
            biased;
            () = stop_requested(&mut stop) => return Ok(()),
            permit = shared.requests.clone().acquire_owned() => permit.map_err(io::Error::other)?,
        };
        // Once accepted, finish the operation even if shutdown is requested.
        let response = dispatch_record(shared.clone(), peer, service, false, record).await;
        drop(permit);
        if let Some(response) = response {
            tokio::time::timeout(shared.config.idle_timeout, async {
                socket.write_u32(0x8000_0000 | response.len() as u32).await?;
                socket.write_all(&response).await
            }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "RPC reply timed out"))??;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RequestKey {
    peer: SocketAddr,
    service: Service,
    // Includes the XID, credentials, and every argument, not just a hash/XID.
    request: Arc<[u8]>,
}

struct CachedReply { key: RequestKey, reply: Arc<[u8]>, completed: Instant }

struct ReplayCache {
    entries: VecDeque<CachedReply>,
    pending: HashSet<RequestKey>,
    bytes: usize,
    capacity: usize,
    byte_limit: usize,
    ttl: Duration,
}

impl ReplayCache {
    fn new(config: &ServerConfig) -> Self {
        Self { entries: VecDeque::new(), pending: HashSet::new(), bytes: 0,
            capacity: config.udp_cache_entries, byte_limit: config.udp_cache_bytes,
            ttl: config.udp_cache_ttl }
    }

    fn lookup(&mut self, key: &RequestKey) -> Option<Arc<[u8]>> {
        let now = Instant::now();
        self.entries.retain(|entry| now.duration_since(entry.completed) <= self.ttl);
        self.bytes = self.entries.iter().map(|entry| entry.key.request.len() + entry.reply.len()).sum();
        let index = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(index)?;
        let reply = entry.reply.clone();
        self.entries.push_back(entry);
        Some(reply)
    }

    fn complete(&mut self, key: RequestKey, reply: Arc<[u8]>) {
        self.pending.remove(&key);
        let added = key.request.len() + reply.len();
        while self.entries.len() >= self.capacity || self.bytes + added > self.byte_limit {
            let Some(entry) = self.entries.pop_front() else { break; };
            self.bytes -= entry.key.request.len() + entry.reply.len();
        }
        self.bytes += added;
        self.entries.push_back(CachedReply { key, reply, completed: Instant::now() });
    }
}

struct PendingRequest { cache: Arc<Mutex<ReplayCache>>, key: RequestKey }

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Ok(mut cache) = self.cache.lock() { cache.pending.remove(&self.key); }
    }
}

async fn udp_loop<T: NfsFileSystem + 'static>(
    socket: UdpSocket, service: Service, shared: Arc<Shared<T>>,
    mut stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let socket = Arc::new(socket);
    // Larger than the largest IPv4 UDP datagram, so oversize detection cannot
    // accidentally process a silently truncated, valid-looking request.
    let mut buffer = vec![0_u8; 65536];
    let mut workers = JoinSet::new();
    let mut failure = None;
    loop {
        tokio::select! {
            biased;
            () = stop_requested(&mut stop) => break,
            Some(result) = workers.join_next(), if !workers.is_empty() => {
                if let Err(error) = result { warn!(%error, "UDP worker failed"); }
            }
            incoming = socket.recv_from(&mut buffer) => {
                let (length, peer) = match incoming {
                    Ok(pair) => pair,
                    Err(error) => { failure = Some(error); break; }
                };
                if !(shared.config.allow_client)(peer.ip()) || !(8..=MAX_UDP_BYTES).contains(&length)
                    || length > shared.config.max_rpc_record { continue; }
                let key = RequestKey { peer, service, request: Arc::from(&buffer[..length]) };
                let cached = shared.replay.lock().map_err(|_| io::Error::other("Replay cache poisoned"))?.lookup(&key);
                if let Some(reply) = cached {
                    let _ = socket.try_send_to(&reply, peer);
                    continue;
                }
                let Ok(permit) = shared.requests.clone().try_acquire_owned() else { continue; };
                {
                    let mut cache = shared.replay.lock().map_err(|_| io::Error::other("Replay cache poisoned"))?;
                    if !cache.pending.insert(key.clone()) { continue; }
                }
                let pending = PendingRequest { cache: shared.replay.clone(), key: key.clone() };
                let state = shared.clone();
                let sender = socket.clone();
                workers.spawn(async move {
                    let _permit = permit;
                    let _pending = pending;
                    let response = dispatch_record(state.clone(), peer, service, true, key.request.to_vec()).await;
                    if let Some(response) = response {
                        let response: Arc<[u8]> = response.into();
                        if let Ok(mut cache) = state.replay.lock() { cache.complete(key, response.clone()); }
                        // Store before send: a lost reply must not repeat a mutation.
                        let _ = tokio::time::timeout(state.config.idle_timeout, sender.send_to(&response, peer)).await;
                    }
                });
            }
        }
    }
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result { warn!(%error, "UDP worker failed during shutdown"); }
    }
    failure.map_or(Ok(()), Err)
}

fn generic_error(xid: u32, status: accept_stat_data) -> Vec<u8> {
    let message = rpc::rpc_msg {
        xid,
        body: rpc::msg_body::REPLY(rpc::reply_body::MSG_ACCEPTED(rpc::accepted_reply {
            verf: rpc::opaque_auth::default(), reply_data: status,
        })),
    };
    let mut data = Vec::new();
    // Fixed-size header serialized to an in-memory Vec cannot fail.
    let _ = message.pack(&mut data);
    data
}

async fn dispatch_record<T: NfsFileSystem + 'static>(
    shared: Arc<Shared<T>>, peer: SocketAddr, service: Service, udp: bool,
    record: Vec<u8>,
) -> Option<Vec<u8>> {
    if record.len() < 8 || record[4..8] != [0, 0, 0, 0] { return None; }
    let xid = u32::from_be_bytes(record[..4].try_into().ok()?);
    let result = dispatch(shared.clone(), peer, service, udp, record).await;
    let max_reply = if udp { MAX_UDP_BYTES } else { shared.config.max_rpc_record };
    match result {
        Ok(HandleResult::Reply(response)) => {
            let response = response.into_inner();
            if response.len() <= max_reply { Some(response) }
            else {
                warn!(%peer, xid, "RPC response exceeded transport limit");
                Some(generic_error(xid, accept_stat_data::SYSTEM_ERR))
            }
        }
        Ok(HandleResult::NoReply) => None,
        Err(error) => {
            debug!(%peer, xid, %error, "Rejected malformed RPC request");
            Some(generic_error(xid, accept_stat_data::GARBAGE_ARGS))
        }
    }
}

async fn dispatch<T: NfsFileSystem + 'static>(
    shared: Arc<Shared<T>>, peer: SocketAddr, service: Service, udp: bool,
    record: Vec<u8>,
) -> anyhow::Result<HandleResult> {
    let message = IncomingRpcMessage::try_from(CompleteRpcMessage::from_bytes(record))?;
    let call = message.body();
    if call.rpcvers != rpc::RPC_VERSION_2 { return message.into_rpc_mismatch(); }
    if call.cred.body.len() > 400 { return message.into_auth_error(auth_stat::AUTH_BADCRED); }
    if call.verf.body.len() > 400 || call.verf.flavor != auth_flavor::AUTH_NULL || !call.verf.body.is_empty() {
        return message.into_auth_error(auth_stat::AUTH_BADVERF);
    }
    let auth = match call.cred.flavor {
        auth_flavor::AUTH_NULL => {
            if !call.cred.body.is_empty() { return message.into_auth_error(auth_stat::AUTH_BADCRED); }
            auth_unix::default()
        }
        auth_flavor::AUTH_UNIX => {
            let mut cursor = Cursor::new(call.cred.body.as_ref());
            let (auth, _) = match auth_unix::unpack(&mut cursor) {
                Ok(auth) => auth,
                Err(_) => return message.into_auth_error(auth_stat::AUTH_BADCRED),
            };
            if cursor.position() as usize != call.cred.body.len() || auth.gids.len() > 16
                || auth.machinename.len() > 255 {
                return message.into_auth_error(auth_stat::AUTH_BADCRED);
            }
            auth
        }
        _ => return message.into_auth_error(auth_stat::AUTH_TOOWEAK),
    };
    if call.prog != service.program() { return message.into_error_reply(accept_stat_data::PROG_UNAVAIL); }
    let context = RPCContext {
        local_port: service.port(shared.ports),
        max_io_bytes: shared.config.max_io_bytes,
        max_reply_bytes: if udp { MAX_UDP_BYTES } else { shared.config.max_rpc_record },
        client_addr: peer.to_string(), auth, vfs: shared.fs.clone(), mount_signal: None,
        export_name: Arc::new("/".into()), transaction_tracker: shared.tracker.clone(),
        file_handle_converter: shared.converter,
    };
    match service {
        Service::Nfs => crate::nfs_handlers::handle_nfs(context, message).await,
        Service::Mount => dispatch_mount(shared, peer, message).await,
        Service::Portmap => dispatch_portmap(&shared, message),
    }
}

fn unpack_args<T: Unpack>(message: &mut IncomingRpcMessage) -> anyhow::Result<T> {
    let mut data = message.take_data();
    let (value, _) = T::unpack(&mut data)?;
    if data.position() != data.get_ref().len() as u64 { anyhow::bail!("Trailing RPC arguments"); }
    Ok(value)
}

fn portmap_entries<T: NfsFileSystem>(shared: &Shared<T>) -> Vec<portmap::mapping> {
    let mut entries = Vec::new();
    for (protocol, enabled) in [(portmap::IPPROTO_TCP, shared.config.enable_tcp), (portmap::IPPROTO_UDP, shared.config.enable_udp)] {
        if !enabled { continue; }
        for service in [Service::Portmap, Service::Mount, Service::Nfs] {
            entries.push(portmap::mapping {
                prog: service.program(), vers: if service == Service::Portmap { 2 } else { 3 },
                prot: protocol, port: u32::from(service.port(shared.ports)),
            });
        }
    }
    entries
}

fn dispatch_portmap<T: NfsFileSystem>(shared: &Shared<T>, mut message: IncomingRpcMessage) -> anyhow::Result<HandleResult> {
    if message.body().vers != portmap::VERSION {
        return message.into_error_reply(accept_stat_data::PROG_MISMATCH { low: 2, high: 2 });
    }
    match message.body().proc {
        0 => { unpack_args::<Void>(&mut message)?; message.into_success_reply(&Void) }
        1 | 2 => {
            unpack_args::<portmap::mapping>(&mut message)?;
            // This embedded table is static; remote registration is forbidden.
            message.into_success_reply(&false)
        }
        3 => {
            let query = unpack_args::<portmap::mapping>(&mut message)?;
            let port = portmap_entries(shared).iter().find(|entry| entry.prog == query.prog
                && entry.vers == query.vers && entry.prot == query.prot).map_or(0, |entry| entry.port);
            message.into_success_reply(&port)
        }
        4 => { unpack_args::<Void>(&mut message)?; message.into_success_reply(&List(portmap_entries(shared))) }
        _ => message.into_error_reply(accept_stat_data::PROC_UNAVAIL),
    }
}

async fn dispatch_mount<T: NfsFileSystem + 'static>(
    shared: Arc<Shared<T>>, peer: SocketAddr, mut message: IncomingRpcMessage,
) -> anyhow::Result<HandleResult> {
    if message.body().vers != mount::VERSION {
        return message.into_error_reply(accept_stat_data::PROG_MISMATCH { low: 3, high: 3 });
    }
    match message.body().proc {
        0 => { unpack_args::<Void>(&mut message)?; message.into_success_reply(&Void) }
        1 => {
            let arg = unpack_args::<mount::dirpath<'static>>(&mut message)?;
            let response = mount_path(&shared, peer, &arg.0).await;
            message.into_success_reply(&response)
        }
        2 => {
            unpack_args::<Void>(&mut message)?;
            let mounts = shared.mounts.lock().map_err(|_| anyhow::anyhow!("Mount list poisoned"))?;
            let list: mount::mountlist<'static, 'static> = List(mounts.iter().map(|(ip, path)| mount::mountbody {
                ml_hostname: mount::name(Opaque::owned(ip.to_string().into_bytes())),
                ml_directory: mount::dirpath(Opaque::owned(path.as_bytes().to_vec())),
            }).collect());
            message.into_success_reply(&list)
        }
        3 => {
            let arg = unpack_args::<mount::dirpath<'static>>(&mut message)?;
            if let Ok(text) = std::str::from_utf8(&arg.0) {
                if let Some(path) = normalize_mount_path(text) {
                    shared.mounts.lock().map_err(|_| anyhow::anyhow!("Mount list poisoned"))?.remove(&(peer.ip(), path));
                }
            }
            message.into_success_reply(&Void)
        }
        4 => {
            unpack_args::<Void>(&mut message)?;
            shared.mounts.lock().map_err(|_| anyhow::anyhow!("Mount list poisoned"))?.retain(|(ip, _)| *ip != peer.ip());
            message.into_success_reply(&Void)
        }
        5 => {
            unpack_args::<Void>(&mut message)?;
            let exports: mount::exports<'static, 'static> = List(shared.config.exports.iter().map(|path| mount::export_node {
                ex_dir: mount::dirpath(Opaque::owned(path.as_bytes().to_vec())), ex_groups: List::default(),
            }).collect());
            message.into_success_reply(&exports)
        }
        _ => message.into_error_reply(accept_stat_data::PROC_UNAVAIL),
    }
}

async fn mount_path<T: NfsFileSystem>(shared: &Shared<T>, peer: SocketAddr, bytes: &[u8]) -> mount::mountres3<'static> {
    use mount::{mountres3, mountstat3};
    let path = match std::str::from_utf8(bytes).ok().and_then(normalize_mount_path) {
        Some(path) => path,
        None => return mountres3::Err(mountstat3::MNT3ERR_INVAL),
    };
    if !is_exported(&path, &shared.config.exports) { return mountres3::Err(mountstat3::MNT3ERR_ACCES); }
    let id = match shared.fs.lookup_by_path(&path).await {
        Ok(id) => id,
        Err(_) => return mountres3::Err(mountstat3::MNT3ERR_NOENT),
    };
    match shared.fs.getattr(&id).await {
        Ok(attr) if attr.type_ == nfs3::ftype3::NF3DIR => {},
        Ok(_) => return mountres3::Err(mountstat3::MNT3ERR_NOTDIR),
        Err(_) => return mountres3::Err(mountstat3::MNT3ERR_ACCES),
    }
    let Ok(mut mounts) = shared.mounts.lock() else { return mountres3::Err(mountstat3::MNT3ERR_SERVERFAULT); };
    let entry = (peer.ip(), path);
    if !mounts.contains(&entry) {
        let byte_count: usize = mounts.iter().map(|(_, path)| path.len() + 64).sum();
        if mounts.len() >= MAX_MOUNTS || byte_count + entry.1.len() + 64 > MAX_MOUNT_BYTES {
            return mountres3::Err(mountstat3::MNT3ERR_SERVERFAULT);
        }
        mounts.insert(entry);
    }
    mountres3::Ok(mount::mountres3_ok {
        fhandle: mount::fhandle3(shared.converter.fh_to_nfs(&id).data),
        auth_flavors: vec![auth_flavor::AUTH_UNIX as u32, auth_flavor::AUTH_NULL as u32],
    })
}

#[cfg(test)]
mod tests;
