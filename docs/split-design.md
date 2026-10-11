# Split design notes: control chip (B) + data chip (A)

Design notes behind the [README](../README.md). The wiring diagram is [wiring.png](wiring.png).

Status: **phase 1** (software). Builds for `xtensa-esp32-espidf` (no PSRAM)
and is tested on the host; **not yet run on hardware**.

```
              tailnet (Wi-Fi)                       tailnet (Wi-Fi)
                    |                                      |
   +----------------+-------------+       +----------------+-----------------+
   | B  role-control              |       | A  role-data                     |
   |  ts2021 control (Noise :80,  | UART1 |  WireGuard (node private key)    |
   |  TLS fallback), HTTP/2,      |<=====>|  disco/STUN, DERP, ACL filter,   |
   |  map stream, machine key,    | 2Mbps |  smoltcp, TCP bridge             |
   |  setup shell on UART0        |RTS/CTS|  target console on UART2 --------+--> target
   +------------------------------+       +----------------------------------+
```

Both chips join Wi-Fi. B talks to the control server only; A carries every
packet. The node private key is generated and stored on A; B only ever sees
its public half. The machine key stays on B.

## Build

```
cd firmware
CARGO_TARGET_DIR=target-control cargo build --release --features role-control   # chip B
CARGO_TARGET_DIR=target-data    cargo build --release --features role-data      # chip A
```

Both roles use `variants/wroom32-nopsram.defaults`. Flash with the 4 MB
partition table (`partitions.csv` has a 6 MB app slot for the S3 boards):

```
espflash flash --chip esp32 --port /dev/ttyUSB0 --flash-size 4mb \
  --partition-table partitions-wroom32.csv \
  target-control/xtensa-esp32-espidf/release/usb-serial-over-tailscale-firmware
```

## Pin map (ESP32-WROOM-32 / DevKitC)

| Function                 | B (control) | A (data) | Notes |
|--------------------------|-------------|----------|-------|
| Console UART0 TX / RX    | GPIO1 / 3   | GPIO1 / 3 | USB-UART of the DevKitC. B: setup shell + logs. A: logs only |
| Link UART1 TX            | GPIO25      | GPIO25   | to the other chip's RX |
| Link UART1 RX            | GPIO26      | GPIO26   | from the other chip's TX |
| Link UART1 RTS (out)     | GPIO33      | GPIO33   | to the other chip's CTS |
| Link UART1 CTS (in)      | GPIO32      | GPIO32   | from the other chip's RTS |
| Target UART2 TX          | -           | GPIO17   | to target RX (3.3 V levels) |
| Target UART2 RX          | -           | GPIO16   | from target TX (3.3 V levels) |
| Setup button             | GPIO0 (BOOT)| -        | hold at boot for setup mode |

Not used: GPIO6-11 (SPI flash), strapping pins GPIO0 (only as the BOOT
button input on B), 2, 5, 12, 15. GPIO16/17 are free on WROOM-32 (they are
the PSRAM pins on WROVER modules). GPIO32/33 are free when no 32 kHz crystal
is fitted (DevKitC: none).

## Wiring

```
   B (control)                    A (data)
   GPIO25 TX  ------------------> GPIO26 RX
   GPIO26 RX  <------------------ GPIO25 TX
   GPIO33 RTS ------------------> GPIO32 CTS
   GPIO32 CTS <------------------ GPIO33 RTS
   GND        -------------------- GND          <- required
                                  GPIO17 TX  ---> target RX
                                  GPIO16 RX  <--- target TX
                                  GND        ---- target GND  <- required
```

- **Common ground** between B and A, and between A and the target, is
  required even if each board is powered from its own USB port.
- Keep the link wires short (< 20 cm jumpers) at 2 Mbit/s. For longer wires
  lower `LINK_BAUD` in `firmware/src/link_uart.rs` (same value on both).
- Do not power one board's 3V3 from the other's; power both from USB (or one
  5 V supply to both 5V pins).

## Setup

1. Flash `role-data` to A and `role-control` to B, wire as above.
2. On B's UART0 run the setup shell as for the single-chip firmware (Wi-Fi,
   hostname, auth key, TCP port, baud rate).
3. B forwards Wi-Fi networks, TCP port and baud rate to A (stored in A's
   NVS, namespace `tsdata`); A restarts once to start the bridge.
4. B registers with control using the node public key A reported, then
   streams the map and keeps A in sync.

## Link protocol (`crates/splitlink`)

- Framing: COBS, `0x00` delimiter, CRC-32 (IEEE) over the frame.
- Header: kind (DATA/ACK/PING), sender epoch (random per boot), receiver
  epoch, 16-bit sequence.
- Stop-and-wait ARQ: one DATA in flight, retransmit after `ack_timeout`, up
  to `max_retries`; receiver ACKs everything and delivers only newer
  sequences (duplicates dropped). A new sender epoch = peer rebooted
  (`Event::Up { restarted: true }`); PING keeps the link alive and
  `peer_timeout` reports `Event::Down`.
- Messages: postcard. B -> A: `Begin/Item/End` snapshots (addresses, home
  DERP, DERP regions, peers, packet filter), deltas (`PeersUpsert`,
  `PeersRemove`, `Patches`, each with `base` and `gen`), `ConnectHomeDerp`,
  `ProbeDerp`, `ProposeNodeKey`/`CommitNodeKey`, `DeviceConfig`,
  `ClockOffset`, `Heartbeat{gen}`. A -> B: `Status` (node/disco public key,
  endpoints, home DERP, DERP latency, applied generation), `PeerStatus`,
  `ProbeResult`, `NodeKeyProposal`, `RequestResync`.
- Generations: B numbers every configuration change. A applies a snapshot
  atomically at `End`, and a delta only when `base` equals its applied
  generation; otherwise it asks for a resync. A reports its generation in
  every `Status` (2 s); a mismatch makes B send a snapshot.

## Failure behaviour

| Event | Behaviour |
|-------|-----------|
| B reboots / hangs | A keeps WireGuard, DERP, peers and ACL of its last applied generation; existing sessions continue. Peer changes are missed until B is back. |
| B back without a map yet | Nothing is sent (B has generation 0); A keeps running. After B's first map response A gets one snapshot. |
| A reboots | Node key from A's NVS (same identity). B sees the new epoch / `applied_gen = 0` and sends a snapshot. |
| Frame lost / corrupt | CRC rejects it, sender retransmits. A delta whose ACK never arrives turns into a snapshot. |
| Node key expired | B asks A for a new key (`ProposeNodeKey`), registers it, then `CommitNodeKey`; A stores it and switches. |

## Code layout

- `crates/splitlink`: framing, link, messages (host-testable).
- `crates/tsnode/src/dataplane.rs`: `DataPlane` trait (every call from the
  control plane into magicsock/DERP/WireGuard/netstack) and
  `LocalDataPlane` (single-chip behaviour, unchanged).
- `crates/tsnode/src/split.rs` (feature `split`): `RemoteDataPlane` (B) and
  `DataServer` (A).
- `crates/tsnode/tests/split.rs`: both roles in one process over a lossy
  socketpair.
- `firmware/src/link_uart.rs`, `role_control.rs`, `role_data.rs`.

## Known limitations (phase 1)

- Not run on hardware: UART1 RTS/CTS at 2 Mbit/s, Wi-Fi on both chips and
  heap headroom are estimates.
- Thread stack sizes for `split-*` threads are estimates (not measured
  with high-water marks yet).
- A keeps its configuration only in RAM: if A reboots while B is down, A
  has no peers until B is back (persisting the last snapshot to NVS is
  phase 2).
- Peer status on B is refreshed every 2 s (not live).
- No cross-reset wiring (EN) or watchdog between the chips yet.
- Wi-Fi passwords cross the link in clear (a wire between two boards).
