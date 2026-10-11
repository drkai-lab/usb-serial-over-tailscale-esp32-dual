//! Buffer sizes and caps. The `lowmem` feature selects values for targets
//! with ~200 KB of free internal RAM and no PSRAM (plain ESP32-WROOM-32).

macro_rules! limit {
    ($(#[$m:meta])* $name:ident: $t:ty = $big:expr, $small:expr) => {
        $(#[$m])*
        #[cfg(not(feature = "lowmem"))]
        pub const $name: $t = $big;
        $(#[$m])*
        #[cfg(feature = "lowmem")]
        pub const $name: $t = $small;
    };
}

limit!(
    /// UDP receive buffer of the magicsock socket. WireGuard packets carry
    /// at most MTU (1280) + 32 bytes; disco and STUN are smaller.
    UDP_RECV_BUF: usize = 65536, 2048
);
limit!(
    /// Packets queued per DERP connection before new ones are dropped.
    DERP_QUEUE_LIMIT: usize = 64, 8
);
limit!(
    /// Concurrent DERP connections (each is a TLS session and a thread).
    /// When full, the least recently used non-home region is closed.
    MAX_DERP_CONNS: usize = usize::MAX, 2
);
limit!(
    /// HTTP/2 receive window we advertise for the control connection.
    H2_WINDOW: u32 = 256 * 1024, 32 * 1024
);
limit!(
    /// Largest HTTP/2 header block accepted.
    H2_MAX_HEADER_BLOCK: usize = 64 * 1024, 16 * 1024
);
limit!(
    /// smoltcp socket buffers (per TCP socket).
    TCP_RX_BUF: usize = 8 * 1024, 4 * 1024
);
limit!(TCP_TX_BUF: usize = 8 * 1024, 4 * 1024);
limit!(
    /// Bytes queued between the socket and its user, per direction.
    USER_TX_LIMIT: usize = 32 * 1024, 8 * 1024
);
limit!(USER_RX_LIMIT: usize = 16 * 1024, 4 * 1024);
limit!(
    /// Peers kept from a map response (the rest are ignored while parsing).
    MAX_PEERS: usize = usize::MAX, 32
);
limit!(
    /// DERP nodes kept per region (the first ones the server lists).
    MAX_DERP_NODES_PER_REGION: usize = usize::MAX, 2
);
limit!(
    /// Map messages up to this size are buffered and parsed in one go;
    /// larger ones are parsed while they stream in.
    MAP_BUFFER_MAX: usize = 8 << 20, 8 * 1024
);
