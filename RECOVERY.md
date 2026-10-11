# Recovery: usb-serial-1 reset loop (2026-10-03)

`usb-serial-1` is the N8R2 adapter that serves the Toradex Dahlia's FT4232H
(Verdin AM62P console on port 35933). It went offline on the tailnet at about
11:00 on 2026-10-03.

## What the board does

Its COM port log shows a reset every 1.1 to 3.5 seconds:

```text
W (934) wifi:Password length is zero, but authmode threshold is 3, ...
I (1054) tsnode::node: node: state ConnectingControl
I (1065) tsnode::node: node: state Error("control: tls connect ...: ESP_FAIL")
I (1107) ...usb_host: usb: host installed at 115200 baud, ...
ESP-ROM:esp32s3-20210327
rst:0x3 (RTC_SW_SYS_RST),boot:0x8 (SPI_FAST_FLASH_BOOT)
Saved PC:0x40376a9c
```

All 39 resets captured that day are `rst:0x3` with `Saved PC:0x40376a9c`.
In the flashed image that address is the endless loop right after
`esp_rom_software_reset_system()` in the IDF's `rtc_brownout_isr_handler`
(`esp_system/port/brownout.c`). We checked this against an N8R2 build with
symbols: the image bytes around the address match. **The resets are
brownouts.** The chip's supply voltage drops below the detector threshold.
This happens at the moments that draw the most current: Wi-Fi scanning
and joining (one reset came right after `wifi: choosing "ORBI47"`), and the
USB host start, which wakes the bus-powered FT4232H. The firmware it runs had
been stable since 2026-09-24, so something changed on the power side.

The log lines that look like causes are not causes:

- `Password length is zero` comes from the placeholder station configuration
  that the firmware sets before any stored network is used (`firmware/src/wifi.rs`,
  `WifiManager::new`). It appears on every boot of every board. It says nothing
  about the stored password. This branch removes it.
- A failed control connection does not reset the board. The node retries with
  a back-off of 2 s to 60 s (`crates/tsnode/src/node.rs`, `sleep_backoff`). The
  control dial fails because Wi-Fi is not up yet.
- `wifi ORBI47 <password>` typed during the loop could not help. The command
  saves the network only after it has joined and has an IP address
  (`crates/adapter-core/src/shell.rs`, `set_wifi`), and joining is the moment
  the board browns out. So nothing was written, and nothing was erased.
  The node identity and the stored network are still there: every boot
  in the log prints the same machine key, and `wifi list` shows ORBI47.

## 1. Fix the power (the actual repair)

The COM port supplies the ESP32-S3 and, through the bridged `USB-OTG` pad,
the Dahlia's FT4232H as well (see "Power and wiring" in docs/UPSTREAM_README.md). On Lepton
the COM port (CH343, serial `5CF9120331`) sits on a VIA Labs hub (bus 7, port
1.1). The same hub also feeds a 4K capture device (400 mA), an FT232R and the
Pi Debug Probe.

- Check that the hub's own power adapter is plugged in and working, or
- plug the COM port cable straight into a PC port or a USB charger (1 A or
  more), with a short cable.

Do not plug the native port into a PC while the COM port is connected (the
OTG pad joins the two VBUS lines).

The loop has stopped when the COM port log runs past 5 s without a new
`ESP-ROM` banner, and `wifi: connected to "ORBI47"` appears.

## 2. Flash the fixed firmware (recommended)

With the firmware from this branch, a brownout can no longer hide.
The boot log prints `last reset: brownout: ...` as a warning, and
`status` shows a `Reset:` line. `wifi list` also shows whether each network has
a stored password.

esptool can flash through the COM port (its DTR and RTS lines drive the
auto-download circuit). Writing only the bootloader, the partition table and
the app keeps the NVS partition, which holds the Wi-Fi networks and the
tailnet registration. Fix the power first if you can, so the flash is not
written while the voltage is sagging.

```bash
cd firmware
ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;variants/n8r2.defaults" \
  CARGO_TARGET_DIR=.target cargo build --release
CARGO_TARGET_DIR=.target tools/mkimage.sh n8r2 release dist/n8r2
PY=$(ls -d .embuild/espressif/python_env/*/bin/python | head -1)
$PY -m esptool --chip esp32s3 \
  --port /dev/serial/by-id/usb-1a86_USB_Single_Serial_5CF9120331-if00 \
  --baud 921600 write_flash \
  0x0 dist/n8r2/bootloader.bin 0x8000 dist/n8r2/partition-table.bin \
  0x10000 dist/n8r2/app.bin
```

Never write the merged `usb-serial-over-tailscale-n8r2.bin` at 0x0: it fills
the NVS area with 0xFF and erases the configuration and the registration.

## 3. Check the Wi-Fi credentials

Open the shell on the COM port. Opening the port resets the chip once (DTR):

```bash
P=/dev/serial/by-id/usb-1a86_USB_Single_Serial_5CF9120331-if00
stty -F $P 115200 raw -echo -hupcl clocal
cat $P &                      # the log and the shell's answers
printf 'wifi list\r' > $P
```

- `ORBI47  [password set]`: nothing to do.
- `ORBI47  [no password (open network)]`: enter it again with
  `printf 'wifi ORBI47 <password>\r' > $P`. The shell prints
  `Connecting to ORBI47...`, then `Connected (<ip>)`. It saves the network only
  after that line. `Wi-Fi failed: ...` means nothing was saved.

## 4. Verify

Wait about 30 s after the boot, then:

```bash
printf 'status\r' > $P
```

Expected:

- `Wi-Fi:     connected to ORBI47 (<ip>)`
- `Tailscale: running as usb-serial-1...`
- `USB:       FTDI 0403:6011 if0:35930 if1:35931 if2:35932 if3:35933 ...`
- `Reset:     power-on` (or `software` after a `reboot`), not `brownout`

From Lepton:

```bash
tailscale status | grep usb-serial-1     # no "offline"
socat -,rawer,escape=0x1d TCP:usb-serial-1:35933   # Verdin console, Ctrl-] to exit
```

If `status` or the boot log ever shows `Reset: brownout` again, the supply is
still too weak. Change the power, not the firmware.
