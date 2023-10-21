use glib::{clone, ExitCode};
use log::{debug, info};

use gtk::{gio, glib, prelude::*, subclass::prelude::*};

use crate::config::{APP_ID, PKGDATADIR, PROFILE, VERSION};
use crate::window::AppWindow;

mod imp {

    use crate::spawn;

    use super::*;
    use adw::subclass::prelude::AdwApplicationImpl;

    #[derive(Debug)]
    pub struct App {
        pub settings: gio::Settings,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for App {
        const NAME: &'static str = "ImpressionApp";
        type Type = super::App;
        type ParentType = adw::Application;

        fn new() -> Self {
            Self {
                settings: gio::Settings::new(APP_ID),
            }
        }
    }

    impl ObjectImpl for App {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            obj.setup_gactions();
            obj.setup_accels();
            obj.setup_settings();
        }
    }

    impl ApplicationImpl for App {
        fn activate(&self) {
            debug!("Application::activate");

            self.obj().present_main_window();
        }

        fn startup(&self) {
            debug!("Application::startup");
            self.parent_startup();

            // Set icons for shell
            gtk::Window::set_default_icon_name(APP_ID);
        }

        fn open(&self, files: &[gio::File], _hint: &str) {
            if let Some(file) = files.first() {
                let application = self.obj();
                application.present_main_window();
                if let Some(window) = application.active_window() {
                    let file_path = file.path().unwrap();
                    spawn!(async move {
                        window
                            .downcast_ref::<AppWindow>()
                            .unwrap()
                            .open_file(file_path)
                            .await;
                    });
                }
            }
            debug!("Application::open");
        }
    }

    impl gtk::subclass::prelude::GtkApplicationImpl for App {}
    impl AdwApplicationImpl for App {}
}

glib::wrapper! {
    pub struct App(ObjectSubclass<imp::App>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionMap, gio::ActionGroup;
}

impl Default for App {
    fn default() -> Self {
        glib::Object::builder::<Self>()
            .property("application-id", Some(APP_ID))
            .property("flags", gio::ApplicationFlags::HANDLES_OPEN)
            .property("resource-base-path", "/io/gitlab/adhami3310/Impression/")
            .build()
    }
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }

    fn setup_settings(&self) {}

    fn setup_gactions(&self) {
        self.add_action_entries([
            gio::ActionEntry::builder("quit")
                .activate(clone!(@weak self as app => move |_,_, _| {
                    app.quit();
                }))
                .build(),
        ]);
    }

    // Sets up keyboard shortcuts
    fn setup_accels(&self) {
        self.set_accels_for_action("app.quit", &["<Control>q"]);
        self.set_accels_for_action("win.close", &["<Control>w"]);
        self.set_accels_for_action("win.open", &["<Control>o"]);
    }

    fn present_main_window(&self) {
        let window = AppWindow::new(self);
        let window: gtk::Window = window.upcast();
        window.present();
    }

    pub fn run(&self) -> ExitCode {
        info!("Impression ({})", APP_ID);
        info!("Version: {} ({})", VERSION, PROFILE);
        info!("Datadir: {}", PKGDATADIR);

        ApplicationExtManual::run(self)
    }
}
