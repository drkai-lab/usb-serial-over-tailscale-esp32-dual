//! Data chip ("A") of the two-chip split: Wi-Fi, WireGuard (the node private
//! key lives only here), disco/STUN, DERP, ACL, smoltcp, the TCP bridge and
//! the target console on UART2. Configuration comes from the control chip
//! over the UART1 link; when that chip is away A keeps running on the last
//! configuration it applied.
//!
//! There is no setup shell on this chip: Wi-Fi credentials, TCP port and
//! baud rate are set on the control chip and forwarded (stored in NVS here).

use crate::{bridge, link_uart, nvs_store, platform, tls::EspDialer, uart_serial, wifi};
use adapter_core::shell::{SavedConfig, WifiNetwork, DEFAULT_TCP_PORT};
use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use log::{info, warn};
use splitlink::msg::DeviceConfig;
use std::sync::Arc;
use std::time::Duration;
use tsnode::dataplane::{DataPlane, LocalConfig, LocalDataPlane};
use tsnode::split::{DataServer, DataServerConfig};

fn spawn_wifi_connect(w: Arc<wifi::WifiManager>) {
    let _ = std::thread::Builder::new()
        .name("wifi-connect".into())
        .stack_size(5 * 1024)
        .spawn(move || {
            if let Err(e) = w.connect_best(Duration::from_secs(30)) {
                warn!("wifi: {e}");
            }
        });
}

pub fn run(
    peripherals: Peripherals,
    sysloop: EspSystemEventLoop,
    nvs_part: EspDefaultNvsPartition,
) -> Result<()> {
    info!("role: data chip (A)");
    let store = Arc::new(nvs_store::NvsStore::open(nvs_part.clone(), "tsdata")?);
    let config = nvs_store::load_config(store.as_ref());

    let wifi =
        wifi::WifiManager::new(peripherals.modem, sysloop.clone(), nvs_part).context("wifi")?;
    let _sntp = esp_idf_svc::sntp::EspSntp::new_default().ok();
    if let Some(cfg) = &config {
        wifi.set_networks(cfg.networks.clone());
        spawn_wifi_connect(wifi.clone());
    } else {
        info!("no Wi-Fi configuration yet; waiting for the control chip");
    }

    let dp = LocalDataPlane::start(
        LocalConfig {
            udp_port: 41641,
            local_ip: None,
            worker_stack: 12 * 1024,
            spawner: platform::spawner(),
        },
        store.clone(),
        Arc::new(EspDialer),
    )
    .map_err(|e| anyhow::anyhow!(e))
    .context("data plane")?;

    let (link, events) = link_uart::start(
        peripherals.uart1,
        peripherals.pins.gpio25,
        peripherals.pins.gpio26,
        peripherals.pins.gpio32,
        peripherals.pins.gpio33,
        link_uart::LINK_BAUD,
    )
    .context("link uart")?;

    let running = config.clone();
    let (w, s) = (wifi.clone(), store.clone());
    let on_device = Box::new(move |d: DeviceConfig| {
        let networks: Vec<WifiNetwork> = d
            .wifi
            .iter()
            .map(|(ssid, password)| WifiNetwork { ssid: ssid.clone(), password: password.clone() })
            .collect();
        let new = SavedConfig {
            networks: networks.clone(),
            tcp_port: Some(d.tcp_port),
            baud_rate: Some(d.baud),
            ..Default::default()
        };
        if nvs_store::load_config(s.as_ref()).as_ref() == Some(&new) {
            return;
        }
        if let Err(e) = nvs_store::save_config(s.as_ref(), &new) {
            warn!("device config: save failed: {e}");
            return;
        }
        info!("device config from control chip saved ({} networks)", networks.len());
        let restart = match &running {
            None => true, // bridge not started yet
            Some(r) => r.tcp_port != new.tcp_port || r.baud_rate != new.baud_rate,
        };
        if restart {
            info!("restarting to apply port/baud");
            std::thread::sleep(Duration::from_millis(300));
            unsafe { sys::esp_restart() };
        }
        w.set_networks(networks);
        spawn_wifi_connect(w.clone());
    });
    let server = DataServer::start(
        link,
        events,
        dp.clone(),
        DataServerConfig {
            max_payload: link_uart::link_config().max_payload,
            status_period: Duration::from_secs(2),
            has_device_config: config.is_some(),
            stack: 12 * 1024,
        },
        Some(on_device),
        &platform::spawner(),
    )?;

    // Target console on UART2: TX=GPIO17, RX=GPIO16.
    if let Some(cfg) = &config {
        let baud = cfg.baud_rate.unwrap_or(crate::DEFAULT_BAUD);
        let port = cfg.tcp_port.unwrap_or(DEFAULT_TCP_PORT);
        let serial = uart_serial::UartSerial::new(
            peripherals.uart2,
            peripherals.pins.gpio17,
            peripherals.pins.gpio16,
            baud,
        )
        .context("target uart")?;
        let listener = dp.net().listen(port);
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

    // Supervisor on the main task, subscribed to the task watchdog.
    unsafe {
        sys::esp_task_wdt_add(core::ptr::null_mut());
    }
    let mut tick = 0u32;
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let applied = server.applied();
        let home = dp.home_derp();
        unsafe {
            sys::esp_task_wdt_reset();
        }
        tick += 1;
        if tick % 6 == 0 {
            info!(
                "status: wifi={} link={} gen={} peers={} derp={}{} heap free {} KB min {} KB",
                wifi.describe(),
                if server.link().is_up() { "up" } else { "down" },
                applied.gen,
                applied.peers,
                home,
                if home != 0 && dp.derp_connected(home) { "(connected)" } else { "" },
                unsafe { sys::esp_get_free_heap_size() } / 1024,
                unsafe { sys::esp_get_minimum_free_heap_size() } / 1024,
            );
        }
    }
}
