//! Control role (B) and data role (A) in one process, connected through a
//! Unix socketpair that drops and corrupts some frames. No control server
//! and no network: map responses are injected from fixtures, DERP nodes are
//! rewritten to 127.0.0.1 and every outbound TCP/TLS dial fails at once.

use splitlink::link::std_spawner as link_spawner;
use splitlink::{Link, LinkConfig, LinkRead};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tsnode::dataplane::{DataPlane, LocalConfig, LocalDataPlane};
use tsnode::keys::{fmt_disco_pub, fmt_node_pub, KeyPair};
use tsnode::net::{Conn, Dialer};
use tsnode::node::{Node as TsNode, NodeConfig, Store, KEY_MACHINE, KEY_NODE};
use tsnode::split::{DataServer, DataServerConfig, RemoteDataPlane};
use tsnode::types::*;

#[derive(Default, Clone)]
struct MemStore(Arc<Mutex<HashMap<String, String>>>);
impl Store for MemStore {
    fn get(&self, k: &str) -> Option<String> {
        self.0.lock().unwrap().get(k).cloned()
    }
    fn set(&self, k: &str, v: &str) -> io::Result<()> {
        self.0.lock().unwrap().insert(k.into(), v.into());
        Ok(())
    }
    fn remove(&self, k: &str) -> io::Result<()> {
        self.0.lock().unwrap().remove(k);
        Ok(())
    }
}

struct NoNet;
impl Dialer for NoNet {
    fn dial_tcp(&self, _: &str, _: u16) -> io::Result<Box<dyn Conn>> {
        Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test: no network"))
    }
    fn dial_tls(&self, _: &str, _: Option<std::net::IpAddr>, _: u16) -> io::Result<Box<dyn Conn>> {
        Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test: no network"))
    }
}

struct SockRead(UnixStream);
impl LinkRead for SockRead {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.0.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match self.0.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(0),
            Err(e) => Err(e),
        }
    }
}

/// Drops ~3 % and corrupts ~2 % of frames.
struct Lossy {
    s: UnixStream,
    rng: u64,
}
impl Write for Lossy {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let r = self.rng % 100;
        if r < 3 {
            return Ok(buf.len());
        }
        let mut b = buf.to_vec();
        if r < 5 && b.len() > 4 {
            let i = (self.rng as usize >> 8) % (b.len() - 1);
            b[i] ^= 0x41;
        }
        self.s.write_all(&b)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn link_cfg() -> LinkConfig {
    LinkConfig {
        ack_timeout: Duration::from_millis(20),
        max_retries: 50,
        keepalive: Duration::from_millis(200),
        peer_timeout: Duration::from_millis(1000),
        ..Default::default()
    }
}

fn link(sock: &UnixStream, seed: u64) -> (Link, std::sync::mpsc::Receiver<splitlink::Event>) {
    Link::start(
        Box::new(SockRead(sock.try_clone().unwrap())),
        Box::new(Lossy { s: sock.try_clone().unwrap(), rng: seed }),
        link_cfg(),
        splitlink::link::random_epoch(),
        &link_spawner(),
    )
    .unwrap()
}

fn start_a(sock: &UnixStream, store: &MemStore) -> DataServer {
    let dp = LocalDataPlane::start(
        LocalConfig {
            udp_port: 0,
            local_ip: Some("127.0.0.1".parse().unwrap()),
            worker_stack: 256 * 1024,
            spawner: tsnode::net::std_spawner(),
        },
        Arc::new(store.clone()),
        Arc::new(NoNet),
    )
    .unwrap();
    let (l, ev) = link(sock, 0xA5A5_1234);
    DataServer::start(
        l,
        ev,
        dp,
        DataServerConfig {
            max_payload: link_cfg().max_payload,
            status_period: Duration::from_millis(500),
            has_device_config: false,
            stack: 512 * 1024,
        },
        None,
        &link_spawner(),
    )
    .unwrap()
}

fn start_b(sock: &UnixStream, store: &MemStore) -> (TsNode, Arc<RemoteDataPlane>) {
    let (l, ev) = link(sock, 0x5A5A_4321);
    let rdp = RemoteDataPlane::start(l, ev, link_cfg().max_payload, &link_spawner(), 512 * 1024).unwrap();
    let node = TsNode::start_with_dataplane(
        NodeConfig {
            control_url: "http://127.0.0.1:9".into(),
            hostname: "split-test".into(),
            udp_port: 0,
            ..Default::default()
        },
        Arc::new(store.clone()),
        Arc::new(NoNet),
        rdp.clone(),
    )
    .unwrap();
    (node, rdp)
}

fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn derp_map() -> DerpMap {
    let mut m: DerpMap =
        serde_json::from_str(include_str!("fixtures/derpmap-default.json")).unwrap();
    for r in m.regions.values_mut() {
        for n in &mut r.nodes {
            n.host_name = "127.0.0.1".into();
            n.ipv4 = "127.0.0.1".into();
            n.ipv6 = String::new();
            n.stun_port = 9;
        }
    }
    m
}

fn peer(i: i64) -> Node {
    let k = KeyPair::generate();
    let d = KeyPair::generate();
    Node {
        id: 1000 + i,
        name: format!("peer{i:02}.example.ts.net."),
        key: fmt_node_pub(k.public()),
        disco_key: fmt_disco_pub(d.public()),
        addresses: vec![format!("100.64.1.{i}/32")],
        allowed_ips: vec![format!("100.64.1.{i}/32")],
        endpoints: vec![format!("192.0.2.{i}:41641")],
        home_derp: 7,
        machine_authorized: true,
        ..Default::default()
    }
}

fn full_map(peers: &[Node]) -> MapResponse {
    MapResponse {
        node: Some(Node {
            name: "split-test.example.ts.net.".into(),
            addresses: vec!["100.64.0.5/32".into(), "fd7a:115c:a1e0::5/128".into()],
            ..Default::default()
        }),
        derp_map: Some(derp_map()),
        peers: Some(peers.to_vec()),
        packet_filter: Some(vec![FilterRule {
            src_ips: vec!["100.64.1.0/24".into()],
            dst_ports: vec![NetPortRange { ip: "*".into(), ports: PortRange { first: 35930, last: 35937 } }],
            ip_proto: vec![6],
        }]),
        control_time: Some("2026-10-10T00:00:00Z".into()),
        ..Default::default()
    }
}

fn in_sync(a: &DataServer, b: &RemoteDataPlane) -> bool {
    b.generation() != 0 && a.applied().gen == b.generation()
}

#[test]
fn config_propagates_and_survives_reboots() {
    let _ = env_logger::builder().is_test(true).try_init();
    let (sa, sb) = UnixStream::pair().unwrap();
    let store_a = MemStore::default();
    let store_b = MemStore::default();

    // --- Bring-up: B learns A's public keys; private node key stays on A.
    let a = start_a(&sa, &store_a);
    let (node_b, rdp) = start_b(&sb, &store_b);
    wait("data chip status", 10, || rdp.ready());
    let a_pub = a.dataplane().node_public();
    assert_eq!(rdp.node_public(), a_pub);
    assert_eq!(rdp.disco_public(), a.dataplane().disco_public());
    assert!(store_b.get(KEY_MACHINE).is_some());
    assert!(store_b.get(KEY_NODE).is_none(), "node key must not exist on the control chip");
    assert!(store_a.get(KEY_NODE).is_some());
    assert!(!rdp.local_endpoints().is_empty());

    // --- Full map -> snapshot.
    let peers: Vec<Node> = (1..=20).map(peer).collect();
    let t = Instant::now();
    node_b.inject_map_response(full_map(&peers));
    wait("snapshot applied on A", 15, || in_sync(&a, &rdp));
    let ap = a.applied();
    println!("snapshot gen {} applied in {:?}: {ap:?}", ap.gen, t.elapsed());
    assert_eq!(ap.peers, 20);
    assert_eq!(ap.regions, derp_map().regions.len());
    assert_eq!(ap.filter_rules, 1);
    assert_eq!(ap.addresses.len(), 2);
    assert_eq!(a.dataplane().magicsock().peer_status().len(), 20);
    assert!(ap.home_derp != 0, "home DERP chosen (fallback region)");
    wait("peer status mirrored on B", 10, || node_b.status().peers.len() == 20);
    assert_eq!(node_b.status().addresses.len(), 2);
    let snaps = ap.snapshots;

    // --- Deltas: upsert, remove, patch; no new snapshot.
    let mut changed = peers[0].clone();
    changed.endpoints = vec!["192.0.2.200:41641".into()];
    let delta = MapResponse {
        peers_changed: Some(vec![changed, peer(21)]),
        peers_removed: Some(vec![peers[1].id, peers[2].id]),
        peers_changed_patch: Some(vec![PeerChange {
            node_id: peers[3].id,
            derp_region: 9,
            endpoints: Some(vec!["198.51.100.4:41641".into()]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    node_b.inject_map_response(delta);
    wait("deltas applied on A", 10, || in_sync(&a, &rdp));
    let ap = a.applied();
    assert_eq!(ap.snapshots, snaps, "deltas must not trigger a snapshot");
    assert!(ap.deltas >= 3);
    assert_eq!(a.dataplane().magicsock().peer_status().len(), 19);
    let p4 = a
        .dataplane()
        .magicsock()
        .peer_status()
        .into_iter()
        .find(|p| p.name.starts_with("peer04"))
        .unwrap();
    assert_eq!(p4.home_derp, 9);

    // --- Node key rotation goes through A; B never sees the private key.
    let new_pub = rdp.propose_node_key().unwrap();
    assert_ne!(new_pub, a_pub);
    rdp.commit_node_key(new_pub).unwrap();
    wait("rotated key reported", 10, || rdp.node_public() == new_pub);
    assert_eq!(a.dataplane().node_public(), new_pub);
    assert_eq!(
        KeyPair::from_secret_hex(&store_a.get(KEY_NODE).unwrap()).unwrap().public(),
        &new_pub
    );
    assert!(store_b.get(KEY_NODE).is_none());

    // --- Control chip reboots: A keeps running on its configuration.
    let gen_before = a.applied().gen;
    node_b.shutdown();
    std::thread::sleep(Duration::from_millis(1800)); // > peer_timeout
    // (Frames A retransmitted meanwhile sit in the socket buffer and show up
    // as duplicates on the new B; on a real UART they are simply lost.)
    assert!(!a.link().is_up());
    assert_eq!(a.applied().gen, gen_before);
    assert_eq!(a.dataplane().magicsock().peer_status().len(), 19);
    let (node_b, rdp) = start_b(&sb, &store_b);
    wait("data chip status after B reboot", 10, || rdp.ready());
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(a.applied().gen, gen_before, "B without a map must not wipe A");
    // B gets a fresh map from control: A is brought to it in one snapshot.
    let peers2: Vec<Node> = (1..=12).map(peer).collect();
    node_b.inject_map_response(full_map(&peers2));
    wait("resync after B reboot", 15, || in_sync(&a, &rdp));
    assert_eq!(a.applied().peers, 12);
    assert_eq!(a.dataplane().magicsock().peer_status().len(), 12);

    // --- Data chip reboots: same node key from its store; B resends.
    let key_before = a.dataplane().node_public();
    println!("before A reboot: link A1 {:?}\n  link B2 {:?}", a.link().stats(), rdp.link().stats());
    a.shutdown();
    a.dataplane().shutdown();
    std::thread::sleep(Duration::from_millis(300));
    let a = start_a(&sa, &store_a);
    assert_eq!(a.applied().gen, 0);
    wait("resync after A reboot", 15, || in_sync(&a, &rdp));
    assert_eq!(a.dataplane().node_public(), key_before);
    assert_eq!(a.applied().peers, 12);
    assert_eq!(a.dataplane().magicsock().peer_status().len(), 12);
    println!("link B: {:?}", rdp.link().stats());
    println!("link A: {:?}", a.link().stats());

    node_b.shutdown();
    a.shutdown();
    a.dataplane().shutdown();
}
