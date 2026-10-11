//! Application messages between the control chip (B) and the data chip (A).
//!
//! B holds the whole configuration A needs, versioned by a generation
//! number `gen`. A full configuration goes as `Begin{gen}`, items,
//! `End{gen}`; A stages the items and applies them atomically at `End`.
//! Smaller changes go as deltas that each carry the new `gen` and the `gen`
//! they apply on top of (`base`). A applies a delta only when `base` equals
//! what it has applied; otherwise it asks for a resync. A reports its
//! applied generation in every `Status`; when it differs from B's, B sends
//! everything again. That covers lost messages and a reboot of either side:
//! A keeps running on the last configuration while B is away.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

pub type Key = [u8; 32];

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    pub ip: IpAddr,
    pub prefix: u8,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub id: i64,
    pub name: String,
    pub key: Key,
    pub disco: Option<Key>,
    pub endpoints: Vec<SocketAddr>,
    /// Addresses and AllowedIPs, merged.
    pub allowed_ips: Vec<Cidr>,
    pub derp_region: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerPatch {
    pub id: i64,
    pub derp_region: i32,
    pub endpoints: Option<Vec<SocketAddr>>,
    pub disco: Option<Key>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DerpNode {
    pub name: String,
    pub host_name: String,
    pub cert_name: String,
    pub ipv4: String,
    pub ipv6: String,
    pub stun_port: i32,
    pub stun_only: bool,
    pub derp_port: i32,
    pub can_port80: bool,
    pub insecure_for_tests: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DerpRegion {
    pub id: i32,
    pub code: String,
    pub avoid: bool,
    pub nodes: Vec<DerpNode>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FilterRule {
    pub src_ips: Vec<String>,
    /// (IP or CIDR or "*", first port, last port)
    pub dst: Vec<(String, u16, u16)>,
    pub protos: Vec<i32>,
}

/// One part of a full configuration (between `Begin` and `End`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Addresses(Vec<Cidr>),
    HomeDerp(i32),
    DerpRegion(DerpRegion),
    Peers(Vec<Peer>),
    FilterRules(Vec<FilterRule>),
}

/// Settings the data chip needs for itself (forwarded from B's setup shell).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceConfig {
    pub wifi: Vec<(String, String)>,
    pub tcp_port: u16,
    pub baud: u32,
}

/// Control chip (B) -> data chip (A).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ToData {
    Begin { gen: u32 },
    Item { gen: u32, item: Item },
    End { gen: u32 },
    PeersUpsert { base: u32, gen: u32, peers: Vec<Peer> },
    PeersRemove { base: u32, gen: u32, ids: Vec<i64> },
    Patches { base: u32, gen: u32, patches: Vec<PeerPatch> },
    ConnectHomeDerp { region: i32 },
    /// STUN-probe every region for `wait_ms` and answer with `ProbeResult`.
    ProbeDerp { req: u32, wait_ms: u32 },
    /// Generate a candidate node key and answer with `NodeKeyProposal`.
    ProposeNodeKey { req: u32 },
    /// The control server accepted this key: persist it and use it.
    CommitNodeKey { key: Key },
    DeviceConfig(DeviceConfig),
    /// Wall-clock offset from control time (WireGuard timestamps).
    ClockOffset { secs: i32 },
    Heartbeat { gen: u32 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    pub name: String,
    pub key: Key,
    pub direct: Option<SocketAddr>,
    pub home_derp: i32,
    pub has_session: bool,
    pub active: bool,
}

/// Data chip (A) -> control chip (B).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ToControl {
    Status {
        node_key: Key,
        disco_key: Key,
        endpoints: Vec<SocketAddr>,
        home_derp: i32,
        derp_connected: bool,
        /// (region, STUN round trip in microseconds)
        derp_latency: Vec<(i32, u32)>,
        /// Generation of the configuration A runs on (0: none).
        applied_gen: u32,
        has_device_config: bool,
    },
    /// Up to a few peers' state; sent after `Status` (for `status` on B).
    PeerStatus { peers: Vec<PeerStatus>, last: bool },
    ProbeResult { req: u32, region: Option<i32> },
    NodeKeyProposal { req: u32, key: Key },
    /// A could not apply a delta (or just booted): send everything.
    RequestResync { applied_gen: u32 },
}

pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    postcard::to_allocvec(m).expect("postcard encoding cannot fail for these types")
}

pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Option<T> {
    postcard::from_bytes(b).ok()
}

/// Splits a full configuration into messages no larger than `max_payload`.
pub fn snapshot_messages(gen: u32, items: Vec<Item>, max_payload: usize) -> Vec<ToData> {
    let mut out = vec![ToData::Begin { gen }];
    for item in items {
        match item {
            Item::Peers(peers) => {
                for chunk in chunk_by_size(peers, max_payload) {
                    out.push(ToData::Item {
                        gen,
                        item: Item::Peers(chunk),
                    });
                }
            }
            Item::FilterRules(rules) => {
                for chunk in chunk_by_size(rules, max_payload) {
                    out.push(ToData::Item {
                        gen,
                        item: Item::FilterRules(chunk),
                    });
                }
            }
            other => out.push(ToData::Item { gen, item: other }),
        }
    }
    out.push(ToData::End { gen });
    out
}

/// Groups elements so each group encodes under `max` bytes (with room for
/// the enclosing message). A single element larger than that is sent alone
/// and will be rejected by the link as too large.
pub fn chunk_by_size<T: Serialize>(v: Vec<T>, max: usize) -> Vec<Vec<T>> {
    let budget = max.saturating_sub(32);
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut size = 0;
    for e in v {
        let n = encode(&e).len();
        if !cur.is_empty() && size + n > budget {
            out.push(std::mem::take(&mut cur));
            size = 0;
        }
        size += n;
        cur.push(e);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(i: u8) -> Peer {
        Peer {
            id: i as i64,
            name: format!("peer-{i}.example.ts.net"),
            key: [i; 32],
            disco: Some([i ^ 0xFF; 32]),
            endpoints: vec!["203.0.113.5:41641".parse().unwrap(), "[2001:db8::1]:41641".parse().unwrap()],
            allowed_ips: vec![Cidr { ip: "100.64.0.1".parse().unwrap(), prefix: 32 }],
            derp_region: 7,
        }
    }

    #[test]
    fn roundtrip_and_size() {
        let m = ToData::PeersUpsert { base: 1, gen: 2, peers: vec![peer(1)] };
        let b = encode(&m);
        assert_eq!(decode::<ToData>(&b), Some(m));
        // A peer is compact: keys are raw bytes, addresses binary.
        assert!(b.len() < 160, "peer message is {} bytes", b.len());
    }

    #[test]
    fn snapshot_chunks_fit() {
        let peers: Vec<Peer> = (0..40).map(peer).collect();
        let msgs = snapshot_messages(5, vec![Item::HomeDerp(3), Item::Peers(peers)], 1024);
        assert_eq!(msgs.first(), Some(&ToData::Begin { gen: 5 }));
        assert_eq!(msgs.last(), Some(&ToData::End { gen: 5 }));
        let mut n = 0;
        for m in &msgs {
            assert!(encode(m).len() <= 1024);
            if let ToData::Item { item: Item::Peers(p), .. } = m {
                n += p.len();
            }
        }
        assert_eq!(n, 40);
    }
}
