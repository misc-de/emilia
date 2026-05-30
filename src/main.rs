mod core;
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
