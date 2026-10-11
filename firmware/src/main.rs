//! usb-serial-over-tailscale firmware for ESP32-S3 N16R8.
//!
//! Boot flow:
//! - Setup Mode (BOOT button held, or Wi-Fi/Tailscale not yet set up): the
//!   native USB port is a CDC-ACM serial device running an interactive setup
//!   shell; the same shell is available on UART0.
//! - Normal Mode: native USB port is a USB host for the target's serial
//!   console(s); Wi-Fi + Tailscale node + one TCP bridge per serial interface
//!   of the target, on the first port plus the interface number. The shell
//!   stays available on UART0 (`status`, `setup`, ...).
//!
//! Features `role-control` / `role-data` build the two halves of the
//! two-chip split instead (README_split.md).
#![allow(unexpected_cfgs)]
// Each role uses a subset of the shared modules.
#![cfg_attr(any(feature = "role-control", feature = "role-data"), allow(dead_code, unused_imports))]

#[cfg(all(feature = "role-control", feature = "role-data"))]
compile_error!("features role-control and role-data are mutually exclusive");

mod bridge;
mod button;
mod console;
#[cfg(any(feature = "role-control", feature = "role-data"))]
mod link_uart;
mod nvs_store;
mod platform;
#[cfg(feature = "role-control")]
mod role_control;
#[cfg(feature = "role-data")]
mod role_data;
mod tls;
mod uart_serial;
mod wifi;

use adapter_core::shell::{Platform, Shell, DEFAULT_TCP_PORT};
use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::AnyIOPin;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::uart::UartDriver;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{info, warn};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_BAUD: u32 = 115_200;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();
    info!("usb-serial-over-tailscale firmware {FIRMWARE_VERSION} starting");
    let reset = platform::last_reset();
    if reset.is_fault() {
        warn!("last reset: {}", reset.describe());
    } else {
        info!("last reset: {}", reset.describe());
    }

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs_part = EspDefaultNvsPartition::take()?;
    #[cfg(feature = "role-data")]
    {
        // Logs stay on UART0 (default console); no shell on this chip.
        role_data::run(peripherals, sysloop, nvs_part)
    }
    #[cfg(not(feature = "role-data"))]
    node_main(peripherals, sysloop, nvs_part)
}

/// Single-chip node, or the control chip of the split.
#[cfg(not(feature = "role-data"))]
fn node_main(
    peripherals: Peripherals,
    sysloop: EspSystemEventLoop,
    nvs_part: EspDefaultNvsPartition,
) -> Result<()> {
    let store = Arc::new(nvs_store::NvsStore::open(nvs_part.clone(), "tsusb")?);

    // UART0 (GPIO1/3, the DevKitC's USB-UART bridge) for the shell and logs.
    let uart = UartDriver::new(
        peripherals.uart0,
        peripherals.pins.gpio1,
        peripherals.pins.gpio3,
        Option::<AnyIOPin>::None,
        Option::<AnyIOPin>::None,
        &Default::default(),
    )?;

    let mut boot_button = button::Button::new(peripherals.pins.gpio0)?;
    let setup_requested = boot_button.held_at_boot();

    let config = nvs_store::load_config(store.as_ref());
    let registered = nvs_store::is_registered(store.as_ref());
    let setup_mode = setup_requested || config.is_none() || !registered;
    info!(
        "boot mode: {}",
        if setup_requested {
            "setup (button)"
        } else if config.is_none() {
            "setup (no Wi-Fi configured)"
        } else if !registered {
            "setup (Tailscale login not completed)"
        } else {
            "normal"
        }
    );

    let wifi =
        wifi::WifiManager::new(peripherals.modem, sysloop.clone(), nvs_part).context("wifi")?;
    let _sntp = esp_idf_svc::sntp::EspSntp::new_default().ok();

    // Native USB port: host (normal) or CDC device (setup); never both.
    // The host is started below, after the node, so a target present at
    // boot finds a node to be bridged on.

    let platform = Arc::new(platform::EspPlatform {
        store: store.clone(),
        wifi: wifi.clone(),
        node: std::sync::Mutex::new(None),
        usb: OnceLock::new(),
        setup_mode,
        #[cfg(feature = "role-control")]
        remote: OnceLock::new(),
    });

    #[cfg(feature = "role-control")]
    role_control::attach(
        &platform,
        peripherals.uart1,
        peripherals.pins.gpio25,
        peripherals.pins.gpio26,
        peripherals.pins.gpio32,
        peripherals.pins.gpio33,
    )
    .context("link")?;

    if let Some(cfg) = &config {
        // Bring Wi-Fi up in the background; the shell can still reconfigure it.
        wifi.set_networks(cfg.networks.clone());
        let w = wifi.clone();
        std::thread::Builder::new()
            .name("wifi-connect".into())
            .stack_size(5 * 1024)
            .spawn(move || {
                if let Err(e) = w.connect_best(Duration::from_secs(30)) {
                    warn!("wifi: {e}");
                }
            })?;
        if !setup_mode {
            platform
                .start_node()
                .map_err(|e| anyhow::anyhow!(e))
                .context("node")?;
        }
    }

    // Target console on UART2: TX=GPIO17, RX=GPIO16 (free without PSRAM).
    // In the split it is on the data chip.
    #[cfg(not(feature = "role-control"))]
    if !setup_mode {
        let cfg = config.as_ref().expect("normal mode has a configuration");
        let baud = cfg.baud_rate.unwrap_or(DEFAULT_BAUD);
        let port = cfg.tcp_port.unwrap_or(DEFAULT_TCP_PORT);
        let serial = uart_serial::UartSerial::new(
            peripherals.uart2,
            peripherals.pins.gpio17,
            peripherals.pins.gpio16,
            baud,
        )
        .context("target uart")?;
        let _ = platform.usb.set(serial.clone());
        let node = platform.node().expect("node started in normal mode");
        let listener = node.listen(port);
        platform::spawn_thread(
            &format!("bridge{port}"),
            6 * 1024,
            true,
            Box::new(move || {
                if let Err(e) = bridge::run(listener, serial, port) {
                    warn!("bridge: port {port} stopped: {e:#}");
                }
            }),
        )?;
    }

    spawn_supervisor(platform.clone());

    if setup_mode {
        info!("setup: open the USB serial port (or press Enter on UART0) to configure");
    }
    // The UART0 shell runs on the main task (CONFIG_ESP_MAIN_TASK_STACK_SIZE)
    // instead of a thread of its own: one 12 KB stack less.
    let mut con = console::UartConsole::new(uart);
    let mut sh = Shell::new(&mut con, platform.as_ref());
    sh.run(setup_mode, !setup_mode);
    Ok(())
}

/// Periodic status log, subscribed to the Task Watchdog. Building the status
/// takes the node, magicsock and DERP locks, so a deadlock or a stuck network
/// thread stops this loop and the watchdog (60 s, panic and reboot) fires.
#[cfg(not(feature = "role-data"))]
fn spawn_supervisor(platform: Arc<platform::EspPlatform>) {
    let _ = std::thread::Builder::new()
        .name("supervisor".into())
        .stack_size(4 * 1024)
        .spawn(move || {
            unsafe {
                esp_idf_svc::sys::esp_task_wdt_add(core::ptr::null_mut());
            }
            let mut tick = 0u32;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let s = platform.status();
                unsafe {
                    esp_idf_svc::sys::esp_task_wdt_reset();
                }
                tick += 1;
                if tick % 6 == 0 {
                    info!(
                        "status: wifi={} node={:?} derp={} peers={} usb={} heap={}",
                        s.wifi, s.node, s.derp, s.peers, s.usb, s.heap
                    );
                }
            }
        });
}
