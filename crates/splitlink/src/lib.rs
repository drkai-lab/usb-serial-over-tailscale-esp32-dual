//! `splitlink`: the wire between the two chips of a split Tailscale node.
//!
//! - [`cobs`], [`crc`]: byte stuffing and integrity.
//! - [`frame`]: one link frame (DATA / ACK / PING) with epochs and sequence.
//! - [`link`]: stop-and-wait ARQ over any byte stream, peer up/down and
//!   reboot detection (each side picks a random epoch at boot).
//! - [`msg`]: application messages (control chip <-> data chip), postcard.
//!
//! Plain `std` (threads, mpsc), so it runs unchanged on ESP-IDF and on the
//! host, where the tests run.

pub mod cobs;
pub mod crc;
pub mod frame;
pub mod link;
pub mod msg;

pub use link::{Event, Link, LinkConfig, LinkRead, SendError, Spawner};
