//! Running the control plane and the data plane on two chips.
//!
//! - [`RemoteDataPlane`] (control chip "B") implements [`DataPlane`] by
//!   keeping a versioned copy of the configuration the data chip needs and
//!   shipping it over a [`splitlink::Link`]: a full snapshot when bulk state
//!   (addresses, DERP map, full peer list, ACL) changed or when the data
//!   chip reports a different generation, small deltas otherwise.
//! - [`DataServer`] (data chip "A") owns a [`LocalDataPlane`] (WireGuard,
//!   disco, DERP, ACL, smoltcp; the node private key never leaves it),
//!   applies what B sends and reports status back. When B goes away A keeps
//!   running on the last applied configuration.

use crate::dataplane::{DataPlane, LocalDataPlane, NodeIdMap};
use crate::keys::{fmt_disco_pub, fmt_node_pub, parse_prefixed, DISCO_PUB_PREFIX, NODE_PUB_PREFIX};
use crate::magicsock::PeerStatus;
use crate::types::{self, DerpMap, FilterRule, Node, PeerChange};
use log::{debug, info, warn};
use splitlink::msg::{self, Cidr, DeviceConfig, Item, Key, ToControl, ToData};
use splitlink::{Event, Link, Spawner};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Conversions between control-plane JSON types and compact link types.

fn parse_cidr(s: &str) -> Option<Cidr> {
    let (ip, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip.parse::<IpAddr>().ok()?, p.parse::<u8>().ok()?),
        None => {
            let ip = s.parse::<IpAddr>().ok()?;
            (ip, if ip.is_ipv4() { 32 } else { 128 })
        }
    };
    Some(Cidr { ip, prefix })
}

pub fn peer_to_link(n: &Node) -> Option<msg::Peer> {
    let key = parse_prefixed(NODE_PUB_PREFIX, &n.key)?;
    let mut allowed: Vec<Cidr> = n
        .addresses
        .iter()
        .chain(n.allowed_ips.iter())
        .filter_map(|c| parse_cidr(c))
        .collect();
    allowed.dedup();
    Some(msg::Peer {
        id: n.id,
        name: n.name.trim_end_matches('.').to_string(),
        key,
        disco: parse_prefixed(DISCO_PUB_PREFIX, &n.disco_key),
        endpoints: n.endpoints.iter().filter_map(|e| e.parse().ok()).collect(),
        allowed_ips: allowed,
        derp_region: n.home_derp_region().unwrap_or(0),
    })
}

pub fn peer_from_link(p: &msg::Peer) -> Node {
    Node {
        id: p.id,
        name: p.name.clone(),
        key: fmt_node_pub(&p.key),
        disco_key: p.disco.map(|d| fmt_disco_pub(&d)).unwrap_or_default(),
        allowed_ips: p.allowed_ips.iter().map(|c| format!("{}/{}", c.ip, c.prefix)).collect(),
        endpoints: p.endpoints.iter().map(|e| e.to_string()).collect(),
        home_derp: p.derp_region,
        machine_authorized: true,
        ..Default::default()
    }
}

pub fn region_to_link(r: &types::DerpRegion) -> msg::DerpRegion {
    msg::DerpRegion {
        id: r.region_id,
        code: r.region_code.clone(),
        avoid: r.avoid,
        nodes: r
            .nodes
            .iter()
            .map(|n| msg::DerpNode {
                name: n.name.clone(),
                host_name: n.host_name.clone(),
                cert_name: n.cert_name.clone(),
                ipv4: n.ipv4.clone(),
                ipv6: n.ipv6.clone(),
                stun_port: n.stun_port,
                stun_only: n.stun_only,
                derp_port: n.derp_port,
                can_port80: n.can_port80,
                insecure_for_tests: n.insecure_for_tests,
            })
            .collect(),
    }
}

pub fn map_from_link(regions: &[msg::DerpRegion]) -> DerpMap {
    let mut map = DerpMap::default();
    for r in regions {
        map.regions.insert(
            r.id.to_string(),
            types::DerpRegion {
                region_id: r.id,
                region_code: r.code.clone(),
                region_name: r.code.clone(),
                avoid: r.avoid,
                nodes: r
                    .nodes
                    .iter()
                    .map(|n| types::DerpNode {
                        name: n.name.clone(),
                        region_id: r.id,
                        host_name: n.host_name.clone(),
                        cert_name: n.cert_name.clone(),
                        ipv4: n.ipv4.clone(),
                        ipv6: n.ipv6.clone(),
                        stun_port: n.stun_port,
                        stun_only: n.stun_only,
                        derp_port: n.derp_port,
                        insecure_for_tests: n.insecure_for_tests,
                        can_port80: n.can_port80,
                    })
                    .collect(),
            },
        );
    }
    map
}

pub fn rule_to_link(r: &FilterRule) -> msg::FilterRule {
    msg::FilterRule {
        src_ips: r.src_ips.clone(),
        dst: r.dst_ports.iter().map(|d| (d.ip.clone(), d.ports.first, d.ports.last)).collect(),
        protos: r.ip_proto.clone(),
    }
}

pub fn rule_from_link(r: &msg::FilterRule) -> FilterRule {
    FilterRule {
        src_ips: r.src_ips.clone(),
        dst_ports: r
            .dst
            .iter()
            .map(|(ip, first, last)| types::NetPortRange {
                ip: ip.clone(),
                ports: types::PortRange { first: *first, last: *last },
            })
            .collect(),
        ip_proto: r.protos.clone(),
    }
}

fn patch_to_link(p: &PeerChange) -> msg::PeerPatch {
    msg::PeerPatch {
        id: p.node_id,
        derp_region: p.derp_region,
        endpoints: p
            .endpoints
            .as_ref()
            .map(|v| v.iter().filter_map(|e| e.parse().ok()).collect()),
        disco: p.disco_key.as_deref().and_then(|d| parse_prefixed(DISCO_PUB_PREFIX, d)),
    }
}

fn patch_from_link(p: &msg::PeerPatch) -> PeerChange {
    PeerChange {
        node_id: p.id,
        derp_region: p.derp_region,
        endpoints: p.endpoints.as_ref().map(|v| v.iter().map(|e| e.to_string()).collect()),
        disco_key: p.disco.map(|d| fmt_disco_pub(&d)),
        ..Default::default()
    }
}

fn is_gen_delta(m: &ToData) -> bool {
    matches!(
        m,
        ToData::PeersUpsert { .. } | ToData::PeersRemove { .. } | ToData::Patches { .. }
    )
}

// ---------------------------------------------------------------------------
// Control chip (B).

#[derive(Clone, Debug, Default)]
pub struct DataStatus {
    pub node_key: Key,
    pub disco_key: Key,
    pub endpoints: Vec<SocketAddr>,
    pub home_derp: i32,
    pub derp_connected: bool,
    pub derp_latency: Vec<(i32, u32)>,
    pub applied_gen: u32,
    pub has_device_config: bool,
}

#[derive(Default)]
struct RState {
    a: Option<DataStatus>,
    peer_status: Vec<msg::PeerStatus>,
    peer_status_building: Vec<msg::PeerStatus>,
    probes: HashMap<u32, Option<i32>>,
    proposals: HashMap<u32, Key>,

    gen: u32,
    addresses: Vec<Cidr>,
    home_derp: i32,
    regions: BTreeMap<i32, msg::DerpRegion>,
    peers: BTreeMap<i64, msg::Peer>,
    filter: Vec<msg::FilterRule>,
    full_dirty: bool,
    pending: Vec<ToData>,
    outbox: VecDeque<ToData>,
    need_snapshot: bool,
    snapshot_in_flight: bool,
    clock: Option<i32>,
    device: Option<DeviceConfig>,
    device_dirty: bool,
    snapshots_sent: u32,
}

impl RState {
    fn items(&self) -> Vec<Item> {
        let mut v = vec![Item::Addresses(self.addresses.clone()), Item::HomeDerp(self.home_derp)];
        v.extend(self.regions.values().cloned().map(Item::DerpRegion));
        v.push(Item::Peers(self.peers.values().cloned().collect()));
        v.push(Item::FilterRules(self.filter.clone()));
        v
    }
    fn has_gen_deltas_queued(&self) -> bool {
        self.outbox.iter().any(is_gen_delta)
    }
}

struct RInner {
    link: Link,
    max_payload: usize,
    st: Mutex<RState>,
    cv: Condvar,
    endpoints_cb: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    next_req: AtomicU32,
    shutdown: AtomicBool,
}

/// [`DataPlane`] whose implementation runs on the other chip.
pub struct RemoteDataPlane {
    inner: Arc<RInner>,
}

impl RemoteDataPlane {
    pub fn start(
        link: Link,
        events: Receiver<Event>,
        max_payload: usize,
        spawner: &Spawner,
        stack: usize,
    ) -> io::Result<Arc<RemoteDataPlane>> {
        let inner = Arc::new(RInner {
            link,
            max_payload,
            st: Mutex::new(RState::default()),
            cv: Condvar::new(),
            endpoints_cb: Mutex::new(None),
            next_req: AtomicU32::new(1),
            shutdown: AtomicBool::new(false),
        });
        let i = inner.clone();
        spawner("split-ev", stack, Box::new(move || i.event_loop(events)))?;
        let i = inner.clone();
        spawner("split-tx", stack, Box::new(move || i.tx_loop()))?;
        Ok(Arc::new(RemoteDataPlane { inner }))
    }

    /// Wi-Fi credentials etc. for the data chip; resent whenever it reports
    /// that it has none.
    pub fn set_device_config(&self, cfg: DeviceConfig) {
        let mut st = self.inner.st.lock().unwrap();
        st.device = Some(cfg);
        st.device_dirty = true;
        self.inner.cv.notify_all();
    }

    /// The data chip's last status report.
    pub fn data_status(&self) -> Option<DataStatus> {
        self.inner.st.lock().unwrap().a.clone()
    }

    /// Generation of the configuration held for the data chip.
    pub fn generation(&self) -> u32 {
        self.inner.st.lock().unwrap().gen
    }

    pub fn snapshots_sent(&self) -> u32 {
        self.inner.st.lock().unwrap().snapshots_sent
    }

    pub fn link(&self) -> &Link {
        &self.inner.link
    }

    fn modify(&self, f: impl FnOnce(&mut RState)) {
        let mut st = self.inner.st.lock().unwrap();
        f(&mut st);
    }

    fn command(&self, m: ToData) {
        let mut st = self.inner.st.lock().unwrap();
        st.outbox.push_back(m);
        self.inner.cv.notify_all();
    }

    fn request<T>(
        &self,
        m: impl FnOnce(u32) -> ToData,
        take: impl Fn(&mut RState, u32) -> Option<T>,
        timeout: Duration,
    ) -> Option<T> {
        let req = self.inner.next_req.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + timeout;
        let mut st = self.inner.st.lock().unwrap();
        st.outbox.push_back(m(req));
        self.inner.cv.notify_all();
        loop {
            if let Some(v) = take(&mut st, req) {
                return Some(v);
            }
            let now = Instant::now();
            if now >= deadline || self.inner.shutdown.load(Ordering::Relaxed) {
                return None;
            }
            st = self.inner.cv.wait_timeout(st, deadline - now).unwrap().0;
        }
    }
}

impl RInner {
    fn event_loop(self: Arc<Self>, events: Receiver<Event>) {
        while !self.shutdown.load(Ordering::Relaxed) {
            let ev = match events.recv_timeout(Duration::from_millis(500)) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match ev {
                Event::Up { peer_epoch, restarted } => {
                    info!("split: data chip up (epoch {peer_epoch:08x}, restarted {restarted})");
                    let mut st = self.st.lock().unwrap();
                    st.snapshot_in_flight = false;
                    self.cv.notify_all();
                }
                Event::Down => warn!("split: data chip not responding"),
                Event::Message(b) => match msg::decode::<ToControl>(&b) {
                    Some(m) => self.handle(m),
                    None => warn!("split: undecodable message ({} bytes)", b.len()),
                },
            }
        }
    }

    fn handle(&self, m: ToControl) {
        let mut notify_endpoints = false;
        {
            let mut st = self.st.lock().unwrap();
            match m {
                ToControl::Status {
                    node_key,
                    disco_key,
                    endpoints,
                    home_derp,
                    derp_connected,
                    derp_latency,
                    applied_gen,
                    has_device_config,
                } => {
                    if let Some(prev) = &st.a {
                        notify_endpoints = prev.endpoints != endpoints
                            || prev.home_derp != home_derp
                            || prev.node_key != node_key;
                    }
                    st.a = Some(DataStatus {
                        node_key,
                        disco_key,
                        endpoints,
                        home_derp,
                        derp_connected,
                        derp_latency,
                        applied_gen,
                        has_device_config,
                    });
                    if st.gen != 0
                        && applied_gen != st.gen
                        && !st.snapshot_in_flight
                        && !st.need_snapshot
                        && !st.has_gen_deltas_queued()
                    {
                        info!("split: data chip at gen {applied_gen}, have {}: resync", st.gen);
                        st.need_snapshot = true;
                    }
                    if !has_device_config && st.device.is_some() {
                        st.device_dirty = true;
                    }
                }
                ToControl::PeerStatus { peers, last } => {
                    st.peer_status_building.extend(peers);
                    if last {
                        st.peer_status = std::mem::take(&mut st.peer_status_building);
                    }
                }
                ToControl::ProbeResult { req, region } => {
                    st.probes.insert(req, region);
                }
                ToControl::NodeKeyProposal { req, key } => {
                    st.proposals.insert(req, key);
                }
                ToControl::RequestResync { applied_gen } => {
                    if st.gen != 0 && !st.snapshot_in_flight {
                        info!("split: data chip asked for resync (has gen {applied_gen})");
                        st.need_snapshot = true;
                    }
                }
            }
            self.cv.notify_all();
        }
        if notify_endpoints {
            if let Some(cb) = self.endpoints_cb.lock().unwrap().clone() {
                cb();
            }
        }
    }

    fn send(&self, m: &ToData) -> bool {
        match self.link.send(&msg::encode(m)) {
            Ok(()) => true,
            Err(e) => {
                debug!("split: send failed: {e:?}");
                false
            }
        }
    }

    fn tx_loop(self: Arc<Self>) {
        let mut last_heartbeat = Instant::now();
        while !self.shutdown.load(Ordering::Relaxed) {
            enum Work {
                Snapshot(Vec<ToData>),
                One(ToData),
                Heartbeat(u32),
            }
            let work = {
                let mut st = self.st.lock().unwrap();
                loop {
                    if self.shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    let up = self.link.is_up();
                    if up && st.device_dirty && st.device.is_some() {
                        st.device_dirty = false;
                        break Work::One(ToData::DeviceConfig(st.device.clone().unwrap()));
                    }
                    if up && st.need_snapshot && st.gen != 0 {
                        st.need_snapshot = false;
                        st.snapshot_in_flight = true;
                        st.snapshots_sent += 1;
                        st.outbox.retain(|m| !is_gen_delta(m));
                        let msgs = msg::snapshot_messages(st.gen, st.items(), self.max_payload);
                        break Work::Snapshot(msgs);
                    }
                    if up {
                        if let Some(m) = st.outbox.pop_front() {
                            break Work::One(m);
                        }
                    }
                    if up && last_heartbeat.elapsed() > Duration::from_secs(2) {
                        break Work::Heartbeat(st.gen);
                    }
                    st = self.cv.wait_timeout(st, Duration::from_millis(250)).unwrap().0;
                }
            };
            match work {
                Work::Snapshot(msgs) => {
                    let n = msgs.len();
                    let gen = match msgs.first() {
                        Some(ToData::Begin { gen }) => *gen,
                        _ => 0,
                    };
                    let ok = msgs.iter().all(|m| self.send(m));
                    let mut st = self.st.lock().unwrap();
                    st.snapshot_in_flight = false;
                    if ok {
                        debug!("split: snapshot gen {gen} sent ({n} messages)");
                    } else {
                        st.need_snapshot = true;
                    }
                }
                Work::One(m) => {
                    if !self.send(&m) {
                        let mut st = self.st.lock().unwrap();
                        if is_gen_delta(&m) {
                            st.need_snapshot = true;
                        } else if matches!(m, ToData::DeviceConfig(_)) {
                            st.device_dirty = true;
                        } else {
                            st.outbox.push_front(m);
                        }
                        drop(st);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                Work::Heartbeat(gen) => {
                    last_heartbeat = Instant::now();
                    self.send(&ToData::Heartbeat { gen });
                }
            }
        }
    }
}

impl DataPlane for RemoteDataPlane {
    fn ready(&self) -> bool {
        self.inner.st.lock().unwrap().a.is_some()
    }
    fn node_public(&self) -> [u8; 32] {
        self.inner.st.lock().unwrap().a.as_ref().map(|a| a.node_key).unwrap_or_default()
    }
    fn disco_public(&self) -> [u8; 32] {
        self.inner.st.lock().unwrap().a.as_ref().map(|a| a.disco_key).unwrap_or_default()
    }
    fn propose_node_key(&self) -> io::Result<[u8; 32]> {
        self.request(
            |req| ToData::ProposeNodeKey { req },
            |st, req| st.proposals.remove(&req),
            Duration::from_secs(10),
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "data chip did not propose a key"))
    }
    fn commit_node_key(&self, public: [u8; 32]) -> io::Result<()> {
        self.command(ToData::CommitNodeKey { key: public });
        Ok(())
    }
    fn set_addresses(&self, cidrs: Vec<(IpAddr, u8)>) {
        let v: Vec<Cidr> = cidrs.into_iter().map(|(ip, prefix)| Cidr { ip, prefix }).collect();
        self.modify(|st| {
            if st.addresses != v {
                st.addresses = v;
                st.full_dirty = true;
            }
        });
    }
    fn set_derp_map(&self, map: DerpMap) {
        let regions: BTreeMap<i32, msg::DerpRegion> = map
            .regions
            .values()
            .map(|r| (r.region_id, region_to_link(r)))
            .collect();
        self.modify(|st| {
            if st.regions != regions {
                st.regions = regions;
                st.full_dirty = true;
            }
        });
    }
    fn set_peers(&self, nodes: &[Node]) {
        let peers: BTreeMap<i64, msg::Peer> =
            nodes.iter().filter_map(peer_to_link).map(|p| (p.id, p)).collect();
        self.modify(|st| {
            st.peers = peers;
            st.full_dirty = true;
        });
    }
    fn peers_changed(&self, nodes: &[Node]) {
        let peers: Vec<msg::Peer> = nodes.iter().filter_map(peer_to_link).collect();
        self.modify(|st| {
            for p in &peers {
                st.peers.insert(p.id, p.clone());
            }
            st.pending.push(ToData::PeersUpsert { base: 0, gen: 0, peers });
        });
    }
    fn peers_removed(&self, ids: &[i64], _by_id: &NodeIdMap) {
        self.modify(|st| {
            for id in ids {
                st.peers.remove(id);
            }
            st.pending.push(ToData::PeersRemove { base: 0, gen: 0, ids: ids.to_vec() });
        });
    }
    fn apply_patches(&self, patches: &[PeerChange], _by_id: &NodeIdMap) {
        let patches: Vec<msg::PeerPatch> = patches.iter().map(patch_to_link).collect();
        self.modify(|st| {
            for pp in &patches {
                if let Some(p) = st.peers.get_mut(&pp.id) {
                    if pp.derp_region != 0 {
                        p.derp_region = pp.derp_region;
                    }
                    if let Some(e) = &pp.endpoints {
                        p.endpoints = e.clone();
                    }
                    if pp.disco.is_some() {
                        p.disco = pp.disco;
                    }
                }
            }
            st.pending.push(ToData::Patches { base: 0, gen: 0, patches });
        });
    }
    fn set_packet_filter(&self, rules: &[FilterRule]) {
        let v: Vec<msg::FilterRule> = rules.iter().map(rule_to_link).collect();
        self.modify(|st| {
            if st.filter != v {
                st.filter = v;
                st.full_dirty = true;
            }
        });
    }
    fn set_clock_offset(&self, secs: i32) {
        let mut st = self.inner.st.lock().unwrap();
        if st.clock.map(|c| (c - secs).abs() > 1).unwrap_or(true) {
            st.clock = Some(secs);
            st.outbox.push_back(ToData::ClockOffset { secs });
            self.inner.cv.notify_all();
        }
    }
    fn commit(&self) {
        let mut st = self.inner.st.lock().unwrap();
        if st.full_dirty {
            st.full_dirty = false;
            st.gen = st.gen.wrapping_add(1).max(1);
            st.pending.clear();
            st.need_snapshot = true;
        } else {
            let pending = std::mem::take(&mut st.pending);
            for mut m in pending {
                let base = st.gen;
                let gen = base.wrapping_add(1).max(1);
                match &mut m {
                    ToData::PeersUpsert { base: b, gen: g, .. }
                    | ToData::PeersRemove { base: b, gen: g, .. }
                    | ToData::Patches { base: b, gen: g, .. } => {
                        *b = base;
                        *g = gen;
                    }
                    _ => {}
                }
                st.gen = gen;
                if !st.need_snapshot {
                    st.outbox.push_back(m);
                }
            }
        }
        self.inner.cv.notify_all();
    }
    fn connect_home_derp(&self, region: i32) {
        let mut st = self.inner.st.lock().unwrap();
        st.home_derp = region;
        st.outbox.push_back(ToData::ConnectHomeDerp { region });
        self.inner.cv.notify_all();
    }
    fn home_derp(&self) -> i32 {
        let st = self.inner.st.lock().unwrap();
        match &st.a {
            Some(a) if a.home_derp != 0 => a.home_derp,
            _ => st.home_derp,
        }
    }
    fn derp_connected(&self, region: i32) -> bool {
        let st = self.inner.st.lock().unwrap();
        self.inner.link.is_up()
            && st.a.as_ref().map(|a| a.derp_connected && a.home_derp == region).unwrap_or(false)
    }
    fn probe_derp_regions(&self, wait: Duration) -> Option<i32> {
        self.request(
            |req| ToData::ProbeDerp { req, wait_ms: wait.as_millis() as u32 },
            |st, req| st.probes.remove(&req),
            wait + Duration::from_secs(5),
        )
        .flatten()
    }
    fn fallback_region(&self, code: &str) -> Option<i32> {
        let st = self.inner.st.lock().unwrap();
        let usable: Vec<&msg::DerpRegion> =
            st.regions.values().filter(|r| !r.avoid && !r.nodes.is_empty()).collect();
        usable
            .iter()
            .find(|r| r.code == code)
            .or_else(|| usable.first())
            .map(|r| r.id)
    }
    fn local_endpoints(&self) -> Vec<SocketAddr> {
        self.inner.st.lock().unwrap().a.as_ref().map(|a| a.endpoints.clone()).unwrap_or_default()
    }
    fn derp_latency(&self) -> HashMap<String, f64> {
        let st = self.inner.st.lock().unwrap();
        st.a.as_ref()
            .map(|a| {
                a.derp_latency
                    .iter()
                    .map(|(r, us)| (r.to_string(), *us as f64 / 1e6))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn set_endpoints_changed(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        *self.inner.endpoints_cb.lock().unwrap() = Some(cb);
    }
    fn peer_status(&self) -> Vec<PeerStatus> {
        let st = self.inner.st.lock().unwrap();
        st.peer_status
            .iter()
            .map(|p| PeerStatus {
                name: p.name.clone(),
                key_short: crate::keys::short(&p.key),
                direct: p.direct,
                home_derp: p.home_derp,
                has_session: p.has_session,
                active: p.active,
            })
            .collect()
    }
    fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        self.inner.cv.notify_all();
        self.inner.link.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Data chip (A).

#[derive(Default)]
struct Staging {
    gen: u32,
    addresses: Vec<Cidr>,
    home: i32,
    regions: Vec<msg::DerpRegion>,
    peers: Vec<msg::Peer>,
    filter: Vec<msg::FilterRule>,
}

/// What the data chip currently runs on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppliedConfig {
    pub gen: u32,
    pub addresses: Vec<Cidr>,
    pub home_derp: i32,
    pub regions: usize,
    pub peers: usize,
    pub filter_rules: usize,
    pub snapshots: u32,
    pub deltas: u32,
}

#[derive(Default)]
struct DState {
    applied: AppliedConfig,
    staging: Option<Staging>,
    by_id: NodeIdMap,
}

pub type DeviceConfigSink = Box<dyn Fn(DeviceConfig) + Send + Sync>;

struct DInner {
    link: Link,
    dp: Arc<LocalDataPlane>,
    max_payload: usize,
    st: Mutex<DState>,
    on_device: Option<DeviceConfigSink>,
    has_device: AtomicBool,
    status_now: AtomicBool,
    status_period: Duration,
    shutdown: AtomicBool,
}

/// Serves a [`LocalDataPlane`] to the control chip.
#[derive(Clone)]
pub struct DataServer {
    inner: Arc<DInner>,
}

pub struct DataServerConfig {
    pub max_payload: usize,
    pub status_period: Duration,
    /// Whether this chip already has Wi-Fi credentials etc.
    pub has_device_config: bool,
    pub stack: usize,
}

impl DataServer {
    pub fn start(
        link: Link,
        events: Receiver<Event>,
        dp: Arc<LocalDataPlane>,
        cfg: DataServerConfig,
        on_device: Option<DeviceConfigSink>,
        spawner: &Spawner,
    ) -> io::Result<DataServer> {
        let inner = Arc::new(DInner {
            link,
            dp,
            max_payload: cfg.max_payload,
            st: Mutex::new(DState::default()),
            on_device,
            has_device: AtomicBool::new(cfg.has_device_config),
            status_now: AtomicBool::new(true),
            status_period: cfg.status_period,
            shutdown: AtomicBool::new(false),
        });
        let flag = inner.clone();
        inner.dp.set_endpoints_changed(Arc::new(move || {
            flag.status_now.store(true, Ordering::Relaxed)
        }));
        let i = inner.clone();
        spawner("split-ev", cfg.stack, Box::new(move || i.event_loop(events)))?;
        let i = inner.clone();
        spawner("split-status", cfg.stack, Box::new(move || i.status_loop()))?;
        Ok(DataServer { inner })
    }

    pub fn applied(&self) -> AppliedConfig {
        self.inner.st.lock().unwrap().applied.clone()
    }

    pub fn dataplane(&self) -> &Arc<LocalDataPlane> {
        &self.inner.dp
    }

    pub fn link(&self) -> &Link {
        &self.inner.link
    }

    /// Stops the link (the data plane keeps running).
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::Relaxed);
        self.inner.link.shutdown();
    }
}

impl DInner {
    fn send(&self, m: &ToControl) -> bool {
        self.link.send(&msg::encode(m)).is_ok()
    }

    #[inline(never)] // keeps the event thread's frame small (ESP32 stacks)
    fn status(&self) -> ToControl {
        let dp = &self.dp;
        let home = dp.home_derp();
        let mut lat: Vec<(i32, u32)> = dp
            .derp_latency()
            .iter()
            .filter_map(|(r, s)| Some((r.parse().ok()?, (s * 1e6) as u32)))
            .collect();
        lat.sort_unstable(); // no 4 KB stable-sort scratch on the stack
        ToControl::Status {
            node_key: dp.node_public(),
            disco_key: dp.disco_public(),
            endpoints: dp.local_endpoints(),
            home_derp: home,
            derp_connected: home != 0 && dp.derp_connected(home),
            derp_latency: lat,
            applied_gen: self.st.lock().unwrap().applied.gen,
            has_device_config: self.has_device.load(Ordering::Relaxed),
        }
    }

    #[inline(never)] // keeps the event thread's frame small (ESP32 stacks)
    fn send_status(&self) {
        if !self.send(&self.status()) {
            return;
        }
        let peers: Vec<msg::PeerStatus> = self
            .dp
            .magicsock()
            .peer_status_full()
            .into_iter()
            .map(|(key, p)| msg::PeerStatus {
                name: p.name,
                key,
                direct: p.direct,
                home_derp: p.home_derp,
                has_session: p.has_session,
                active: p.active,
            })
            .collect();
        let chunks = msg::chunk_by_size(peers, self.max_payload);
        if chunks.is_empty() {
            self.send(&ToControl::PeerStatus { peers: Vec::new(), last: true });
        }
        let n = chunks.len();
        for (i, c) in chunks.into_iter().enumerate() {
            if !self.send(&ToControl::PeerStatus { peers: c, last: i + 1 == n }) {
                break;
            }
        }
    }

    fn status_loop(self: Arc<Self>) {
        let mut last = Instant::now() - self.status_period;
        while !self.shutdown.load(Ordering::Relaxed) {
            if self.link.is_up()
                && (self.status_now.swap(false, Ordering::Relaxed) || last.elapsed() >= self.status_period)
            {
                last = Instant::now();
                self.send_status();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn event_loop(self: Arc<Self>, events: Receiver<Event>) {
        while !self.shutdown.load(Ordering::Relaxed) {
            let ev = match events.recv_timeout(Duration::from_millis(500)) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match ev {
                Event::Up { peer_epoch, restarted } => {
                    info!("split: control chip up (epoch {peer_epoch:08x}, restarted {restarted})");
                    self.st.lock().unwrap().staging = None;
                    self.status_now.store(true, Ordering::Relaxed);
                }
                Event::Down => {
                    warn!("split: control chip not responding; keeping current configuration")
                }
                Event::Message(b) => match msg::decode::<ToData>(&b) {
                    Some(m) => self.handle(m),
                    None => warn!("split: undecodable message ({} bytes)", b.len()),
                },
            }
        }
    }

    fn resync(&self, have: u32) {
        self.send(&ToControl::RequestResync { applied_gen: have });
    }

    #[inline(never)] // keeps the event thread's frame small (ESP32 stacks)
    fn handle(&self, m: ToData) {
        let dp = &self.dp;
        match m {
            ToData::Begin { gen } => {
                self.st.lock().unwrap().staging = Some(Staging { gen, ..Default::default() });
            }
            ToData::Item { gen, item } => {
                let mut st = self.st.lock().unwrap();
                match st.staging.as_mut() {
                    Some(s) if s.gen == gen => match item {
                        Item::Addresses(a) => s.addresses = a,
                        Item::HomeDerp(h) => s.home = h,
                        Item::DerpRegion(r) => s.regions.push(r),
                        Item::Peers(p) => s.peers.extend(p),
                        Item::FilterRules(r) => s.filter.extend(r),
                    },
                    _ => debug!("split: item for gen {gen} outside a snapshot"),
                }
            }
            ToData::End { gen } => {
                let staged = {
                    let mut st = self.st.lock().unwrap();
                    match st.staging.take() {
                        Some(s) if s.gen == gen => Some(s),
                        _ => None,
                    }
                };
                match staged {
                    Some(s) => self.apply_snapshot(s),
                    None => {
                        let have = self.st.lock().unwrap().applied.gen;
                        self.resync(have);
                    }
                }
            }
            ToData::PeersUpsert { base, gen, peers } => {
                if self.check_base(base) {
                    let nodes: Vec<Node> = peers.iter().map(peer_from_link).collect();
                    {
                        let mut st = self.st.lock().unwrap();
                        for p in &peers {
                            st.by_id.insert(p.id, p.key);
                        }
                    }
                    dp.peers_changed(&nodes);
                    self.applied_delta(gen);
                }
            }
            ToData::PeersRemove { base, gen, ids } => {
                if self.check_base(base) {
                    let by_id = self.st.lock().unwrap().by_id.clone();
                    dp.peers_removed(&ids, &by_id);
                    let mut st = self.st.lock().unwrap();
                    for id in &ids {
                        st.by_id.remove(id);
                    }
                    drop(st);
                    self.applied_delta(gen);
                }
            }
            ToData::Patches { base, gen, patches } => {
                if self.check_base(base) {
                    let by_id = self.st.lock().unwrap().by_id.clone();
                    let pcs: Vec<PeerChange> = patches.iter().map(patch_from_link).collect();
                    dp.apply_patches(&pcs, &by_id);
                    self.applied_delta(gen);
                }
            }
            ToData::ConnectHomeDerp { region } => {
                if dp.home_derp() != region {
                    dp.connect_home_derp(region);
                }
                self.st.lock().unwrap().applied.home_derp = region;
                self.status_now.store(true, Ordering::Relaxed);
            }
            ToData::ProbeDerp { req, wait_ms } => {
                let region = dp.probe_derp_regions(Duration::from_millis(wait_ms as u64));
                self.send(&ToControl::ProbeResult { req, region });
            }
            ToData::ProposeNodeKey { req } => match dp.propose_node_key() {
                Ok(key) => {
                    self.send(&ToControl::NodeKeyProposal { req, key });
                }
                Err(e) => warn!("split: propose key: {e}"),
            },
            ToData::CommitNodeKey { key } => {
                if let Err(e) = dp.commit_node_key(key) {
                    warn!("split: commit key: {e}");
                }
                self.status_now.store(true, Ordering::Relaxed);
            }
            ToData::DeviceConfig(cfg) => {
                if let Some(f) = &self.on_device {
                    f(cfg);
                }
                self.has_device.store(true, Ordering::Relaxed);
                self.status_now.store(true, Ordering::Relaxed);
            }
            ToData::ClockOffset { secs } => dp.set_clock_offset(secs),
            ToData::Heartbeat { gen } => {
                let have = self.st.lock().unwrap().applied.gen;
                if gen != have {
                    self.resync(have);
                }
            }
        }
    }

    fn check_base(&self, base: u32) -> bool {
        let have = self.st.lock().unwrap().applied.gen;
        if base != have {
            debug!("split: delta on gen {base}, have {have}");
            self.resync(have);
            return false;
        }
        true
    }

    fn applied_delta(&self, gen: u32) {
        let mut st = self.st.lock().unwrap();
        st.applied.gen = gen;
        st.applied.peers = st.by_id.len();
        st.applied.deltas += 1;
    }

    #[inline(never)] // keeps the event thread's frame small (ESP32 stacks)
    fn apply_snapshot(&self, s: Staging) {
        let dp = &self.dp;
        dp.set_addresses(s.addresses.iter().map(|c| (c.ip, c.prefix)).collect());
        dp.set_derp_map(map_from_link(&s.regions));
        let nodes: Vec<Node> = s.peers.iter().map(peer_from_link).collect();
        dp.set_peers(&nodes);
        let rules: Vec<FilterRule> = s.filter.iter().map(rule_from_link).collect();
        dp.set_packet_filter(&rules);
        if s.home != 0 && dp.home_derp() != s.home {
            dp.connect_home_derp(s.home);
        }
        let mut st = self.st.lock().unwrap();
        st.by_id = s.peers.iter().map(|p| (p.id, p.key)).collect();
        let snapshots = st.applied.snapshots + 1;
        let deltas = st.applied.deltas;
        st.applied = AppliedConfig {
            gen: s.gen,
            addresses: s.addresses,
            home_derp: s.home,
            regions: s.regions.len(),
            peers: s.peers.len(),
            filter_rules: s.filter.len(),
            snapshots,
            deltas,
        };
        info!(
            "split: applied gen {} ({} peers, {} regions, {} rules)",
            s.gen,
            st.applied.peers,
            st.applied.regions,
            st.applied.filter_rules
        );
        drop(st);
        self.status_now.store(true, Ordering::Relaxed);
    }
}
