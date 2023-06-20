use std::path::PathBuf;

use adw::prelude::*;
use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
use dbus_udisks2::DiskDevice;
use gettextrs::gettext;
use glib::{clone, timeout_add_seconds_local};
use gtk::{gio, glib, subclass::prelude::*};
use itertools::Itertools;

use crate::{
    config::{APP_ID, VERSION},
    flash::{refresh_devices, FlashRequest, FlashStatus},
    get_size_string, spawn,
    widgets::device_list,
    RemoveAll,
};

mod imp {

    use std::{
        cell::{Cell, RefCell},
        sync::atomic::AtomicBool,
    };

    use crate::config::APP_ID;

    use super::*;

    use adw::subclass::prelude::AdwApplicationWindowImpl;
    use gtk::CompositeTemplate;

    #[derive(Debug, CompositeTemplate)]
    #[template(resource = "/io/gitlab/adhami3310/Impression/blueprints/window.ui")]
    pub struct AppWindow {
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub welcome_page: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub open_image_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub available_devices_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub name_value_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub size_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub flash_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub try_again_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub done_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub loading_spinner: TemplateChild<gtk::Spinner>,
        #[template_child]
        pub progress_bar: TemplateChild<gtk::ProgressBar>,
        #[template_child]
        pub cancel_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub flashing_page: TemplateChild<adw::StatusPage>,

        pub selected_device_index: Cell<Option<usize>>,
        pub is_running: std::sync::Arc<AtomicBool>,
        pub selected_image_path: RefCell<Option<PathBuf>>,
        pub available_devices: RefCell<Vec<DiskDevice>>,
        pub provider: gtk::CssProvider,
        pub settings: gio::Settings,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AppWindow {
        const NAME: &'static str = "AppWindow";
        type Type = super::AppWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }

        fn new() -> Self {
            Self {
                stack: TemplateChild::default(),
                welcome_page: TemplateChild::default(),
                open_image_button: TemplateChild::default(),
                available_devices_list: TemplateChild::default(),
                name_value_label: TemplateChild::default(),
                size_label: TemplateChild::default(),
                flash_button: TemplateChild::default(),
                done_button: TemplateChild::default(),
                try_again_button: TemplateChild::default(),
                loading_spinner: TemplateChild::default(),
                progress_bar: TemplateChild::default(),
                cancel_button: TemplateChild::default(),
                flashing_page: TemplateChild::default(),

                is_running: std::sync::Arc::new(AtomicBool::new(false)),
                selected_device_index: Cell::new(None),
                available_devices: RefCell::new(Vec::new()),
                selected_image_path: RefCell::new(None),
                provider: gtk::CssProvider::new(),
                settings: gio::Settings::new(APP_ID),
            }
        }
    }

    impl ObjectImpl for AppWindow {
        fn constructed(&self) {
            self.parent_constructed();

            if APP_ID.ends_with("Devel") {
                self.obj().add_css_class("devel");
            }

            self.welcome_page.set_icon_name(Some(APP_ID));

            let obj = self.obj();
            obj.load_window_size();
            obj.setup_gactions();
        }
    }

    impl WidgetImpl for AppWindow {}
    impl WindowImpl for AppWindow {
        fn close_request(&self) -> gtk::Inhibit {
            let obj = self.obj();

            if let Err(err) = obj.save_window_size() {
                dbg!("Failed to save window state, {}", &err);
            }

            if self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
                obj.cancel_request();
                glib::signal::Inhibit(true)
            } else {
                // Pass close request on to the parent
                self.parent_close_request()
            }
        }
    }

    impl ApplicationWindowImpl for AppWindow {}
    impl AdwApplicationWindowImpl for AppWindow {}
}

glib::wrapper! {
    pub struct AppWindow(ObjectSubclass<imp::AppWindow>)
        @extends gtk::Widget, gtk::Window,  gtk::ApplicationWindow,
        @implements gio::ActionMap, gio::ActionGroup, gtk::Root;
}

#[gtk::template_callbacks]
impl AppWindow {
    pub fn new<P: glib::IsA<gtk::Application>>(app: &P) -> Self {
        let win = glib::Object::builder::<AppWindow>()
            .property("application", app)
            .build();

        win.setup_callbacks();
        win.imp().open_image_button.grab_focus();

        win
    }

    fn setup_gactions(&self) {
        self.add_action_entries([
            gio::ActionEntry::builder("close")
                .activate(clone!(@weak self as window => move |_,_, _| {
                    window.close();
                }))
                .build(),
            gio::ActionEntry::builder("about")
                .activate(clone!(@weak self as window => move |_, _, _| {
                    window.show_about();
                }))
                .build(),
            gio::ActionEntry::builder("open")
                .activate(clone!(@weak self as window => move |_, _, _| {
                    if !window.imp().is_running.load(std::sync::atomic::Ordering::SeqCst) {
                        spawn!(async move {
                            window.open_dialog().await.ok();
                        });
                    }
                }))
                .build(),
        ]);
    }

    fn cancel_request(&self) {
        let dialog = adw::MessageDialog::new(
            Some(self),
            Some(&gettext("Stop Flashing?")),
            Some(&gettext("This might leave the drive in a faulty state.")),
        );

        dialog.add_responses(&[("cancel", &gettext("_Cancel")), ("stop", &gettext("_Stop"))]);
        dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);

        dialog.connect_response(
            None,
            clone!(@weak self as this => move |_, response_id| {
                if response_id == "stop" {
                    this.imp()
                        .is_running
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    this.imp().stack.set_visible_child_name("failure");
                }
            }),
        );

        dialog.present();
    }

    fn flash(&self) {
        self.imp().stack.set_visible_child_name("flashing");
        glib::MainContext::default().iteration(true);
        self.imp()
            .is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);
        let f = FlashRequest::new(
            self.image_path(),
            self.selected_device().unwrap(),
            tx,
            self.imp().is_running.clone(),
        );
        rx.attach(
            None,
            clone!(@weak self as this => @default-return Continue(false), move |x| {
                if !this.imp().is_running.load(std::sync::atomic::Ordering::SeqCst) {
                    this.imp().stack.set_visible_child_name("failure");
                    return Continue(false);
                }
                match x {
                    FlashStatus::Active(_, x) => {
                        // match p {
                        //     FlashPhase::Copy => this
                        //     .imp()
                        //     .flashing_page
                        //     .set_description(Some(&gettext("Copying files…"))),
                        //     _ => this
                        //     .imp()
                        //     .flashing_page
                        //     .set_description(Some(&gettext("Validating…"))),
                        // }
                        this.imp().progress_bar.set_fraction(x);
                        glib::MainContext::default().iteration(true);
                        Continue(true)
                    }
                    FlashStatus::Done(Some(_)) => {
                        this.imp().stack.set_visible_child_name("failure");
                        this.imp().is_running.store(false, std::sync::atomic::Ordering::SeqCst);
                        this.send_notification(gettext("Failed to flash image"));
                        glib::MainContext::default().iteration(true);
                        Continue(false)
                    }
                    FlashStatus::Done(None) => {
                        this.imp().stack.set_visible_child_name("success");
                        this.imp().is_running.store(false, std::sync::atomic::Ordering::SeqCst);
                        this.send_notification(gettext("Image flashed"));
                        glib::MainContext::default().iteration(true);
                        Continue(false)
                    }
                }
            }),
        );
        std::thread::spawn(move || {
            f.perform();
        });
    }

    fn send_notification(&self, message: String) {
        if !self.is_focus() {
            spawn!(async move {
                let proxy = ashpd::desktop::notification::NotificationProxy::new()
                    .await
                    .unwrap();
                proxy
                    .add_notification(
                        APP_ID,
                        ashpd::desktop::notification::Notification::new(&gettext("Impression"))
                            .body(Some(message.as_ref()))
                            .priority(Some(ashpd::desktop::notification::Priority::Normal)),
                    )
                    .await
                    .unwrap();
            });
        }
    }

    fn image_path(&self) -> PathBuf {
        let x = self.imp().selected_image_path.borrow().to_owned();
        x.unwrap()
    }

    fn selected_device_index(&self) -> Option<usize> {
        self.imp().selected_device_index.get()
    }

    pub fn set_selected_device_index(&self, new_index: Option<usize>) {
        self.imp().flash_button.set_sensitive(new_index.is_some());
        self.imp().selected_device_index.replace(new_index);
    }

    fn selected_device(&self) -> Option<DiskDevice> {
        let index = self.selected_device_index();
        index.map(|index| {
            self.imp()
                .available_devices
                .borrow()
                .clone()
                .get(index)
                .unwrap()
                .clone()
        })
    }

    fn setup_callbacks(&self) {
        let imp = self.imp();

        imp.open_image_button
            .connect_clicked(clone!(@weak self as this => move |_| {
                spawn!(async move {
                    this.open_dialog().await.ok();
                });
            }));
        imp.stack
            .connect_visible_child_notify(clone!(@weak self as win => move |stack| {
                if stack.visible_child_name().unwrap() == "device_list" {
                    win.set_selected_device_index(None);
                }
            }));
        imp.flash_button
            .connect_clicked(clone!(@weak self as this => move |_| {
                this.flash();
            }));
        imp.cancel_button
            .connect_clicked(clone!(@weak self as this => move |_| {
                this.cancel_request();
            }));
        imp.done_button
            .connect_clicked(clone!(@weak self as this => move |_| {
                this.imp().available_devices.replace(vec![]);
                this.imp().stack.set_visible_child_name("welcome");
                this.imp().open_image_button.grab_focus();
            }));
        imp.try_again_button
            .connect_clicked(clone!(@weak self as this => move |_| {
                this.imp().available_devices.replace(vec![]);
                this.imp().stack.set_visible_child_name("welcome");
                this.imp().open_image_button.grab_focus();
            }));
        timeout_add_seconds_local(
            2,
            clone!(@weak self as this => @default-return Continue(false), move || {
                let current_stack = this.imp().stack.visible_child_name().unwrap();
                if current_stack == "no_devices" || current_stack == "device_list" {
                    spawn!(async move {
                        let (sender, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);

                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_multi_thread()
                                .enable_all()
                                .build()
                                .unwrap();

                            sender
                                .send(rt.block_on(async { refresh_devices().await.unwrap() }))
                                .expect("Concurrency Issues");
                        });

                        receiver.attach(
                            None,
                            clone!(@weak this as that => @default-return Continue(false), move |new_devices| {
                                that.load_devices(new_devices, true);
                                Continue(false)
                            }),
                        );
                    });
                }
                Continue(true)
            }),
        );
    }

    async fn open_dialog(&self) -> ashpd::Result<()> {
        let files = SelectedFiles::open_file()
            .modal(true)
            .multiple(Some(false))
            .filter(
                FileFilter::new(&gettext("Disk Images")).mimetype("application/x-iso9660-image"),
            )
            .send()
            .await?
            .response()?;

        let path = files.uris().first().unwrap().to_file_path().unwrap();

        self.open_file(path).await
    }

    pub async fn open_file(&self, path: PathBuf) -> ashpd::Result<()> {
        let filename = path.file_name().unwrap().to_str().unwrap().to_owned();

        self.imp().name_value_label.set_text(&filename);

        self.imp().selected_image_path.replace(Some(path.clone()));

        self.imp().size_label.set_text(&get_size_string(
            std::fs::File::open(path).unwrap().metadata().unwrap().len(),
        ));

        self.refresh_devices().await
    }

    async fn refresh_devices(&self) -> ashpd::Result<()> {
        let (sender, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);

        self.imp().loading_spinner.start();
        self.imp().stack.set_visible_child_name("loading");
        glib::MainContext::default().iteration(true);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            sender
                .send(rt.block_on(async { refresh_devices().await.unwrap() }))
                .expect("Concurrency Issues");
        });

        receiver.attach(
            None,
            clone!(@weak self as this => @default-return Continue(false), move |e| {
                this.imp().loading_spinner.stop();
                this.load_devices(e, false);
                Continue(false)
            }),
        );

        Ok(())
    }

    fn load_devices(&self, devices: Vec<DiskDevice>, quiet: bool) {
        let imp = self.imp();

        let current_devices = imp.available_devices.borrow().clone();

        if devices
            .iter()
            .map(|d| d.parent.preferred_device.as_path().to_str().unwrap())
            .collect_vec()
            == current_devices
                .iter()
                .map(|d| d.parent.preferred_device.as_path().to_str().unwrap())
                .collect_vec()
            && !devices.is_empty()
            && quiet
        {
            return;
        }

        imp.selected_device_index.set(None);

        let selected_device = self
            .selected_device()
            .map(|x| x.parent.preferred_device.to_str().unwrap().to_owned());

        imp.available_devices_list.remove_all();
        imp.available_devices.replace(devices.clone());

        if devices.is_empty() {
            self.imp().stack.set_visible_child_name("no_devices");
        } else {
            for device in device_list::new(self, devices, selected_device) {
                imp.available_devices_list.append(&device);
            }

            self.imp().stack.set_visible_child_name("device_list");
        }
    }

    fn show_about(&self) {
        let about = adw::AboutWindow::builder()
            .transient_for(self)
            .application_icon(APP_ID)
            .application_name(gettext("Impression"))
            .developer_name("Khaleel Al-Adhami")
            .website("https://gitlab.com/adhami3310/Impression")
            .issue_url("https://gitlab.com/adhami3310/Impression/-/issues")
            .developers(vec!["Khaleel Al-Adhami"])
            .designers(vec!["Brage Fuglseth", "Saptarshi Mondal"])
            .artists(vec!["Brage Fuglseth"])
            .release_notes_version("2.1")
            .release_notes(
"<p>This minor release of Impression delivers:</p>
<ul>
  <li>Support for mobile screen sizes</li>
  <li>Various bug fixes, improving reliability and stability</li>
  <li>Brazillian Portugese translations, making Impression available in a total of 9 languages</li>
</ul>
<p>Impression is made possible by volunteer developers, designers, and translators. Thank you for your contributions!</p>"
)
            // Translators: Replace "translator-credits" with your names, one name per line
            .translator_credits(gettext("translator-credits"))
            .license_type(gtk::License::Gpl30)
            .version(VERSION)
            .build();

        about.add_acknowledgement_section(
            Some(&gettext("Code borrowed from")),
            &["Popsicle https://github.com/pop-os/popsicle"],
        );

        about.present();
    }
}

trait SettingsStore {
    fn save_window_size(&self) -> Result<(), glib::BoolError>;
    fn load_window_size(&self);
}

impl SettingsStore for AppWindow {
    fn save_window_size(&self) -> Result<(), glib::BoolError> {
        let imp = self.imp();

        let (width, height) = self.default_size();

        imp.settings.set_int("window-width", width)?;
        imp.settings.set_int("window-height", height)?;

        imp.settings
            .set_boolean("is-maximized", self.is_maximized())?;

        Ok(())
    }

    fn load_window_size(&self) {
        let imp = self.imp();

        let width = imp.settings.int("window-width");
        let height = imp.settings.int("window-height");
        let is_maximized = imp.settings.boolean("is-maximized");

        self.set_default_size(width, height);

        if is_maximized {
            self.maximize();
        }
    }
}
