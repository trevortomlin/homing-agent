//! Minimal `sd_notify` (systemd readiness protocol) without a dependency: send a datagram to
//! `$NOTIFY_SOCKET` if it is set. Does nothing when not run by systemd with `Type=notify`.

use std::os::unix::net::UnixDatagram;

/// Send `state` (e.g. `"READY=1"`) to systemd. Errors are ignored: notification is best effort.
pub fn notify(state: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    let bytes = path.as_encoded_bytes();
    let res = match bytes.strip_prefix(b"@") {
        // Abstract socket address (Linux).
        Some(name) => {
            use std::os::linux::net::SocketAddrExt as _;
            std::os::unix::net::SocketAddr::from_abstract_name(name)
                .and_then(|addr| sock.send_to_addr(state.as_bytes(), &addr))
        }
        None => sock.send_to(state.as_bytes(), &path),
    };
    if let Err(e) = res {
        eprintln!("homing-agent: sd_notify failed: {e}");
    }
}
