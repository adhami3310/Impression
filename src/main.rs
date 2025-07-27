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

use self::application::App;
use self::config::{GETTEXT_PACKAGE, LOCALEDIR, RESOURCES_FILE};

#[macro_export]
macro_rules! spawn {
    ($future:expr) => {
        let ctx = glib::MainContext::default();
        ctx.spawn_local($future);
    };
}

fn get_size_string(bytes_size: u64) -> String {
    let mbs = bytes_size / 1024 / 1024;
    match mbs {
        0..=1023 => format!("{mbs}MB"),
        _ => {
            format!("{:.2}GB", (mbs as f64) / 1024.0)
        }
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Runtime::new().expect("Setting up tokio runtime needs to succeed.")
    })
}

fn main() -> ExitCode {
    // Initialize logger
    pretty_env_logger::init();

    // Prepare i18n
    gettextrs::setlocale(LocaleCategory::LcAll, "");
    gettextrs::bindtextdomain(GETTEXT_PACKAGE, LOCALEDIR).expect("Unable to bind the text domain");
    gettextrs::textdomain(GETTEXT_PACKAGE).expect("Unable to switch to the text domain");

    glib::set_application_name(&gettext("Impression"));

    let res = gio::Resource::load(RESOURCES_FILE).expect("Could not load gresource file");
    gio::resources_register(&res);

    // let _ = collect_online_distros();

    let app = App::new();
    app.run()
}
