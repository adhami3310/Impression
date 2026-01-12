use glib::{ExitCode, clone};
use log::{debug, info};

use gtk::{gio, glib, prelude::*, subclass::prelude::*};

use crate::config::{APP_ID, PKGDATADIR, PROFILE, VERSION};
use crate::window::ImpressionAppWindow;

mod imp {

    use super::*;
    use adw::subclass::prelude::AdwApplicationImpl;

    #[derive(Debug, Default)]
    pub struct ImpressionApp {}

    #[glib::object_subclass]
    impl ObjectSubclass for ImpressionApp {
        const NAME: &'static str = "ImpressionApp";
        type Type = super::ImpressionApp;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for ImpressionApp {}

    impl ApplicationImpl for ImpressionApp {
        fn activate(&self) {
            debug!("Application::activate");

            self.obj().present_main_window();
        }

        fn startup(&self) {
            debug!("Application::startup");
            self.parent_startup();

            // Set icons for shell
            gtk::Window::set_default_icon_name(APP_ID);

            let app = self.obj();
            app.setup_gactions();
            app.setup_accels();
        }

        fn open(&self, files: &[gio::File], _hint: &str) {
            if let Some(file) = files.first() {
                let window = self.obj().present_main_window();
                window
                    .downcast_ref::<ImpressionAppWindow>()
                    .expect("Failed to downcast to ImpressionAppWindow")
                    .open_file(file);
            }
            debug!("Application::open");
        }
    }

    impl GtkApplicationImpl for ImpressionApp {}
    impl AdwApplicationImpl for ImpressionApp {}
}

glib::wrapper! {
    pub struct ImpressionApp(ObjectSubclass<imp::ImpressionApp>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionMap, gio::ActionGroup;
}

impl ImpressionApp {
    fn setup_gactions(&self) {
        self.add_action_entries([gio::ActionEntry::builder("quit")
            .activate(clone!(
                #[weak(rename_to=app)]
                self,
                move |_, _, _| {
                    app.quit();
                }
            ))
            .build()]);
    }

    // Sets up keyboard shortcuts
    fn setup_accels(&self) {
        self.set_accels_for_action("app.quit", &["<Control>q"]);
        self.set_accels_for_action("win.close", &["<Control>w"]);
        self.set_accels_for_action("win.open", &["<Control>o"]);
        self.set_accels_for_action("win.show-help-overlay", &["<Control>question"]);
    }

    fn present_main_window(&self) -> gtk::Window {
        let window = self.active_window().unwrap_or_else(|| {
            let window = ImpressionAppWindow::new(self);
            window.upcast()
        });
        window.present();
        window
    }

    pub fn run(&self) -> ExitCode {
        info!("Impression ({APP_ID})");
        info!("Version: {VERSION} ({PROFILE})");
        info!("Datadir: {PKGDATADIR}");

        ApplicationExtManual::run(self)
    }
}

impl Default for ImpressionApp {
    fn default() -> Self {
        glib::Object::builder::<Self>()
            .property("application-id", Some(APP_ID))
            .property("flags", gio::ApplicationFlags::HANDLES_OPEN)
            .property("resource-base-path", "/io/gitlab/adhami3310/Impression/")
            .build()
    }
}
