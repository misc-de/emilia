# Emilia

**Adaptive music, podcast, streaming and audio-drama player for Linux Phosh smartphones** (Librem 5,
PinePhone & co.) – runs equally well on the desktop. Written in **Rust**

⚠️ **AI-assisted project**  

> App ID: `de.cais.Emilia` · License: GPL-3.0-or-later

---

## Screenshots

| | |
|:-:|:-:|
| <img src="data/screenshots/desktop_artists.png" width="420"><br>*Library by artist and albums* | <img src="data/screenshots/desktop_streaming.png" width="420"><br>*Internet radio &amp; recordings* |
| <img src="data/screenshots/desktop_podcast.png" width="420"><br>*Podcasts* | <img src="data/screenshots/desktop_youtube.png" width="420"><br>*YouTube as a music source* |
| <img src="data/screenshots/desktop_equalizer.png" width="420"><br>*10-band equalizer with cascade* | <img src="data/screenshots/desktop_statistics.png" width="420"><br>*Listening statistics* |
| <img src="data/screenshots/desktop_preferences.png" width="420"><br>*Preferences* | <img src="data/screenshots/mobile_artists.png" width="210"><br>*Adaptive phone layout (portrait)* |

---

## What Emilia can do

- **Adaptive interface** – works in narrow portrait (phone) and on the desktop;
  the navigation collapses automatically on mobile.
- **Scan a music folder** – recursive background scan, tags & covers via
  `lofty`. Audio files are only ever **read**, never modified.
- **Several views** of the library:
  - **File system** – reliable navigation even with patchy tags (important for
    audio dramas). Additional music sources (a second local folder or a
    Nextcloud/WebDAV remote) appear here as their own **tabs**.
  - **Artists** – a single tap opens a subpage with the artist's **albums** (with
    covers) and, below them, the **single tracks** (guest/feature tracks and
    tracks without an album). An album opens its track list.
  - **Albums** – all albums with covers.
  - **Concerts** – mark and collect live/unplugged recordings; an import suggests
    likely candidates.
  - **Favorites**, **Audiobooks**, **Statistics**.
- **Cover & photo galleries** – open an album's or artist's detail view to pick
  among several cover/photo candidates (swipe the carousel or **tap the dots** to
  jump straight to one), or upload your own image. Choosing an artist photo never
  changes the album covers.
- **Playback** – play/pause, next/previous, shuffle, repeat, a queue, and a
  bottom mini-player with a **seek bar** (scrub through long tracks).
- **Resume for audio dramas** – long tracks (15 min+ or classified as
  audiobook/podcast) remember the playback position and continue there next time;
  once a track ends it starts from the beginning again.
- **Lock screen & media keys** – control via **MPRIS** (play/pause, next/previous,
  seek) including title/album display.
- **Playlists** – create your own playlists, add tracks/albums/folders via the
  options, play, rename and remove individual tracks.
- **Podcasts** – subscribe to feeds by RSS address or search the iTunes directory;
  episodes are **streamed** directly (no download), with show notes, chapter marks
  and resume. Refresh a feed, remove a podcast.
- **Streaming / Internet radio** – add a stream URL or **search stations
  worldwide** (Radio-Browser API). Live now-playing title from the ICY metadata,
  plus a **timeshift recorder**:
  - a rolling ring buffer (configurable up to 60 minutes) lets you **record a song
    even after it has played** – press record at the end of a song and the whole
    song is saved;
  - automatic split at song boundaries, online cover/metadata embedded into the
    saved file;
  - saved songs live in a dedicated **Recordings** section. From there you can
    **edit** a recording in a built-in **waveform editor** – mark a region, cut it
    out with the scissors, zoom (+/− or scroll) and pan, scrub a timeline and play
    from the playhead (*Save re-encodes and overwrites the file*) – or **add a
    recording to your music library** as a regular track.
- **YouTube** – search for tracks and play them in-app, or **add a track to your
  library**: it is downloaded via `yt-dlp` and transcoded to MP3 with cover and
  metadata, filed under `Artist/Album`. The section can be hidden in the
  navigation if you don't need it.
- **Nextcloud** – connect a Nextcloud (login QR code or manual), then **index its
  music into the library** so the tracks behave 1:1 like local songs (Artists,
  Albums, queue, resume). Audio streams on demand (cached on play); duration,
  covers and photos are cached locally for performance. A red **disconnected**
  badge appears on the affected covers, photos and songs while the source is
  unreachable.
- **Device sync** – share library/resume data between devices over the LAN with a
  QR-code pairing handshake.
- **Equalizer with cascade** – 10-band EQ (`equalizer-10bands`), live during
  playback. Settings apply in the order **Global → Artist → Album → Track** (the
  most specific level wins), additionally per **output device/headphones**
  (PipeWire sink).
- **Fetch online metadata** (optional) – from open sources:
  - album covers via **MusicBrainz** + **Cover Art Archive**
  - artist photos via **Deezer** (no key required)
  - track recognition via **AcoustID/Chromaprint** (needs `fpcalc` + a free
    AcoustID key) for files with patchy tags
  - extra image galleries via **fanart.tv** (optional key)

  Everything is stored only in the local database and the XDG cache – never in the
  audio files.

---

## Installation

### Flatpak (recommended)

Pre-built, **GPG-signed** bundle for **x86_64 and aarch64** – ideal for the phone,
no build tools needed. From the project repo (GitHub Pages):

```bash
flatpak remote-add --if-not-exists emilia https://misc-de.github.io/emilia/de.cais.Emilia.flatpakrepo
flatpak install emilia de.cais.Emilia
flatpak run de.cais.Emilia
```

Update later with `flatpak update de.cais.Emilia`. The signing key is already
embedded in the `.flatpakrepo` file – nothing needs to be imported separately.

> Prefer to compile it yourself? See
> [Building from source](#building-from-source-for-developers) at the bottom.

---

## Getting started

1. Start Emilia and open **Settings** (the gear) at the top.
2. Pick the **music folder** – Emilia scans the library in the background.
3. Browse and play via **Artists** / **Albums** / **File system**.
4. Optional: under **Search**, enable "Fetch automatically" to fill in covers,
   artist photos and (with `fpcalc` + an AcoustID key) missing tracks.
5. Equalizer: long-press a track/album/artist → **Equalizer**, or the global EQ in
   the settings.

### Streaming & recordings

- Open the **Streaming** section, tap **+** to add a stream URL or search for a
  station worldwide.
- Tap a station to play; the player bar shows a red **record** button next to
  play/pause. Set the recording buffer under **Settings → Cache & recordings**
  (0 turns it off). Recorded songs appear under **Recordings**.
- Long-press a recording for its detail page: **Play**, **Add to library**,
  **Edit** (open the waveform editor to trim it) or delete it.

### Nextcloud

- **Settings → Library → Connect to Nextcloud**: scan the login QR code (the
  camera starts immediately) or expand the manual entry, then set the music
  folder. On connect the cloud library is indexed in the background and shows up
  under Artists/Albums.

### Online metadata (optional)

- An **AcoustID key** (free, for fingerprint track recognition) and a
  **fanart.tv key** (for extra images) can be stored under **Settings → Search**.
  Without keys those phases are simply skipped.
- Covers (MusicBrainz/Cover Art Archive) and artist photos (Deezer) work without a
  key.

---

## Where data is stored

| Content                  | Path                                        |
|--------------------------|---------------------------------------------|
| Library & settings       | `~/.local/share/emilia/library.db`          |
| Cover cache              | `~/.cache/emilia/covers/`                   |
| Artist photo cache       | `~/.cache/emilia/artists/`                  |
| Remote (Nextcloud) cache | `~/.local/share/emilia/cache/<source-id>/`  |

All settings (music folder, API keys, window state …) live in the SQLite database.

---

## Building from source (for developers)

> **Not needed** for normal use – that's what the
> [Flatpak](#flatpak-recommended) is for. This section is for developers and
> anyone who wants to compile it themselves.

### Requirements

- **Rust toolchain** (edition 2021), easiest via [rustup](https://rustup.rs)
- **GTK ≥ 4.14** and **libadwaita ≥ 1.5** (incl. dev headers)
- **GStreamer 1.x** (core + dev headers) plus the plugin packages *base*, *good*,
  *bad*, *ugly* and *libav* (for `playbin3`, the equalizer and common codecs)
- **PipeWire** for audio output (present on Phosh / recent distros)
- a **C compiler** + `pkg-config` (SQLite is bundled and built from source)
- *optional:* **`fpcalc`** (Chromaprint) for fingerprint track recognition, and
  **`yt-dlp`** for the YouTube source

### 1. Install dependencies

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
  libgtk-4-dev libadwaita-1-dev \
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

### 2. Build & run

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

### 3. Install (optional)

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

### Build the Flatpak yourself

If you prefer to build a bundle yourself (instead of the pre-built one above): a
manifest is in [`de.cais.Emilia.yaml`](de.cais.Emilia.yaml) (GNOME runtime +
rust-stable SDK). Build with `flatpak-builder` – the exact commands are in the
header of the manifest.

---

## Project layout

```
src/
  main.rs            App start (Adw::Application, app ID de.cais.Emilia)
  model.rs           Data models (Track, AlbumMeta, ArtistMeta, …)
  ui/
    app.rs           Root component (init/update/view!), navigation, player
    app_views.rs     Loading/grouping, subpages, ctx/cover helpers
    app_playback.rs  Playback, queue, resume (local & remote)
    app_playlist.rs  Playlists (list, subpage, dialogs)
    app_podcast.rs   Podcasts (subscribe to feeds, stream episodes)
    app_streaming.rs Internet radio (stations, timeshift recording, replay)
    app_rec_edit.rs  Recording waveform editor (mark/cut, zoom/pan, overwrite)
    app_youtube.rs   YouTube source (search, play, add to library)
    cloud_page.rs    Nextcloud connect dialog (QR camera + manual)
    sync_page.rs     Device sync UI (QR pairing, optional webcam)
    stats_page.rs    Listening statistics component
    app_eq.rs        Equalizer editor + property dialogs
    app_dialogs.rs   Context menu, share, settings
    app_concert.rs   Concerts
    enrich.rs        Online enrichment worker (background)
    artist_row.rs    Artist card (with photo)
    album_row.rs     Album card (with cover)
    fs_row.rs        File-system row
    widgets.rs       Shared UI helpers (cover frames, thumbnails)
  core/
    scanner.rs       Directory scan + lofty metadata → DB (background worker)
    db/              SQLite (rusqlite, bundled) + queries (split into submodules)
    player.rs        GStreamer wrapper (playbin3 + equalizer-10bands)
    waveform.rs      Recording waveform decode + region cut/re-encode
    online.rs        Online enrichment (MusicBrainz/CAA/Deezer/AcoustID/fanart)
    podcast.rs       Read podcast feeds (RSS), provide episodes
    streaming.rs     Station search (Radio-Browser API)
    recorder.rs      Timeshift ring buffer + ICY reader for recording
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

---

## License

GPL-3.0-or-later. Online metadata comes from open sources (MusicBrainz/Cover Art
Archive: CC0; Deezer search API; AcoustID/Chromaprint; fanart.tv; Radio-Browser).
