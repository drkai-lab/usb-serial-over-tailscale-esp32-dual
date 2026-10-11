//! Target serial console on a hardware UART (replaces the USB host on
//! chips without USB OTG, e.g. the original ESP32 / M5StickC Plus2).
//! Same surface as `usb_host::UsbSerial` so `bridge.rs` is unchanged apart
//! from the import.

use anyhow::Result;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{AnyIOPin, InputPin, OutputPin};
use esp_idf_svc::hal::uart::{config::Config, Uart, UartDriver};
use esp_idf_svc::hal::units::Hertz;
use std::sync::Arc;
use std::time::Duration;

struct Inner {
    drv: UartDriver<'static>,
    baud: u32,
}
// uart_read_bytes / uart_write_bytes take the driver's own locks; reading
// and writing from two threads at once is supported by the IDF driver.
unsafe impl Sync for Inner {}
unsafe impl Send for Inner {}

#[derive(Clone)]
pub struct UartSerial {
    inner: Arc<Inner>,
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum OpenError {
    NoDevice,
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

impl UartSerial {
    pub fn new(
        uart: impl Uart + 'static,
        tx: impl OutputPin + 'static,
        rx: impl InputPin + 'static,
        baud: u32,
    ) -> Result<Self> {
        let cfg = Config::new()
            .baudrate(Hertz(baud))
            .rx_fifo_size(4096)
            .tx_fifo_size(1024);
        let drv = UartDriver::new(
            uart,
            tx,
            rx,
            Option::<AnyIOPin>::None,
            Option::<AnyIOPin>::None,
            &cfg,
        )?;
        Ok(UartSerial {
            inner: Arc::new(Inner { drv, baud }),
        })
    }

    pub fn describe(&self) -> String {
        format!("uart {} baud", self.inner.baud)
    }

    /// A UART is always "attached"; drop whatever piled up while no client
    /// was connected.
    pub fn open(&self) -> std::result::Result<(), OpenError> {
        let _ = self.inner.drv.clear_rx();
        Ok(())
    }

    pub fn close(&self) {}

    pub fn read(&self, buf: &mut [u8], timeout: Duration) -> usize {
        self.inner
            .drv
            .read(buf, TickType::from(timeout).ticks())
            .unwrap_or(0)
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut rest = data;
        while !rest.is_empty() {
            let n = self.inner.drv.write(rest)?;
            rest = &rest[n..];
        }
        Ok(())
    }
}
