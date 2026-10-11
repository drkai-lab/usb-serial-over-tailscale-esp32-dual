//! `Console` implementations: UART0 (shared with the log output) and the
//! ESP32-S3 USB Serial/JTAG port used as the Setup Mode console.

use adapter_core::shell::Console;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::uart::UartDriver;
use std::time::Duration;

/// UART0 through the UART driver (logs keep using the VFS console path on
/// the same pins; interleaving is acceptable for a debug port).
pub struct UartConsole {
    drv: UartDriver<'static>,
}

impl UartConsole {
    pub fn new(drv: UartDriver<'static>) -> Self {
        UartConsole { drv }
    }
}

impl Console for UartConsole {
    fn read_byte(&mut self, timeout: Duration) -> Option<u8> {
        let mut b = [0u8; 1];
        match self.drv.read(&mut b, TickType::from(timeout).ticks()) {
            Ok(1) => Some(b[0]),
            _ => None,
        }
    }

    fn write(&mut self, s: &str) {
        let _ = self.drv.write(s.as_bytes());
    }
}

