#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod dns;
mod error;
mod ports;
mod wireguard;

pub use dns::DnsServer;
pub use error::{NetError, Result};
pub use ports::PortAllocator;
pub use wireguard::{BoxFuture, Mesh, PeerInfo, WireGuard, WireGuardIdentity};
