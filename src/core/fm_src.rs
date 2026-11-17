//! GStreamer source for FM playback: `emilia-fm://<freq>` (see [`crate::core::fm`]).
//!
//! A bin around `pulsesrc device=hal_fm_input` — the tuner's sound as the
//! audio-hal-bridge offers it — that switches the tuner on, tuned to the URI's
//! frequency, when it leaves NULL, and off again when it returns there. Being a
//! URI handler, `playbin3` takes it like any stream: equalizer, volume, fades,
//! the output choice and MPRIS work for FM unchanged, and the sound reaches
//! the speaker, wired or Bluetooth headphones alike.
//!
//! Registered once, process-wide, by [`register`] (from `Player::new`).

use std::sync::Mutex;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;

use crate::core::fm;

mod imp {
    use super::*;

    #[derive(Default)]
    struct State {
        freq: u16,
        lease: Option<fm::Lease>,
    }

    pub struct FmSrc {
        state: Mutex<State>,
    }

    impl Default for FmSrc {
        fn default() -> Self {
            Self {
                state: Mutex::new(State {
                    freq: fm::FREQ_DEFAULT,
                    lease: None,
                }),
            }
        }
    }

    impl FmSrc {
        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FmSrc {
        const NAME: &'static str = "EmiliaFmSrc";
        type Type = super::FmSrc;
        type ParentType = gst::Bin;
        type Interfaces = (gst::URIHandler,);
    }

    impl ObjectImpl for FmSrc {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let src = match gst::ElementFactory::make("pulsesrc")
                .name("fmpulsesrc")
                .property("device", fm::SOURCE)
                .property("client-name", "Emilia FM")
                .build()
            {
                Ok(src) => src,
                Err(e) => {
                    tracing::error!("FM: pulsesrc unavailable: {e}");
                    return;
                }
            };
            if obj.add(&src).is_err() {
                return;
            }
            let Some(target) = src.static_pad("src") else {
                return;
            };
            let templ = obj
                .pad_template("src")
                .expect("FmSrc has a src pad template");
            match gst::GhostPad::from_template_with_target(&templ, &target) {
                Ok(ghost) => {
                    let _ = obj.add_pad(&ghost);
                }
                Err(e) => tracing::error!("FM: no ghost pad: {e}"),
            }
        }
    }

    impl GstObjectImpl for FmSrc {}

    impl ElementImpl for FmSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();
            Some(META.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "Emilia FM radio source",
                    "Source/Audio",
                    "The phone's FM tuner, tuned to the URI's frequency",
                    "Emilia",
                )
            }))
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> =
                std::sync::OnceLock::new();
            TEMPLATES.get_or_init(|| {
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &gst::Caps::new_any(),
                    )
                    .expect("valid pad template"),
                ]
            })
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            if transition == gst::StateChange::NullToReady {
                let freq = self.state().freq;
                match fm::Lease::tuned(freq) {
                    Ok(lease) => self.state().lease = Some(lease),
                    Err(e) => {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::OpenRead,
                            ["FM tuner {}: {}", fm::DEVICE, e]
                        );
                        return Err(gst::StateChangeError);
                    }
                }
            }
            let res = self.parent_change_state(transition);
            if transition == gst::StateChange::ReadyToNull || res.is_err() {
                // Drop the lease (last holder → tuner off) after the source
                // stopped — off this thread: the power-down waits for the
                // tuner's lock, which a running seek holds for seconds, and
                // this is usually the GTK thread stopping the player.
                if let Some(lease) = self.state().lease.take() {
                    std::thread::spawn(move || drop(lease));
                }
            }
            res
        }
    }

    impl BinImpl for FmSrc {}

    impl URIHandlerImpl for FmSrc {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &[fm::URI_SCHEME]
        }

        fn uri(&self) -> Option<String> {
            Some(fm::uri(self.state().freq))
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            let freq = fm::parse_uri(uri).ok_or_else(|| {
                glib::Error::new(gst::URIError::BadUri, &format!("not an FM URI: {uri}"))
            })?;
            self.state().freq = freq;
            Ok(())
        }
    }
}

glib::wrapper! {
    pub struct FmSrc(ObjectSubclass<imp::FmSrc>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::URIHandler, gst::ChildProxy;
}

/// Registers the `emilia-fm://` source with GStreamer (idempotent). Only on a
/// device with a tuner: elsewhere the scheme stays unknown. (Whether the
/// platform also carries the sound decides whether the FM section shows.)
pub fn register() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !fm::tuner_present() {
            return;
        }
        if let Err(e) = gst::Element::register(
            None,
            "emiliafmsrc",
            gst::Rank::PRIMARY,
            FmSrc::static_type(),
        ) {
            tracing::error!("FM: cannot register the source: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_the_fm_scheme() {
        gst::init().unwrap();
        let src: FmSrc = glib::Object::new();
        let handler = src.upcast_ref::<gst::URIHandler>();
        assert!(handler.set_uri("emilia-fm://9860").is_ok());
        assert_eq!(handler.uri().as_deref(), Some("emilia-fm://9860"));
        assert!(handler.set_uri("http://example.org").is_err());
        assert_eq!(handler.protocols(), vec!["emilia-fm".to_string()]);
    }
}
