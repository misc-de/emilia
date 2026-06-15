# Flathub-Einreichung

Diese Datei beschreibt den Weg von Emilia auf [Flathub](https://flathub.org).
Der **eigene** signierte OSTree-Repo-Release (GitHub Pages) bleibt davon
unberührt und läuft weiter über `make flatpak-build` mit `de.cais.Emilia.yaml`.

## Zwei Manifeste – warum?

| Datei | Quelle | Zweck |
|-------|--------|-------|
| `de.cais.Emilia.yaml` | `type: dir` (Arbeitsverzeichnis) | Lokale Builds + eigener GitHub-Pages-Repo-Release |
| `de.cais.Emilia.flathub.yaml` | `type: git` (fester Tag) | Flathub – baut nur aus dem gepushten, getaggten Stand |

Beide nutzen dasselbe `generated-sources.json` (Offline-Vendoring der Crates).

## Status der Flathub-Voraussetzungen

- [x] Reverse-DNS-App-ID, MetaInfo/Desktop/Icon valide, OARS, GPL-3.0-or-later
- [x] Screenshots im `<screenshots>`-Block (`data/screenshots/`, Raw-URLs auf `main`)
- [x] Offline-Build via `generated-sources.json` (verifiziert: baut ohne Netz)
- [x] **App-Quelle auf `type: git` + Tag** → `de.cais.Emilia.flathub.yaml` (`tag: v0.6.1` + `commit:` gesetzt; Tag gepusht). Nicht von Hand pflegen: `scripts/sync-flathub.sh` zieht `tag`/`commit` aus dem MetaInfo-Top-Release + Git-Tag, der pre-commit-Hook warnt bei Drift.

## Schritte für die Einreichung

1. **Alles committen**, was ins Release gehört – insbesondere:
   - `generated-sources.json`
   - `data/screenshots/desktop_artists.png`, `data/screenshots/mobile_artists.png`
   - `data/de.cais.Emilia.metainfo.xml` (mit `<screenshots>`)
   - `de.cais.Emilia.flathub.yaml`

   > Hinweis: Die Tag-Version (Schritt 3) muss zum obersten `<release>`-Eintrag
   > der MetaInfo passen – die **Marketing-Version**, die du selbst setzt (aktuell:
   > 0.6.1). Das ist NICHT die `version` in `Cargo.toml`: die zählt ein
   > pre-commit-Hook bei jedem Commit automatisch hoch (reiner Commit-Zähler) und
   > weicht daher bewusst ab.

2. **Pushen** nach `main` (sonst sind die Screenshot-Raw-URLs und der Tag für
   Flathub nicht erreichbar):

   ```sh
   git push origin main
   ```

3. **Tag setzen und pushen** (Version = oberster MetaInfo-`<release>`, hier `vX.Y.Z`):

   ```sh
   git tag vX.Y.Z <release-commit>   # Commit des MetaInfo-Top-Release
   git push origin vX.Y.Z
   ```

   Dann `tag:`/`commit:` im Manifest **nicht von Hand** eintragen, sondern:

   ```sh
   scripts/sync-flathub.sh   # liest MetaInfo-Top + Git-Tag, schreibt tag+commit
   ```

   Das Script ist die Single-Source-of-Truth-Brücke und vermeidet Tippfehler im
   SHA. Drift fällt sonst beim nächsten Commit über die Hook-Warnung auf
   (`scripts/sync-flathub.sh --check`).

4. **Lokal gegen den getaggten Stand testen** (baut jetzt wirklich aus Git):

   ```sh
   flatpak-builder --user --install --force-clean build-dir \
       de.cais.Emilia.flathub.yaml
   flatpak run de.cais.Emilia
   ```

5. **Flathub-Linter** laufen lassen (das prüft Flathub im CI):

   ```sh
   flatpak install flathub org.flatpak.Builder
   flatpak run --command=flatpak-builder-lint org.flatpak.Builder \
       manifest de.cais.Emilia.flathub.yaml
   flatpak run --command=flatpak-builder-lint org.flatpak.Builder \
       repo repo            # nach einem Build mit --repo=repo
   ```

6. **Einreichen**: Fork von `flathub/flathub`, neuer Branch `de.cais.Emilia`,
   darin `de.cais.Emilia.yaml` (= diese flathub-Variante, umbenannt) **und**
   `generated-sources.json` ablegen, dann Pull Request öffnen. Details:
   <https://docs.flathub.org/docs/for-app-authors/submission>

## Reviewer-Fragen — vorab beantwortet

Legende: ✅ begründet/unkritisch · ⚠️ Reviewer hakt nach, Antwort steht ·
❌ echter Handlungsbedarf vor Einreichung (siehe letzter Abschnitt).

### Laufzeit-Tools in `org.gnome.Platform//49` (gegen das installierte Runtime verifiziert)

Die App ruft mehrere externe Programme/GStreamer-Plugins auf. Was vorhanden ist
und was nicht:

| Extern aufgerufen | Im Runtime? | Feature | Folge bei Fehlen |
|---|---|---|---|
| `ffmpeg` | ✅ | YouTube-Offline → MP3-Transcode | — |
| `pactl` | ✅ | Audio-Ausgabegeräte auflisten | — |
| `secret-tool` (libsecret) | ✅ | Keyring statt Klartext-DB | — |
| `python3` | ✅ | führt den `yt-dlp`-Zipapp aus | — |
| `gtk4paintablesink` | ✅ | Live-Kameravorschau | Vorschau aus (Code degradiert sauber) |
| `v4l2src`/`autovideosrc` | ✅ | Kamera-Quelle (QR-Scan) | — (Halium: via `droidcam2v4l2`) |
| ~~`zxing`~~ (nicht mehr nötig) | — | QR-Dekodierung jetzt **in-process** (`rqrr`) | — |
| `fpcalc` (Chromaprint) | ✅ **gebündelt** (Modul v1.6.0) | AcoustID-Fingerprint | — |

### finish-args — Begründung je Berechtigung

- `--share=ipc`, `--socket=wayland`, `--socket=fallback-x11`, `--device=dri` —
  Standard-GUI/Rendering. ✅
- `--socket=pulseaudio` — Audiowiedergabe (PipeWire über Pulse-Kompat). ✅
- `--share=network` — Online-Metadaten (MusicBrainz/Deezer/Cover Art Archive/
  iTunes), Podcasts/Radiostreams **und** LAN-Geräte-Sync (HTTPS-Server+Client).
  Der Sync-Server lauscht nur, solange die App offen ist — **kein**
  Hintergrunddienst, **kein** Autostart. ✅ begründbar
- `--filesystem=xdg-music` — eng auf den Musikordner begrenzt; weitere Ordner
  wählt der Nutzer über den **Portal**-Dateidialog (ohne Zusatzberechtigung).
  Schreibzugriff nur für Stream-Aufnahmen + per Sync empfangene Dateien. ✅ eng
- `--own-name=org.mpris.MediaPlayer2.Emilia.*` — MPRIS (Sperrbildschirm/
  Medientasten), Suffix pro Instanz. ✅
- `--talk-name=org.freedesktop.secrets` — libsecret/Keyring für Nextcloud-Zugang
  + API-Keys; `secret-tool` ist im Runtime vorhanden, der Keyring greift also
  wirklich (keine Klartext-Secrets im Flatpak). ✅
- **Kamera (QR-Pairing) — über das Camera-Portal, KEIN `--device=all`.** Das
  Flathub-Manifest greift die Kamera ausschließlich über das **XDG-Camera-Portal**
  (PipeWire-fd via `pipewiresrc`) ab; im Manifest steht daher nur `--device=dri`
  (GPU/Rendering), kein Rohzugriff auf `/dev/video*`. QR-Dekodierung läuft
  in-process (`rqrr`), kein `zxing`-Plugin, `gtk4paintablesink` (Vorschau)
  optional. ✅ Reviewer-konform. *(Nur der Selbst-Repo-Build für Halium/FuriOS/
  Droidian nutzt zusätzlich `--device=all` + `EMILIA_CAMERA_SRC`, weil das Portal
  die V4L2-Loopbacks der Halium-Brücke dort nicht zuverlässig exponiert — das
  betrifft das Flathub-Manifest nicht.)

### App-Metadaten / Identität

- App-ID `de.cais.Emilia`, Code auf `github.com/misc-de` → der Reviewer verlangt
  einen **Nachweis der Kontrolle über `cais.de`** (Homepage-URL zeigt bereits
  dorthin; ggf. genügt ein Link von cais.de aufs Repo oder umgekehrt). ⚠️
- `metadata_license: CC0-1.0`, `project_license: GPL-3.0-or-later`, OARS-
  `content_rating`, Screenshots, `<developer>`, Release-Notes — alle vorhanden. ✅
  Der oberste `<release>`-Eintrag in der MetaInfo ist aktuell `0.6.1` und passt
  zum Git-Tag `v0.6.1` (und damit zu `tag:`/`commit:` im Flathub-Manifest).
- **In-App-Self-Update entfernt** ✅ — das frühere `src/core/update.rs` + der
  Titelleisten-Knopf „Update verfügbar" wurden ausgebaut (Flathub-Apps werden
  vom Host aktualisiert; ein eigener Updater ist ein Reviewer-Streitpunkt). Die
  Berechtigung `--talk-name=org.freedesktop.portal.Flatpak` ist aus beiden
  Manifesten entfernt, das „self-update"-Wording aus den MetaInfo-Release-Notes.

### Build

- Offline-Build (`cargo build --offline`, vendored Crates via
  `generated-sources.json`), kein Netz beim Bauen. ✅
- App-Quelle `type: git` + fester Tag/Commit. ✅ (synchron via `scripts/sync-flathub.sh`)

### `yt-dlp` — gebündelt + verifiziert ✅ (gelöst)

Früher lud die App `yt-dlp` zur Laufzeit **ungeprüft** (latest, ohne Prüfsumme)
von GitHub — genau das spricht ein Reviewer an. Jetzt:

- **Mitgeliefert + sha256-verifiziert:** `yt-dlp`-Modul im Manifest pinnt eine
  konkrete Release (`type: file` + `sha256`) und installiert sie nach
  `/app/bin/yt-dlp`. Kein ungeprüftes Nachladen mehr; der ausgelieferte Stand ist
  reproduzierbar und Teil des Builds.
- **Bleibt automatisch aktuell:** `x-checker-data` (json gegen die GitHub-
  Releases-API) lässt Flathubs `flatpak-external-data-checker` neue yt-dlp-
  Versionen erkennen und **selbständig Update-PRs** öffnen — kein manuelles
  Nachziehen des Pins bei jedem Commit.
- **Fallback für Aktualität zwischen Releases:** Der In-App-„yt-dlp
  aktualisieren"-Knopf lädt bei Bedarf eine neuere Version in den Datenordner;
  diese Kopie hat Vorrang vor der gebündelten Baseline (`src/core/youtube.rs`).
  Außerdem frischt die App nach einem Emilia-Update beim Start (online) yt-dlp auf.
- **Reststatus:** Der optionale Nutzer-Download (Fallback) ist weiterhin ein
  HTTPS-Transfer ohne GPG-Pin; das Supply-Chain-Restrisiko betrifft aber nur noch
  diesen freiwilligen Schritt, nicht den ausgelieferten Standard. Ausführung
  ohnehin nur in der Sandbox mit App-Rechten.

## Vor Einreichung zu klärende Punkte (echter Handlungsbedarf)

1. **Kamera/QR** ✅ **gelöst** — QR-Dekodierung läuft jetzt in-process (`rqrr`),
   kein `zxing`-Plugin mehr nötig; Kamera-Quelle `v4l2src`/`autovideosrc` aus der
   Runtime, auf Halium/FuriOS/Droidian über `droidcam2v4l2` (`/dev/video*`).
   `EMILIA_CAMERA_SRC` erlaubt Port-Overrides. Das Flathub-Manifest nutzt das
   **Camera-Portal** (PipeWire) und nur `--device=dri` – kein `--device=all`
   (Begründung oben), damit kein Reviewer-Streitpunkt mehr.
2. **`yt-dlp` zur Laufzeit** ✅ **gelöst** — jetzt gebündelt + sha256-verifiziert
   im Manifest, automatisch aktuell via `x-checker-data`, optionaler Nutzer-Update
   als Fallback (Vorrang vor Baseline). Details oben.
3. **`fpcalc`** ✅ **gelöst** — Chromaprint 1.6.0 wird als `cmake-ninja`-Modul
   aus der Quelle gebaut (gebündeltes KissFFT → keine externe FFT-Abhängigkeit,
   `libswresample` aus der Runtime) und installiert `fpcalc` nach `/app/bin`. Mit
   einem echten `flatpak-builder`-Lauf verifiziert (`fpcalc 1.6.0` baut + läuft).
   **1.6.0, nicht 1.5.1:** Letzteres nutzt die alte FFmpeg-Channel-Layout-API
   (`AVCodecContext.channels`) und kompiliert nicht mehr gegen das Runtime-FFmpeg.

## generated-sources.json neu erzeugen (nach Cargo.lock-Änderung)

```sh
.flathub-tools/venv/bin/python .flathub-tools/flatpak-cargo-generator.py \
    Cargo.lock -o generated-sources.json
```
