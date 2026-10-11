//! The chip-to-chip link (control chip B <-> data chip A) on UART1 with
//! RTS/CTS flow control. Same pins on both chips; the wires cross
//! (TX->RX, RTS->CTS). See README_split.md.
//!
//! | signal | GPIO |
//! |--------|------|
//! | TX     | 25   |
//! | RX     | 26   |
//! | RTS    | 33   |
//! | CTS    | 32   |

use anyhow::Result;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{Gpio25, Gpio26, Gpio32, Gpio33};
use esp_idf_svc::hal::uart::config::{Config, FlowControl};
use esp_idf_svc::hal::uart::{UartDriver, UART1};
use esp_idf_svc::hal::units::Hertz;
use splitlink::{Event, Link, LinkConfig, LinkRead};
use std::io::{self, Write};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

/// Default link speed; both chips must use the same value.
pub const LINK_BAUD: u32 = 2_000_000;

struct Drv(UartDriver<'static>);
// The IDF UART driver locks internally; one reader and one writer thread.
unsafe impl Send for Drv {}
unsafe impl Sync for Drv {}

struct Reader(Arc<Drv>);
impl LinkRead for Reader {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.0
             .0
            .read(buf, TickType::from(timeout).0.max(1))
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

struct Writer(Arc<Drv>);
impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut off = 0;
        while off < buf.len() {
            off += self
                .0
                 .0
                .write(&buf[off..])
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn link_config() -> LinkConfig {
    LinkConfig {
        // A 1.5 KB frame takes ~8 ms at 2 Mbit/s; the ACK comes back after
        // the receiver's task wakes up.
        ack_timeout: Duration::from_millis(60),
        max_retries: 20,
        keepalive: Duration::from_millis(500),
        peer_timeout: Duration::from_secs(3),
        max_payload: 1536,
        rx_stack: 6 * 1024,
    }
}

pub fn start(
    uart: UART1<'static>,
    tx: Gpio25<'static>,
    rx: Gpio26<'static>,
    cts: Gpio32<'static>,
    rts: Gpio33<'static>,
    baud: u32,
) -> Result<(Link, Receiver<Event>)> {
    let cfg = Config::new()
        .baudrate(Hertz(baud))
        .flow_control(FlowControl::CTSRTS)
        .flow_control_rts_threshold(96)
        .rx_fifo_size(4096)
        .tx_fifo_size(2048);
    let drv = UartDriver::new(uart, tx, rx, Some(cts), Some(rts), &cfg)?;
    let drv = Arc::new(Drv(drv));
    let epoch = unsafe { esp_idf_svc::sys::esp_random() };
    let (link, ev) = Link::start(
        Box::new(Reader(drv.clone())),
        Box::new(Writer(drv)),
        link_config(),
        epoch,
        &crate::platform::spawner(),
    )?;
    log::info!("link: UART1 {baud} baud, RTS/CTS, epoch {epoch:08x}");
    Ok((link, ev))
}
