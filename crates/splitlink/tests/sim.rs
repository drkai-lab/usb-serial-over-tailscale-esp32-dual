//! Link over a simulated wire that drops, corrupts, duplicates and reorders
//! frames (each `write` from the link is one frame).

use splitlink::link::{random_epoch, std_spawner};
use splitlink::{Event, Link, LinkConfig, LinkRead};
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Default)]
struct Impair {
    drop: u32,
    corrupt: u32,
    dup: u32,
    reorder: u32, // percent each
}

struct Pipe {
    q: Mutex<VecDeque<u8>>,
    cv: Condvar,
}

struct Wire {
    pipe: Arc<Pipe>,
    imp: Impair,
    rng: u64,
    held: Option<Vec<u8>>,
}

impl Wire {
    fn rand(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % 100) as u32
    }
    fn deliver(&self, b: &[u8]) {
        self.pipe.q.lock().unwrap().extend(b);
        self.pipe.cv.notify_all();
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut b = buf.to_vec();
        if self.rand() < self.imp.drop {
            return Ok(buf.len());
        }
        if self.rand() < self.imp.corrupt && b.len() > 2 {
            let i = (self.rand() as usize * 7919) % (b.len() - 1);
            b[i] ^= 0x24;
        }
        if self.rand() < self.imp.reorder && self.held.is_none() {
            self.held = Some(b);
            return Ok(buf.len());
        }
        self.deliver(&b);
        if let Some(h) = self.held.take() {
            self.deliver(&h);
        }
        if self.rand() < self.imp.dup {
            self.deliver(&b);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct End {
    pipe: Arc<Pipe>,
}

impl LinkRead for End {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let deadline = Instant::now() + timeout;
        let mut q = self.pipe.q.lock().unwrap();
        while q.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                return Ok(0);
            }
            q = self.pipe.cv.wait_timeout(q, deadline - now).unwrap().0;
        }
        let n = buf.len().min(q.len());
        for (i, b) in q.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n)
    }
}

fn pipe() -> Arc<Pipe> {
    Arc::new(Pipe {
        q: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
    })
}

fn cfg() -> LinkConfig {
    LinkConfig {
        ack_timeout: Duration::from_millis(15),
        max_retries: 200,
        keepalive: Duration::from_millis(100),
        peer_timeout: Duration::from_millis(600),
        ..Default::default()
    }
}

fn start(rx: &Arc<Pipe>, tx: &Arc<Pipe>, imp: Impair, seed: u64, epoch: u32) -> (Link, Receiver<Event>) {
    Link::start(
        Box::new(End { pipe: rx.clone() }),
        Box::new(Wire { pipe: tx.clone(), imp, rng: seed, held: None }),
        cfg(),
        epoch,
        &std_spawner(),
    )
    .unwrap()
}

fn messages(rx: &Receiver<Event>, n: usize, timeout: Duration) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut got = Vec::new();
    while got.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Message(m)) => got.push(m),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    got
}

#[test]
fn lossy_wire_delivers_everything_once_in_order() {
    let (ab, ba) = (pipe(), pipe());
    let imp = Impair { drop: 20, corrupt: 10, dup: 10, reorder: 10 };
    let (a, a_ev) = start(&ba, &ab, imp, 0x1234_5678, 11);
    let (b, b_ev) = start(&ab, &ba, imp, 0x9abc_def0, 22);
    const N: usize = 300;
    let sender = |l: Link, tag: u8| {
        std::thread::spawn(move || {
            for i in 0..N {
                let mut p = vec![tag, (i >> 8) as u8, i as u8];
                p.extend(std::iter::repeat(0u8).take(i % 40)); // zeros exercise COBS
                l.send(&p).expect("send");
            }
        })
    };
    let ta = sender(a.clone(), b'A');
    let tb = sender(b.clone(), b'B');
    let at_b = messages(&b_ev, N, Duration::from_secs(60));
    let at_a = messages(&a_ev, N, Duration::from_secs(60));
    ta.join().unwrap();
    tb.join().unwrap();
    for (got, tag) in [(&at_b, b'A'), (&at_a, b'B')] {
        assert_eq!(got.len(), N);
        for (i, m) in got.iter().enumerate() {
            assert_eq!(m[0], tag);
            assert_eq!(((m[1] as usize) << 8) | m[2] as usize, i, "out of order / duplicate");
            assert_eq!(m.len(), 3 + i % 40);
        }
    }
    let (sa, sb) = (a.stats(), b.stats());
    println!("A: {sa:?}\nB: {sb:?}");
    assert!(sa.retransmits > 0 && sb.corrupt + sa.corrupt > 0 && sa.duplicates + sb.duplicates > 0);
    a.shutdown();
    b.shutdown();
}

#[test]
fn peer_reboot_is_reported_and_new_sequence_accepted() {
    let (ab, ba) = (pipe(), pipe());
    let (a1, _a1_ev) = start(&ba, &ab, Impair::default(), 1, 100);
    let (b, b_ev) = start(&ab, &ba, Impair::default(), 2, 200);
    for i in 0..5u8 {
        a1.send(&[i]).unwrap();
    }
    let first: Vec<Event> = (0..6).map(|_| b_ev.recv_timeout(Duration::from_secs(2)).unwrap()).collect();
    assert_eq!(first[0], Event::Up { peer_epoch: 100, restarted: false });
    // "Reboot" A: new epoch, sequence restarts at 0.
    a1.shutdown();
    std::thread::sleep(Duration::from_millis(250));
    let (a2, _a2_ev) = start(&ba, &ab, Impair::default(), 3, random_epoch());
    a2.send(b"after").unwrap();
    let mut saw_restart = false;
    let mut saw_msg = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !(saw_restart && saw_msg) {
        match b_ev.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Up { restarted: true, .. }) => saw_restart = true,
            Ok(Event::Message(m)) if m == b"after" => saw_msg = true,
            _ => {}
        }
    }
    assert!(saw_restart && saw_msg);
    b.shutdown();
    a2.shutdown();
}

#[test]
fn silence_reports_down_and_send_gives_up() {
    let (ab, ba) = (pipe(), pipe());
    let (a, a_ev) = start(&ba, &ab, Impair::default(), 5, 1);
    let (b, _b_ev) = start(&ab, &ba, Impair::default(), 6, 2);
    a.send(b"x").unwrap();
    b.shutdown(); // B goes silent
    let mut down = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !down {
        down = matches!(a_ev.recv_timeout(Duration::from_millis(200)), Ok(Event::Down));
    }
    assert!(down);
    let quick = Link::start(
        Box::new(End { pipe: ba.clone() }),
        Box::new(Wire { pipe: ab.clone(), imp: Impair::default(), rng: 9, held: None }),
        LinkConfig { max_retries: 3, ack_timeout: Duration::from_millis(10), ..cfg() },
        7,
        &std_spawner(),
    )
    .unwrap()
    .0;
    assert_eq!(quick.send(b"nobody"), Err(splitlink::SendError::NoAck));
    a.shutdown();
    quick.shutdown();
}
