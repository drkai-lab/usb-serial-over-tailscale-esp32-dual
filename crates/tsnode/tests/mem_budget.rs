//! Heap measurements for the map decode path (run with `--nocapture`, with
//! and without `--features lowmem`). A counting global allocator records the
//! live and peak bytes; the input buffer is allocated before measuring.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        LIVE.fetch_sub(l.size(), Relaxed);
    }
}
#[global_allocator]
static A: Counting = Counting;

fn reset() -> usize {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    base
}

fn key(i: usize, p: &str) -> String {
    format!("{p}:{:064x}", i * 7919 + 1)
}

/// A peer shaped like what the Tailscale control plane sends (capver ~106):
/// keys, addresses, endpoints and a Hostinfo with services.
fn peer(i: usize) -> String {
    format!(
        r#"{{"ID":{id},"StableID":"n{id}CNTRL","Name":"host-{i}.tail1234.ts.net.","User":{u},"Key":"{k}","KeyExpiry":"2027-03-01T00:00:00Z","Machine":"{m}","DiscoKey":"{d}","Addresses":["100.64.{a}.{b}/32","fd7a:115c:a1e0::{a:x}{b:02x}/128"],"AllowedIPs":["100.64.{a}.{b}/32","fd7a:115c:a1e0::{a:x}{b:02x}/128"],"Endpoints":["203.0.113.{b}:41641","192.168.1.{b}:41641","[2001:db8::{b:x}]:41641"],"DERP":"127.3.3.40:{r}","HomeDERP":{r},"Hostinfo":{{"IPNVersion":"1.76.1-t1234abcd-g5678","OS":"linux","OSVersion":"6.8.0-45-generic","Distro":"ubuntu","DistroVersion":"24.04","Hostname":"host-{i}","GoArch":"amd64","GoVersion":"go1.23.1","Services":[{{"Proto":"tcp","Port":22}},{{"Proto":"peerapi4","Port":56789}},{{"Proto":"peerapi6","Port":56789}}],"NetInfo":{{"MappingVariesByDestIP":false,"PreferredDERP":{r},"LinkType":"wired"}}}},"Created":"2025-01-01T00:00:00Z","Cap":106,"Tags":["tag:server"],"PrimaryRoutes":[],"LastSeen":"2026-10-01T00:00:00Z","Online":true,"MachineAuthorized":true,"Capabilities":["https://tailscale.com/cap/file-sharing","https://tailscale.com/cap/ssh"],"CapMap":{{"https://tailscale.com/cap/file-sharing":null}},"ComputedName":"host-{i}","ComputedNameWithHost":"host-{i}"}}"#,
        id = 1000 + i,
        u = 1 + i % 3,
        k = key(i, "nodekey"),
        m = key(i, "mkey"),
        d = key(i, "discokey"),
        a = 1 + i / 250,
        b = 1 + i % 250,
        r = 1 + i % 20,
    )
}

fn map_json(n: usize) -> Vec<u8> {
    let derp = include_str!("fixtures/derpmap-default.json");
    let peers: Vec<String> = (0..n).map(peer).collect();
    let me = peer(99_999);
    let s = format!(
        r#"{{"MapSessionHandle":"abc","Seq":1,"Node":{me},"DERPMap":{derp},"Peers":[{}],"PacketFilter":[{{"SrcIPs":["*"],"DstPorts":[{{"IP":"*","Ports":{{"First":0,"Last":65535}}}}]}}],"DNSConfig":{{"Resolvers":[{{"Addr":"100.100.100.100"}}],"Domains":["tail1234.ts.net"]}},"Domain":"example.com","UserProfiles":[{{"ID":1,"LoginName":"a@example.com","DisplayName":"A","ProfilePicURL":"https://example.com/a.png"}}],"ControlTime":"2026-10-12T00:00:00Z","SSHPolicy":{{"rules":[]}}}}"#,
        peers.join(",")
    );
    s.into_bytes()
}

/// Simulates the HTTP/2 path: each read hands over at most one 16 KB frame,
/// allocated per frame as `H2Conn::read_body` does.
struct Frames<'a> {
    src: &'a [u8],
    cur: Vec<u8>,
    pos: usize,
}
impl Read for Frames<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.cur.len() {
            if self.src.is_empty() {
                return Ok(0);
            }
            let n = self.src.len().min(16_384);
            self.cur = self.src[..n].to_vec();
            self.src = &self.src[n..];
            self.pos = 0;
        }
        let n = out.len().min(self.cur.len() - self.pos);
        out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[test]
fn map_decode_memory() {
    let mode = if cfg!(feature = "lowmem") { "lowmem" } else { "default" };
    println!("mode={mode}");
    println!("peers | json bytes | buffered: peak(json+parse) retained | streamed: peak retained | kept peers, derp nodes");
    for n in [10usize, 50, 100, 200] {
        let json = map_json(n);
        // Buffered: the JSON accumulates in a Vec (counted) and is parsed.
        let base = reset();
        let mut buf = Vec::new();
        for c in json.chunks(16_384) {
            buf.extend_from_slice(c);
        }
        let m = tsnode::control::decode_map(&buf).unwrap();
        drop(buf);
        let buf_peak = PEAK.load(Relaxed) - base;
        let buf_ret = LIVE.load(Relaxed) - base;
        drop(m);
        // Streamed: only the current 16 KB frame is held.
        let base = reset();
        let m = tsnode::control::decode_map_reader(Frames { src: &json, cur: Vec::new(), pos: 0 }).unwrap();
        let st_peak = PEAK.load(Relaxed) - base;
        let st_ret = LIVE.load(Relaxed) - base;
        let kept = m.peers.as_ref().map(|p| p.len()).unwrap_or(0);
        let dn: usize = m.derp_map.as_ref().map(|d| d.regions.values().map(|r| r.nodes.len()).sum()).unwrap_or(0);
        println!(
            "{n:5} | {:10} | {:>10} {:>10} | {:>10} {:>10} | {kept}, {dn}",
            json.len(), buf_peak, buf_ret, st_peak, st_ret
        );
        drop(m);
    }
}

#[test]
fn struct_sizes() {
    println!(
        "size_of: Tunn={} Node={} MapResponse={} (host, 64-bit)",
        std::mem::size_of::<tsnode::wireguard::Tunn>(),
        std::mem::size_of::<tsnode::types::Node>(),
        std::mem::size_of::<tsnode::types::MapResponse>()
    );
}
