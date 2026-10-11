//! Stop-and-wait ARQ over a byte stream.
//!
//! - Every DATA frame is retransmitted every `ack_timeout` until the peer's
//!   ACK (carrying our epoch and the sequence) arrives, at most
//!   `max_retries` times; then `send` fails and the sequence moves on.
//! - The receiver delivers a DATA frame when its sequence is newer than the
//!   last delivered one (serial-number arithmetic), so duplicates and stale
//!   reordered copies are dropped while a sequence skipped by a sender that
//!   gave up does not stall the link. It ACKs every DATA frame it sees.
//! - Each side picks a random epoch at boot. A frame carrying an epoch not
//!   seen before means the peer (re)started: the receive window is reset and
//!   [`Event::Up`] is reported with `restarted` set if an older epoch was
//!   known. Silence for `peer_timeout` reports [`Event::Down`]; PING frames
//!   keep an idle link alive.
//!
//! Lost messages are the application's business (see [`crate::msg`]: the
//! control chip resends its whole configuration when the data chip reports
//! an older generation).

use crate::frame::{Deframer, Frame, Kind, CRC_LEN, HEADER_LEN};
use log::{debug, info, warn};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Same shape as `tsnode::net::Spawner`.
pub type Spawner =
    Arc<dyn Fn(&str, usize, Box<dyn FnOnce() + Send>) -> io::Result<()> + Send + Sync>;

pub fn std_spawner() -> Spawner {
    Arc::new(|name: &str, stack: usize, f: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(stack)
            .spawn(f)
            .map(|_| ())
    })
}

/// Reading half of the transport: returns `Ok(0)` when nothing arrived
/// within `timeout`.
pub trait LinkRead: Send {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
}

#[derive(Clone, Debug)]
pub struct LinkConfig {
    pub ack_timeout: Duration,
    pub max_retries: u32,
    pub keepalive: Duration,
    pub peer_timeout: Duration,
    /// Largest application payload per frame.
    pub max_payload: usize,
    pub rx_stack: usize,
}

impl Default for LinkConfig {
    fn default() -> Self {
        LinkConfig {
            ack_timeout: Duration::from_millis(40),
            max_retries: 25,
            keepalive: Duration::from_millis(500),
            peer_timeout: Duration::from_secs(3),
            max_payload: 1536,
            rx_stack: 6 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// First frame from a new peer epoch. `restarted`: an older epoch was
    /// known, i.e. the peer rebooted (or its link task restarted).
    Up { peer_epoch: u32, restarted: bool },
    /// Nothing heard for `peer_timeout`.
    Down,
    Message(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    TooLarge,
    /// No ACK after all retries; the message may or may not have arrived.
    NoAck,
    Io(String),
    Closed,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SendError {}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub sent: u64,
    pub retransmits: u64,
    pub failed: u64,
    pub delivered: u64,
    pub duplicates: u64,
    pub corrupt: u64,
}

struct AckState {
    /// Last sequence acked by the peer for our epoch.
    acked: Option<u16>,
}

struct RxState {
    peer_epoch: Option<u32>,
    last_delivered: Option<u16>,
    last_heard: Option<Instant>,
    up: bool,
}

struct Inner {
    cfg: LinkConfig,
    epoch: u32,
    writer: Mutex<Box<dyn Write + Send>>,
    send_lock: Mutex<u16>, // next sequence
    ack: Mutex<AckState>,
    ack_cv: Condvar,
    rx: Mutex<RxState>,
    last_tx: Mutex<Instant>,
    closed: AtomicBool,
    stats: Mutex<Stats>,
    frames_in: AtomicU32,
}

#[derive(Clone)]
pub struct Link {
    inner: Arc<Inner>,
}

/// A random-enough boot epoch (never 0).
pub fn random_epoch() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    (h.finish() as u32).max(1)
}

impl Link {
    /// Starts the receive thread. `epoch` should be random per boot
    /// ([`random_epoch`]); on ESP-IDF pass a value from `esp_random()`.
    pub fn start(
        reader: Box<dyn LinkRead>,
        writer: Box<dyn Write + Send>,
        cfg: LinkConfig,
        epoch: u32,
        spawner: &Spawner,
    ) -> io::Result<(Link, Receiver<Event>)> {
        let (tx, rx) = channel();
        let inner = Arc::new(Inner {
            epoch: epoch.max(1),
            writer: Mutex::new(writer),
            send_lock: Mutex::new(0),
            ack: Mutex::new(AckState { acked: None }),
            ack_cv: Condvar::new(),
            rx: Mutex::new(RxState {
                peer_epoch: None,
                last_delivered: None,
                last_heard: None,
                up: false,
            }),
            last_tx: Mutex::new(Instant::now() - Duration::from_secs(3600)),
            closed: AtomicBool::new(false),
            stats: Mutex::new(Stats::default()),
            frames_in: AtomicU32::new(0),
            cfg,
        });
        let link = Link { inner };
        let l2 = link.clone();
        spawner(
            "splitlink-rx",
            link.inner.cfg.rx_stack,
            Box::new(move || l2.rx_loop(reader, tx)),
        )?;
        Ok((link, rx))
    }

    pub fn epoch(&self) -> u32 {
        self.inner.epoch
    }

    pub fn is_up(&self) -> bool {
        self.inner.rx.lock().unwrap().up
    }

    pub fn stats(&self) -> Stats {
        self.inner.stats.lock().unwrap().clone()
    }

    /// Stops the receive thread (after its current read times out).
    pub fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.inner.ack_cv.notify_all();
    }

    fn write_frame(&self, f: &Frame) -> io::Result<()> {
        let mut buf = Vec::with_capacity(f.payload.len() + f.payload.len() / 254 + 24);
        f.encode(&mut buf);
        let mut w = self.inner.writer.lock().unwrap();
        w.write_all(&buf)?;
        w.flush()?;
        *self.inner.last_tx.lock().unwrap() = Instant::now();
        Ok(())
    }

    fn peer_epoch(&self) -> u32 {
        self.inner.rx.lock().unwrap().peer_epoch.unwrap_or(0)
    }

    /// Sends one message and waits for its ACK. Calls are serialized.
    pub fn send(&self, payload: &[u8]) -> Result<(), SendError> {
        if payload.len() > self.inner.cfg.max_payload {
            return Err(SendError::TooLarge);
        }
        let mut next = self.inner.send_lock.lock().unwrap();
        let seq = *next;
        *next = next.wrapping_add(1);
        let frame = Frame {
            kind: Kind::Data,
            src_epoch: self.inner.epoch,
            dst_epoch: self.peer_epoch(),
            seq,
            payload: payload.to_vec(),
        };
        self.inner.stats.lock().unwrap().sent += 1;
        for attempt in 0..=self.inner.cfg.max_retries {
            if self.inner.closed.load(Ordering::Relaxed) {
                return Err(SendError::Closed);
            }
            if attempt > 0 {
                self.inner.stats.lock().unwrap().retransmits += 1;
            }
            self.write_frame(&frame)
                .map_err(|e| SendError::Io(e.to_string()))?;
            let deadline = Instant::now() + self.inner.cfg.ack_timeout;
            let mut a = self.inner.ack.lock().unwrap();
            loop {
                if a.acked == Some(seq) {
                    return Ok(());
                }
                let now = Instant::now();
                if now >= deadline || self.inner.closed.load(Ordering::Relaxed) {
                    break;
                }
                a = self.inner.ack_cv.wait_timeout(a, deadline - now).unwrap().0;
            }
        }
        self.inner.stats.lock().unwrap().failed += 1;
        Err(SendError::NoAck)
    }

    fn rx_loop(&self, mut reader: Box<dyn LinkRead>, events: Sender<Event>) {
        let max_frame = (HEADER_LEN + self.inner.cfg.max_payload + CRC_LEN) * 256 / 254 + 8;
        let mut de = Deframer::new(max_frame);
        let mut buf = [0u8; 256];
        let tick = (self.inner.cfg.keepalive / 2).max(Duration::from_millis(5));
        while !self.inner.closed.load(Ordering::Relaxed) {
            match reader.read(&mut buf, tick) {
                Ok(n) if n > 0 => {
                    let mut frames = Vec::new();
                    let bad = de.push(&buf[..n], |f| frames.push(f));
                    if bad > 0 {
                        self.inner.stats.lock().unwrap().corrupt += bad as u64;
                    }
                    for f in frames {
                        self.inner.frames_in.fetch_add(1, Ordering::Relaxed);
                        self.handle(f, &events);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("splitlink: read error: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            self.timers(&events);
        }
        debug!("splitlink: rx loop exit");
    }

    fn timers(&self, events: &Sender<Event>) {
        let now = Instant::now();
        let idle = now.duration_since(*self.inner.last_tx.lock().unwrap());
        if idle >= self.inner.cfg.keepalive {
            let f = Frame {
                kind: Kind::Ping,
                src_epoch: self.inner.epoch,
                dst_epoch: self.peer_epoch(),
                seq: 0,
                payload: Vec::new(),
            };
            let _ = self.write_frame(&f);
        }
        let mut rx = self.inner.rx.lock().unwrap();
        if rx.up
            && rx
                .last_heard
                .map(|t| now.duration_since(t) > self.inner.cfg.peer_timeout)
                .unwrap_or(true)
        {
            rx.up = false;
            drop(rx);
            info!("splitlink: peer down");
            let _ = events.send(Event::Down);
        }
    }

    fn handle(&self, f: Frame, events: &Sender<Event>) {
        // Any valid frame proves the peer is alive and tells its epoch.
        let mut up_event = None;
        {
            let mut rx = self.inner.rx.lock().unwrap();
            rx.last_heard = Some(Instant::now());
            if rx.peer_epoch != Some(f.src_epoch) {
                let restarted = rx.peer_epoch.is_some();
                rx.peer_epoch = Some(f.src_epoch);
                rx.last_delivered = None;
                rx.up = true;
                up_event = Some(Event::Up {
                    peer_epoch: f.src_epoch,
                    restarted,
                });
            } else if !rx.up {
                rx.up = true;
                up_event = Some(Event::Up {
                    peer_epoch: f.src_epoch,
                    restarted: false,
                });
            }
        }
        if let Some(e) = up_event {
            info!("splitlink: peer up ({e:?})");
            let _ = events.send(e);
        }
        match f.kind {
            Kind::Ping => {}
            Kind::Ack => {
                if f.dst_epoch == self.inner.epoch {
                    let mut a = self.inner.ack.lock().unwrap();
                    a.acked = Some(f.seq);
                    self.inner.ack_cv.notify_all();
                }
            }
            Kind::Data => {
                let ack = Frame {
                    kind: Kind::Ack,
                    src_epoch: self.inner.epoch,
                    dst_epoch: f.src_epoch,
                    seq: f.seq,
                    payload: Vec::new(),
                };
                let deliver = {
                    let mut rx = self.inner.rx.lock().unwrap();
                    let newer = match rx.last_delivered {
                        None => true,
                        Some(last) => (f.seq.wrapping_sub(last) as i16) > 0,
                    };
                    if newer {
                        rx.last_delivered = Some(f.seq);
                    }
                    newer
                };
                // ACK before handing the message on, so the sender is not
                // held up by a slow consumer.
                let _ = self.write_frame(&ack);
                let mut st = self.inner.stats.lock().unwrap();
                if deliver {
                    st.delivered += 1;
                    drop(st);
                    let _ = events.send(Event::Message(f.payload));
                } else {
                    st.duplicates += 1;
                    log::trace!("splitlink: duplicate seq {} from {:08x}", f.seq, f.src_epoch);
                }
            }
        }
    }
}
