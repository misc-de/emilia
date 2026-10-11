//! Finding renderers on the LAN: an SSDP `M-SEARCH` for UPnP MediaRenderers
//! and Sonos ZonePlayers, then one description fetch per answering device.
//!
//! Runs only on demand (when the device picker opens), never in the
//! background — a phone should not wake the Wi-Fi for multicast traffic
//! nobody looks at.

use std::collections::{BTreeSet, HashMap};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use super::upnp;
use super::{Device, Kind};

const SSDP_ADDR: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

/// Search targets: generic DLNA renderers, plus Sonos' root device (a Sonos
/// speaker answers both; the description fetch is deduplicated by location).
const TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    "urn:schemas-upnp-org:device:ZonePlayer:1",
];

/// The `LOCATION` header of an SSDP reply.
fn location_of(reply: &str) -> Option<String> {
    reply.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("location")
            .then(|| v.trim().to_string())
    })
}

/// Sends the searches and collects the description URLs answering within
/// `wait`. Each search goes out twice: SSDP rides on UDP and a single lost
/// datagram would hide a device for this round.
fn search(wait: Duration) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return found;
    };
    let _ = sock.set_multicast_ttl_v4(2);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
    let send_all = || {
        for st in TARGETS {
            let msg = format!(
                "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
                 MAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\
                 USER-AGENT: Linux UPnP/1.1 Emilia/1\r\n\r\n"
            );
            let _ = sock.send_to(msg.as_bytes(), SSDP_ADDR);
        }
    };
    send_all();
    let start = Instant::now();
    let mut resent = false;
    let mut buf = [0u8; 2048];
    while start.elapsed() < wait {
        if !resent && start.elapsed() > Duration::from_millis(600) {
            send_all();
            resent = true;
        }
        if let Ok((n, _)) = sock.recv_from(&mut buf)
            && let Some(loc) = location_of(&String::from_utf8_lossy(&buf[..n]))
            && loc.starts_with("http://")
        {
            found.insert(loc);
        }
    }
    found
}

/// Finds the renderers on the LAN (blocking, about `wait` plus the
/// description fetches, which run in parallel). Sonos speakers are folded
/// into their room groups: only a group's coordinator is listed, named after
/// its room with the number of further rooms ("Küche +1").
pub fn discover(wait: Duration) -> Vec<Device> {
    let locations = search(wait);
    let agent = upnp::agent();
    let handles: Vec<_> = locations
        .into_iter()
        .map(|loc| {
            let agent = agent.clone();
            std::thread::spawn(move || upnp::describe(&agent, &loc))
        })
        .collect();
    let mut devices: Vec<Device> = handles
        .into_iter()
        .filter_map(|h| h.join().ok().flatten())
        .collect();
    // One entry per device even if it answered under several locations.
    let mut seen = BTreeSet::new();
    devices.retain(|d| seen.insert(d.id.clone()));
    fold_sonos_groups(&agent, &mut devices);
    devices.sort_by_key(|d| d.name.to_lowercase());
    devices
}

fn fold_sonos_groups(agent: &ureq::Agent, devices: &mut Vec<Device>) {
    let Some(zgt) = devices
        .iter()
        .find(|d| d.kind == Kind::Sonos)
        .and_then(|d| d.zgt.clone())
    else {
        return;
    };
    let Ok(groups) = upnp::sonos_groups(agent, &zgt) else {
        // Without the topology every speaker stays listed on its own; playing
        // to a group member then fails with a clear error from the speaker.
        return;
    };
    let size: HashMap<&str, usize> = groups
        .iter()
        .map(|g| (g.coordinator.as_str(), g.members))
        .collect();
    devices.retain_mut(|d| {
        if d.kind != Kind::Sonos {
            return true;
        }
        match size.get(d.id.as_str()) {
            Some(&n) => {
                if n > 1 {
                    d.name = format!("{} +{}", d.name, n - 1);
                }
                true
            }
            None => false,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_header_is_case_insensitive() {
        let reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\n\
                     Location: http://192.168.0.67:8080/dd.xml\r\nST: x\r\n\r\n";
        assert_eq!(
            location_of(reply).as_deref(),
            Some("http://192.168.0.67:8080/dd.xml")
        );
        assert_eq!(location_of("HTTP/1.1 200 OK\r\n\r\n"), None);
    }
}
