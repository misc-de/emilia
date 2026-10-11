//! UPnP/DLNA control: the device description parser, a minimal SOAP client
//! and the DIDL-Lite metadata a renderer shows for the running track.
//!
//! Everything here is blocking and meant for the cast worker thread. Sonos
//! speakers are plain UPnP MediaRenderers for this purpose (AVTransport +
//! RenderingControl on port 1400); the few Sonos specifics (group
//! coordinator, group volume, the radio URI scheme) sit next to the generic
//! calls and are picked by [`super::Kind`].

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, anyhow};
use quick_xml::events::Event;

use super::{Device, Kind, Service};

const SOAP_TIMEOUT: Duration = Duration::from_secs(6);

/// Service types the cast feature talks to (version-less prefixes; the
/// description carries the version the device actually implements).
const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:";
const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:";
const CM: &str = "urn:schemas-upnp-org:service:ConnectionManager:";
const GRC: &str = "urn:schemas-upnp-org:service:GroupRenderingControl:";
const ZGT: &str = "urn:schemas-upnp-org:service:ZoneGroupTopology:";

pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(SOAP_TIMEOUT)
        .build()
}

// --- Device description ------------------------------------------------------

/// What the description of one root device yields: the first (outermost)
/// device's identity plus every service of it and its embedded devices.
#[derive(Debug, Default)]
struct Description {
    device_type: String,
    friendly_name: String,
    room_name: String,
    manufacturer: String,
    model: String,
    udn: String,
    url_base: String,
    /// (serviceType, controlURL) of all services, embedded devices included.
    services: Vec<(String, String)>,
}

fn parse_description(xml: &str) -> Description {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut d = Description::default();
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut device_depth = 0usize;
    let mut svc_type = String::new();
    let mut svc_ctrl = String::new();
    loop {
        let ev = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(ev) => ev,
        };
        match &ev {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "device" {
                    device_depth += 1;
                }
                if name == "service" {
                    svc_type.clear();
                    svc_ctrl.clear();
                }
                path.push(name);
                text.clear();
            }
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                let value = std::mem::take(&mut text).trim().to_string();
                let parent = path.last().map(String::as_str).unwrap_or("");
                // Identity fields only from the outermost device.
                if parent == "device" && device_depth == 1 {
                    match name.as_str() {
                        "deviceType" => d.device_type = value.clone(),
                        "friendlyName" => d.friendly_name = value.clone(),
                        "roomName" => d.room_name = value.clone(),
                        "manufacturer" => d.manufacturer = value.clone(),
                        "modelName" => d.model = value.clone(),
                        "UDN" => d.udn = value.clone(),
                        _ => {}
                    }
                }
                if parent == "root" && name == "URLBase" {
                    d.url_base = value.clone();
                }
                if parent == "service" {
                    match name.as_str() {
                        "serviceType" => svc_type = value,
                        "controlURL" => svc_ctrl = value,
                        _ => {}
                    }
                } else if name == "service" && !svc_type.is_empty() && !svc_ctrl.is_empty() {
                    d.services
                        .push((std::mem::take(&mut svc_type), std::mem::take(&mut svc_ctrl)));
                } else if name == "device" {
                    device_depth = device_depth.saturating_sub(1);
                }
            }
            other => crate::core::xml::push_text(&mut text, other),
        }
    }
    d
}

/// `scheme://host:port` of a URL (the base relative control URLs hang off).
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split('/').next()?;
    Some(format!("{scheme}://{host}"))
}

/// Host part (without port) of a URL.
pub(crate) fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let hostport = rest.split('/').next()?;
    let host = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => hostport,
    };
    Some(
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
    )
}

/// Resolves a (usually relative) control URL against the description's base.
fn resolve(base: &str, url: &str) -> String {
    if url.contains("://") {
        return url.to_string();
    }
    let base = base.trim_end_matches('/');
    if url.starts_with('/') {
        format!("{base}{url}")
    } else {
        format!("{base}/{url}")
    }
}

fn find_service(d: &Description, base: &str, prefix: &str) -> Option<Service> {
    d.services
        .iter()
        .find(|(t, _)| t.starts_with(prefix))
        .map(|(t, c)| Service {
            service_type: t.clone(),
            control_url: resolve(base, c),
        })
}

/// Fetches and interprets the description at `location`. `None` for anything
/// that is not a usable renderer.
pub(crate) fn describe(agent: &ureq::Agent, location: &str) -> Option<Device> {
    let xml = agent.get(location).call().ok()?.into_string().ok()?;
    let d = parse_description(&xml);
    let base = if d.url_base.is_empty() {
        origin(location)?
    } else {
        d.url_base.clone()
    };
    let is_sonos = d.manufacturer.to_ascii_lowercase().contains("sonos")
        || d.device_type.contains(":ZonePlayer:");
    let mut avt = find_service(&d, &base, AVT);
    let mut rc = find_service(&d, &base, RC);
    let mut cm = find_service(&d, &base, CM);
    // Some firmwares (Frontier Silicon radios, for one) announce the services
    // via SSDP but leave them out of the description. The conventional paths
    // work on those; confirm with one cheap call before trusting the guess.
    if avt.is_none() && d.device_type.contains(":MediaRenderer:") {
        let guess = |name: &str| Service {
            service_type: format!("urn:schemas-upnp-org:service:{name}:1"),
            control_url: format!("{base}/{name}/control"),
        };
        let g = guess("AVTransport");
        if transport_info(agent, &g).is_ok() {
            avt = Some(g);
            rc = rc.or_else(|| Some(guess("RenderingControl")));
            cm = cm.or_else(|| Some(guess("ConnectionManager")));
        }
    }
    let avt = avt?;
    let name = if is_sonos && !d.room_name.is_empty() {
        d.room_name.clone()
    } else if !d.friendly_name.is_empty() {
        d.friendly_name.clone()
    } else {
        host_of(location).unwrap_or_default()
    };
    Some(Device {
        id: d.udn.trim_start_matches("uuid:").to_string(),
        name,
        model: [d.manufacturer.as_str(), d.model.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" "),
        kind: if is_sonos { Kind::Sonos } else { Kind::Dlna },
        location: location.to_string(),
        host: host_of(location).unwrap_or_default(),
        avt,
        rc,
        cm,
        grc: find_service(&d, &base, GRC),
        zgt: find_service(&d, &base, ZGT),
    })
}

// --- SOAP --------------------------------------------------------------------

/// XML-escapes text for an element body or attribute.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// The text of every leaf element in `xml`, by local name (the last one wins).
fn leaf_values(xml: &str) -> HashMap<String, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = HashMap::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    loop {
        let ev = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(ev) => ev,
        };
        match &ev {
            Event::Start(e) => {
                stack.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
                text.clear();
            }
            Event::End(_) => {
                if let Some(name) = stack.pop() {
                    out.insert(name, std::mem::take(&mut text));
                }
            }
            other => crate::core::xml::push_text(&mut text, other),
        }
    }
    out
}

/// Calls `action` on `svc` with the given (already escaped) arguments and
/// returns the response's leaf values. A UPnP fault becomes an error carrying
/// the device's error code and description.
pub(crate) fn soap(
    agent: &ureq::Agent,
    svc: &Service,
    action: &str,
    args: &[(&str, &str)],
) -> Result<HashMap<String, String>> {
    let mut body = String::new();
    for (k, v) in args {
        body.push_str(&format!("<{k}>{v}</{k}>"));
    }
    let envelope = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action} xmlns:u=\"{st}\">{body}</u:{action}></s:Body></s:Envelope>",
        st = svc.service_type
    );
    let resp = agent
        .post(&svc.control_url)
        .set("Content-Type", "text/xml; charset=\"utf-8\"")
        .set("SOAPACTION", &format!("\"{}#{action}\"", svc.service_type))
        .send_string(&envelope);
    match resp {
        Ok(r) => Ok(leaf_values(&r.into_string()?)),
        Err(ureq::Error::Status(code, r)) => {
            let v = leaf_values(&r.into_string().unwrap_or_default());
            Err(anyhow!(
                "{action} failed (HTTP {code}, UPnP {} {})",
                v.get("errorCode").map(String::as_str).unwrap_or("?"),
                v.get("errorDescription").map(String::as_str).unwrap_or("")
            ))
        }
        Err(e) => Err(anyhow!("{action}: {e}")),
    }
}

const ID: (&str, &str) = ("InstanceID", "0");

pub(crate) fn set_uri(agent: &ureq::Agent, avt: &Service, uri: &str, didl: &str) -> Result<()> {
    soap(
        agent,
        avt,
        "SetAVTransportURI",
        &[
            ID,
            ("CurrentURI", &esc(uri)),
            ("CurrentURIMetaData", &esc(didl)),
        ],
    )
    .map(|_| ())
}

pub(crate) fn play(agent: &ureq::Agent, avt: &Service) -> Result<()> {
    soap(agent, avt, "Play", &[ID, ("Speed", "1")]).map(|_| ())
}

pub(crate) fn pause(agent: &ureq::Agent, avt: &Service) -> Result<()> {
    soap(agent, avt, "Pause", &[ID]).map(|_| ())
}

pub(crate) fn stop(agent: &ureq::Agent, avt: &Service) -> Result<()> {
    soap(agent, avt, "Stop", &[ID]).map(|_| ())
}

pub(crate) fn seek(agent: &ureq::Agent, avt: &Service, ms: i64) -> Result<()> {
    soap(
        agent,
        avt,
        "Seek",
        &[ID, ("Unit", "REL_TIME"), ("Target", &fmt_hms(ms))],
    )
    .map(|_| ())
}

/// `CurrentTransportState` (PLAYING, PAUSED_PLAYBACK, STOPPED, TRANSITIONING,
/// NO_MEDIA_PRESENT).
pub(crate) fn transport_info(agent: &ureq::Agent, avt: &Service) -> Result<String> {
    let v = soap(agent, avt, "GetTransportInfo", &[ID])?;
    Ok(v.get("CurrentTransportState").cloned().unwrap_or_default())
}

/// (position ms, duration ms, track URI) of the running track.
pub(crate) fn position_info(
    agent: &ureq::Agent,
    avt: &Service,
) -> Result<(Option<i64>, Option<i64>, String)> {
    let v = soap(agent, avt, "GetPositionInfo", &[ID])?;
    let get = |k: &str| v.get(k).map(String::as_str).unwrap_or("");
    Ok((
        parse_hms(get("RelTime")),
        parse_hms(get("TrackDuration")).filter(|&d| d > 0),
        get("TrackURI").to_string(),
    ))
}

/// Renderer volume 0…100. Sonos: the group volume on the coordinator, so a
/// grouped room set follows as a whole.
pub(crate) fn get_volume(agent: &ureq::Agent, dev: &Device) -> Result<u32> {
    let v = if let (Kind::Sonos, Some(grc)) = (&dev.kind, &dev.grc) {
        soap(agent, grc, "GetGroupVolume", &[ID])?
    } else {
        let rc = dev
            .rc
            .as_ref()
            .ok_or_else(|| anyhow!("no RenderingControl"))?;
        soap(agent, rc, "GetVolume", &[ID, ("Channel", "Master")])?
    };
    v.get("CurrentVolume")
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| anyhow!("no volume in reply"))
}

pub(crate) fn set_volume(agent: &ureq::Agent, dev: &Device, vol: u32) -> Result<()> {
    let vol = vol.min(100).to_string();
    if let (Kind::Sonos, Some(grc)) = (&dev.kind, &dev.grc) {
        return soap(agent, grc, "SetGroupVolume", &[ID, ("DesiredVolume", &vol)]).map(|_| ());
    }
    let rc = dev
        .rc
        .as_ref()
        .ok_or_else(|| anyhow!("no RenderingControl"))?;
    soap(
        agent,
        rc,
        "SetVolume",
        &[ID, ("Channel", "Master"), ("DesiredVolume", &vol)],
    )
    .map(|_| ())
}

/// MIME types the renderer accepts over HTTP (`GetProtocolInfo` sink list).
/// `None` when the device does not say — callers then assume the common ones.
pub(crate) fn sink_mimes(agent: &ureq::Agent, dev: &Device) -> Option<Vec<String>> {
    let cm = dev.cm.as_ref()?;
    let v = soap(agent, cm, "GetProtocolInfo", &[]).ok()?;
    let mimes: Vec<String> = v
        .get("Sink")?
        .split(',')
        .filter_map(|p| {
            let mut f = p.trim().split(':');
            let proto = f.next()?;
            let _net = f.next()?;
            let mime = f.next()?;
            (proto == "http-get").then(|| mime.to_ascii_lowercase())
        })
        .collect();
    (!mimes.is_empty()).then_some(mimes)
}

// --- Sonos groups -------------------------------------------------------------

/// One Sonos room group: the coordinator's UUID and the member count.
#[derive(Debug, PartialEq)]
pub(crate) struct SonosGroup {
    pub coordinator: String,
    pub members: usize,
}

/// The visible groups of a Sonos household (`GetZoneGroupState`). Members that
/// are not rooms of their own (sub, surrounds: `Invisible="1"`) do not count.
pub(crate) fn sonos_groups(agent: &ureq::Agent, zgt: &Service) -> Result<Vec<SonosGroup>> {
    let v = soap(agent, zgt, "GetZoneGroupState", &[])?;
    let state = v
        .get("ZoneGroupState")
        .ok_or_else(|| anyhow!("no ZoneGroupState"))?;
    Ok(parse_zone_groups(state))
}

fn parse_zone_groups(xml: &str) -> Vec<SonosGroup> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out: Vec<SonosGroup> = Vec::new();
    loop {
        let ev = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(ev) => ev,
        };
        let (Event::Start(e) | Event::Empty(e)) = &ev else {
            continue;
        };
        let attr = |name: &str| {
            e.attributes()
                .flatten()
                .find(|a| a.key.as_ref() == name.as_bytes())
                .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
                .map(|v| v.into_owned())
        };
        match e.local_name().as_ref() {
            b"ZoneGroup" => out.push(SonosGroup {
                coordinator: attr("Coordinator").unwrap_or_default(),
                members: 0,
            }),
            b"ZoneGroupMember" if attr("Invisible").as_deref() != Some("1") => {
                if let Some(g) = out.last_mut() {
                    g.members += 1;
                }
            }
            _ => {}
        }
    }
    out
}

// --- Metadata ------------------------------------------------------------------

/// What the renderer shows for the running item.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Local cover file; published on the LAN server at load time.
    pub art_path: Option<String>,
    pub duration_ms: Option<i64>,
}

/// DIDL-Lite for one item. `live` marks an endless stream (radio) — renderers
/// then show it as a broadcast and expect no duration.
pub(crate) fn didl(
    url: &str,
    mime: &str,
    meta: &Meta,
    art_url: Option<&str>,
    live: bool,
) -> String {
    let mut item = format!("<dc:title>{}</dc:title>", esc(&meta.title));
    if let Some(a) = meta.artist.as_deref().filter(|s| !s.is_empty()) {
        item.push_str(&format!(
            "<dc:creator>{0}</dc:creator><upnp:artist>{0}</upnp:artist>",
            esc(a)
        ));
    }
    if let Some(a) = meta.album.as_deref().filter(|s| !s.is_empty()) {
        item.push_str(&format!("<upnp:album>{}</upnp:album>", esc(a)));
    }
    if let Some(a) = art_url {
        item.push_str(&format!("<upnp:albumArtURI>{}</upnp:albumArtURI>", esc(a)));
    }
    let class = if live {
        "object.item.audioItem.audioBroadcast"
    } else {
        "object.item.audioItem.musicTrack"
    };
    item.push_str(&format!("<upnp:class>{class}</upnp:class>"));
    let duration = match meta.duration_ms {
        Some(ms) if !live && ms > 0 => format!(" duration=\"{}.000\"", fmt_hms(ms)),
        _ => String::new(),
    };
    item.push_str(&format!(
        "<res protocolInfo=\"http-get:*:{mime}:*\"{duration}>{}</res>",
        esc(url)
    ));
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"1\" parentID=\"0\" restricted=\"1\">{item}</item></DIDL-Lite>"
    )
}

// --- Time --------------------------------------------------------------------

/// `H:MM:SS[.fff]` → ms. `None` for `NOT_IMPLEMENTED`, empty or junk.
pub(crate) fn parse_hms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: i64 = parts[0].trim_start_matches('+').parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let sec: i64 = parts[2].parse().ok()?;
    // Fractions come as `.123` or `.5/10`-style; only the decimal form counts.
    let ms = frac
        .get(..frac.len().min(3))
        .filter(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()))
        .map(|f| f.parse::<i64>().unwrap_or(0) * 10i64.pow(3 - f.len() as u32))
        .unwrap_or(0);
    Some(((h * 60 + m) * 60 + sec) * 1000 + ms)
}

pub(crate) fn fmt_hms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_round_trip() {
        assert_eq!(parse_hms("0:03:12"), Some(192_000));
        assert_eq!(parse_hms("1:00:00.500"), Some(3_600_500));
        assert_eq!(parse_hms("00:00:01.5"), Some(1_500));
        assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
        assert_eq!(parse_hms(""), None);
        assert_eq!(fmt_hms(192_999), "0:03:12");
        assert_eq!(fmt_hms(3_723_000), "1:02:03");
    }

    #[test]
    fn description_collects_embedded_services() {
        // Shape of a Sonos description: the root ZonePlayer carries the
        // topology service, the embedded MediaRenderer the transport.
        let xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType>
<friendlyName>192.168.1.20 - Sonos One</friendlyName>
<manufacturer>Sonos, Inc.</manufacturer><modelName>Sonos One</modelName>
<roomName>K&#xFC;che</roomName><UDN>uuid:RINCON_AAA01400</UDN>
<serviceList><service><serviceType>urn:schemas-upnp-org:service:ZoneGroupTopology:1</serviceType>
<controlURL>/ZoneGroupTopology/Control</controlURL></service></serviceList>
<deviceList><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
<friendlyName>inner</friendlyName><UDN>uuid:RINCON_AAA01400_MR</UDN>
<serviceList>
<service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
<controlURL>/MediaRenderer/AVTransport/Control</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:GroupRenderingControl:1</serviceType>
<controlURL>/MediaRenderer/GroupRenderingControl/Control</controlURL></service>
</serviceList></device></deviceList></device></root>"#;
        let d = parse_description(xml);
        assert_eq!(d.friendly_name, "192.168.1.20 - Sonos One");
        assert_eq!(d.room_name, "Küche");
        assert_eq!(d.udn, "uuid:RINCON_AAA01400");
        assert_eq!(d.services.len(), 3);
        let s = find_service(&d, "http://192.168.1.20:1400", AVT).unwrap();
        assert_eq!(
            s.control_url,
            "http://192.168.1.20:1400/MediaRenderer/AVTransport/Control"
        );
    }

    #[test]
    fn zone_groups_skip_invisible_members() {
        let xml = r#"<ZoneGroupState><ZoneGroups>
<ZoneGroup Coordinator="RINCON_A" ID="x"><ZoneGroupMember UUID="RINCON_A" ZoneName="Wohnzimmer"/>
<ZoneGroupMember UUID="RINCON_S" ZoneName="Wohnzimmer" Invisible="1"/>
<ZoneGroupMember UUID="RINCON_B" ZoneName="Küche"/></ZoneGroup>
<ZoneGroup Coordinator="RINCON_C" ID="y"><ZoneGroupMember UUID="RINCON_C" ZoneName="Bad"/></ZoneGroup>
</ZoneGroups></ZoneGroupState>"#;
        assert_eq!(
            parse_zone_groups(xml),
            vec![
                SonosGroup {
                    coordinator: "RINCON_A".into(),
                    members: 2
                },
                SonosGroup {
                    coordinator: "RINCON_C".into(),
                    members: 1
                },
            ]
        );
    }

    #[test]
    fn didl_escapes_and_marks_streams() {
        let meta = Meta {
            title: "Rock & Roll".into(),
            artist: Some("A<B".into()),
            duration_ms: Some(61_000),
            ..Default::default()
        };
        let d = didl("http://h/x?a=1&b=2", "audio/mpeg", &meta, None, false);
        assert!(d.contains("<dc:title>Rock &amp; Roll</dc:title>"));
        assert!(d.contains("<upnp:artist>A&lt;B</upnp:artist>"));
        assert!(d.contains("duration=\"0:01:01.000\""));
        assert!(d.contains(">http://h/x?a=1&amp;b=2</res>"));
        let live = didl(
            "http://h/s",
            "audio/mpeg",
            &meta,
            Some("http://h/c.jpg"),
            true,
        );
        assert!(live.contains("audioBroadcast"));
        assert!(!live.contains("duration="));
        assert!(live.contains("<upnp:albumArtURI>http://h/c.jpg</upnp:albumArtURI>"));
    }

    #[test]
    fn urls_resolve_against_the_base() {
        assert_eq!(resolve("http://h:1400", "/a/b"), "http://h:1400/a/b");
        assert_eq!(resolve("http://h:1400/", "a/b"), "http://h:1400/a/b");
        assert_eq!(resolve("http://h", "http://x/y"), "http://x/y");
        assert_eq!(
            host_of("http://192.168.0.67:8080/dd.xml").as_deref(),
            Some("192.168.0.67")
        );
        assert_eq!(
            origin("http://h:8080/dd.xml").as_deref(),
            Some("http://h:8080")
        );
    }
}
