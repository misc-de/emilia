//! What a cast session needs from a renderer protocol. The session worker
//! (publishing items on the LAN server, transcoding, end/takeover detection,
//! position bookkeeping) is protocol-neutral; UPnP/DLNA/Sonos and Google
//! Cast plug in here.

use anyhow::{Result, anyhow};

use super::session::Transport;
use super::upnp::{self, Meta};
use super::{Device, Kind, Service};

/// One item to put on the renderer (already published on the LAN server).
pub(crate) struct LoadItem<'a> {
    pub url: &'a str,
    pub mime: &'a str,
    pub meta: &'a Meta,
    pub art_url: Option<&'a str>,
    pub live: bool,
    pub play: bool,
    /// Position to start at; see [`Backend::load`]'s return value.
    pub start_ms: i64,
}

/// One look at the renderer.
pub(crate) struct Poll {
    pub state: Transport,
    pub pos: Option<i64>,
    pub dur: Option<i64>,
    /// What the renderer plays (URL / content id); empty = it did not say.
    pub current: String,
    /// The item could not be played.
    pub failed: bool,
    /// Someone else took the renderer over.
    pub lost: bool,
}

pub(crate) trait Backend: Send {
    /// MIME types the renderer takes (`None`: it did not say).
    fn accepts(&mut self) -> Option<Vec<String>>;
    /// Volume 0…100.
    fn volume(&mut self) -> Result<u32>;
    fn set_volume(&mut self, v: u32) -> Result<()>;
    /// Puts the item on the renderer. `true` when `start_ms` was applied with
    /// the load; otherwise the session seeks once the renderer plays.
    fn load(&mut self, item: &LoadItem) -> Result<bool>;
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn seek(&mut self, ms: i64) -> Result<()>;
    /// `active`: something of ours is loaded (position worth asking for).
    fn poll(&mut self, active: bool) -> Result<Poll>;
}

/// Opens the backend for `dev` (blocking: Cast connects over TLS).
pub(crate) fn open(dev: &Device) -> Result<Box<dyn Backend>> {
    match dev.kind {
        Kind::Cast => Ok(Box::new(super::castv2::CastBackend::connect(
            &dev.host, dev.port,
        )?)),
        Kind::Sonos | Kind::Dlna => {
            let avt = dev
                .avt
                .clone()
                .ok_or_else(|| anyhow!("renderer has no AVTransport"))?;
            Ok(Box::new(UpnpBackend {
                agent: upnp::agent(),
                dev: dev.clone(),
                avt,
            }))
        }
    }
}

struct UpnpBackend {
    agent: ureq::Agent,
    dev: Device,
    avt: Service,
}

impl Backend for UpnpBackend {
    fn accepts(&mut self) -> Option<Vec<String>> {
        upnp::sink_mimes(&self.agent, &self.dev)
    }

    fn volume(&mut self) -> Result<u32> {
        upnp::get_volume(&self.agent, &self.dev)
    }

    fn set_volume(&mut self, v: u32) -> Result<()> {
        upnp::set_volume(&self.agent, &self.dev, v)
    }

    fn load(&mut self, item: &LoadItem) -> Result<bool> {
        // Sonos plays an endless MP3 stream only under its radio scheme.
        let target = if item.live && self.dev.kind == Kind::Sonos && item.mime == "audio/mpeg" {
            item.url.replacen("http://", "x-rincon-mp3radio://", 1)
        } else {
            item.url.to_string()
        };
        let didl = upnp::didl(item.url, item.mime, item.meta, item.art_url, item.live);
        let _ = upnp::stop(&self.agent, &self.avt);
        upnp::set_uri(&self.agent, &self.avt, &target, &didl)?;
        if item.play {
            upnp::play(&self.agent, &self.avt)?;
        }
        // Most renderers cannot seek before they play: the session does it.
        Ok(false)
    }

    fn play(&mut self) -> Result<()> {
        upnp::play(&self.agent, &self.avt)
    }

    fn pause(&mut self) -> Result<()> {
        upnp::pause(&self.agent, &self.avt)
    }

    fn stop(&mut self) -> Result<()> {
        upnp::stop(&self.agent, &self.avt)
    }

    fn seek(&mut self, ms: i64) -> Result<()> {
        upnp::seek(&self.agent, &self.avt, ms)
    }

    fn poll(&mut self, active: bool) -> Result<Poll> {
        let state = Transport::parse(&upnp::transport_info(&self.agent, &self.avt)?);
        let (pos, dur, current) = if active {
            upnp::position_info(&self.agent, &self.avt).unwrap_or((None, None, String::new()))
        } else {
            (None, None, String::new())
        };
        Ok(Poll {
            state,
            pos,
            dur,
            current,
            failed: false,
            lost: false,
        })
    }
}
