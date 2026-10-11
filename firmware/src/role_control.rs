//! Control chip ("B") of the two-chip split: Wi-Fi, the ts2021 control
//! client (Noise, HTTP/2, map stream), the machine key and the setup shell.
//! Everything that touches packets runs on the data chip; this chip drives
//! it through `tsnode::split::RemoteDataPlane` over the UART1 link.

use crate::{link_uart, platform::EspPlatform};
use adapter_core::shell::{SavedConfig, DEFAULT_TCP_PORT};
use anyhow::Result;
use esp_idf_svc::hal::gpio::{Gpio25, Gpio26, Gpio32, Gpio33};
use esp_idf_svc::hal::uart::UART1;
use splitlink::msg::DeviceConfig;
use std::sync::Arc;
use tsnode::split::RemoteDataPlane;

pub fn device_config(cfg: &SavedConfig) -> DeviceConfig {
    DeviceConfig {
        wifi: cfg.networks.iter().map(|n| (n.ssid.clone(), n.password.clone())).collect(),
        tcp_port: cfg.tcp_port.unwrap_or(DEFAULT_TCP_PORT),
        baud: cfg.baud_rate.unwrap_or(crate::DEFAULT_BAUD),
    }
}

/// Opens the link and starts the remote data plane (also in setup mode, so
/// the data chip gets Wi-Fi credentials as soon as they are saved).
pub fn attach(
    platform: &EspPlatform,
    uart: UART1<'static>,
    tx: Gpio25<'static>,
    rx: Gpio26<'static>,
    cts: Gpio32<'static>,
    rts: Gpio33<'static>,
) -> Result<()> {
    log::info!("role: control chip (B)");
    let (link, events) = link_uart::start(uart, tx, rx, cts, rts, link_uart::LINK_BAUD)?;
    let rdp = RemoteDataPlane::start(
        link,
        events,
        link_uart::link_config().max_payload,
        &crate::platform::spawner(),
        8 * 1024,
    )?;
    if let Some(cfg) = nvs_store_config(platform) {
        rdp.set_device_config(device_config(&cfg));
    }
    let _ = platform.remote.set(rdp);
    Ok(())
}

fn nvs_store_config(p: &EspPlatform) -> Option<SavedConfig> {
    crate::nvs_store::load_config(p.store.as_ref())
}

pub fn describe(rdp: &Arc<RemoteDataPlane>) -> String {
    match rdp.data_status() {
        None => "data chip: not seen yet".into(),
        Some(a) => format!(
            "data chip: link {}, config gen {}/{}, endpoints {:?}",
            if rdp.link().is_up() { "up" } else { "down" },
            a.applied_gen,
            rdp.generation(),
            a.endpoints
        ),
    }
}
