mod core;
mod i18n;
mod model;
mod ui;

use relm4::{adw, gtk, RelmApp};

const APP_ID: &str = "de.cais.Emilia";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "emilia=info".into()),
        )
        .init();

    // i18n vor jedem UI-Aufbau initialisieren. Die gespeicherte Sprache hat
    // Vorrang vor der System-Locale; "system"/kein Eintrag folgt der Locale.
    let lang = core::db::Library::open()
        .ok()
        .and_then(|lib| lib.get_setting("ui_language").ok().flatten())
        .filter(|code| code == "de" || code == "en");
    i18n::init(lang.as_deref());

    // NON_UNIQUE: jede Ausführung öffnet ein eigenes Fenster (kein „Reuse" einer
    // bereits laufenden Instanz). Während der Entwicklung verlässlich; für ein
    // Release kann das wieder entfernt werden.
    let gtk_app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let app = RelmApp::from_app(gtk_app);
    app.run::<ui::app::App>(());
}
