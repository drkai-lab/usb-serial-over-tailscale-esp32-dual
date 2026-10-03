//! `shell::Platform` for the ESP32: wires the setup shell to Wi-Fi, NVS, the
//! Tailscale node and the USB host driver.

use crate::nvs_store::{self, NvsStore};
use crate::tls::EspDialer;
use crate::usb_host::UsbHost;
use crate::wifi::WifiManager;
use adapter_core::shell::{ApInfo, NodeState, Platform, SavedConfig, StatusSnapshot};
use esp_idf_svc::sys;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tsnode::net::Spawner;
use tsnode::node::{Node, NodeConfig, NodeState as TsState};

pub struct EspPlatform {
    pub store: Arc<NvsStore>,
    pub wifi: Arc<WifiManager>,
    pub node: Mutex<Option<Node>>,
    /// The USB host, set once it is started (normal mode only).
    pub usb: OnceLock<UsbHost>,
    pub setup_mode: bool,
}

impl EspPlatform {
    pub fn node(&self) -> Option<Node> {
        self.node.lock().unwrap().clone()
    }

    /// Builds the node configuration from the saved settings.
    pub fn node_config(cfg: &SavedConfig) -> NodeConfig {
        NodeConfig {
            control_url: "https://controlplane.tailscale.com".into(),
            auth_key: cfg.auth_key.clone(),
            hostname: if cfg.hostname.is_empty() {
                "target-console".into()
            } else {
                cfg.hostname.clone()
            },
            version: crate::FIRMWARE_VERSION.into(),
            udp_port: 41641,
            local_ip: None,
            worker_stack: 20 * 1024,
            control_stack: 40 * 1024,
            fallback_region_code: "tok".into(),
            spawner: Some(spawner()),
        }
    }
}

impl Platform for EspPlatform {
    fn status(&self) -> StatusSnapshot {
        let node = self.node().map(|n| n.status());
        let (derp, peers) = match &node {
            Some(s) => (
                if s.home_derp == 0 {
                    "none".to_string()
                } else {
                    format!(
                        "region {}{}",
                        s.home_derp,
                        if s.derp_connected {
                            " (connected)"
                        } else {
                            " (down)"
                        }
                    )
                },
                {
                    let active: Vec<String> = s
                        .peers
                        .iter()
                        .filter(|p| p.active || p.has_session)
                        .map(|p| {
                            format!(
                                "{}{}",
                                p.name,
                                p.direct
                                    .map(|d| format!("@{d}"))
                                    .unwrap_or_else(|| "@derp".into())
                            )
                        })
                        .collect();
                    format!(
                        "{} known, active: {}",
                        s.peers.len(),
                        if active.is_empty() {
                            "-".into()
                        } else {
                            active.join(", ")
                        }
                    )
                },
            ),
            None => ("-".into(), "-".into()),
        };
        let heap = unsafe {
            format!(
                "free {} KB, min {} KB, internal {} KB",
                sys::esp_get_free_heap_size() / 1024,
                sys::esp_get_minimum_free_heap_size() / 1024,
                sys::esp_get_free_internal_heap_size() / 1024
            )
        };
        StatusSnapshot {
            firmware: crate::FIRMWARE_VERSION.into(),
            setup_mode: self.setup_mode,
            wifi: self.wifi.describe(),
            node: Some(self.node_state()),
            derp,
            peers,
            usb: match self.usb.get() {
                Some(h) => format!("{} ({})", h.describe(), crate::usb_host::root_port_state()),
                None if self.setup_mode => "device mode (setup console)".into(),
                None => "starting".into(),
            },
            heap,
            last_reset: last_reset().describe(),
        }
    }

    fn scan_wifi(&self) -> Result<Vec<ApInfo>, String> {
        self.wifi.scan().map_err(|e| e.to_string())
    }

    fn connect_wifi(&self, ssid: &str, password: &str) -> Result<String, String> {
        self.wifi
            .connect(ssid, password, Duration::from_secs(20))
            .map(|ip| ip.to_string())
            .map_err(|e| e.to_string())
    }

    fn reconnect_wifi(&self) -> Result<String, String> {
        if let Some(c) = self.config() {
            self.wifi.set_networks(c.networks);
        }
        self.wifi
            .connect_best(Duration::from_secs(30))
            .map(|ip| ip.to_string())
            .map_err(|e| e.to_string())
    }

    fn config(&self) -> Option<SavedConfig> {
        nvs_store::load_config(self.store.as_ref())
    }

    fn save_config(&self, cfg: &SavedConfig) -> Result<(), String> {
        nvs_store::save_config(self.store.as_ref(), cfg).map_err(|e| e.to_string())?;
        self.wifi.set_networks(cfg.networks.clone());
        Ok(())
    }

    fn start_node(&self) -> Result<(), String> {
        let mut slot = self.node.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let cfg = self.config().ok_or("not configured")?;
        let node = Node::start(
            Self::node_config(&cfg),
            self.store.clone(),
            Arc::new(EspDialer),
        )
        .map_err(|e| e.to_string())?;
        if cfg.auth_key.is_some() {
            // Once registered the key is not needed again; drop it from flash.
            let (n, store) = (node.clone(), self.store.clone());
            let _ = spawn_thread(
                "authkey-scrub",
                6 * 1024,
                true,
                Box::new(move || {
                    while !matches!(n.status().state, TsState::Running) {
                        std::thread::sleep(Duration::from_secs(5));
                    }
                    match nvs_store::forget_auth_key(store.as_ref()) {
                        Ok(()) => log::info!("auth key removed from flash after registration"),
                        Err(e) => log::warn!("could not remove auth key: {e}"),
                    }
                }),
            );
        }
        *slot = Some(node);
        Ok(())
    }

    fn node_state(&self) -> NodeState {
        match self.node() {
            None => NodeState::Off,
            Some(n) => {
                let s = n.status();
                match s.state {
                    TsState::Starting | TsState::ConnectingControl | TsState::Registering => {
                        NodeState::Connecting
                    }
                    TsState::NeedsLogin(url) => NodeState::NeedsLogin(url),
                    TsState::Running => NodeState::Running {
                        addresses: s.addresses.iter().map(|a| a.to_string()).collect(),
                        dns_name: s.dns_name,
                    },
                    TsState::Error(e) => NodeState::Error(e),
                }
            }
        }
    }

    fn is_registered(&self) -> bool {
        nvs_store::is_registered(self.store.as_ref())
    }

    fn factory_reset(&self) -> Result<(), String> {
        nvs_store::factory_reset(self.store.as_ref()).map_err(|e| e.to_string())
    }

    fn reboot(&self) -> ! {
        std::thread::sleep(Duration::from_millis(300));
        unsafe { sys::esp_restart() }
    }
}

/// Why the chip last reset, from `esp_reset_reason()`.
///
/// The IDF's brownout handler ends in a software system reset, so the ROM
/// banner says `rst:0x3 (RTC_SW_SYS_RST)`, the same as after `reboot`, and
/// its "Brownout detector was triggered" line can be missing from the log
/// (it was, on every one of the 39 brownouts of usb-serial-1 on 2026-10-03).
/// The reason the IDF keeps in RTC memory across the reset is the record
/// that survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    PowerOn,
    Software,
    Panic,
    Watchdog,
    Brownout,
    Other(sys::esp_reset_reason_t),
}

impl ResetReason {
    pub fn from_raw(r: sys::esp_reset_reason_t) -> Self {
        match r {
            sys::esp_reset_reason_t_ESP_RST_POWERON => ResetReason::PowerOn,
            sys::esp_reset_reason_t_ESP_RST_SW => ResetReason::Software,
            sys::esp_reset_reason_t_ESP_RST_PANIC => ResetReason::Panic,
            sys::esp_reset_reason_t_ESP_RST_INT_WDT
            | sys::esp_reset_reason_t_ESP_RST_TASK_WDT
            | sys::esp_reset_reason_t_ESP_RST_WDT => ResetReason::Watchdog,
            sys::esp_reset_reason_t_ESP_RST_BROWNOUT => ResetReason::Brownout,
            other => ResetReason::Other(other),
        }
    }

    /// True for a reset nobody asked for.
    pub fn is_fault(self) -> bool {
        matches!(
            self,
            ResetReason::Panic | ResetReason::Watchdog | ResetReason::Brownout
        )
    }

    pub fn describe(self) -> String {
        match self {
            ResetReason::PowerOn => "power-on".into(),
            ResetReason::Software => "software (reboot command or esp_restart)".into(),
            ResetReason::Panic => "panic".into(),
            ResetReason::Watchdog => "watchdog".into(),
            ResetReason::Brownout => {
                "brownout: the chip supply dropped below the detector threshold".into()
            }
            ResetReason::Other(n) => format!("esp_reset_reason {n}"),
        }
    }
}

pub fn last_reset() -> ResetReason {
    ResetReason::from_raw(unsafe { sys::esp_reset_reason() })
}

/// Thread spawner that puts the stacks of network worker threads in PSRAM.
/// Threads that may write flash (NVS) must keep internal-RAM stacks because
/// the flash cache is disabled during writes; only the control thread does.
pub fn spawner() -> Spawner {
    Arc::new(|name: &str, stack: usize, f: Box<dyn FnOnce() + Send>| {
        spawn_thread(name, stack, name != "control", f)
    })
}

pub fn spawn_thread(
    name: &str,
    stack: usize,
    psram: bool,
    f: Box<dyn FnOnce() + Send>,
) -> std::io::Result<()> {
    use esp_idf_svc::hal::task::thread::{MallocCap, ThreadSpawnConfiguration};
    let cfg = ThreadSpawnConfiguration {
        stack_size: stack,
        inherit: false,
        stack_alloc_caps: if psram {
            MallocCap::Spiram | MallocCap::Cap8bit
        } else {
            MallocCap::Internal | MallocCap::Cap8bit
        },
        ..Default::default()
    };
    cfg.set()
        .map_err(|e| std::io::Error::other(format!("thread config: {e}")))?;
    let r = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(stack)
        .spawn(f)
        .map(|_| ());
    let _ = ThreadSpawnConfiguration::default().set();
    r
}
