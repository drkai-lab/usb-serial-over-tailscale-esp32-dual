//! Transparent bridge between one TCP client on the tailnet and the USB serial
//! console. Bytes are forwarded unchanged in both directions.
//!
//! Accepting runs on its own thread so that a second client is told the
//! bridge is busy right away instead of waiting in the backlog.
//!
//! One bridge serves one channel. A device with several serial interfaces (an
//! FT4232H has four) has one bridge per interface, each on its own port, and
//! "busy" is per bridge: a client on one port does not block the other. The
//! USB interface is claimed for the length of a session, so the host
//! controller's pipes are shared by the ports that are in use.

use crate::uart_serial::{OpenError, UartSerial as UsbSerial};
use anyhow::{anyhow, Result};
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tsnode::netstack::TcpStream;
use tsnode::node::Node;

pub fn run(node: Node, serial: UsbSerial, port: u16) -> Result<()> {
    let listener = node.listen(port);
    info!("bridge: listening on tailnet port {port}");
    let busy = Arc::new(AtomicBool::new(false));
    let accept_busy = busy.clone();
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(1);
    crate::platform::spawn_thread(
        &format!("bridge-accept{port}"),
        3 * 1024,
        true,
        Box::new(move || loop {
            let Some(conn) = listener.accept(Duration::from_secs(1)) else {
                continue;
            };
            if accept_busy.swap(true, Ordering::SeqCst) {
                warn!("bridge: rejecting {} on {port} (busy)", conn.peer_addr());
                let _ = conn.write_all(b"busy: another client is connected\r\n");
                conn.close();
                continue;
            }
            if tx.send(conn).is_err() {
                break;
            }
        }),
    )?;
    loop {
        let conn = rx
            .recv()
            .map_err(|_| anyhow!("bridge: accept thread exited"))?;
        serve(&serial, conn, port)?;
        busy.store(false, Ordering::SeqCst);
    }
}

/// Runs one client session until either side closes.
fn serve(serial: &UsbSerial, conn: TcpStream, port: u16) -> Result<()> {
    let peer = conn.peer_addr();
    info!("bridge: client connected from {peer} on {port}");
    match serial.open() {
        Ok(()) => {}
        Err(OpenError::NoDevice) => info!("bridge: {peer} on {port} waits for a device"),
        Err(OpenError::Failed(e)) => {
            warn!("bridge: rejecting {peer} on {port}: {e}");
            let _ = conn.write_all(format!("usb: cannot open the interface: {e}\r\n").as_bytes());
            conn.close();
            return Ok(());
        }
    }
    let conn = Arc::new(conn);
    let serial2 = serial.clone();
    let c2 = conn.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    // Serial -> TCP
    let rx_done = Arc::new(AtomicBool::new(false));
    let rx_done2 = rx_done.clone();
    crate::platform::spawn_thread(
        &format!("bridge-rx{port}"),
        4 * 1024,
        true,
        Box::new(move || {
            let mut buf = [0u8; 512];
            while !stop2.load(Ordering::Relaxed) {
                let n = serial2.read(&mut buf, Duration::from_millis(100));
                if n > 0 && c2.write_all(&buf[..n]).is_err() {
                    break;
                }
                if c2.is_closed() {
                    break;
                }
            }
            rx_done2.store(true, Ordering::Relaxed);
        }),
    )?;
    // TCP -> Serial
    let mut buf = [0u8; 512];
    let mut dropped_warned = false;
    loop {
        match conn.read(&mut buf, Duration::from_millis(100)) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = serial.write(&buf[..n]) {
                    if !dropped_warned {
                        warn!("bridge: dropping input, {e}");
                        dropped_warned = true;
                    }
                } else {
                    dropped_warned = false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    while !rx_done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
    conn.close();
    serial.close();
    info!("bridge: client {peer} on {port} disconnected");
    Ok(())
}
