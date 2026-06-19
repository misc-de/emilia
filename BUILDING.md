# Building Emilia from source

> **Not needed** for normal use – the [Flatpak](README.md#flatpak-recommended)
> is the recommended way to install Emilia. This page is for developers and
> anyone who wants to compile it themselves.

See the [README](README.md) for what Emilia is and how to use it.

---

## Requirements

- **Rust toolchain** (edition 2021), easiest via [rustup](https://rustup.rs)
- **GTK ≥ 4.14** and **libadwaita ≥ 1.5** (incl. dev headers)
- **GStreamer 1.x** (core + dev headers) plus the plugin packages *base*, *good*,
  *bad*, *ugly* and *libav* (for `playbin3`, the equalizer and common codecs)
- **PipeWire** for audio output (present on Phosh / recent distros)
- a **C compiler** + `pkg-config` (SQLite is bundled and built from source)
- *optional:* **`fpcalc`** (Chromaprint) for fingerprint track recognition, and
  **`yt-dlp`** for the YouTube source

---

## 1. Install dependencies

**Arch / Manjaro**

```bash
sudo pacman -S --needed rustup base-devel pkgconf \
  gtk4 libadwaita \
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav \
  chromaprint yt-dlp        # optional: fpcalc + the YouTube source
rustup default stable
```

**Debian / Ubuntu / Mobian**

```bash
sudo apt install build-essential pkg-config curl \
  libgtk-4-dev libadwaita-1-dev libdbus-1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
  gstreamer1.0-pipewire \
  libchromaprint-tools yt-dlp   # optional: fpcalc + the YouTube source
# Rust via rustup if not already present:
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**Fedora**

```bash
sudo dnf install cargo rust gcc pkgconf-pkg-config \
  gtk4-devel libadwaita-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel \
  gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-libav \
  chromaprint-tools yt-dlp     # optional: fpcalc + the YouTube source
# gstreamer1-plugins-ugly is in RPM Fusion
```

---

## 2. Build & run

```bash
git clone <repo-url> Emilia
cd Emilia

# During development:
cargo run

# Optimized release binary:
cargo build --release
./target/release/emilia
```

> Note: when started from the project folder (`cargo run`) the icons in
> `data/icons` are found. For permanent use, install it instead (below).

---

## 3. Install (optional)

So Emilia shows up in the app grid and displays its icon on the lock screen, the
`Makefile` installs the binary, `.desktop` file, app icon and AppStream metainfo
to the right XDG locations:

```bash
# system-wide (needs root):
sudo make install

# or just for your user (good for the phone, no root):
make install PREFIX=$HOME/.local
```

Remove again with `make uninstall` (same `PREFIX`). `make check` validates the
`.desktop` and metainfo with `desktop-file-validate` / `appstreamcli`.

---

## Build the Flatpak yourself

If you prefer to build a bundle yourself (instead of the pre-built one): a
manifest is in [`de.cais.Emilia.yaml`](de.cais.Emilia.yaml) (GNOME runtime +
rust-stable SDK). Build with `flatpak-builder` – the exact commands are in the
header of the manifest.

---

## Project layout

```
src/
  main.rs            App start (Adw::Application, app ID de.cais.Emilia)
  model.rs           Data models (Track, AlbumMeta, ArtistMeta, MemoItem, …)
  i18n.rs            Internationalization (gettext)
  ui/
    app.rs           Root component (init/update/view!), navigation, player bar
    app_init.rs      Post-view_output!() wiring split out of init()
    setup.rs         First-run setup assistant (standalone component)
    app_views.rs     Load/group folder/album/artist, subpages, ctx/cover helpers
    app_sort.rs      Per-category sorting of the library overviews
    app_filter.rs    Inline list filter (funnel button + live search bar)
    app_favorites.rs Favorites / audiobooks / concerts unified lists
    app_concert.rs   Concerts (detect & collect live recordings)
    app_gallery.rs   Cover/photo gallery carousel (dot navigation, upload)
    app_playback.rs  Playback, queue, resume (local & remote), running EQ
    app_queue.rs     Explicit user-queue dialog ("Add to queue")
    app_gapless.rs   Gapless + crossfade integration (sequential local queues)
    app_sleep.rs     Sleep timer (header "zzz" button, countdown, fade-out)
    app_lyrics.rs    Lyrics & karaoke (embedded → DB → LRCLIB, live highlight)
    app_playlist.rs  Playlists (list, subpage, dialogs)
    podcasts_page.rs Podcasts page component (feeds, episodes, detail dialogs)
    app_episode_playback.rs  Podcast-episode playback on the shared transport
    stream_page.rs   Internet-radio page component (stations, recordings)
    app_streaming.rs Streaming/timeshift transport (ICY, ring buffer, replay)
    app_rec_edit.rs  Recording/memo waveform editor (mark/cut, zoom/pan, overwrite)
    yt_page.rs       YouTube page component (search, lists, dialogs)
    app_yt_glue.rs   YouTube transport + yt-dlp/settings glue on App
    app_memo.rs      Voice-memo page (Recent/Category tabs, mic record button)
    cloud_page.rs    Nextcloud connect dialog (QR camera + manual)
    sync_page.rs     Device sync UI (QR pairing, optional webcam)
    sync_share_ui.rs Sync share flow (size confirm + receiver review)
    stats_page.rs    Listening statistics component
    app_eq.rs        Equalizer editor + property dialogs
    app_dialogs.rs   Action menu (long press), share, settings
    enrich.rs        Online enrichment worker (background)
    artist_row.rs    Artist card (with photo)
    album_row.rs     Album card (with cover)
    track_row.rs     Track row (relm4 factory)
    fs_row.rs        File-system row
    app_helpers.rs   Small shared App helpers
    widgets.rs       Shared UI helpers (cover frames, thumbnails)
  core/
    scanner.rs       Directory scan + lofty metadata → DB (background worker)
    db/              SQLite (rusqlite, bundled) + queries (split into submodules)
    player.rs        GStreamer wrapper (playbin3, equalizer-10bands, gapless/crossfade)
    waveform.rs      Recording/memo waveform decode + region cut/re-encode
    online.rs        Online enrichment (MusicBrainz/CAA/Deezer/AcoustID/fanart)
    lyrics.rs        LRC parsing + lyrics model (LRCLIB lookup)
    podcast.rs       Read podcast feeds (RSS), provide episodes
    streaming.rs     Station search (Radio-Browser API)
    recorder.rs      Timeshift ring buffer + ICY reader for recording
    mic.rs           Microphone capture for voice memos (Ogg/Opus)
    webdav.rs        Nextcloud/WebDAV: list, read tags, index, stream
    source.rs        Add local/WebDAV sources and secret-backed credentials
    secrets.rs       Secret Service bridge for app passwords
    sync/            LAN device sync (server, client, QR scanner)
    mpris.rs         MPRIS lock-screen / media-key control
    fingerprint.rs   Chromaprint (fpcalc) for track recognition
    cover.rs         Embedded & folder covers
    category.rs      EQ/property keys and cascade resolution
    output.rs        Audio outputs (PipeWire sinks) for EQ profiles
    concert.rs       Detect concert candidates
    artist.rs        "feat." splitting of artist credits
    youtube.rs       YouTube resolve/download/transcode (yt-dlp)
    net.rs           Shared download helpers (size-capped streaming)
```
