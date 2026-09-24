//! Platform-independent logic for the adapter firmware, kept separate so it can
//! be unit-tested on the host: the interactive setup shell, the baud-rate
//! encodings of USB-serial chips and the reading of USB descriptors.
pub mod shell;
pub mod usbdesc;
pub mod usbserial;
