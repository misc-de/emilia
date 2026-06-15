//! Cover/photo management dialogs: options, upload, remove, set album/artist image.
//! Split out of app_dialogs.rs – pure reordering, no functional change.

use crate::i18n::gettext;
use crate::ui::app::{App, CtxTarget, Msg};
use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

impl App {
    /// The album/artist a custom cover/photo applies to for the current detail
    /// target. Folders are resolved as an album or artist. `None` where no custom
    /// image can be set.
    pub(crate) fn cover_dest(&self) -> Option<CoverDest> {
        match self.nav.context_target.as_ref() {
            Some(CtxTarget::Album(m)) => Some(CoverDest::Album(m.artist.clone(), m.album.clone())),
            Some(CtxTarget::Artist(m)) => Some(CoverDest::Artist(m.name.clone())),
            // Folder in the file browser: resolve as an album or artist.
            _ => match self.ctx_album() {
                Some((a, al)) => Some(CoverDest::Album(a, al)),
                None => self.ctx_artist().map(CoverDest::Artist),
            },
        }
    }

    /// Asks first whether to upload a new cover/photo, remove the current one, or
    /// cancel — instead of jumping straight into the file picker.
    pub(crate) fn open_cover_options_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if self.cover_dest().is_none() {
            self.toast(&gettext("No custom image can be set here"));
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some(&gettext("Cover / photo")),
            Some(&gettext("Upload a new image or remove the current one?")),
        );
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("remove", &gettext("Remove"));
        dialog.add_response("upload", &gettext("Upload"));
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("upload", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("upload"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| match resp {
                "upload" => sender.input(Msg::UploadCover),
                "remove" => sender.input(Msg::RemoveCover),
                _ => {}
            });
        }
        dialog.present(Some(root));
    }

    /// Removes the stored cover/photo of the current target (reverts to embedded
    /// art / the online fallback), refreshing the views and the open dialog.
    pub(crate) fn remove_cover(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        match self.cover_dest() {
            Some(CoverDest::Album(artist, album)) => {
                if let Some(mut meta) = self.library.get_album_meta(&artist, &album).ok().flatten()
                {
                    meta.cover_path = None;
                    let _ = self.library.upsert_album_meta(&meta);
                }
                if let Some(CtxTarget::Album(m)) = self.nav.context_target.as_mut() {
                    if m.artist == artist && m.album == album {
                        m.cover_path = None;
                    }
                }
                self.reload_albums();
                self.refresh_context_dialog(root, sender);
            }
            Some(CoverDest::Artist(name)) => {
                if let Some(mut meta) = self.library.get_artist_meta(&name).ok().flatten() {
                    meta.image_path = None;
                    let _ = self.library.upsert_artist_meta(&meta);
                }
                if let Some(CtxTarget::Artist(m)) = self.nav.context_target.as_mut() {
                    if m.name == name {
                        m.image_path = None;
                    }
                }
                self.reload_artists();
                self.refresh_context_dialog(root, sender);
            }
            None => self.toast(&gettext("No custom image can be set here")),
        }
    }

    /// File dialog for uploading a custom cover/photo for the current detail
    /// target (album → cover, artist → photo). The chosen image is copied into
    /// the cache and set as the primary image.
    pub(crate) fn open_cover_upload_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(dest) = self.cover_dest() else {
            self.toast(&gettext("No custom image can be set here"));
            return;
        };

        let filter = gtk::FileFilter::new();
        filter.add_pixbuf_formats();
        filter.set_name(Some(&gettext("Images")));
        let chooser = gtk::FileDialog::builder()
            .title(gettext("Choose a custom image"))
            .default_filter(&filter)
            .build();

        let sender = sender.clone();
        chooser.open(Some(root), gtk::gio::Cancellable::NONE, move |res| {
            let Ok(file) = res else {
                return;
            };
            let Some(src) = file.path() else {
                return;
            };
            let is_artist = matches!(dest, CoverDest::Artist(_));
            let Some(cached) = store_custom_image(&src, is_artist) else {
                return;
            };
            match dest {
                CoverDest::Album(artist, album) => sender.input(Msg::SetAlbumCover {
                    artist,
                    album,
                    path: cached,
                }),
                CoverDest::Artist(name) => sender.input(Msg::SetArtistImage { name, path: cached }),
            }
        });
    }

    /// Set an album's cover (from the picker), refreshing the albums view and the
    /// open detail dialog on a real change.
    pub(crate) fn set_album_cover(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        artist: String,
        album: String,
        path: String,
    ) {
        let mut meta = self
            .library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::model::AlbumMeta::pending(&artist, &album));
        if meta.cover_path.as_deref() != Some(path.as_str()) {
            // Keep the previous cover as a gallery alternative so it isn't lost.
            if let Some(old) = meta.cover_path.as_deref() {
                if std::path::Path::new(old).exists() {
                    let _ = self
                        .library
                        .add_album_image(&artist, &album, old, "cover", "local");
                }
            }
            meta.cover_path = Some(path.clone());
            let _ = self.library.upsert_album_meta(&meta);
            // Mirror onto the open detail target so the rebuilt dialog (below)
            // shows the new cover; a song target reads it from the DB instead.
            if let Some(CtxTarget::Album(m)) = self.nav.context_target.as_mut() {
                if m.artist == artist && m.album == album {
                    m.cover_path = Some(path);
                }
            }
            self.reload_albums();
            self.refresh_context_dialog(root, sender);
        }
    }

    /// Set an artist's image (from the picker), refreshing the artists view and
    /// the open detail dialog on a real change.
    pub(crate) fn set_artist_image(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        name: String,
        path: String,
    ) {
        let mut meta = self
            .library
            .get_artist_meta(&name)
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::model::ArtistMeta::pending(&name));
        if meta.image_path.as_deref() != Some(path.as_str()) {
            // Keep the previous photo (e.g. from metadata) as a gallery
            // alternative so it isn't lost when the upload becomes the primary.
            if let Some(old) = meta.image_path.as_deref() {
                if std::path::Path::new(old).exists() {
                    let _ = self.library.add_artist_image(&name, old, "photo", "local");
                }
            }
            meta.image_path = Some(path.clone());
            let _ = self.library.upsert_artist_meta(&meta);
            // Mirror onto the open detail target so the rebuilt dialog shows it.
            if let Some(CtxTarget::Artist(m)) = self.nav.context_target.as_mut() {
                if m.name == name {
                    m.image_path = Some(path);
                }
            }
            self.reload_artists();
            self.refresh_context_dialog(root, sender);
        }
    }
}

/// What a custom cover/photo applies to (see [`App::cover_dest`]).
pub(crate) enum CoverDest {
    Album(String, String),
    Artist(String),
}

/// Copies a chosen image into the cover or artist cache and returns the new
/// path. The file name is unique (timestamp), so the image is loaded fresh
/// immediately and no old cache entry applies.
fn store_custom_image(src: &std::path::Path, is_artist: bool) -> Option<String> {
    let dir = if is_artist {
        crate::core::online::artist_cache_dir()
    } else {
        crate::core::online::cover_cache_dir()
    };
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 5)
        .unwrap_or("img");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let out = dir.join(format!("custom_{stamp}.{ext}"));
    std::fs::copy(src, &out).ok()?;
    Some(out.to_string_lossy().into_owned())
}
