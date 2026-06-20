//! The shared player-bar record button. Each helper reads `visible_child_name()`
//! once instead of the view watches calling it 6–7× per pass. Extracted from
//! app.rs – pure reordering, no change in behavior; the methods remain inherent
//! `impl App` methods. Used from the `view!` watches and `update`.

use crate::i18n::gettext;
use crate::ui::app::App;

impl App {
    pub(crate) fn on_streaming_section(&self) -> bool {
        self.nav.view_stack.visible_child_name().as_deref() == Some("streaming")
    }
    /// The shared record button acts on the timeshift recorder while on the
    /// Streaming section **or** whenever a timeshift recording is actually
    /// running — so a running recording can still be seen and stopped after
    /// navigating to another section. Otherwise it is the voice-memo button.
    pub(crate) fn record_btn_is_timeshift(&self) -> bool {
        self.on_streaming_section() || self.streaming.record_state.is_some()
    }
    pub(crate) fn record_btn_visible(&self) -> bool {
        // A running timeshift recording keeps the button visible everywhere, so
        // it never runs without a reachable Stop control.
        if self.streaming.record_state.is_some() {
            return true;
        }
        match self.nav.view_stack.visible_child_name().as_deref() {
            Some("memo") => true,
            Some("streaming") => {
                self.streaming.playing_stream.is_some()
                    && self.streaming.recording_buffer_minutes > 0
            }
            _ => false,
        }
    }
    pub(crate) fn record_btn_icon(&self) -> &'static str {
        if self.record_btn_is_timeshift() {
            "media-record-symbolic"
        } else {
            "audio-input-microphone-symbolic"
        }
    }
    pub(crate) fn record_btn_tooltip(&self) -> String {
        if self.record_btn_is_timeshift() {
            if self.streaming.record_state.is_some() {
                gettext("Stop recording")
            } else {
                gettext("Record")
            }
        } else if self.memo.recording {
            gettext("Stop the voice memo")
        } else {
            gettext("Record a voice memo")
        }
    }
    pub(crate) fn record_btn_recording(&self) -> bool {
        if self.record_btn_is_timeshift() {
            self.streaming.record_state.is_some()
        } else {
            self.memo.recording
        }
    }
}
