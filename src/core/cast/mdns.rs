//! A minimal mDNS/DNS-SD **browser** — one PTR query for a service type
//! (`_googlecast._tcp.local`), answers collected for a moment, instances
//! assembled from the PTR, SRV, TXT and A records — and an equally small
//! **responder** that announces one service of our own (the AirPlay
//! receiver's `_raop._tcp`).
//!
//! The query goes out from an ephemeral port ("legacy unicast", RFC 6762
//! §6.7): responders then answer unicast, directly to us, with everything
//! they know about the instance in one packet — no need to share port 5353
//! with the system's own mDNS daemon. Hand-written to keep the dependency
//! tree (and the Flatpak's vendored sources) as it is.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const T_A: u16 = 1;
const T_PTR: u16 = 12;
const T_TXT: u16 = 16;
const T_SRV: u16 = 33;

/// One instance of a service, as far as the answers told.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Instance {
    /// Full instance name (`Living Room-abc._googlecast._tcp.local`).
    pub name: String,
    pub host: String,
    pub addr: Option<Ipv4Addr>,
    pub port: u16,
    pub txt: HashMap<String, String>,
}

/// Browses `service` (e.g. `_googlecast._tcp.local`) for `wait`.
pub fn browse(service: &str, wait: Duration) -> Vec<Instance> {
    let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let query = build_query(service);
    let mut records = Records::default();
    let start = Instant::now();
    let mut sent = 0;
    let mut buf = [0u8; 9000];
    while start.elapsed() < wait {
        // Twice: a lost datagram would hide a device for this round.
        if sent < 2 && start.elapsed() >= Duration::from_millis(700 * sent) {
            let _ = sock.send_to(&query, (MDNS_GROUP, 5353));
            sent += 1;
        }
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            parse_packet(&buf[..n], &mut records);
        }
    }
    records.instances(service)
}

fn push_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn build_query(service: &str) -> Vec<u8> {
    // id 0, flags 0 (standard query), one question.
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    push_name(&mut q, service);
    q.extend_from_slice(&T_PTR.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // class IN
    q
}

#[derive(Debug, Default)]
struct Records {
    /// service → instance names
    ptr: Vec<(String, String)>,
    /// instance → (target host, port)
    srv: HashMap<String, (String, u16)>,
    txt: HashMap<String, HashMap<String, String>>,
    a: HashMap<String, Ipv4Addr>,
}

impl Records {
    fn instances(&self, service: &str) -> Vec<Instance> {
        let service = service.trim_end_matches('.').to_ascii_lowercase();
        let mut out: Vec<Instance> = Vec::new();
        for (svc, inst) in &self.ptr {
            if svc.to_ascii_lowercase() != service || out.iter().any(|i| &i.name == inst) {
                continue;
            }
            let (host, port) = self.srv.get(inst).cloned().unwrap_or_default();
            out.push(Instance {
                name: inst.clone(),
                addr: self.a.get(&host.to_ascii_lowercase()).copied(),
                host,
                port,
                txt: self.txt.get(inst).cloned().unwrap_or_default(),
            });
        }
        out
    }
}

/// Reads a (possibly compressed) name at `pos`; returns it and the position
/// after it in the original stream.
fn read_name(p: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *p.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *p.get(pos + 1)? as usize;
            if end.is_none() {
                end = Some(pos + 2);
            }
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            pos = ptr;
            continue;
        }
        let label = p.get(pos + 1..pos + 1 + len)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
    }
    Some((labels.join("."), end.unwrap_or(pos)))
}

fn u16_at(p: &[u8], pos: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*p.get(pos)?, *p.get(pos + 1)?]))
}

fn parse_packet(p: &[u8], rec: &mut Records) -> Option<()> {
    let qd = u16_at(p, 4)? as usize;
    let total = u16_at(p, 6)? as usize + u16_at(p, 8)? as usize + u16_at(p, 10)? as usize;
    let mut pos = 12;
    for _ in 0..qd {
        let (_, after) = read_name(p, pos)?;
        pos = after + 4;
    }
    for _ in 0..total {
        let (name, after) = read_name(p, pos)?;
        let ty = u16_at(p, after)?;
        let rdlen = u16_at(p, after + 8)? as usize;
        let rd = after + 10;
        let rdata = p.get(rd..rd + rdlen)?;
        match ty {
            T_PTR => {
                if let Some((target, _)) = read_name(p, rd) {
                    rec.ptr.push((name, target));
                }
            }
            T_SRV if rdlen >= 6 => {
                let port = u16_at(p, rd + 4)?;
                if let Some((target, _)) = read_name(p, rd + 6) {
                    rec.srv.insert(name, (target, port));
                }
            }
            T_TXT => {
                let mut map = HashMap::new();
                let mut i = 0;
                while i < rdata.len() {
                    let l = rdata[i] as usize;
                    let Some(entry) = rdata.get(i + 1..i + 1 + l) else {
                        break;
                    };
                    let entry = String::from_utf8_lossy(entry);
                    if let Some((k, v)) = entry.split_once('=') {
                        map.insert(k.to_ascii_lowercase(), v.to_string());
                    }
                    i += 1 + l;
                }
                rec.txt.insert(name, map);
            }
            T_A if rdlen == 4 => {
                rec.a.insert(
                    name.to_ascii_lowercase(),
                    Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]),
                );
            }
            _ => {}
        }
        pos = rd + rdlen;
    }
    Some(())
}

// --- Responder -------------------------------------------------------------------

/// One service we announce: `instance._service._tcp.local` on `host.local`.
pub struct Advert {
    stop: Arc<AtomicBool>,
}

impl Drop for Advert {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Service {
    service: String,
    instance: String,
    host: String,
    port: u16,
    txt: Vec<String>,
}

const TTL: u32 = 120;

/// Announces `instance` of `service` (e.g. `_raop._tcp.local`) on `port`,
/// answering queries until the returned [`Advert`] is dropped (then says
/// goodbye). `host` is a host label of our own (`emilia-1a2b`), resolved to
/// our LAN address by this responder, so no system daemon is needed.
pub fn advertise(
    service: &str,
    instance: &str,
    host: &str,
    port: u16,
    txt: Vec<String>,
) -> anyhow::Result<Advert> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 5353).into())?;
    s.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    s.set_multicast_ttl_v4(255)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    let sock: UdpSocket = s.into();
    let svc = Service {
        service: service.trim_end_matches('.').to_string(),
        instance: format!("{instance}.{}", service.trim_end_matches('.')),
        host: format!("{host}.local"),
        port,
        txt,
    };
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("mdns-advert".into())
            .spawn(move || respond_loop(sock, svc, stop))?;
    }
    Ok(Advert { stop })
}

fn our_ip() -> Ipv4Addr {
    crate::core::sync::local_ip()
        .parse()
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

fn push_rr(out: &mut Vec<u8>, name: &str, ty: u16, flush: bool, ttl: u32, rdata: &[u8]) {
    push_name(out, name);
    out.extend_from_slice(&ty.to_be_bytes());
    out.extend_from_slice(&(if flush { 0x8001u16 } else { 1 }).to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
}

/// A response with all four records. `query` = (id, question bytes) for a
/// legacy unicast answer, which must echo them.
fn response(svc: &Service, ttl: u32, query: Option<(u16, &[u8])>) -> Vec<u8> {
    let (id, question) = query.unwrap_or((0, &[]));
    let qd = u16::from(!question.is_empty());
    let mut p = Vec::with_capacity(512);
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&0x8400u16.to_be_bytes());
    p.extend_from_slice(&qd.to_be_bytes());
    p.extend_from_slice(&4u16.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]);
    p.extend_from_slice(question);
    let mut inst = Vec::new();
    push_name(&mut inst, &svc.instance);
    // Legacy unicast answers must not carry the cache-flush bit.
    let flush = query.is_none();
    push_rr(&mut p, &svc.service, T_PTR, false, ttl, &inst);
    let mut srv = vec![0, 0, 0, 0];
    srv.extend_from_slice(&svc.port.to_be_bytes());
    push_name(&mut srv, &svc.host);
    push_rr(&mut p, &svc.instance, T_SRV, flush, ttl, &srv);
    let mut txt = Vec::new();
    for e in &svc.txt {
        txt.push(e.len().min(255) as u8);
        txt.extend_from_slice(&e.as_bytes()[..e.len().min(255)]);
    }
    push_rr(&mut p, &svc.instance, T_TXT, flush, ttl, &txt);
    push_rr(&mut p, &svc.host, T_A, flush, ttl, &our_ip().octets());
    p
}

/// The questions of a query: (name, type) plus the raw question section.
fn questions(p: &[u8]) -> Option<(u16, Vec<(String, u16)>, Vec<u8>)> {
    let flags = u16_at(p, 2)?;
    if flags & 0x8000 != 0 {
        return None; // a response, not a query
    }
    let id = u16_at(p, 0)?;
    let qd = u16_at(p, 4)? as usize;
    let mut pos = 12;
    let mut out = Vec::new();
    let mut raw = Vec::new();
    for _ in 0..qd {
        let (name, after) = read_name(p, pos)?;
        let ty = u16_at(p, after)?;
        // Re-encode uncompressed for the echo.
        push_name(&mut raw, &name);
        raw.extend_from_slice(&ty.to_be_bytes());
        raw.extend_from_slice(&1u16.to_be_bytes());
        out.push((name, ty));
        pos = after + 4;
    }
    Some((id, out, raw))
}

fn respond_loop(sock: UdpSocket, svc: Service, stop: Arc<AtomicBool>) {
    let group = SocketAddr::from((MDNS_GROUP, 5353));
    // Announce twice, a second apart (RFC 6762 §8.3).
    let _ = sock.send_to(&response(&svc, TTL, None), group);
    let mut announced = 1;
    let started = Instant::now();
    let mut buf = [0u8; 9000];
    while !stop.load(Ordering::Relaxed) {
        if announced < 2 && started.elapsed() > Duration::from_secs(1) {
            let _ = sock.send_to(&response(&svc, TTL, None), group);
            announced += 1;
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Some((id, qs, raw)) = questions(&buf[..n]) else {
            continue;
        };
        let ours = qs.iter().any(|(name, ty)| {
            let name = name.to_ascii_lowercase();
            (name == svc.service.to_ascii_lowercase() && matches!(*ty, T_PTR | 255))
                || name == svc.instance.to_ascii_lowercase()
                || (name == svc.host.to_ascii_lowercase() && matches!(*ty, T_A | 255))
        });
        if !ours {
            continue;
        }
        if from.port() == 5353 {
            let _ = sock.send_to(&response(&svc, TTL, None), group);
        } else {
            let _ = sock.send_to(&response(&svc, 10, Some((id, &raw))), from);
        }
    }
    // Goodbye: TTL 0 tells caches to drop us.
    let _ = sock.send_to(&response(&svc, 0, None), group);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one resource record (uncompressed owner name).
    fn rr(out: &mut Vec<u8>, name: &str, ty: u16, rdata: &[u8]) {
        push_name(out, name);
        out.extend_from_slice(&ty.to_be_bytes());
        out.extend_from_slice(&0x8001u16.to_be_bytes());
        out.extend_from_slice(&120u32.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
    }

    #[test]
    fn assembles_an_instance_from_a_reply() {
        let svc = "_googlecast._tcp.local";
        let inst = "Kitchen-1234._googlecast._tcp.local";
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3];
        let mut name = Vec::new();
        push_name(&mut name, inst);
        rr(&mut p, svc, T_PTR, &name);
        let mut srv = vec![0, 0, 0, 0, 0x1f, 0x49]; // port 8009
        push_name(&mut srv, "abc.local");
        rr(&mut p, inst, T_SRV, &srv);
        let mut txt = Vec::new();
        for e in ["id=1234", "fn=Küche", "md=Nest Audio"] {
            txt.push(e.len() as u8);
            txt.extend_from_slice(e.as_bytes());
        }
        rr(&mut p, inst, T_TXT, &txt);
        rr(&mut p, "abc.local", T_A, &[192, 168, 0, 50]);
        let mut rec = Records::default();
        parse_packet(&p, &mut rec).unwrap();
        let found = rec.instances(svc);
        assert_eq!(found.len(), 1);
        let i = &found[0];
        assert_eq!(i.port, 8009);
        assert_eq!(i.addr, Some(Ipv4Addr::new(192, 168, 0, 50)));
        assert_eq!(i.txt.get("fn").map(String::as_str), Some("Küche"));
        assert_eq!(i.txt.get("md").map(String::as_str), Some("Nest Audio"));
    }

    #[test]
    fn responder_answers_parse_back() {
        let svc = Service {
            service: "_raop._tcp.local".into(),
            instance: "AABBCCDDEEFF@Emilia._raop._tcp.local".into(),
            host: "emilia-1.local".into(),
            port: 5000,
            txt: vec!["txtvers=1".into(), "cn=0,1".into()],
        };
        let mut rec = Records::default();
        parse_packet(&response(&svc, TTL, None), &mut rec).unwrap();
        let found = rec.instances("_raop._tcp.local");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].port, 5000);
        assert_eq!(found[0].txt.get("cn").map(String::as_str), Some("0,1"));
        assert!(found[0].addr.is_some());
        // A legacy unicast answer echoes id and question.
        let q = build_query("_raop._tcp.local");
        let (id, qs, raw) = questions(&q).unwrap();
        assert_eq!(qs, vec![("_raop._tcp.local".to_string(), T_PTR)]);
        let r = response(&svc, 10, Some((id, &raw)));
        assert_eq!(u16_at(&r, 4), Some(1));
        let mut rec = Records::default();
        parse_packet(&r, &mut rec).unwrap();
        assert_eq!(rec.instances("_raop._tcp.local").len(), 1);
    }

    #[test]
    fn follows_compression_pointers() {
        // "local" at offset 12, then "abc" + pointer to it.
        let mut p = vec![0u8; 12];
        push_name(&mut p, "local");
        let at = p.len();
        p.extend_from_slice(&[3, b'a', b'b', b'c', 0xC0, 12]);
        assert_eq!(read_name(&p, at), Some(("abc.local".to_string(), at + 6)));
        // A pointer loop does not hang.
        let looped = [0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 12];
        assert_eq!(read_name(&looped, 12), None);
    }
}
