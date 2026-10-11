//! The boundary between the control plane (`node.rs`: ts2021, map stream,
//! registration) and the data plane (magicsock, DERP, WireGuard, netstack).
//!
//! Every call that crosses that line goes through [`DataPlane`]. The
//! in-process implementation, [`LocalDataPlane`], is what a single-chip node
//! uses; `split::RemoteDataPlane` (feature `split`) forwards the same calls to
//! a second chip over a serial link.

use crate::filter::Filter;
use crate::keys::KeyPair;
use crate::magicsock::{MagicSock, PeerStatus};
use crate::net::{Dialer, Spawner};
use crate::netstack::NetStack;
use crate::node::{Store, KEY_DISCO, KEY_NODE};
use crate::types::{DerpMap, FilterRule, Node, PeerChange};
use log::{info, warn};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type NodeIdMap = HashMap<i64, [u8; 32]>;

pub trait DataPlane: Send + Sync {
    /// False until the data plane can answer key/endpoint queries (a remote
    /// data plane must first hear from its peer chip).
    fn ready(&self) -> bool {
        true
    }
    fn node_public(&self) -> [u8; 32];
    fn disco_public(&self) -> [u8; 32];
    /// Generates a new node key (kept by the data plane) and returns its
    /// public half; it only takes effect after [`DataPlane::commit_node_key`].
    fn propose_node_key(&self) -> io::Result<[u8; 32]>;
    fn commit_node_key(&self, public: [u8; 32]) -> io::Result<()>;

    fn set_addresses(&self, cidrs: Vec<(IpAddr, u8)>);
    fn set_derp_map(&self, map: DerpMap);
    fn set_peers(&self, nodes: &[Node]);
    fn peers_changed(&self, nodes: &[Node]);
    fn peers_removed(&self, ids: &[i64], by_id: &NodeIdMap);
    fn apply_patches(&self, patches: &[PeerChange], by_id: &NodeIdMap);
    fn set_packet_filter(&self, rules: &[FilterRule]);
    fn set_clock_offset(&self, _secs: i32) {}
    /// End of one map response: a remote data plane flushes here.
    fn commit(&self) {}

    fn connect_home_derp(&self, region: i32);
    fn home_derp(&self) -> i32;
    fn derp_connected(&self, region: i32) -> bool;
    fn probe_derp_regions(&self, wait: Duration) -> Option<i32>;
    fn fallback_region(&self, code: &str) -> Option<i32>;
    fn local_endpoints(&self) -> Vec<SocketAddr>;
    fn derp_latency(&self) -> HashMap<String, f64>;
    fn set_endpoints_changed(&self, cb: Arc<dyn Fn() + Send + Sync>);
    fn peer_status(&self) -> Vec<PeerStatus>;
    /// The TCP stack, when it lives in this process.
    fn netstack(&self) -> Option<&NetStack> {
        None
    }
    fn shutdown(&self);
}

pub(crate) fn load_or_create_key(store: &dyn Store, name: &str) -> io::Result<KeyPair> {
    if let Some(v) = store.get(name) {
        if let Some(k) = KeyPair::from_secret_hex(&v) {
            return Ok(k);
        }
        warn!("node: stored {name} is invalid; regenerating");
    }
    let k = KeyPair::generate();
    store.set(name, &k.secret_hex())?;
    Ok(k)
}

/// Parameters for [`LocalDataPlane::start`].
pub struct LocalConfig {
    pub udp_port: u16,
    pub local_ip: Option<IpAddr>,
    pub worker_stack: usize,
    pub spawner: Spawner,
}

/// magicsock + netstack in this process; owns the node and disco keys.
pub struct LocalDataPlane {
    magicsock: MagicSock,
    netstack: NetStack,
    store: Arc<dyn Store>,
    pending_key: Mutex<Option<KeyPair>>,
}

impl LocalDataPlane {
    /// Loads (or creates) the node/disco keys from `store` and starts the
    /// UDP, DERP, tick and netstack threads.
    pub fn start(
        cfg: LocalConfig,
        store: Arc<dyn Store>,
        dialer: Arc<dyn Dialer>,
    ) -> io::Result<Arc<LocalDataPlane>> {
        let node_key = load_or_create_key(store.as_ref(), KEY_NODE)?;
        let disco_key = load_or_create_key(store.as_ref(), KEY_DISCO)?;
        info!("dataplane: node {}", crate::keys::short(node_key.public()));
        let magicsock = MagicSock::start(
            node_key,
            disco_key,
            dialer,
            cfg.udp_port,
            cfg.local_ip,
            cfg.worker_stack,
            cfg.spawner.clone(),
        )?;
        let ms = magicsock.clone();
        let netstack = NetStack::start(
            Box::new(move |pkt| ms.send_ip(&pkt)),
            cfg.worker_stack,
            &cfg.spawner,
        )?;
        let ns = netstack.clone();
        magicsock.set_ip_sink(Arc::new(move |pkt| ns.inject(pkt)));
        Ok(Arc::new(LocalDataPlane {
            magicsock,
            netstack,
            store,
            pending_key: Mutex::new(None),
        }))
    }

    pub fn magicsock(&self) -> &MagicSock {
        &self.magicsock
    }
    pub fn net(&self) -> &NetStack {
        &self.netstack
    }
}

impl DataPlane for LocalDataPlane {
    fn node_public(&self) -> [u8; 32] {
        self.magicsock.node_public()
    }
    fn disco_public(&self) -> [u8; 32] {
        *self.magicsock.disco_public()
    }
    fn propose_node_key(&self) -> io::Result<[u8; 32]> {
        let k = KeyPair::generate();
        let p = *k.public();
        *self.pending_key.lock().unwrap() = Some(k);
        Ok(p)
    }
    fn commit_node_key(&self, public: [u8; 32]) -> io::Result<()> {
        let mut pending = self.pending_key.lock().unwrap();
        match pending.take() {
            Some(k) if *k.public() == public => {
                self.store.set(KEY_NODE, &k.secret_hex())?;
                self.magicsock.set_node_key(k);
                info!("dataplane: node key rotated to {}", crate::keys::short(&public));
                Ok(())
            }
            other => {
                *pending = other;
                Err(io::Error::new(io::ErrorKind::InvalidInput, "no such proposed node key"))
            }
        }
    }
    fn set_addresses(&self, cidrs: Vec<(IpAddr, u8)>) {
        self.netstack.set_addresses(cidrs)
    }
    fn set_derp_map(&self, map: DerpMap) {
        self.magicsock.set_derp_map(map)
    }
    fn set_peers(&self, nodes: &[Node]) {
        self.magicsock.set_peers(nodes)
    }
    fn peers_changed(&self, nodes: &[Node]) {
        self.magicsock.peers_changed(nodes)
    }
    fn peers_removed(&self, ids: &[i64], by_id: &NodeIdMap) {
        self.magicsock.peers_removed(ids, by_id)
    }
    fn apply_patches(&self, patches: &[PeerChange], by_id: &NodeIdMap) {
        self.magicsock.apply_patches(patches, by_id)
    }
    fn set_packet_filter(&self, rules: &[FilterRule]) {
        self.magicsock.set_packet_filter(Filter::compile(rules))
    }
    fn set_clock_offset(&self, secs: i32) {
        crate::wireguard::WALL_CLOCK_OFFSET_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
    }
    fn connect_home_derp(&self, region: i32) {
        self.magicsock.connect_home_derp(region)
    }
    fn home_derp(&self) -> i32 {
        self.magicsock.home_derp()
    }
    fn derp_connected(&self, region: i32) -> bool {
        self.magicsock.derp_connected(region)
    }
    fn probe_derp_regions(&self, wait: Duration) -> Option<i32> {
        self.magicsock.probe_derp_regions(wait)
    }
    fn fallback_region(&self, code: &str) -> Option<i32> {
        self.magicsock.fallback_region(code)
    }
    fn local_endpoints(&self) -> Vec<SocketAddr> {
        self.magicsock.local_endpoints()
    }
    fn derp_latency(&self) -> HashMap<String, f64> {
        self.magicsock.derp_latency()
    }
    fn set_endpoints_changed(&self, cb: Arc<dyn Fn() + Send + Sync>) {
        self.magicsock.set_endpoints_changed(cb)
    }
    fn peer_status(&self) -> Vec<PeerStatus> {
        self.magicsock.peer_status()
    }
    fn netstack(&self) -> Option<&NetStack> {
        Some(&self.netstack)
    }
    fn shutdown(&self) {
        self.magicsock.shutdown();
        self.netstack.shutdown();
    }
}
