#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::vfs::{FileHandleU64, NfsReadFileSystem, ReadDirPlusIterator};
use crate::vfs::adapters::ReadOnlyAdapter;
use nfs3_types::nfs3::*;

struct TestFs { reads: Arc<AtomicUsize> }

fn attrs(id: u64) -> fattr3 {
    fattr3 {
        type_: if id == 3 { ftype3::NF3REG } else { ftype3::NF3DIR },
        mode: 0o755, nlink: 1, uid: 0, gid: 0, size: 10, used: 10,
        rdev: specdata3 { specdata1: 0, specdata2: 0 }, fsid: 1, fileid: id,
        atime: nfstime3 { seconds: 1, nseconds: 0 },
        mtime: nfstime3 { seconds: 1, nseconds: 0 },
        ctime: nfstime3 { seconds: 1, nseconds: 0 },
    }
}

struct EmptyDir;
impl ReadDirPlusIterator<FileHandleU64> for EmptyDir {
    async fn next(&mut self) -> crate::vfs::NextResult<crate::vfs::DirEntryPlus<FileHandleU64>> {
        crate::vfs::NextResult::Eof
    }
}

impl NfsReadFileSystem for TestFs {
    type Handle = FileHandleU64;
    fn root_dir(&self) -> Self::Handle { 1.into() }
    async fn lookup(&self, id: &Self::Handle, name: &filename3<'_>) -> Result<Self::Handle, nfsstat3> {
        match (id.as_u64(), name.as_ref()) {
            (1, b"share") | (1, b"data") => Ok(2.into()),
            (2, b"file") => Ok(3.into()),
            _ => Err(nfsstat3::NFS3ERR_NOENT),
        }
    }
    async fn getattr(&self, id: &Self::Handle) -> Result<fattr3, nfsstat3> { Ok(attrs(id.as_u64())) }
    async fn read(&self, _: &Self::Handle, offset: u64, _: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        Ok((vec![offset as u8], true))
    }
    async fn readdirplus(&self, _: &Self::Handle, cookie: u64) -> Result<impl ReadDirPlusIterator<Self::Handle>, nfsstat3> {
        if cookie != 0 { return Err(nfsstat3::NFS3ERR_BAD_COOKIE); }
        Ok(EmptyDir)
    }
    async fn readlink(&self, _: &Self::Handle) -> Result<nfspath3<'_>, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

fn test_config() -> ServerConfig {
    ServerConfig {
        portmap_port: 0, nfs_port: 0, mount_port: 0,
        exports: vec!["/share".into(), "/data".into()],
        ..ServerConfig::default()
    }
}

async fn server(config: ServerConfig) -> (ServerHandle, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let fs = Arc::new(ReadOnlyAdapter::new(TestFs { reads: reads.clone() }));
    (start(fs, config).await.unwrap(), reads)
}

fn request(xid: u32, program: u32, version: u32, procedure: u32, args: &impl Pack) -> Vec<u8> {
    let call = rpc::rpc_msg {
        xid, body: rpc::msg_body::CALL(rpc::call_body {
            rpcvers: 2, prog: program, vers: version, proc: procedure,
            cred: rpc::opaque_auth::default(), verf: rpc::opaque_auth::default(),
        }),
    };
    let mut record = Vec::new();
    call.pack(&mut record).unwrap();
    args.pack(&mut record).unwrap();
    record
}

fn body<T: Unpack>(response: &[u8]) -> T {
    let mut cursor = Cursor::new(response);
    let (header, _) = rpc::rpc_msg::unpack(&mut cursor).unwrap();
    assert!(matches!(header.body, rpc::msg_body::REPLY(rpc::reply_body::MSG_ACCEPTED(
        rpc::accepted_reply { reply_data: accept_stat_data::SUCCESS, .. }
    ))));
    let value = T::unpack(&mut cursor).unwrap().0;
    assert_eq!(cursor.position() as usize, response.len());
    value
}

async fn udp_rpc(socket: &UdpSocket, port: u16, record: &[u8]) -> Vec<u8> {
    socket.send_to(record, (Ipv4Addr::LOCALHOST, port)).await.unwrap();
    let mut response = vec![0; 65536];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut response)).await.unwrap().unwrap();
    response.truncate(length);
    response
}

async fn tcp_rpc(port: u16, record: &[u8]) -> Vec<u8> {
    let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
    // Exercise nontrivial TCP record fragmentation.
    socket.write_u32(12).await.unwrap();
    socket.write_all(&record[..12]).await.unwrap();
    socket.write_u32(0x8000_0000 | (record.len() - 12) as u32).await.unwrap();
    socket.write_all(&record[12..]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), read_record(&mut socket, MAX_RPC_RECORD_BYTES, 4)).await.unwrap().unwrap()
}

#[test]
fn mount_boundaries_are_component_based() {
    let exports = vec!["/share".into(), "/data".into()];
    assert!(is_exported("/share", &exports));
    assert!(is_exported("/share/subdir", &exports));
    assert!(!is_exported("/share-evil", &exports));
    assert!(!is_exported("/", &exports));
    for path in ["/share/../data", "/share/./file", "/share\\file", "/C:/file", "relative", "/share\0"] {
        assert!(normalize_mount_path(path).is_none(), "{path:?}");
    }
}

#[test]
fn replay_cache_uses_full_request_and_bounded_lru() {
    let mut config = test_config();
    config.udp_cache_entries = 2;
    let mut cache = ReplayCache::new(&config);
    let key = |bytes: &'static [u8]| RequestKey { peer: "127.0.0.1:40000".parse().unwrap(), service: Service::Nfs, request: Arc::from(bytes) };
    let one = key(b"same xid, args A");
    let two = key(b"same xid, args B");
    let three = key(b"same xid, args C");
    cache.complete(one.clone(), Arc::from(&b"reply A"[..]));
    cache.complete(two.clone(), Arc::from(&b"reply B"[..]));
    assert_eq!(&*cache.lookup(&one).unwrap(), b"reply A");
    cache.complete(three.clone(), Arc::from(&b"reply C"[..]));
    assert!(cache.lookup(&two).is_none());
    assert!(cache.lookup(&one).is_some());
    assert!(cache.lookup(&three).is_some());
    assert_eq!(cache.entries.len(), 2);
    let different_peer = RequestKey { peer: "127.0.0.1:40001".parse().unwrap(), ..one };
    assert!(cache.lookup(&different_peer).is_none());
    assert!(cache.bytes <= cache.byte_limit);
}

#[tokio::test]
async fn record_bounds_reject_length_before_body_allocation() {
    let bytes = (0x8000_0000_u32 | (MAX_RPC_RECORD_BYTES as u32 + 1)).to_be_bytes();
    assert!(read_record(&mut &bytes[..], MAX_RPC_RECORD_BYTES, 256).await.is_err());
    let empty_fragments = [0_u8; 16];
    assert!(read_record(&mut &empty_fragments[..], 1024, 3).await.is_err());
    let mut fragments = Vec::new();
    fragments.extend_from_slice(&3_u32.to_be_bytes());
    fragments.extend_from_slice(b"abc");
    fragments.extend_from_slice(&0x8000_0003_u32.to_be_bytes());
    fragments.extend_from_slice(b"def");
    assert_eq!(read_record(&mut &fragments[..], 16, 2).await.unwrap(), b"abcdef");
}

#[tokio::test]
async fn portmap_and_mount_tcp_udp_are_consistent() {
    let (handle, _) = server(test_config()).await;
    let ports = handle.ports();
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    for (protocol, version, expected) in [(6, 3, u32::from(ports.nfs_port)), (17, 3, u32::from(ports.nfs_port)), (6, 2, 0), (7, 3, 0)] {
        let query = portmap::mapping { prog: nfs3::PROGRAM, vers: version, prot: protocol, port: 0 };
        let request = request(protocol + version, portmap::PROGRAM, 2, 3, &query);
        let reply = udp_rpc(&socket, ports.portmap_port, &request).await;
        assert_eq!(body::<u32>(&reply), expected);
        assert_eq!(body::<u32>(&tcp_rpc(ports.portmap_port, &request).await), expected);
    }
    let dump = request(20, portmap::PROGRAM, 2, 4, &Void);
    let entries: portmap::pmaplist = body(&udp_rpc(&socket, ports.portmap_port, &dump).await);
    assert_eq!(entries.0.len(), 6);
    let exports = request(21, mount::PROGRAM, 3, 5, &Void);
    let list: mount::exports = body(&tcp_rpc(ports.mount_port, &exports).await);
    assert_eq!(list.0.len(), 2);
    for (index, path) in ["/share-evil", "/share/../data", "/share/file"].iter().enumerate() {
        let args = mount::dirpath(Opaque::owned(path.as_bytes().to_vec()));
        let record = request(30 + index as u32, mount::PROGRAM, 3, 1, &args);
        let result: mount::mountres3 = body(&udp_rpc(&socket, ports.mount_port, &record).await);
        assert!(matches!(result, mount::mountres3::Err(_)));
    }
    let args = mount::dirpath(Opaque::owned(b"/share".to_vec()));
    let record = request(40, mount::PROGRAM, 3, 1, &args);
    let result: mount::mountres3 = body(&udp_rpc(&socket, ports.mount_port, &record).await);
    assert!(matches!(result, mount::mountres3::Ok(_)));
    let dump = request(41, mount::PROGRAM, 3, 2, &Void);
    assert_eq!(body::<mount::mountlist>(&udp_rpc(&socket, ports.mount_port, &dump).await).0.len(), 1);
    let umnt = request(42, mount::PROGRAM, 3, 3, &args);
    body::<Void>(&udp_rpc(&socket, ports.mount_port, &umnt).await);
    let dump = request(43, mount::PROGRAM, 3, 2, &Void);
    assert!(body::<mount::mountlist>(&udp_rpc(&socket, ports.mount_port, &dump).await).0.is_empty());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn udp_replays_completed_response_and_distinguishes_same_xid_arguments() {
    let (handle, reads) = server(test_config()).await;
    let ports = handle.ports();
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let args = mount::dirpath(Opaque::owned(b"/share".to_vec()));
    let mount_request = request(1, mount::PROGRAM, 3, 1, &args);
    let result: mount::mountres3 = body(&udp_rpc(&socket, ports.mount_port, &mount_request).await);
    let mount::mountres3::Ok(mounted) = result else { panic!("mount failed"); };
    let handle_data = mounted.fhandle.0.into_owned();
    let read = |offset| request(100, nfs3::PROGRAM, 3, 6, &READ3args {
        file: nfs_fh3 { data: Opaque::owned(handle_data.clone()) }, offset, count: 1,
    });
    let first = read(0);
    socket.send_to(&first, (Ipv4Addr::LOCALHOST, ports.nfs_port)).await.unwrap();
    socket.send_to(&first, (Ipv4Addr::LOCALHOST, ports.nfs_port)).await.unwrap();
    let mut response = [0_u8; 1024];
    let length = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut response)).await.unwrap().unwrap();
    let first_reply = response[..length].to_vec();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(udp_rpc(&socket, ports.nfs_port, &first).await, first_reply);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    let second_reply = udp_rpc(&socket, ports.nfs_port, &read(1)).await;
    assert_ne!(second_reply, first_reply);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_xdr_and_oversize_udp_do_not_kill_listener() {
    let (handle, _) = server(test_config()).await;
    let port = handle.ports().portmap_port;
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let mut bad = request(1, portmap::PROGRAM, 2, 0, &Void);
    bad[28..32].copy_from_slice(&u32::MAX.to_be_bytes());
    let reply = udp_rpc(&socket, port, &bad).await;
    assert_eq!(u32::from_be_bytes(reply[20..24].try_into().unwrap()), 4);
    socket.send_to(&vec![0; MAX_UDP_BYTES + 1], (Ipv4Addr::LOCALHOST, port)).await.unwrap();
    let good = request(2, portmap::PROGRAM, 2, 0, &Void);
    body::<Void>(&udp_rpc(&socket, port, &good).await);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn bind_conflicts_identify_endpoint_and_release_partial_listeners() {
    // Exercise each TCP/UDP conflict with real occupied sockets. In particular,
    // failures late in startup must release the sockets opened beforehand.
    for (blocked_index, label) in [(0, "Portmapper"), (1, "MOUNT"), (2, "NFS")] {
        for protocol in ["TCP", "UDP"] {
            let mut reserved = Vec::new();
            for _ in 0..3 {
                let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
                let port = tcp.local_addr().unwrap().port();
                let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await.unwrap();
                reserved.push((port, tcp, udp));
            }
            let ports: Vec<_> = reserved.iter().map(|(port, _, _)| *port).collect();
            let mut blocker_tcp = None;
            let mut blocker_udp = None;
            for (index, (_, tcp, udp)) in reserved.into_iter().enumerate() {
                if index == blocked_index {
                    if protocol == "TCP" { blocker_tcp = Some(tcp); }
                    else { blocker_udp = Some(udp); }
                }
            }
            let mut config = test_config();
            config.portmap_port = ports[0];
            config.mount_port = ports[1];
            config.nfs_port = ports[2];
            let fs = Arc::new(ReadOnlyAdapter::new(TestFs { reads: Arc::new(AtomicUsize::new(0)) }));
            let error = match start(fs, config).await {
                Err(error) => error,
                Ok(handle) => { handle.shutdown().await.unwrap(); panic!("occupied port must reject startup"); }
            };
            assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
            let message = error.to_string();
            assert!(message.contains(&format!("{label} {protocol} 127.0.0.1:{}", ports[blocked_index])), "{message}");
            assert!(message.contains("端口已被占用"), "{message}");
            // Keep the deliberate blocker, but every other socket must be free.
            for (index, port) in ports.iter().enumerate() {
                if index != blocked_index || protocol != "TCP" {
                    let _tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, *port)).await.unwrap();
                }
                if index != blocked_index || protocol != "UDP" {
                    let _udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, *port)).await.unwrap();
                }
            }
            drop((blocker_tcp, blocker_udp));
        }
    }
}

#[tokio::test]
async fn shutdown_interrupts_idle_connections_and_releases_all_ports() {
    let (handle, _) = server(test_config()).await;
    let ports = handle.ports();
    let _idle = TcpStream::connect((Ipv4Addr::LOCALHOST, ports.nfs_port)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle.shutdown()).await.unwrap().unwrap();
    for port in [ports.nfs_port, ports.mount_port, ports.portmap_port] {
        let _tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        let _udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await.unwrap();
    }
}

#[tokio::test]
async fn client_filter_applies_to_every_transport() {
    let mut config = test_config();
    config.allow_client = Arc::new(|_| false);
    let (handle, _) = server(config).await;
    let ports = handle.ports();
    for port in [ports.nfs_port, ports.mount_port, ports.portmap_port] {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.unwrap();
        let mut byte = [0];
        let result = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut byte)).await.unwrap();
        assert!(matches!(result, Ok(0) | Err(_)));
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        socket.send_to(&request(1, portmap::PROGRAM, 2, 0, &Void), (Ipv4Addr::LOCALHOST, port)).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), socket.recv(&mut byte)).await.is_err());
    }
    handle.shutdown().await.unwrap();
}
