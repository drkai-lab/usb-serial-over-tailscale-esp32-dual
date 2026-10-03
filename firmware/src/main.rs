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
#![allow(unexpected_cfgs)]

mod bridge;
mod button;
mod console;
mod nvs_store;
mod platform;
mod tls;
mod usb_host;
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
    let store = Arc::new(nvs_store::NvsStore::open(nvs_part.clone(), "tsusb")?);

    // UART0 (GPIO43/44 on ESP32-S3) for the shell; log output shares the pins.
    let uart = UartDriver::new(
        peripherals.uart0,
        peripherals.pins.gpio43,
        peripherals.pins.gpio44,
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
    let cdc = if setup_mode {
        match console::UsbSerialConsole::install(
            peripherals.usb_serial,
            peripherals.pins.gpio19,
            peripherals.pins.gpio20,
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("setup: USB serial console unavailable: {e:#}; use UART0");
                None
            }
        }
    } else {
        None
    };

    let platform = Arc::new(platform::EspPlatform {
        store: store.clone(),
        wifi: wifi.clone(),
        node: std::sync::Mutex::new(None),
        usb: OnceLock::new(),
        setup_mode,
    });

    if let Some(cfg) = &config {
        // Bring Wi-Fi up in the background; the shell can still reconfigure it.
        wifi.set_networks(cfg.networks.clone());
        let w = wifi.clone();
        std::thread::Builder::new()
            .name("wifi-connect".into())
            .stack_size(6 * 1024)
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

    // USB host: every serial interface of the target gets a bridge on
    // `port + interface` the first time the device shows it.
    if !setup_mode {
        let cfg = config.as_ref().expect("normal mode has a configuration");
        let baud = cfg.baud_rate.unwrap_or(DEFAULT_BAUD);
        let base_port = cfg.tcp_port.unwrap_or(DEFAULT_TCP_PORT);
        let forced = cfg
            .usb_driver
            .as_deref()
            .and_then(usb_host::Driver::parse)
            .flatten();
        let node = platform.node().expect("node started in normal mode");
        let host = usb_host::UsbHost::start(
            baud,
            forced,
            base_port,
            Box::new(move |interface, port, serial| {
                let node = node.clone();
                let spawned = platform::spawn_thread(
                    &format!("bridge{port}"),
                    8 * 1024,
                    true,
                    Box::new(move || {
                        if let Err(e) = bridge::run(node, serial, port) {
                            warn!("bridge: interface {interface} on port {port} stopped: {e:#}");
                        }
                    }),
                );
                if let Err(e) = spawned {
                    warn!("bridge: interface {interface} on port {port} not started: {e}");
                }
            }),
        )
        .context("usb host")?;
        if platform.usb.set(host).is_err() {
            warn!("usb: host started twice");
        }
    }

    // Shell on the USB CDC console (setup mode): wizard runs when a terminal opens.
    if let Some(mut cdc) = cdc {
        let p = platform.clone();
        std::thread::Builder::new()
            .name("shell-usb".into())
            .stack_size(12 * 1024)
            .spawn(move || {
                while !cdc.terminal_open() {
                    std::thread::sleep(Duration::from_millis(100));
                }
                std::thread::sleep(Duration::from_millis(300));
                let mut sh = Shell::new(&mut cdc, p.as_ref());
                sh.run(true, false);
            })?;
    }

    // Shell on UART0 (always). Quiet: prompt appears after Enter.
    {
        let p = platform.clone();
        std::thread::Builder::new()
            .name("shell-uart".into())
            .stack_size(12 * 1024)
            .spawn(move || {
                let mut con = console::UartConsole::new(uart);
                let mut sh = Shell::new(&mut con, p.as_ref());
                sh.run(false, true);
            })?;
    }

    spawn_supervisor(platform.clone());

    if setup_mode {
        info!("setup: open the USB serial port (or press Enter on UART0) to configure");
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Periodic status log, subscribed to the Task Watchdog. Building the status
/// takes the node, magicsock and DERP locks, so a deadlock or a stuck network
/// thread stops this loop and the watchdog (60 s, panic and reboot) fires.
fn spawn_supervisor(platform: Arc<platform::EspPlatform>) {
    let _ = std::thread::Builder::new()
        .name("supervisor".into())
        .stack_size(8 * 1024)
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
