//! Startup wiring of the root component, lifted out of `App::init`: the
//! player bus callbacks and the background timers (resume persist, per-second
//! tick, auto-enrich, source checks, yt-dlp updates), plus the launches of the
//! extracted child components with their `Output` → [`Msg`] mapping. Pure code
//! movement – `init` calls these in the same order as before.

use relm4::gtk;
use relm4::prelude::*;

use crate::core::player::Player;
use crate::ui::app::{App, McpState, Msg, AUTO_ENRICH_INTERVAL_SECS};
use crate::ui::app_dialogs::CtxMsg;
use crate::ui::app_episode_playback::PodcastMsg;
use crate::ui::app_playback::TransportMsg;
use crate::ui::app_rec_edit::EditMsg;
use crate::ui::app_settings::SettingMsg;
use crate::ui::app_streaming::StreamMsg;
use crate::ui::app_yt_glue::YtMsg;

impl App {
    /// Connects the player's bus events to the transport messages and starts
    /// the background timers. `tick_active` gates the per-second tick and the
    /// resume-persist timer (see [`App::sync_tick_active`]).
    pub(crate) fn connect_player_and_timers(
        player: &Player,
        sender: &ComponentSender<Self>,
        tick_active: &std::rc::Rc<std::cell::Cell<bool>>,
    ) {
        // At the end of a track, automatically play the next entry of the queue;
        // report title tags (for stations: the running ICY title) as `StreamTitle`.
        {
            let sender = sender.clone();
            player.connect_bus_events(
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::Transport(TransportMsg::TrackFinished))
                },
                {
                    let sender = sender.clone();
                    move |title| sender.input(Msg::Stream(StreamMsg::StreamTitle(title)))
                },
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::Transport(TransportMsg::PlaybackError))
                },
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::Transport(TransportMsg::PlaybackReady))
                },
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::Transport(TransportMsg::GaplessAdvanced))
                },
                move |chapters| {
                    sender.input(Msg::Transport(TransportMsg::EmbeddedChapters(chapters)))
                },
            );
        }

        // During playback, regularly save the resume position, so that
        // an audio drama also resumes there after a crash/close.
        {
            let sender = sender.clone();
            let tick_active = tick_active.clone();
            gtk::glib::timeout_add_seconds_local(5, move || {
                if tick_active.get() {
                    sender.input(Msg::Transport(TransportMsg::PersistResume));
                }
                gtk::glib::ControlFlow::Continue
            });
        }

        // Per-second tick for the seek bar (position/duration), gated by
        // `tick_active` like the resume-persist timer above.
        {
            let sender = sender.clone();
            let tick_active = tick_active.clone();
            gtk::glib::timeout_add_seconds_local(1, move || {
                if tick_active.get() {
                    sender.input(Msg::Transport(TransportMsg::Tick));
                }
                gtk::glib::ControlFlow::Continue
            });
        }

        // Quiet background backfill: gradually fills in missing artist photos
        // (first) and online covers, without user action – so that even without a new
        // scan (returning users, no signal on the first run, failed
        // individual fetches) the overview gets enriched. The worker is rate-limited
        // and skips already loaded/permanently unsuccessful items; if nothing is pending,
        // the tick fizzles out almost for free (no network, no UI update).
        {
            let sender = sender.clone();
            gtk::glib::timeout_add_seconds_local(AUTO_ENRICH_INTERVAL_SECS, move || {
                sender.input(Msg::AutoEnrichTick);
                gtk::glib::ControlFlow::Continue
            });
        }

        // Check reachability of the Nextcloud sources once at startup and then
        // regularly (controls the red "Disconnected" hint).
        {
            let sender = sender.clone();
            sender.input(Msg::Source(crate::ui::app_views_sources::SourceMsg::Check));
            gtk::glib::timeout_add_seconds_local(45, move || {
                sender.input(Msg::Source(crate::ui::app_views_sources::SourceMsg::Check));
                gtk::glib::ControlFlow::Continue
            });
        }

        // Keep the managed yt-dlp fresh hands-off: check once at startup and then
        // every 12 h. The handler is a no-op unless YouTube is on and the copy is
        // actually stale (so it costs nothing on most ticks).
        {
            let sender = sender.clone();
            sender.input(Msg::Yt(YtMsg::YtDlpAutoUpdate));
            gtk::glib::timeout_add_seconds_local(12 * 60 * 60, move || {
                sender.input(Msg::Yt(YtMsg::YtDlpAutoUpdate));
                gtk::glib::ControlFlow::Continue
            });
        }
    }
}

/// Launches the [`SyncPage`](crate::ui::sync_page::SyncPage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_sync_page(
    sender: &ComponentSender<App>,
    mcp: &McpState,
) -> relm4::Controller<crate::ui::sync_page::SyncPage> {
    crate::ui::sync_page::SyncPage::builder()
        .launch(mcp.sync.clone())
        .forward(sender.input_sender(), |out| match out {
            crate::ui::sync_page::SyncOutput::ConnectedChanged(b) => Msg::SyncConnected(b),
            crate::ui::sync_page::SyncOutput::Imported => Msg::SyncImported,
            crate::ui::sync_page::SyncOutput::BusyChanged(b) => Msg::SyncBusy(b),
        })
}

/// Launches the [`CloudPage`](crate::ui::cloud_page::CloudPage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_cloud_page(
    sender: &ComponentSender<App>,
) -> relm4::Controller<crate::ui::cloud_page::CloudPage> {
    crate::ui::cloud_page::CloudPage::builder()
        .launch(())
        .forward(sender.input_sender(), |out| match out {
            crate::ui::cloud_page::CloudOutput::SourcesChanged(id) => {
                Msg::Source(crate::ui::app_views_sources::SourceMsg::Added(id))
            }
            crate::ui::cloud_page::CloudOutput::Indexed => {
                Msg::Source(crate::ui::app_views_sources::SourceMsg::CloudIndexed)
            }
        })
}

/// Launches the [`SmbPage`](crate::ui::smb_page::SmbPage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_smb_page(
    sender: &ComponentSender<App>,
) -> relm4::Controller<crate::ui::smb_page::SmbPage> {
    crate::ui::smb_page::SmbPage::builder()
        .launch(())
        .forward(sender.input_sender(), |out| match out {
            crate::ui::smb_page::SmbOutput::SourcesChanged(id) => {
                Msg::Source(crate::ui::app_views_sources::SourceMsg::Added(id))
            }
            crate::ui::smb_page::SmbOutput::Indexed => {
                Msg::Source(crate::ui::app_views_sources::SourceMsg::CloudIndexed)
            }
        })
}

/// Launches the [`GDrivePage`](crate::ui::gdrive_page::GDrivePage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_gdrive_page(
    sender: &ComponentSender<App>,
) -> relm4::Controller<crate::ui::gdrive_page::GDrivePage> {
    crate::ui::gdrive_page::GDrivePage::builder()
        .launch(())
        .forward(sender.input_sender(), |out| match out {
            crate::ui::gdrive_page::GDriveOutput::SourcesChanged(id) => {
                Msg::Source(crate::ui::app_views_sources::SourceMsg::Added(id))
            }
            crate::ui::gdrive_page::GDriveOutput::Indexed => {
                Msg::Source(crate::ui::app_views_sources::SourceMsg::CloudIndexed)
            }
        })
}

/// Launches the [`PodcastsPage`](crate::ui::podcasts_page::PodcastsPage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_podcasts_page(
    sender: &ComponentSender<App>,
    podcast_subpage: &std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>>,
    podcast_sort: &crate::ui::app_sort::SortSlot,
) -> relm4::Controller<crate::ui::podcasts_page::PodcastsPage> {
    crate::ui::podcasts_page::PodcastsPage::builder()
        .launch((podcast_subpage.clone(), podcast_sort.clone()))
        .forward(sender.input_sender(), |out| {
            use crate::ui::podcasts_page::PodcastsOutput as O;
            match out {
                O::ToggleEpisode { url, title } => {
                    Msg::Podcast(PodcastMsg::ToggleEpisode { url, title })
                }
                O::EpisodeSeekTo { url, title, ms } => {
                    Msg::Podcast(PodcastMsg::EpisodeSeekTo { url, title, ms })
                }
                O::OpenPodcastEqualizer(id) => Msg::Podcast(PodcastMsg::OpenPodcastEq(id)),
                O::OpenEpisodeEqualizer { url, title } => {
                    Msg::Podcast(PodcastMsg::OpenEpisodeEq { url, title })
                }
                O::PushSubpage => Msg::Podcast(PodcastMsg::PushPodcastSubpage),
                O::Share(sel) => Msg::Ctx(CtxMsg::ShareItems(sel)),
                O::Toast(s) => Msg::Podcast(PodcastMsg::PodcastToast(s)),
                O::DeletedUndoToast(id) => Msg::Podcast(PodcastMsg::PodcastUndoToast(id)),
                O::RefreshStarted(b) => Msg::Podcast(PodcastMsg::PodcastRefreshStarted(b)),
                O::RefreshFinished => Msg::Podcast(PodcastMsg::PodcastRefreshFinished),
                O::RefreshProgress { done, total, label } => {
                    Msg::RefreshProgress { done, total, label }
                }
                O::RefreshSummary(s) => Msg::RefreshSummary(s),
                O::SortChanged => Msg::Sort(crate::ui::app_sort::SortMsg::PodcastChanged),
            }
        })
}

/// Launches the [`YtPage`](crate::ui::yt_page::YtPage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_yt_page(
    sender: &ComponentSender<App>,
    yt_subpage: &std::rc::Rc<std::cell::RefCell<Option<(String, gtk::Box)>>>,
    yt_sort: &crate::ui::app_sort::SortSlot,
) -> relm4::Controller<crate::ui::yt_page::YtPage> {
    crate::ui::yt_page::YtPage::builder()
        .launch((yt_subpage.clone(), yt_sort.clone()))
        .forward(sender.input_sender(), |out| {
            use crate::ui::yt_page::YtOutput as O;
            match out {
                O::PlayVideo {
                    video_id,
                    title,
                    keep_recent_order,
                } => Msg::Yt(YtMsg::YtPlayVideo {
                    video_id,
                    title,
                    keep_recent_order,
                }),
                O::PlayLive { video_id, title } => Msg::Yt(YtMsg::YtPlayLive { video_id, title }),
                O::PlayVideoAt {
                    video_id,
                    title,
                    ms,
                } => Msg::Yt(YtMsg::YtPlayVideoAt {
                    video_id,
                    title,
                    ms,
                }),
                O::PlayChannel(id) => Msg::Yt(YtMsg::YtPlayChannel(id)),
                O::StartPlaylist { url, title } => Msg::Yt(YtMsg::YtStartPlaylist { url, title }),
                O::StartPlaylistAt {
                    url,
                    title,
                    index,
                    close,
                    videos,
                } => Msg::Yt(YtMsg::YtStartPlaylistAt {
                    url,
                    title,
                    index,
                    close,
                    videos,
                }),
                O::OpenTrackEq { path, title } => Msg::OpenTrackEq { path, title },
                O::OpenLiveEq { video_id, title } => Msg::OpenLiveEq { video_id, title },
                O::OpenPlaylist { id, name } => Msg::Yt(YtMsg::YtOpenPlaylist { id, name }),
                O::OpenSettings => Msg::OpenSettings,
                O::Toast(s) => Msg::Yt(YtMsg::YtToast(s)),
                O::Progress(s) => Msg::Yt(YtMsg::YtProgress(s)),
                O::ProgressDone(s) => Msg::Yt(YtMsg::YtProgressDone(s)),
                O::SetLoading(o) => Msg::Yt(YtMsg::YtSetLoading(o)),
                O::LibraryChanged => Msg::Yt(YtMsg::YtLibraryChanged),
                O::PlaylistsChanged => Msg::Yt(YtMsg::YtPlaylistsChanged),
                O::PushSubpage => Msg::Yt(YtMsg::PushYtSubpage),
                O::DeleteChannelUndo(id) => Msg::Yt(YtMsg::YtChannelUndo(id)),
                O::RefreshStarted(b) => Msg::Yt(YtMsg::YtRefreshStarted(b)),
                O::RefreshFinished => Msg::Yt(YtMsg::YtRefreshFinished),
                O::RefreshProgress { done, total, label } => {
                    Msg::RefreshProgress { done, total, label }
                }
                O::RefreshSummary(s) => Msg::RefreshSummary(s),
                O::Share(sel) => Msg::Ctx(CtxMsg::ShareItems(sel)),
                O::SortChanged => Msg::Sort(crate::ui::app_sort::SortMsg::YtChanged),
            }
        })
}

/// Launches the [`StreamPage`](crate::ui::stream_page::StreamPage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_stream_page(
    sender: &ComponentSender<App>,
    stream_sort: &crate::ui::app_sort::SortSlot,
) -> relm4::Controller<crate::ui::stream_page::StreamPage> {
    crate::ui::stream_page::StreamPage::builder()
        .launch(stream_sort.clone())
        .forward(sender.input_sender(), |out| {
            use crate::ui::stream_page::StreamOutput as O;
            match out {
                O::ToggleStream(id) => Msg::Stream(StreamMsg::ToggleStream(id)),
                O::PlayRecording(path) => Msg::Stream(StreamMsg::PlayRecording(path)),
                O::OpenReplay(id) => Msg::Stream(StreamMsg::OpenStreamReplay(id)),
                O::OpenEqualizer(id) => Msg::Stream(StreamMsg::OpenStreamEq(id)),
                O::EditRecording(id) => Msg::Edit(EditMsg::EditRecording(id)),
                O::StreamDeleteUndo(id) => Msg::Stream(StreamMsg::StreamDeleteUndo(id)),
                O::RecordingDeleteUndo(id) => Msg::Stream(StreamMsg::RecordingDeleteUndo(id)),
                O::LibraryChanged => Msg::Stream(StreamMsg::StreamLibraryChanged),
                O::PlayHeard { artist, title } => {
                    Msg::Stream(StreamMsg::PlayHeard { artist, title })
                }
                O::DownloadHeard { artist, title } => {
                    Msg::Stream(StreamMsg::DownloadHeard { artist, title })
                }
                O::Share(sel) => Msg::Ctx(CtxMsg::ShareItems(sel)),
                O::Toast(s) => Msg::Stream(StreamMsg::StreamToast(s)),
                O::SortChanged => Msg::Sort(crate::ui::app_sort::SortMsg::StreamChanged),
            }
        })
}

/// Launches the [`SetupPage`](crate::ui::setup::SetupPage)
/// component and forwards its output into the root component's [`Msg`].
pub(crate) fn launch_setup_page(
    sender: &ComponentSender<App>,
) -> relm4::Controller<crate::ui::setup::SetupPage> {
    crate::ui::setup::SetupPage::builder().launch(()).forward(
        sender.input_sender(),
        |out| match out {
            crate::ui::setup::SetupOutput::Finished {
                lang_code,
                music_dir,
                enabled_sections,
            } => Msg::Setting(SettingMsg::SetupFinished {
                lang_code,
                music_dir,
                enabled_sections,
            }),
        },
    )
}
