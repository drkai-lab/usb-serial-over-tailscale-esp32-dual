//! USB Host with the CDC-ACM class driver: the target board's USB serial
//! console(s). Handles attach/detach without restarting anything else.
//!
//! One [`UsbHost`] owns the host library and the CDC driver. When a device
//! is attached, its configuration descriptor says which of its interfaces
//! are serial ports (an FT4232H has four, a Raspberry Pi Debug Probe one on
//! interface 1); each of those becomes a [`UsbSerial`] channel, created the
//! first time it is seen and kept for the program's life, and the caller is
//! told about it so it can start a bridge on the channel's port (the first
//! port plus the interface number).
//!
//! A channel claims its USB interface only while a client is connected to
//! its port ([`UsbSerial::open`] and [`UsbSerial::close`]). The ESP32-S3's
//! host controller has eight channels ("pipes"): one for control transfers
//! and two per open serial interface (three for CDC-ACM, which adds a
//! notification pipe), so an FT4232H can have three of its four ports open
//! at once but not all four. A channel outlives the device: a session waits
//! while nothing is plugged in and carries on when the device comes back.

use adapter_core::usbdesc::serial_interfaces;
use adapter_core::usbserial::{ch34x_baud_registers, ftdi_baud_request, FtdiChip};
use anyhow::{anyhow, Result};
use esp_idf_svc::sys::{self, usb};
use log::{debug, info, warn};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

const OUT_BUFFER: usize = 512;
const IN_BUFFER: usize = 512;
const RX_QUEUE_LIMIT: usize = 16 * 1024;

/// Vendor IDs of the USB-serial bridges we know how to drive beyond CDC-ACM.
const VID_FTDI: u16 = 0x0403;
const VID_SILABS: u16 = 0x10c4;
const VID_WCH: u16 = 0x1a86;

/// WCH product IDs that speak the vendor protocol. Newer parts (CH343, CH9102)
/// are CDC-ACM compliant and are handled by the CDC path instead.
const PID_CH34X: [u16; 5] = [0x5512, 0x5523, 0x5584, 0x7522, 0x7523];

/// How the attached device's serial port is configured and framed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    /// A CDC-ACM compliant console (the Phase 1 target).
    Cdc,
    /// FTDI bridges: vendor control requests for the line settings, and a
    /// 2-byte status header on every IN packet. `chip` (from `bcdDevice`)
    /// selects the baud clock and the wIndex layout.
    Ftdi { packet_size: usize, chip: FtdiChip },
    /// Silicon Labs CP210x: vendor requests addressed to the interface.
    Cp210x,
    /// WCH CH340/CH341: vendor register writes with a computed divisor.
    Ch34x,
}

impl Driver {
    pub fn name(&self) -> &'static str {
        match self {
            Driver::Cdc => "CDC ACM",
            Driver::Ftdi { .. } => "FTDI",
            Driver::Cp210x => "CP210x",
            Driver::Ch34x => "CH34x",
        }
    }

    /// Parses a driver name from the shell (`auto` yields `None`).
    pub fn parse(name: &str) -> Option<Option<Driver>> {
        match name {
            "auto" => Some(None),
            "cdc" => Some(Some(Driver::Cdc)),
            "ftdi" => Some(Some(Driver::Ftdi {
                packet_size: 64,
                chip: FtdiChip::from_bcd_device(0x0800),
            })),
            "cp210x" => Some(Some(Driver::Cp210x)),
            "ch34x" => Some(Some(Driver::Ch34x)),
            _ => None,
        }
    }

    /// Picks a driver from the device's USB vendor and product ID and, for
    /// FTDI, the chip generation encoded in `bcdDevice`.
    fn detect(vid: u16, pid: u16, bcd_device: u16) -> Driver {
        match vid {
            VID_FTDI => Driver::ftdi_for(bcd_device),
            VID_SILABS => Driver::Cp210x,
            VID_WCH if PID_CH34X.contains(&pid) => Driver::Ch34x,
            _ => Driver::Cdc,
        }
    }

    fn ftdi_for(bcd_device: u16) -> Driver {
        Driver::Ftdi {
            packet_size: 64,
            chip: FtdiChip::from_bcd_device(bcd_device),
        }
    }

    /// A forced `ftdi` still takes the chip generation from the descriptor.
    fn refine(self, bcd_device: u16) -> Driver {
        match self {
            Driver::Ftdi { .. } => Driver::ftdi_for(bcd_device),
            d => d,
        }
    }
}

/// FTDI vendor requests (bmRequestType 0x40, host-to-device, vendor, device).
const FTDI_REQ_TYPE_OUT: u8 = 0x40;
const FTDI_SIO_RESET: u8 = 0x00;
const FTDI_SIO_SET_MODEM_CTRL: u8 = 0x01;
const FTDI_SIO_SET_FLOW_CTRL: u8 = 0x02;
const FTDI_SIO_SET_BAUDRATE: u8 = 0x03;
const FTDI_SIO_SET_DATA: u8 = 0x04;
const FTDI_SIO_SET_LATENCY_TIMER: u8 = 0x09;
/// Milliseconds the chip may hold received bytes before sending a short
/// packet (the power-on default of 16 ms makes a console feel sluggish).
const FTDI_LATENCY_MS: u16 = 2;

/// CP210x vendor requests (bmRequestType 0x41: host-to-device, vendor, interface).
const CP210X_REQ_TYPE_OUT: u8 = 0x41;
const CP210X_IFC_ENABLE: u8 = 0x00;
const CP210X_SET_LINE_CTL: u8 = 0x03;
const CP210X_SET_MHS: u8 = 0x07;
const CP210X_SET_BAUDRATE: u8 = 0x1e;

/// CH34x vendor requests (bmRequestType 0x40: host-to-device, vendor, device).
const CH34X_REQ_TYPE_OUT: u8 = 0x40;
const CH34X_REQ_TYPE_IN: u8 = 0xc0;
const CH34X_REQ_READ_VERSION: u8 = 0x5f;
const CH34X_REQ_WRITE_REG: u8 = 0x9a;
const CH34X_REQ_SERIAL_INIT: u8 = 0xa1;
const CH34X_REQ_MODEM_CTRL: u8 = 0xa4;
const CH34X_REG_PRESCALER: u16 = 0x12;
const CH34X_REG_DIVISOR: u16 = 0x13;
const CH34X_REG_LCR: u16 = 0x18;
const CH34X_REG_LCR2: u16 = 0x25;
/// 8 data bits, no parity, 1 stop bit, receiver and transmitter enabled.
const CH34X_LCR_8N1: u16 = 0x80 | 0x40 | 0x03;
/// DTR and RTS asserted (the chip wants the complement on the wire).
const CH34X_MCR_DTR_RTS: u16 = (1 << 6) | (1 << 5);

/// How long an open waits for the device the descriptor callback just
/// reported; it is already enumerated, so this only covers a race with
/// an unplug.
const OPEN_TIMEOUT_MS: u32 = 200;

/// Raw CDC device handle; the driver is thread-safe for the calls we make.
struct DevHandle(usb::cdc_acm_dev_hdl_t);
unsafe impl Send for DevHandle {}

/// The open device and who is using it. The disconnect callback must not
/// close a handle another thread is sending on, so a close that arrives
/// during a transfer is deferred to that thread.
struct DevState {
    handle: Option<DevHandle>,
    in_use: bool,
    pending_close: Option<DevHandle>,
}

/// One serial channel: an interface of the attached device.
struct Inner {
    host: Weak<HostInner>,
    dev: Mutex<DevState>,
    rx: Mutex<VecDeque<u8>>,
    rx_cv: Condvar,
    /// The interface is claimed and configured.
    attached: AtomicBool,
    /// A client is on this channel's port, so the interface should be open;
    /// the manager opens it when the device (re)appears.
    wanted: AtomicBool,
    /// The manager's last open failed; cleared when a device arrives or the
    /// client leaves, so a failure is logged once rather than every second.
    open_failed: AtomicBool,
    baud: u32,
    interface: u8,
    /// The TCP port this channel is bridged on (first port + interface).
    port: u16,
    /// Driver of the device this channel was last opened on; `data_cb` uses
    /// it to strip FTDI status bytes.
    driver: Mutex<Driver>,
}

#[derive(Clone)]
pub struct UsbSerial {
    inner: Arc<Inner>,
}

/// Identity of the attached device, from its device descriptor.
#[derive(Clone, Copy)]
struct DeviceId {
    vid: u16,
    pid: u16,
}

/// Why a channel could not be opened for a client.
#[derive(Debug)]
pub enum OpenError {
    /// Nothing is attached (or the device has no such interface); the
    /// channel opens by itself once a device with the interface arrives.
    NoDevice,
    /// The interface is there but cannot be opened now, most often because
    /// the host controller has no free pipe.
    Failed(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NoDevice => write!(f, "no device attached"),
            OpenError::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// Called once per newly created channel with its interface, its port and
/// the channel; the firmware starts a bridge on it.
pub type ChannelHook = Box<dyn Fn(u8, u16, UsbSerial) + Send + Sync>;

struct HostInner {
    baud: u32,
    /// Forced driver from the configuration; `None` means auto-detect.
    forced: Option<Driver>,
    /// Interface n is served on `base_port + n`.
    base_port: u16,
    /// The attached device and the driver picked for it; `None` while
    /// nothing is attached.
    device: Mutex<Option<DeviceId>>,
    driver: Mutex<Driver>,
    /// Serial interfaces of the attached device; emptied when it leaves and
    /// filled by `new_dev_cb`.
    present: Mutex<Vec<u8>>,
    wake: Condvar,
    /// Every channel ever created, by interface number. Channels are never
    /// dropped: the CDC callbacks hold raw pointers into them.
    channels: Mutex<BTreeMap<u8, UsbSerial>>,
    /// Interfaces whose port would pass 65535, warned about once.
    refused: Mutex<Vec<u8>>,
    on_channel: ChannelHook,
}

/// The one USB host, for the callbacks the CDC driver calls without a
/// user argument (`new_dev_cb`) and for the disconnect event, which ends
/// the device for every channel.
static HOST: Mutex<Option<Arc<HostInner>>> = Mutex::new(None);

pub struct UsbHost {
    inner: Arc<HostInner>,
}

/// The channel a callback belongs to, from the `user_arg` given at open time.
///
/// # Safety
/// `user` is the `Arc<Inner>` pointer passed to `cdc_acm_host_open_v2`, and the
/// `Arc` it points into is kept alive in `HostInner::channels` for the
/// program's life.
unsafe fn inner_of<'a>(user: *mut c_void) -> Option<&'a Inner> {
    (!user.is_null()).then(|| &*(user as *const Inner))
}

unsafe extern "C" fn data_cb(data: *const u8, len: usize, user: *mut c_void) -> bool {
    if let Some(inner) = inner_of(user) {
        let raw = std::slice::from_raw_parts(data, len);
        // FTDI prefixes every IN packet with two status bytes (modem, line).
        let stripped: Vec<u8>;
        let slice: &[u8] = match *inner.driver.lock().unwrap() {
            Driver::Ftdi { packet_size, .. } => {
                stripped = raw
                    .chunks(packet_size)
                    .flat_map(|c| c.iter().skip(2).copied())
                    .collect();
                &stripped
            }
            // CDC-ACM, CP210x and CH34x deliver the serial bytes as they are.
            _ => raw,
        };
        let len = slice.len();
        if len == 0 {
            return true;
        }
        let mut q = inner.rx.lock().unwrap();
        if q.len() + len > RX_QUEUE_LIMIT {
            let drop_n = (q.len() + len - RX_QUEUE_LIMIT).min(q.len());
            q.drain(..drop_n);
        }
        q.extend(slice);
        inner.rx_cv.notify_all();
    }
    true
}

unsafe extern "C" fn event_cb(event: *const usb::cdc_acm_host_dev_event_data_t, user: *mut c_void) {
    let ev = &*event;
    match ev.type_ {
        usb::cdc_acm_host_dev_event_t_CDC_ACM_HOST_DEVICE_DISCONNECTED => {
            if let Some(inner) = inner_of(user) {
                info!("usb: interface {} disconnected", inner.interface);
                let mut d = inner.dev.lock().unwrap();
                inner.attached.store(false, Ordering::SeqCst);
                if let Some(h) = d.handle.take() {
                    if d.in_use {
                        // A transfer is in flight: its thread closes the handle.
                        d.pending_close = Some(h);
                    } else {
                        drop(d);
                        usb::cdc_acm_host_close(h.0);
                    }
                }
                inner.rx_cv.notify_all();
            }
            // There is one device; when one channel loses it, all do.
            if let Some(host) = HOST.lock().unwrap().as_ref() {
                *host.device.lock().unwrap() = None;
                host.present.lock().unwrap().clear();
            }
        }
        usb::cdc_acm_host_dev_event_t_CDC_ACM_HOST_ERROR => {
            warn!("usb: cdc error {}", ev.data.error)
        }
        usb::cdc_acm_host_dev_event_t_CDC_ACM_HOST_SERIAL_STATE => {
            debug!("usb: serial state changed")
        }
        _ => {}
    }
}

/// Runs on the CDC driver's task once per enumerated device: reads what the
/// device is and which of its interfaces are serial ports, and hands the
/// list to the manager thread, which does the opening (this callback must
/// not block on the driver).
unsafe extern "C" fn new_dev_cb(dev: usb::usb_device_handle_t) {
    let Some(host) = HOST.lock().unwrap().clone() else {
        return;
    };
    let mut desc: *const usb::usb_device_desc_t = core::ptr::null();
    if usb::usb_host_get_device_descriptor(dev, &mut desc) != sys::ESP_OK || desc.is_null() {
        warn!("usb: new device without a readable device descriptor");
        return;
    }
    let d = &(*desc).__bindgen_anon_1;
    let (vid, pid, bcd, class) = (d.idVendor, d.idProduct, d.bcdDevice, d.bDeviceClass);
    let mut cfg: *const usb::usb_config_desc_t = core::ptr::null();
    if usb::usb_host_get_active_config_descriptor(dev, &mut cfg) != sys::ESP_OK || cfg.is_null() {
        warn!("usb: device {vid:04x}:{pid:04x} has no readable configuration descriptor");
        return;
    }
    let total = (*cfg).__bindgen_anon_1.wTotalLength as usize;
    let bytes = std::slice::from_raw_parts(cfg as *const u8, total);
    let driver = match host.forced {
        Some(f) => f.refine(bcd),
        None => Driver::detect(vid, pid, bcd),
    };
    // Vendor-class interfaces are ports only on a known serial chip, or
    // when the user said what the device is.
    let vendor_serial = host.forced.is_some() || driver != Driver::Cdc;
    let interfaces = serial_interfaces(bytes, vendor_serial);
    info!(
        "usb: new device VID {vid:04x} PID {pid:04x} rev {bcd:04x} class {class:02x}: {}, serial interfaces {interfaces:?}",
        driver.name()
    );
    if interfaces.is_empty() {
        warn!("usb: no serial interface found; usbdrv <cdc|ftdi|cp210x|ch34x> forces one");
    }
    *host.driver.lock().unwrap() = driver;
    *host.device.lock().unwrap() = Some(DeviceId { vid, pid });
    for channel in host.channels.lock().unwrap().values() {
        channel.inner.open_failed.store(false, Ordering::Relaxed);
    }
    *host.present.lock().unwrap() = interfaces;
    host.wake.notify_all();
}

/// How many devices the host library currently has enumerated. A device
/// that leaves while no interface of it is open sends no event to us, so
/// the manager polls this to notice.
fn devices_on_bus() -> usize {
    let mut addresses = [0u8; 8];
    let mut count: core::ffi::c_int = 0;
    let rc = unsafe {
        usb::usb_host_device_addr_list_fill(
            addresses.len() as core::ffi::c_int,
            addresses.as_mut_ptr(),
            &mut count,
        )
    };
    if rc == sys::ESP_OK {
        count.max(0) as usize
    } else {
        0
    }
}

fn esp_check(rc: sys::esp_err_t, what: &str) -> Result<()> {
    if rc == sys::ESP_OK {
        Ok(())
    } else {
        Err(anyhow!("{what} failed: esp_err {rc}"))
    }
}

impl UsbHost {
    /// Installs the USB host library and the CDC driver and starts the
    /// manager thread. `on_channel` is called for every serial interface
    /// the first time a device shows it, with the port `base_port +
    /// interface`.
    pub fn start(
        baud: u32,
        forced: Option<Driver>,
        base_port: u16,
        on_channel: ChannelHook,
    ) -> Result<Self> {
        let inner = Arc::new(HostInner {
            baud,
            forced,
            base_port,
            device: Mutex::new(None),
            driver: Mutex::new(forced.unwrap_or(Driver::Cdc)),
            present: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            channels: Mutex::new(BTreeMap::new()),
            refused: Mutex::new(Vec::new()),
            on_channel,
        });
        {
            let mut slot = HOST.lock().unwrap();
            if slot.is_some() {
                return Err(anyhow!("usb host already started"));
            }
            *slot = Some(inner.clone());
        }

        unsafe {
            let mut host_cfg: usb::usb_host_config_t = core::mem::zeroed();
            host_cfg.intr_flags = sys::ESP_INTR_FLAG_LEVEL1 as i32;
            esp_check(usb::usb_host_install(&host_cfg), "usb_host_install")?;
        }
        // USB host library event pump.
        std::thread::Builder::new()
            .name("usb-events".into())
            .stack_size(4 * 1024)
            .spawn(|| unsafe {
                loop {
                    let mut flags: u32 = 0;
                    usb::usb_host_lib_handle_events(u32::MAX, &mut flags);
                    if flags & usb::USB_HOST_LIB_EVENT_FLAGS_NO_CLIENTS != 0 {
                        usb::usb_host_device_free_all();
                    }
                }
            })?;
        unsafe {
            let mut drv: usb::cdc_acm_host_driver_config_t = core::mem::zeroed();
            // The data callback runs on this task (FTDI status stripping,
            // queue locking, logging), so give it room.
            drv.driver_task_stack_size = 8192;
            drv.driver_task_priority = 10;
            drv.xCoreID = 0;
            drv.new_dev_cb = Some(new_dev_cb);
            esp_check(usb::cdc_acm_host_install(&drv), "cdc_acm_host_install")?;
        }
        info!(
            "usb: host installed at {baud} baud, interface n on port {base_port}+n; waiting for a device"
        );
        let host = inner.clone();
        std::thread::Builder::new()
            .name("usb-manager".into())
            .stack_size(8 * 1024)
            .spawn(move || manage(&host))?;
        Ok(UsbHost { inner })
    }

    /// One line for `status`: the device and its channels with their ports.
    pub fn describe(&self) -> String {
        let Some(dev) = *self.inner.device.lock().unwrap() else {
            return "detached".into();
        };
        let driver = *self.inner.driver.lock().unwrap();
        let channels: Vec<String> = self
            .inner
            .channels
            .lock()
            .unwrap()
            .values()
            .map(|c| {
                format!(
                    "if{}:{}{}",
                    c.inner.interface,
                    c.inner.port,
                    if c.inner.attached.load(Ordering::Relaxed) {
                        " (open)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        format!(
            "{} {:04x}:{:04x} {}",
            driver.name(),
            dev.vid,
            dev.pid,
            if channels.is_empty() {
                "no serial interface".to_string()
            } else {
                channels.join(" ")
            }
        )
    }
}

impl HostInner {
    /// The channel for an interface, created (and announced through
    /// `on_channel`) the first time. `None` when its port would not fit.
    fn channel(&self, self_weak: &Weak<HostInner>, interface: u8) -> Option<UsbSerial> {
        let mut channels = self.channels.lock().unwrap();
        if let Some(c) = channels.get(&interface) {
            return Some(c.clone());
        }
        let Some(port) = self.base_port.checked_add(interface as u16) else {
            let mut refused = self.refused.lock().unwrap();
            if !refused.contains(&interface) {
                refused.push(interface);
                warn!(
                    "usb: interface {interface} not bridged: port {} + {interface} is past 65535",
                    self.base_port
                );
            }
            return None;
        };
        let channel = UsbSerial {
            inner: Arc::new(Inner {
                host: self_weak.clone(),
                dev: Mutex::new(DevState {
                    handle: None,
                    in_use: false,
                    pending_close: None,
                }),
                rx: Mutex::new(VecDeque::new()),
                rx_cv: Condvar::new(),
                attached: AtomicBool::new(false),
                wanted: AtomicBool::new(false),
                open_failed: AtomicBool::new(false),
                baud: self.baud,
                interface,
                port,
                driver: Mutex::new(Driver::Cdc),
            }),
        };
        channels.insert(interface, channel.clone());
        drop(channels);
        info!("usb: interface {interface} is served on port {port}");
        (self.on_channel)(interface, port, channel.clone());
        Some(channel)
    }
}

/// Keeps the channels in step with the bus: creates a channel (and its
/// bridge) for every serial interface a device shows, opens the channels
/// whose port has a client once the device is there, and notices a device
/// leaving while no interface of it was open (the driver reports that only
/// to open interfaces). Runs on `new_dev_cb`'s wake and once a second.
fn manage(host: &Arc<HostInner>) {
    let weak = Arc::downgrade(host);
    let mut present = host.present.lock().unwrap();
    loop {
        let interfaces = present.clone();
        drop(present);
        if !interfaces.is_empty() && devices_on_bus() == 0 {
            info!("usb: device gone");
            *host.device.lock().unwrap() = None;
            host.present.lock().unwrap().clear();
        } else {
            let driver = *host.driver.lock().unwrap();
            for interface in interfaces {
                let Some(channel) = host.channel(&weak, interface) else {
                    continue;
                };
                let inner = &channel.inner;
                if !inner.wanted.load(Ordering::SeqCst)
                    || inner.attached.load(Ordering::SeqCst)
                    || inner.open_failed.load(Ordering::Relaxed)
                {
                    continue;
                }
                match channel.try_open(driver) {
                    Ok(()) => info!(
                        "usb: interface {interface} open on port {} for the waiting client",
                        inner.port
                    ),
                    Err(e) => {
                        warn!("usb: interface {interface}: {e}; the client on port {} waits for the next device", inner.port);
                        inner.open_failed.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        present = host.present.lock().unwrap();
        present = host
            .wake
            .wait_timeout(present, Duration::from_secs(1))
            .unwrap()
            .0;
    }
}

impl UsbSerial {
    /// Claims and configures the interface for a client that just connected
    /// to this channel's port. With nothing attached the call returns
    /// [`OpenError::NoDevice`] and the manager opens the interface when a
    /// device arrives; the caller keeps the session either way. Any other
    /// error is for the caller to report and end the session with.
    pub fn open(&self) -> std::result::Result<(), OpenError> {
        self.inner.wanted.store(true, Ordering::SeqCst);
        self.inner.open_failed.store(false, Ordering::Relaxed);
        if self.inner.attached.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some(host) = self.inner.host.upgrade() else {
            return Err(OpenError::Failed("usb host gone".into()));
        };
        if host.device.lock().unwrap().is_none()
            || !host.present.lock().unwrap().contains(&self.inner.interface)
        {
            return Err(OpenError::NoDevice);
        }
        let driver = *host.driver.lock().unwrap();
        self.try_open(driver)
            .map_err(|e| OpenError::Failed(e.to_string()))
    }

    /// Releases the interface when the client leaves, so its pipes are
    /// free for another channel.
    pub fn close(&self) {
        self.inner.wanted.store(false, Ordering::SeqCst);
        self.inner.open_failed.store(false, Ordering::Relaxed);
        self.inner.attached.store(false, Ordering::SeqCst);
        let mut d = self.inner.dev.lock().unwrap();
        if let Some(h) = d.handle.take() {
            if d.in_use {
                // A transfer is in flight: its thread closes the handle.
                d.pending_close = Some(h);
            } else {
                drop(d);
                unsafe { usb::cdc_acm_host_close(h.0) };
            }
        }
        self.inner.rx.lock().unwrap().clear();
        debug!("usb: interface {} closed", self.inner.interface);
    }

    fn try_open(&self, driver: Driver) -> Result<()> {
        unsafe {
            let mut cfg: usb::cdc_acm_host_open_config_t = core::mem::zeroed();
            cfg.vid = usb::CDC_HOST_ANY_VID as u16;
            cfg.pid = usb::CDC_HOST_ANY_PID as u16;
            cfg.interface_idx = self.inner.interface;
            cfg.dev_addr = usb::CDC_HOST_ANY_DEV_ADDR as u8;
            cfg.connection_timeout_ms = OPEN_TIMEOUT_MS;
            cfg.out_buffer_size = OUT_BUFFER;
            cfg.in_buffer_size = IN_BUFFER;
            cfg.event_cb = Some(event_cb);
            cfg.data_cb = Some(data_cb);
            // The callbacks get the channel back through this.
            cfg.user_arg = Arc::as_ptr(&self.inner) as *mut c_void;
            let mut hdl: usb::cdc_acm_dev_hdl_t = core::ptr::null_mut();
            let rc = usb::cdc_acm_host_open_v2(&cfg, &mut hdl);
            if rc == sys::ESP_ERR_NOT_SUPPORTED {
                return Err(anyhow!(
                    "no free USB host pipe (the ESP32-S3 has 8: one for control and two per open interface)"
                ));
            }
            if rc != sys::ESP_OK {
                return Err(anyhow!("open failed (esp_err {rc})"));
            }
            // Publish the handle before configuring, marked in use: a
            // disconnect during setup then defers the close to us instead
            // of freeing a handle we are still sending requests on.
            {
                let mut d = self.inner.dev.lock().unwrap();
                if let Some(stale) = d.pending_close.take() {
                    usb::cdc_acm_host_close(stale.0);
                }
                d.handle = Some(DevHandle(hdl));
                d.in_use = true;
            }
            *self.inner.driver.lock().unwrap() = driver;
            let configured = self.configure(hdl, driver);
            let mut d = self.inner.dev.lock().unwrap();
            d.in_use = false;
            if let Some(h) = d.pending_close.take() {
                // Disconnected while we were configuring.
                d.handle = None;
                drop(d);
                usb::cdc_acm_host_close(h.0);
                return Err(anyhow!("device left during setup"));
            }
            if let Err(e) = configured {
                if let Some(h) = d.handle.take() {
                    drop(d);
                    usb::cdc_acm_host_close(h.0);
                }
                return Err(e);
            }
            self.inner.rx.lock().unwrap().clear();
            self.inner.attached.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn configure(&self, hdl: usb::cdc_acm_dev_hdl_t, driver: Driver) -> Result<()> {
        unsafe {
            match driver {
                Driver::Cdc => {
                    let coding = usb::cdc_acm_line_coding_t {
                        dwDTERate: self.inner.baud,
                        bCharFormat: 0,
                        bParityType: 0,
                        bDataBits: 8,
                    };
                    if usb::cdc_acm_host_line_coding_set(hdl, &coding) != sys::ESP_OK {
                        debug!("usb: device ignores line coding (fine for native CDC consoles)");
                    }
                    let _ = usb::cdc_acm_host_set_control_line_state(hdl, true, true);
                }
                Driver::Ftdi { chip, .. } => self.configure_ftdi(hdl, chip)?,
                Driver::Cp210x => self.configure_cp210x(hdl)?,
                Driver::Ch34x => self.configure_ch34x(hdl)?,
            }
        }
        Ok(())
    }

    /// Applies the line settings a CP210x bridge needs.
    fn configure_cp210x(&self, hdl: usb::cdc_acm_dev_hdl_t) -> Result<()> {
        let iface = self.inner.interface as u16;
        let send = |req: u8, value: u16, data: &mut [u8]| unsafe {
            usb::cdc_acm_host_send_custom_request(
                hdl,
                CP210X_REQ_TYPE_OUT,
                req,
                value,
                iface,
                data.len() as u16,
                if data.is_empty() {
                    core::ptr::null_mut()
                } else {
                    data.as_mut_ptr()
                },
            )
        };
        if send(CP210X_IFC_ENABLE, 0x0001, &mut []) != sys::ESP_OK {
            warn!("usb: CP210x IFC_ENABLE failed");
        }
        // The baud rate travels in the data stage as a little-endian u32.
        let mut baud = self.inner.baud.to_le_bytes();
        if send(CP210X_SET_BAUDRATE, 0, &mut baud) != sys::ESP_OK {
            warn!("usb: CP210x SET_BAUDRATE failed");
        }
        // 8 data bits, no parity, 1 stop bit.
        let _ = send(CP210X_SET_LINE_CTL, 0x0800, &mut []);
        // Assert DTR and RTS (low byte = values, high byte = write mask).
        let _ = send(CP210X_SET_MHS, 0x0303, &mut []);
        debug!(
            "usb: CP210x interface {iface} configured for {} baud",
            self.inner.baud
        );
        Ok(())
    }

    /// Applies the line settings a CH340/CH341 bridge needs.
    fn configure_ch34x(&self, hdl: usb::cdc_acm_dev_hdl_t) -> Result<()> {
        let out = |req: u8, value: u16, index: u16| unsafe {
            usb::cdc_acm_host_send_custom_request(
                hdl,
                CH34X_REQ_TYPE_OUT,
                req,
                value,
                index,
                0,
                core::ptr::null_mut(),
            )
        };
        // The version is only reported; every CH340/CH341 takes the same setup.
        let mut version = [0u8; 2];
        let rc = unsafe {
            usb::cdc_acm_host_send_custom_request(
                hdl,
                CH34X_REQ_TYPE_IN,
                CH34X_REQ_READ_VERSION,
                0,
                0,
                2,
                version.as_mut_ptr(),
            )
        };
        let version = if rc == sys::ESP_OK { version[0] } else { 0 };

        if out(CH34X_REQ_SERIAL_INIT, 0, 0) != sys::ESP_OK {
            warn!("usb: CH34x SERIAL_INIT failed");
        }
        let regs = ch34x_baud_registers(self.inner.baud);
        if out(
            CH34X_REQ_WRITE_REG,
            (CH34X_REG_DIVISOR << 8) | CH34X_REG_PRESCALER,
            regs,
        ) != sys::ESP_OK
        {
            warn!("usb: CH34x baud rate write failed");
        }
        let _ = out(
            CH34X_REQ_WRITE_REG,
            (CH34X_REG_LCR2 << 8) | CH34X_REG_LCR,
            CH34X_LCR_8N1,
        );
        // The modem-control request takes the complement of the wanted bits.
        let _ = out(CH34X_REQ_MODEM_CTRL, !CH34X_MCR_DTR_RTS, 0);
        debug!(
            "usb: CH34x (version {version:#04x}) configured for {} baud (registers {regs:#06x})",
            self.inner.baud
        );
        Ok(())
    }

    /// Applies the line settings an FTDI bridge needs (it ignores CDC requests).
    fn configure_ftdi(&self, hdl: usb::cdc_acm_dev_hdl_t, chip: FtdiChip) -> Result<()> {
        let port = chip.port_index(self.inner.interface);
        let baud = ftdi_baud_request(self.inner.baud, chip, self.inner.interface);
        let requests: [(u8, u16, u16); 6] = [
            (FTDI_SIO_RESET, 0, port),         // reset the port
            (FTDI_SIO_SET_FLOW_CTRL, 0, port), // no flow control
            (FTDI_SIO_SET_BAUDRATE, baud.value, baud.index),
            (FTDI_SIO_SET_DATA, 0x0008, port), // 8 data bits, no parity, 1 stop
            (FTDI_SIO_SET_LATENCY_TIMER, FTDI_LATENCY_MS, port),
            (FTDI_SIO_SET_MODEM_CTRL, 0x0303, port), // DTR and RTS asserted
        ];
        for (req, value, index) in requests {
            let rc = unsafe {
                usb::cdc_acm_host_send_custom_request(
                    hdl,
                    FTDI_REQ_TYPE_OUT,
                    req,
                    value,
                    index,
                    0,
                    core::ptr::null_mut(),
                )
            };
            if rc != sys::ESP_OK {
                warn!("usb: FTDI request {req:#04x} failed ({rc})");
            }
        }
        debug!(
            "usb: FTDI port {port} ({}) configured for {} baud (wValue {:#06x} wIndex {:#06x})",
            if chip.fast_clock {
                "12 MHz clock"
            } else {
                "3 MHz clock"
            },
            self.inner.baud,
            baud.value,
            baud.index
        );
        Ok(())
    }

    /// Reads bytes received from the target; `Ok(0)` on timeout.
    pub fn read(&self, buf: &mut [u8], timeout: Duration) -> usize {
        let mut q = self.inner.rx.lock().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            if !q.is_empty() {
                let n = buf.len().min(q.len());
                for (i, b) in q.drain(..n).enumerate() {
                    buf[i] = b;
                }
                return n;
            }
            let now = Instant::now();
            if now >= deadline {
                return 0;
            }
            q = self.inner.rx_cv.wait_timeout(q, deadline - now).unwrap().0;
        }
    }

    /// Writes to the target. Data is dropped (with an error) while detached.
    pub fn write(&self, data: &[u8]) -> Result<()> {
        let hdl = {
            let mut d = self.inner.dev.lock().unwrap();
            if d.in_use {
                return Err(anyhow!("usb serial busy"));
            }
            match d.handle.as_ref().map(|h| h.0) {
                Some(h) => {
                    d.in_use = true;
                    h
                }
                None => return Err(anyhow!("usb serial detached")),
            }
        };
        let mut result = Ok(());
        for chunk in data.chunks(OUT_BUFFER) {
            let rc = unsafe {
                usb::cdc_acm_host_data_tx_blocking(hdl, chunk.as_ptr(), chunk.len(), 1000)
            };
            if rc != sys::ESP_OK {
                result = Err(anyhow!("usb tx failed ({rc})"));
                break;
            }
        }
        let mut d = self.inner.dev.lock().unwrap();
        d.in_use = false;
        if let Some(h) = d.pending_close.take() {
            // The device left while we were sending; finish the close now.
            drop(d);
            unsafe { usb::cdc_acm_host_close(h.0) };
            return Err(anyhow!("usb serial detached"));
        }
        result
    }
}

/// Raw DWC-OTG host port status (HPRT register), for diagnostics:
/// whether the port is powered and whether a device's pull-up is seen.
pub fn root_port_state() -> String {
    const USB_HPRT_REG: *const u32 = 0x6008_0440 as *const u32;
    let v = unsafe { core::ptr::read_volatile(USB_HPRT_REG) };
    let conn = v & 1 != 0;
    let enabled = v & (1 << 2) != 0;
    let power = v & (1 << 12) != 0;
    let speed = (v >> 17) & 0x3;
    // PHY mux / pad configuration, to verify the pins are routed to the OTG controller.
    const RTC_CNTL_USB_CONF_REG: *const u32 = 0x6000_8120 as *const u32;
    const USB_WRAP_OTG_CONF_REG: *const u32 = 0x6003_9000 as *const u32;
    let rtc = unsafe { core::ptr::read_volatile(RTC_CNTL_USB_CONF_REG) };
    let wrap = unsafe { core::ptr::read_volatile(USB_WRAP_OTG_CONF_REG) };
    format!(
        "port power={} connected={} enabled={} speed={}; rtc_usb_conf={:#x} (sw_hw_sel={} sw_sel={}) otg_conf={:#x} (phy_sel={} pad_en={} pull_ovr={} dp_pu={} dp_pd={} dm_pu={} dm_pd={})",
        power as u8,
        conn as u8,
        enabled as u8,
        match speed { 0 => "high", 1 => "full", 2 => "low", _ => "?" },
        rtc,
        (rtc >> 20) & 1,
        (rtc >> 19) & 1,
        wrap,
        (wrap >> 2) & 1,
        (wrap >> 18) & 1,
        (wrap >> 12) & 1,
        (wrap >> 13) & 1,
        (wrap >> 14) & 1,
        (wrap >> 15) & 1,
        (wrap >> 16) & 1,
    )
}
