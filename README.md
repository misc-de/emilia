# Emilia

**Adaptiver Musik- & Hörspielplayer für Linux-Phosh-Smartphones** (Librem 5,
PinePhone & Co.) – läuft genauso am Desktop. Geschrieben in **Rust** mit
**relm4 + GTK4/libadwaita**, Audio über **GStreamer** (`playbin3`),
Bibliotheksindex in **SQLite**.

> App-ID: `de.cais.Emilia` · Lizenz: GPL-3.0-or-later · Status: in Entwicklung (0.1.0)

---

## Was Emilia kann

- **Adaptive Oberfläche** – funktioniert im schmalen Hochformat (Phone) und am
  Desktop; die Navigation klappt mobil automatisch ein.
- **Musikordner einlesen** – rekursiver Scan im Hintergrund, Tags & Cover über
  `lofty`. Die Audiodateien werden dabei ausschließlich **gelesen**, niemals
  verändert.
- **Mehrere Ansichten** auf die Bibliothek:
  - **Dateisystem** – verlässliche Navigation, auch bei lückenhaften Tags
    (wichtig für Hörspiele).
  - **Interpreten** – einfacher Klick öffnet eine Unterseite mit den **Alben**
    des Interpreten (mit Cover) und darunter den **Einzelliedern** (Gast-/
    Feature-Titel & Titel ohne Album). Ein Album öffnet seine Titelliste.
  - **Alben** – alle Alben mit Cover.
  - **Konzerte** – Live-/Unplugged-Aufnahmen markieren und sammeln; ein Import
    schlägt passende Kandidaten vor.
- **Wiedergabe** – Play/Pause, Vor/Zurück, Zufallswiedergabe, Warteschlange und
  ein Mini-Player am unteren Rand mit **Seekleiste** (Positionsregler zum Spulen
  in langen Titeln).
- **Resume für Hörspiele** – lange Titel (ab 15 min oder als Hörbuch/Podcast
  eingestuft) merken sich die Hörposition und laufen beim nächsten Mal dort
  weiter; bei Titelende wird wieder von vorn begonnen.
- **Sperrbildschirm & Medientasten** – Steuerung über **MPRIS**
  (Play/Pause, Vor/Zurück, Spulen) samt Titel-/Albumanzeige.
- **Playlisten** – eigene Playlisten anlegen, Titel/Alben/Ordner über die
  Optionen hinzufügen, abspielen, umbenennen und einzelne Titel wieder
  entfernen.
- **Podcasts** – Feeds per RSS-Adresse abonnieren; Episoden werden direkt
  **gestreamt** (kein Download). Feed aktualisieren, Podcast entfernen.
- **Equalizer mit Kaskade** – 10-Band-EQ (`equalizer-10bands`), live während der
  Wiedergabe. Einstellungen wirken in der Reihenfolge
  **Global → Interpret → Album → Titel** (spezifischste Ebene gewinnt),
  zusätzlich pro **Ausgabegerät/Kopfhörer** (PipeWire-Sink).
- **Online-Metadaten abrufen** (optional, auf Knopfdruck) – aus offenen Quellen:
  - Album-Cover über **MusicBrainz** + **Cover Art Archive**
  - Interpretenfotos über **Deezer** (kein Schlüssel nötig)
  - Titel-Erkennung per **AcoustID/Chromaprint** (benötigt `fpcalc` + kostenlosen
    AcoustID-Key) für Dateien mit lückenhaften Tags
  - zusätzliche Bildergalerien über **fanart.tv** (optionaler Key)

  Alles landet nur in der lokalen Datenbank bzw. im XDG-Cache – nie in den
  Audiodateien.

### Geplant (Roadmap)

- Streaming-Backend (Subsonic/Navidrome oder Jellyfin)

---

## Installation

### Flatpak (empfohlen)

Vorgebautes, **GPG-signiertes** Bundle für **x86_64 und aarch64** – ideal fürs
Phone, ohne Build-Werkzeuge. Aus dem Projekt-Repo (GitHub Pages):

```bash
flatpak remote-add --if-not-exists emilia https://misc-de.github.io/emilia/de.cais.Emilia.flatpakrepo
flatpak install emilia de.cais.Emilia
flatpak run de.cais.Emilia
```

Später aktualisieren mit `flatpak update de.cais.Emilia`. Der Signaturschlüssel
steckt bereits in der `.flatpakrepo`-Datei – es muss nichts separat importiert
werden.

> Du möchtest lieber selbst kompilieren? Siehe
> [Aus dem Quellcode bauen](#aus-dem-quellcode-bauen-für-entwickler) ganz unten.

---

## Erste Schritte

1. Emilia starten und oben links auf **Einstellungen** (Zahnrad) gehen.
2. Den **Musikordner** auswählen – Emilia liest die Bibliothek im Hintergrund ein.
3. Über **Interpreten** / **Alben** / **Dateisystem** stöbern und abspielen.
4. Optional: oben auf **„Cover & Metadaten online abrufen"** klicken, um Cover,
   Interpretenfotos und (mit `fpcalc` + AcoustID-Key) fehlende Titel zu ergänzen.
5. Equalizer: Titel/Album/Interpret per langem Druck öffnen → **Equalizer**, oder
   den globalen EQ in den Einstellungen.

### Online-Metadaten (optional)

- **AcoustID-Key** (kostenlos, für die Fingerprint-Titelerkennung) und
  **fanart.tv-Key** (für zusätzliche Bilder) lassen sich in den **Einstellungen**
  hinterlegen. Ohne Keys werden diese Phasen einfach übersprungen.
- Cover (MusicBrainz/Cover Art Archive) und Interpretenfotos (Deezer)
  funktionieren ohne Schlüssel.

---

## Datenspeicherorte

| Inhalt                     | Pfad                                         |
|----------------------------|----------------------------------------------|
| Bibliothek & Einstellungen | `~/.local/share/emilia/library.db`           |
| Cover-Cache                | `~/.cache/emilia/covers/`                    |
| Interpretenfotos-Cache     | `~/.cache/emilia/artists/`                   |

Alle Einstellungen (Musikordner, API-Keys, Fensterzustand …) liegen in der
SQLite-Datenbank.

---

## Aus dem Quellcode bauen (für Entwickler)

> Für die normale Nutzung **nicht nötig** – dafür gibt es das
> [Flatpak](#flatpak-empfohlen). Dieser Abschnitt richtet sich an Entwickler und
> alle, die selbst kompilieren möchten.

### Voraussetzungen

- **Rust-Toolchain** (Edition 2021), am einfachsten über [rustup](https://rustup.rs)
- **GTK ≥ 4.14** und **libadwaita ≥ 1.5** (inkl. Dev-Header)
- **GStreamer 1.x** (Core + Dev-Header) sowie die Plugin-Pakete
  *base*, *good*, *bad*, *ugly* und *libav* (für `playbin3`, den Equalizer und
  gängige Codecs)
- **PipeWire** als Audio-Ausgabe (auf Phosh/aktuellen Distros vorhanden)
- ein **C-Compiler** + `pkg-config` (SQLite wird gebündelt aus dem Quellcode gebaut)
- *optional:* **`fpcalc`** (Chromaprint) für die Titel-Erkennung per Fingerprint

### 1. Abhängigkeiten installieren

**Arch / Manjaro**

```bash
sudo pacman -S --needed rustup base-devel pkgconf \
  gtk4 libadwaita \
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav \
  chromaprint        # optional, liefert fpcalc
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
  libchromaprint-tools   # optional, liefert fpcalc
# Rust über rustup, falls noch nicht vorhanden:
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**Fedora**

```bash
sudo dnf install cargo rust gcc pkgconf-pkg-config \
  gtk4-devel libadwaita-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel \
  gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-libav \
  chromaprint-tools     # optional, liefert fpcalc
# gstreamer1-plugins-ugly liegt in RPM Fusion
```

### 2. Bauen & starten

```bash
git clone <repo-url> Emilia
cd Emilia

# Während der Entwicklung:
cargo run

# Optimiertes Release-Binary:
cargo build --release
./target/release/emilia
```

> Hinweis: Beim Start aus dem Projektordner (`cargo run`) werden die Icons aus
> `data/icons` gefunden. Für den dauerhaften Betrieb lieber installieren (unten).

### 3. Installieren (optional)

Damit Emilia im App-Raster erscheint und ihr Icon am Sperrbildschirm zeigt,
installiert das `Makefile` Binary, `.desktop`-Datei, App-Icon und
AppStream-Metainfo an die richtigen XDG-Orte:

```bash
# systemweit (braucht Root):
sudo make install

# oder nur für den eigenen Benutzer (gut fürs Phone, ohne Root):
make install PREFIX=$HOME/.local
```

Wieder entfernen mit `make uninstall` (gleicher `PREFIX`). `make check` prüft
`.desktop` und Metainfo mit `desktop-file-validate` bzw. `appstreamcli`.

### Flatpak selbst bauen

Wer lieber selbst ein Bundle baut (statt das vorgebaute oben zu nutzen): Ein
Manifest liegt unter [`de.cais.Emilia.yaml`](de.cais.Emilia.yaml) (GNOME-Runtime
+ rust-stable-SDK). Bauen mit `flatpak-builder` – die genauen Befehle stehen im
Kopf des Manifests.

---

## Projektstruktur

```
src/
  main.rs            App-Start (Adw::Application, App-ID de.cais.Emilia)
  model.rs           Datenmodelle (Track, AlbumMeta, ArtistMeta, …)
  ui/
    app.rs           Wurzel-Komponente (init/update/view!), Navigation, Player
    app_views.rs     Laden/Gruppieren, Unterseiten, ctx-/Cover-Helfer
    app_playback.rs  Wiedergabe, Warteschlange, Resume
    app_playlist.rs  Playlisten (Liste, Unterseite, Dialoge)
    app_podcast.rs   Podcasts (Feeds abonnieren, Episoden streamen)
    app_eq.rs        Equalizer-Editor + Merkmal-Dialoge
    app_dialogs.rs   Kontextmenü, Teilen, Einstellungen
    app_concert.rs   Konzerte
    enrich.rs        Online-Anreicherungs-Worker (Hintergrund)
    artist_row.rs    Interpreten-Karte (mit Foto)
    album_row.rs     Album-Karte (mit Cover)
    track_row.rs     Titel-Zeile
    fs_row.rs        Dateisystem-Zeile
    widgets.rs       gemeinsame UI-Helfer (Cover-Rahmen, Thumbnails)
  core/
    scanner.rs       Verzeichnis-Scan + lofty-Metadaten → DB (Hintergrund-Worker)
    db.rs            SQLite (rusqlite, gebündelt) + Abfragen
    player.rs        GStreamer-Wrapper (playbin3 + equalizer-10bands)
    online.rs        Online-Anreicherung (MusicBrainz/CAA/Deezer/AcoustID/fanart)
    podcast.rs       Podcast-Feeds (RSS) einlesen, Episoden bereitstellen
    fingerprint.rs   Chromaprint (fpcalc) für die Titel-Erkennung
    cover.rs         eingebettete & Ordner-Cover
    category.rs      EQ-/Merkmals-Schlüssel und Kaskaden-Auflösung
    output.rs        Audio-Ausgänge (PipeWire-Sinks) für EQ-Profile
    concert.rs       Konzert-Kandidaten erkennen
    artist.rs        „feat."-Aufteilung von Interpreten-Angaben
```

---

## Lizenz

GPL-3.0-or-later. Online-Metadaten stammen aus offenen Quellen
(MusicBrainz/Cover Art Archive: CC0; Deezer-Such-API; AcoustID/Chromaprint;
fanart.tv).
