//! LAN service advertisement (Bonjour/mDNS).
//!
//! Advertises `_mvp-server._tcp.local.` so the Apple TV / iPad / web clients
//! can list reachable servers instead of asking the user to type an IP.
//! Best-effort: discovery failing must never affect the server itself.
//! Note for Docker: multicast only reaches the LAN when the container has
//! its own LAN address (macvlan) or uses host networking — on the default
//! bridge network the advertisement stays inside Docker.

use mdns_sd::{ServiceDaemon, ServiceInfo};

/// Service type browsed by the clients.
const SERVICE_TYPE: &str = "_mvp-server._tcp.local.";

/// Start advertising and keep the daemon alive for the process lifetime.
pub fn spawn(port: u16) {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(error) => {
            tracing::warn!(%error, "mDNS advertisement unavailable");
            return;
        }
    };
    // Only the short host label is valid in an mDNS hostname: "Mac.localdomain"
    // must become "Mac.local.", not "Mac.localdomain.local." (the responder
    // silently fails to answer queries for an invalid name).
    let host = hostname();
    let label: String = host
        .split('.')
        .next()
        .unwrap_or("mvp")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let label = if label.is_empty() { "mvp".to_string() } else { label };
    let instance = format!("MVP Server ({label})");
    let service = match ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &format!("{label}.local."),
        (), // addresses: let the responder enumerate interface addresses
        port,
        &[("version", env!("CARGO_PKG_VERSION"))][..],
    ) {
        Ok(service) => service.enable_addr_auto(),
        Err(error) => {
            tracing::warn!(%error, "building mDNS service info failed");
            return;
        }
    };
    match daemon.register(service) {
        Ok(()) => {
            tracing::info!(instance, port, "advertising on the local network (mDNS)");
            // The daemon stops when dropped; it lives as long as the process.
            std::mem::forget(daemon);
        }
        Err(error) => tracing::warn!(%error, "mDNS registration failed"),
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "mvp".to_string())
}
