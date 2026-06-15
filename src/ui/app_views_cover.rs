//! Cover/photo resolution for the detail views: chosen cover/gallery paths,
//! the cover-or-carousel widget and cover texture decoding.
//! Split out of app_views.rs – pure reordering, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::{cover, scanner};
use crate::ui::app::{ActiveSource, App, CtxTarget, FsKind, Msg};
use crate::ui::fs_row::FsEntry;

impl App {
    pub(crate) fn ctx_cover(
        &self,
        target: &CtxTarget,
    ) -> (Option<gtk::gdk::Texture>, &'static str) {
        match target {
            CtxTarget::Fs(e) => {
                // First an (album) cover: cover file, embedded, or online
                // via tags. Applies to album folders and individual tracks.
                if let Some(tex) = self.cover_texture(e) {
                    (Some(tex), "media-optical-symbolic")
                } else {
                    // No cover found: next best – the artist photo.
                    // Folder → folder name, file → artist from the tags.
                    let artist = if e.is_dir() {
                        Some(e.name().to_string())
                    } else {
                        e.path()
                            .and_then(|p| scanner::read_track(p).ok())
                            .and_then(|t| t.artist)
                    };
                    let photo = artist
                        .filter(|a| !a.trim().is_empty())
                        .and_then(|a| self.library.get_artist_meta(&a).ok().flatten())
                        .and_then(|m| m.image_path)
                        .and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
                    match photo {
                        Some(tex) => (Some(tex), "avatar-default-symbolic"),
                        None => (None, "media-optical-symbolic"),
                    }
                }
            }
            CtxTarget::Artist(m) => {
                // Photo, otherwise an album cover of the artist as a substitute.
                let img = m
                    .image_path
                    .clone()
                    .or_else(|| self.artist_album_cover(&m.name));
                let tex = img.and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
                (tex, "avatar-default-symbolic")
            }
            CtxTarget::Album(m) => {
                let img = m
                    .cover_path
                    .clone()
                    .or_else(|| self.album_cover_for(&m.artist, &m.album));
                let tex = img
                    .as_deref()
                    .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
                (tex, "media-optical-symbolic")
            }
        }
    }

    /// Appends the cover/photo: with multiple images a carousel with dots,
    /// otherwise the single (primary) image as before.
    pub(crate) fn append_cover_or_gallery(
        &self,
        content: &gtk::Box,
        entry: &CtxTarget,
        sender: &ComponentSender<Self>,
        dialog: &adw::Dialog,
    ) {
        let (texture, placeholder) = self.ctx_cover(entry);
        // For a song, resolve its album once (one tag read) and reuse it for the
        // gallery candidates, the primary cover and the close-handler below.
        let fs_alb = match entry {
            CtxTarget::Fs(e) => self.fs_album(e),
            _ => None,
        };
        let mut paths = match entry {
            CtxTarget::Fs(_) => fs_alb
                .as_ref()
                .map(|(ar, al)| self.library.album_images(ar, al).unwrap_or_default())
                .unwrap_or_default()
                .into_iter()
                .filter(|p| std::path::Path::new(p).exists())
                .collect(),
            _ => self.ctx_gallery_paths(entry),
        };

        // Long press or right click on the image: choose your own cover/photo.
        let attach_upload = |w: &gtk::Box| {
            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            {
                let sender = sender.clone();
                click.connect_pressed(move |g, _, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::CoverOptions);
                });
            }
            w.add_controller(click);
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::CoverOptions);
                });
            }
            w.add_controller(lp);
        };

        // Move the current primary image to the front so the carousel starts on it
        // (so that closing without scrolling changes nothing unintentionally).
        let primary = match entry {
            CtxTarget::Album(m) => m.cover_path.clone(),
            CtxTarget::Artist(m) => m.image_path.clone(),
            // For a song: the cover of its album (the carousel then starts on it).
            CtxTarget::Fs(_) => fs_alb.as_ref().and_then(|(ar, al)| {
                self.library
                    .get_album_meta(ar, al)
                    .ok()
                    .flatten()
                    .and_then(|m| m.cover_path)
            }),
        };
        if let Some(pos) = primary.and_then(|p| paths.iter().position(|x| *x == p)) {
            let p = paths.remove(pos);
            paths.insert(0, p);
        }

        if paths.len() > 1 {
            let carousel = adw::Carousel::new();
            carousel.set_halign(gtk::Align::Center);
            for path in &paths {
                // Decode downscaled (the carousel shows ~180 px): avoids loading
                // several full-resolution covers synchronously on the UI thread.
                let tex = crate::ui::widgets::decode_scaled(path, 360)
                    .or_else(|| gtk::gdk::Texture::from_filename(path).ok());
                let img = crate::ui::widgets::rounded_image(tex.as_ref(), placeholder, 180);
                carousel.append(&img);
            }
            let dots = adw::CarouselIndicatorDots::new();
            dots.set_carousel(Some(&carousel));
            // Make the dots clickable: a tap jumps straight to that image.
            // The dots are laid out evenly, so map the click x onto an even
            // split of the indicator's width to pick the target page.
            {
                let carousel = carousel.clone();
                let dots_weak = dots.downgrade();
                let click = gtk::GestureClick::new();
                click.connect_released(move |_, _, x, _| {
                    let n = carousel.n_pages();
                    let Some(dots) = dots_weak.upgrade() else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    let w = f64::from(dots.width().max(1));
                    let idx = ((x / w) * f64::from(n))
                        .floor()
                        .clamp(0.0, f64::from(n - 1)) as u32;
                    carousel.scroll_to(&carousel.nth_page(idx), true);
                });
                dots.add_controller(click);
                dots.set_cursor_from_name(Some("pointer"));
            }

            let gallery = gtk::Box::new(gtk::Orientation::Vertical, 6);
            gallery.set_halign(gtk::Align::Center);
            gallery.append(&crate::ui::widgets::carousel_with_arrows(&carousel));
            gallery.append(&dots);
            content.append(&gallery);
            attach_upload(&gallery);

            // When closing the detail view, immediately adopt the image last shown
            // in the carousel as the primary cover/photo (applies everywhere then).
            let album_id = match entry {
                CtxTarget::Album(m) => Some((m.artist.clone(), m.album.clone())),
                // A song adopts the chosen cover for its whole album.
                CtxTarget::Fs(_) => fs_alb.clone(),
                _ => None,
            };
            let artist_id = match entry {
                CtxTarget::Artist(m) => Some(m.name.clone()),
                _ => None,
            };
            let sender = sender.clone();
            dialog.connect_closed(move |_| {
                let idx = carousel.position().round().max(0.0) as usize;
                let Some(path) = paths.get(idx).cloned() else {
                    return;
                };
                if let Some((artist, album)) = &album_id {
                    sender.input(Msg::SetAlbumCover {
                        artist: artist.clone(),
                        album: album.clone(),
                        path,
                    });
                } else if let Some(name) = &artist_id {
                    sender.input(Msg::SetArtistImage {
                        name: name.clone(),
                        path,
                    });
                }
            });
        } else {
            let cover = crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 180);
            let cover_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            cover_box.set_halign(gtk::Align::Center);
            cover_box.set_hexpand(false);
            cover_box.append(&cover);
            content.append(&cover_box);
            attach_upload(&cover_box);
        }
    }

    /// Obtains a cover as a texture. For a **folder** the folder cover
    /// (= album image); for a **single file** deliberately **no** folder image, so
    /// that a track does not inherit a foreign cover from a shared folder – instead
    /// the embedded image of the file or the online-assigned album cover.
    /// `None` if nothing suitable is found.
    pub(crate) fn cover_texture(&self, entry: &FsEntry) -> Option<gtk::gdk::Texture> {
        // Remote (Nextcloud) entries have no local path: resolve the cover via
        // the DB using the synthetic nc: path of the active source. A file uses
        // its (cached/album) cover, a folder the album cover of a track within.
        if entry.is_remote() {
            if let (Some(rel), ActiveSource::Source(id)) =
                (entry.rel_path(), &self.files.active_source)
            {
                let nc = crate::core::webdav::nc_path(*id, rel);
                let scope = if entry.is_dir() { "folder" } else { "track" };
                if let Some(p) = self.entry_cover(scope, &nc, entry.is_dir()) {
                    return gtk::gdk::Texture::from_filename(&p).ok();
                }
            }
            return None;
        }
        // Cover resolution works on the local filesystem.
        let epath = entry.path()?;
        // A single local file reuses the *list's* track-cover resolution
        // (embedded image first, otherwise the album cover – including the
        // album-name-only fallback – from cached DB metadata). This keeps the
        // detail view in sync with the favorites/file list, which previously
        // showed a cover the stricter detail lookup missed.
        if !entry.is_dir() {
            return self
                .entry_cover("track", &epath.to_string_lossy(), false)
                .and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
        }
        if entry.is_dir() {
            // A folder recognized as an album: its stored cover (custom upload
            // first, keyed by the resolved artist+album) wins over scanning the
            // folder/embedded art — so a just-set cover actually shows.
            if let Some(FsKind::Album { artist, album }) = self.fs_music_kind(entry) {
                if let Some(tex) = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .and_then(|m| m.cover_path)
                    .filter(|p| std::path::Path::new(p).exists())
                    .and_then(|p| gtk::gdk::Texture::from_filename(&p).ok())
                {
                    return Some(tex);
                }
            }
            if let Some(path) = cover::find_cover_file(epath) {
                if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                    return Some(texture);
                }
            }
        }

        let audio = if entry.is_dir() {
            std::fs::read_dir(epath)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| scanner::is_audio(p))
                .min()
        } else {
            Some(epath.clone())
        };

        if let Some(audio) = &audio {
            if let Some(bytes) = cover::embedded_cover(audio) {
                if let Ok(tex) =
                    gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(bytes.as_slice()))
                {
                    return Some(tex);
                }
            }
        }

        // Last: online-loaded cover from the cache (assigned via the tags).
        let track = scanner::read_track(audio.as_ref()?).ok()?;
        let (artist, album) = (track.artist?, track.album?);
        let meta = self
            .library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()?;
        let path = meta.cover_path?;
        gtk::gdk::Texture::from_filename(&path).ok()
    }

    /// Stored gallery image paths of a target (only existing files).
    pub(crate) fn ctx_gallery_paths(&self, entry: &CtxTarget) -> Vec<String> {
        let stored = match entry {
            // Offer the artist's current photo (e.g. from Deezer) as the first
            // candidate next to the fanart.tv gallery images, mirroring how an
            // album merges its primary cover into its gallery. Choosing one only
            // updates the artist photo and never touches the albums' covers.
            CtxTarget::Artist(m) => {
                let mut imgs: Vec<String> = m.image_path.clone().into_iter().collect();
                imgs.extend(self.library.artist_images(&m.name).unwrap_or_default());
                imgs
            }
            // Like the artist, merge the album's current (e.g. just-uploaded)
            // cover in as the first candidate next to its gallery images.
            CtxTarget::Album(m) => {
                let mut imgs: Vec<String> = m.cover_path.clone().into_iter().collect();
                imgs.extend(
                    self.library
                        .album_images(&m.artist, &m.album)
                        .unwrap_or_default(),
                );
                imgs
            }
            // A song shares its album's candidates – resolved once in
            // `append_cover_or_gallery` to avoid re-reading the file's tags.
            CtxTarget::Fs(_) => Vec::new(),
        };
        let mut seen = std::collections::HashSet::new();
        stored
            .into_iter()
            .filter(|p| std::path::Path::new(p).exists())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }
}
