mod application;
#[rustfmt::skip]
mod config;
mod drag_overlay;
mod flash;
mod online;
mod widgets;
mod window;

use gettextrs::{LocaleCategory, gettext};
use glib::ExitCode;
use gtk::{gio, glib};

use self::application::ImpressionApp;
use self::config::{GETTEXT_PACKAGE, LOCALEDIR, RESOURCES_FILE};

fn get_size_string(bytes_size: u64) -> String {
    let mebi_bytes = bytes_size / 1024 / 1024;
    match mebi_bytes {
        0..=1023 => format!("{mebi_bytes}MiB"),
        _ => {
            format!("{:.2}GiB", (mebi_bytes as f64) / 1024.0)
        }
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Setting up tokio runtime needs to succeed.")
    })
}

fn main() -> ExitCode {
    // Initialize logger
    tracing_subscriber::fmt::init();

    // Prepare i18n
    gettextrs::setlocale(LocaleCategory::LcAll, "");
    gettextrs::bindtextdomain(GETTEXT_PACKAGE, LOCALEDIR).expect("Unable to bind the text domain");
    gettextrs::textdomain(GETTEXT_PACKAGE).expect("Unable to switch to the text domain");

    glib::set_application_name(&gettext("Impression"));

    let res = gio::Resource::load(RESOURCES_FILE).expect("Could not load gresource file");
    gio::resources_register(&res);

    let app = ImpressionApp::default();
    app.run()
}
