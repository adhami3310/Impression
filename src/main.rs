mod application;
#[rustfmt::skip]
mod config;
mod flash;
mod online;
mod widgets;
mod window;

use gettextrs::{LocaleCategory, gettext};
use glib::ExitCode;
use gtk::{gio, glib};
use log::warn;

use self::application::ImpressionApp;
use self::config::{GETTEXT_PACKAGE, LOCALEDIR, RESOURCES_FILE};

fn get_size_string(bytes_size: u64) -> String {
    let mebi_bytes = bytes_size / 1024 / 1024;
    match mebi_bytes {
        0..=1023 => format!("{mebi_bytes}MiB"),
        #[allow(clippy::cast_precision_loss)]
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

/// # Safety
///
/// This function calls `setlocale()`, which is not thread-safe. It should be called before any threads are spawned or POSIX signals are enabled.
unsafe fn setup_i18n() -> Result<(), std::io::Error> {
    unsafe {
        gettextrs::setlocale(LocaleCategory::LcAll, "");
    }
    gettextrs::bindtextdomain(GETTEXT_PACKAGE, LOCALEDIR)?;
    gettextrs::textdomain(GETTEXT_PACKAGE)?;
    Ok(())
}

fn main() -> ExitCode {
    // Initialize logger
    tracing_subscriber::fmt::init();

    // SAFETY: still single-threaded here. The tokio runtime is built lazily in
    // runtime(), and GTK hasn't started yet.
    if let Err(err) = unsafe { setup_i18n() } {
        warn!("Failed to set up i18n: {err}");
    }

    glib::set_application_name(&gettext("Impression"));

    match gio::Resource::load(RESOURCES_FILE) {
        Ok(res) => gio::resources_register(&res),
        Err(err) => warn!("Failed to load gresource file: {err}"),
    }

    let app = ImpressionApp::default();
    app.run()
}
