//! Which interfaces of a USB device are serial ports, read from its
//! configuration descriptor. Pure functions, unit-tested on the host.
//!
//! The firmware bridges every serial interface of the attached device on its
//! own TCP port (the first port plus the interface number), so it has to know
//! which interfaces those are before it opens anything:
//!
//! - A CDC-ACM function is a control interface (class 2, subclass 2) followed
//!   by a data interface (class 10). The control interface's number names the
//!   function; the data interface is skipped, it belongs to the one before.
//! - A USB-serial bridge chip (FTDI, CP210x, CH34x) is vendor class (255) with
//!   a bulk endpoint in each direction on every port. Such interfaces count
//!   only when the device is known to be a serial bridge, because a vendor
//!   interface with bulk endpoints can also be a debugger or anything else
//!   (the Raspberry Pi Debug Probe has CMSIS-DAP on interface 0).

pub const CLASS_CDC: u8 = 0x02;
pub const SUBCLASS_ACM: u8 = 0x02;
pub const CLASS_CDC_DATA: u8 = 0x0a;
pub const CLASS_VENDOR: u8 = 0xff;

const DESC_INTERFACE: u8 = 4;
const DESC_ENDPOINT: u8 = 5;
const XFER_BULK: u8 = 2;

/// Alternate setting 0 of one interface, with what its endpoints can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub class: u8,
    pub subclass: u8,
    pub bulk_in: bool,
    pub bulk_out: bool,
}

/// Walks a configuration descriptor (the whole `wTotalLength` bytes) and
/// lists its interfaces. Alternate settings other than 0 and descriptors it
/// does not know are skipped; a truncated descriptor ends the walk.
pub fn interfaces(config: &[u8]) -> Vec<Interface> {
    let mut out: Vec<Interface> = Vec::new();
    // The interface whose endpoints are being read; `None` inside an
    // alternate setting, whose endpoints belong to nobody we list.
    let mut current: Option<Interface> = None;
    let mut at = 0;
    while at + 2 <= config.len() {
        let len = config[at] as usize;
        if len < 2 || at + len > config.len() {
            break;
        }
        let d = &config[at..at + len];
        match d[1] {
            DESC_INTERFACE if len >= 9 => {
                if let Some(done) = current.take() {
                    out.push(done);
                }
                if d[3] == 0 {
                    current = Some(Interface {
                        number: d[2],
                        class: d[5],
                        subclass: d[6],
                        bulk_in: false,
                        bulk_out: false,
                    });
                }
            }
            DESC_ENDPOINT if len >= 7 => {
                if let Some(c) = current.as_mut() {
                    if d[3] & 0x03 == XFER_BULK {
                        if d[2] & 0x80 != 0 {
                            c.bulk_in = true;
                        } else {
                            c.bulk_out = true;
                        }
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    if let Some(done) = current {
        out.push(done);
    }
    out
}

/// The interface numbers to bridge as serial channels, in descriptor order.
/// `vendor_serial` says the device is a known USB-serial bridge chip (or the
/// user forced a driver), so its vendor-class interfaces are serial ports.
pub fn serial_interfaces(config: &[u8], vendor_serial: bool) -> Vec<u8> {
    interfaces(config)
        .into_iter()
        .filter(|i| match i.class {
            CLASS_CDC => i.subclass == SUBCLASS_ACM,
            CLASS_VENDOR => vendor_serial && i.bulk_in && i.bulk_out,
            _ => false,
        })
        .map(|i| i.number)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(number: u8, alt: u8, class: u8, subclass: u8) -> Vec<u8> {
        vec![9, DESC_INTERFACE, number, alt, 0, class, subclass, 0, 0]
    }

    fn endpoint(address: u8, xfer: u8) -> Vec<u8> {
        vec![7, DESC_ENDPOINT, address, xfer, 64, 0, 0]
    }

    /// A CDC functional descriptor (class-specific interface, type 0x24).
    fn cdc_functional() -> Vec<u8> {
        vec![5, 0x24, 0x00, 0x10, 0x01]
    }

    fn config(parts: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = parts.concat();
        let total = (9 + body.len()) as u16;
        let mut c = vec![9, 2, total as u8, (total >> 8) as u8, 0, 1, 0, 0x80, 50];
        c.extend(body);
        c
    }

    fn ftdi_port(n: u8) -> Vec<u8> {
        [
            interface(n, 0, CLASS_VENDOR, 0xff),
            endpoint(0x81 + 2 * n, XFER_BULK),
            endpoint(0x02 + 2 * n, XFER_BULK),
        ]
        .concat()
    }

    fn cdc_function(control: u8, notif_ep: u8, in_ep: u8, out_ep: u8) -> Vec<u8> {
        [
            interface(control, 0, CLASS_CDC, SUBCLASS_ACM),
            cdc_functional(),
            endpoint(notif_ep, 3),
            interface(control + 1, 0, CLASS_CDC_DATA, 0),
            endpoint(in_ep, XFER_BULK),
            endpoint(out_ep, XFER_BULK),
        ]
        .concat()
    }

    #[test]
    fn ft4232h_has_four_ports() {
        let c = config(&[ftdi_port(0), ftdi_port(1), ftdi_port(2), ftdi_port(3)]);
        assert_eq!(serial_interfaces(&c, true), vec![0, 1, 2, 3]);
        // Not known to be a serial chip: vendor interfaces are left alone.
        assert_eq!(serial_interfaces(&c, false), Vec::<u8>::new());
    }

    #[test]
    fn debug_probe_uart_is_interface_1_and_cmsis_dap_is_not_a_port() {
        // Interface 0 is CMSIS-DAP v2: vendor class with bulk in and out.
        let c = config(&[
            [
                interface(0, 0, CLASS_VENDOR, 0),
                endpoint(0x01, XFER_BULK),
                endpoint(0x82, XFER_BULK),
            ]
            .concat(),
            cdc_function(1, 0x83, 0x85, 0x04),
        ]);
        assert_eq!(serial_interfaces(&c, false), vec![1]);
        // A forced driver makes the vendor interface a port too.
        assert_eq!(serial_interfaces(&c, true), vec![0, 1]);
    }

    #[test]
    fn ch340_single_port_with_interrupt_endpoint() {
        let c = config(&[[
            interface(0, 0, CLASS_VENDOR, 1),
            endpoint(0x82, XFER_BULK),
            endpoint(0x02, XFER_BULK),
            endpoint(0x81, 3),
        ]
        .concat()]);
        assert_eq!(serial_interfaces(&c, true), vec![0]);
    }

    #[test]
    fn two_cdc_functions_are_named_by_their_control_interfaces() {
        let c = config(&[
            cdc_function(0, 0x81, 0x82, 0x02),
            cdc_function(2, 0x83, 0x84, 0x04),
        ]);
        assert_eq!(serial_interfaces(&c, false), vec![0, 2]);
    }

    #[test]
    fn alternate_settings_do_not_duplicate_or_hide_an_interface() {
        // Interface 1 has an alternate setting 1 with the bulk endpoints and
        // a setting 0 without (as some CDC data interfaces do). Setting 0
        // is what we list; a vendor interface with no bulk endpoints in
        // setting 0 is not a port, and setting 1 is not listed twice.
        let c = config(&[
            ftdi_port(0),
            interface(1, 0, CLASS_VENDOR, 0xff),
            [
                interface(1, 1, CLASS_VENDOR, 0xff),
                endpoint(0x83, XFER_BULK),
                endpoint(0x04, XFER_BULK),
            ]
            .concat(),
        ]);
        assert_eq!(
            interfaces(&c)
                .iter()
                .map(|i| (i.number, i.bulk_in && i.bulk_out))
                .collect::<Vec<_>>(),
            vec![(0, true), (1, false)]
        );
        assert_eq!(serial_interfaces(&c, true), vec![0]);
    }

    #[test]
    fn truncated_or_garbage_descriptors_do_not_panic() {
        let c = config(&[ftdi_port(0), ftdi_port(1)]);
        for cut in 0..c.len() {
            let _ = serial_interfaces(&c[..cut], true);
        }
        assert_eq!(serial_interfaces(&[0, 0, 0], true), Vec::<u8>::new());
        assert_eq!(serial_interfaces(&[9, 4, 0], true), Vec::<u8>::new());
        assert_eq!(serial_interfaces(&[], true), Vec::<u8>::new());
    }
}
