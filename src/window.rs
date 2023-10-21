use std::path::PathBuf;

use adw::prelude::*;
use dbus_udisks2::DiskDevice;
use gettextrs::gettext;
use glib::{clone, timeout_add_seconds_local};
use gtk::{gio, glib, subclass::prelude::*};
use itertools::Itertools;

use crate::{
    config::{APP_ID, VERSION},
    flash::{refresh_devices, FlashPhase, FlashRequest, FlashStatus},
    get_size_string,
    online::collect_online_distros,
    spawn,
    widgets::device_list,
    RemoveAll,
};

#[derive(Debug, Clone)]
pub enum DiskImage {
    Local {
        path: PathBuf,
        filename: String,
        size: u64,
    },
    Online {
        url: String,
        name: String,
    },
}

mod imp {

    use std::{
        cell::{Cell, RefCell},
        sync::atomic::AtomicBool,
    };

    use crate::config::APP_ID;

    use super::*;

    use adw::subclass::prelude::AdwApplicationWindowImpl;
    use derivative::Derivative;
    use gtk::CompositeTemplate;

    #[derive(Debug, CompositeTemplate, Derivative)]
    #[derivative(Default(new = "true"))]
    #[template(resource = "/io/gitlab/adhami3310/Impression/blueprints/window.ui")]
    pub struct AppWindow {
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub app_icon: TemplateChild<gtk::Image>,
        #[template_child]
        pub open_image_button: TemplateChild<adw::ActionRow>,
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
        #[template_child]
        pub download_spinner: TemplateChild<gtk::Box>,
        #[template_child]
        pub offline_screen: TemplateChild<gtk::Box>,
        #[template_child]
        pub distros: TemplateChild<gtk::Box>,
        #[template_child]
        pub amd_distros: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub arm_distros: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub architecture: TemplateChild<gtk::DropDown>,

        pub selected_device_index: Cell<Option<usize>>,
        pub is_running: std::sync::Arc<AtomicBool>,
        pub selected_image: RefCell<Option<DiskImage>>,
        pub available_devices: RefCell<Vec<DiskDevice>>,
        pub provider: gtk::CssProvider,
        #[derivative(Default(value = "gio::Settings::new(APP_ID)"))]
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
    }

    impl ObjectImpl for AppWindow {
        fn constructed(&self) {
            self.parent_constructed();

            if APP_ID.ends_with("Devel") {
                self.obj().add_css_class("devel");
            }

            self.app_icon.set_icon_name(Some(APP_ID));

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
                            window.open_dialog().await;
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

    fn flash_dialog(&self) {
        let flash_dialog = adw::MessageDialog::new(
            Some(self),
            Some(&gettext("Erase drive?")),
            Some(&gettext!(
                "You will lose all data stored on {}",
                device_list::device_label(&self.selected_device().unwrap())
            )),
        );

        flash_dialog.add_response("cancel", &gettext("_Cancel"));
        flash_dialog.add_response("erase", &gettext("_Erase"));
        flash_dialog.set_response_appearance("erase", adw::ResponseAppearance::Destructive);

        flash_dialog.connect_response(
            None,
            clone!(@weak self as this => move |_, response_id| {
                if response_id == "erase" {
                    this.flash();
                }
            }),
        );

        flash_dialog.present();
    }

    fn flash(&self) {
        self.imp().stack.set_visible_child_name("flashing");
        self.imp().progress_bar.set_fraction(0.);
        glib::MainContext::default().iteration(true);
        self.imp()
            .is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);
        let f = FlashRequest::new(
            self.selected_image(),
            self.selected_device().unwrap(),
            tx,
            self.imp().is_running.clone(),
        );
        if matches!(self.selected_image(), DiskImage::Online { url: _, name: _ }) {
            let flashing_page = &self.imp().flashing_page;
            flashing_page.set_description(Some(&gettext(
                "Flashing will begin once the download is completed",
            )));
            flashing_page.set_title(&gettext("Downloading Image…"));
            flashing_page.set_icon_name(Some("folder-download-symbolic"));
        } else {
            let flashing_page = &self.imp().flashing_page;
            flashing_page.set_description(Some(&gettext("Do not remove the drive")));
            flashing_page.set_title(&gettext("Flashing…"));
            flashing_page.set_icon_name(Some("flash-symbolic"));
        }
        rx.attach(
            None,
            clone!(@weak self as this => @default-return Continue(false), move |x| {
                if !this.imp().is_running.load(std::sync::atomic::Ordering::SeqCst) {
                    this.imp().stack.set_visible_child_name("failure");
                    return Continue(false);
                }
                match x {
                    FlashStatus::Active(p, x) => {
                        let flashing_page = &this.imp().flashing_page;
                        flashing_page
                            .set_description(Some(&match p {
                                FlashPhase::Download => {
                                    gettext("Flashing will begin once the download is completed")
                                }
                                FlashPhase::Copy => {
                                    gettext("Copying files…")
                                }
                                _ => {
                                    gettext("Validating…")
                                }
                            }));
                        flashing_page
                            .set_title(&match p {
                                FlashPhase::Download => {
                                    gettext("Downloading Image…")
                                }
                                _ => {
                                    gettext("Flashing…")
                                }
                            });
                        flashing_page
                            .set_icon_name(Some(match p {
                                FlashPhase::Download => {
                                    "folder-download-symbolic"
                                }
                                _ => {
                                    "flash-symbolic"
                                }
                            }));
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
        if !self.is_active() {
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

    fn selected_image(&self) -> DiskImage {
        let x = self.imp().selected_image.borrow().to_owned();
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
            .connect_activated(clone!(@weak self as this => move |_| {
                spawn!(async move {
                    this.open_dialog().await;
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
                this.flash_dialog();
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
                this.imp().stack.set_visible_child_name("device_list");
            }));
        timeout_add_seconds_local(
            2,
            clone!(@weak self as this => @default-return Continue(false), move || {
                let current_stack = this.imp().stack.visible_child_name().unwrap();
                if current_stack == "no_devices" || current_stack == "device_list" {
                    if let Ok(devices) = refresh_devices() {
                        this.load_devices(devices, true);
                    }
                }
                Continue(true)
            }),
        );

        imp.architecture
            .connect_selected_notify(clone!(@weak self as this => move |a| {
                match a.selected() {
                    0 => {
                        this.imp().amd_distros.set_visible(true);
                        this.imp().arm_distros.set_visible(false);
                    }
                    1 => {
                        this.imp().amd_distros.set_visible(false);
                        this.imp().arm_distros.set_visible(true);
                    }
                    _ => {}
                }
            }));

        timeout_add_seconds_local(
            10,
            clone!(@weak self as this => @default-return Continue(false), move || {
                // For some reason this never works (dns error)
                let current_stack = this.imp().stack.visible_child_name().unwrap();
                if current_stack == "welcome" && this.imp().offline_screen.is_visible() {
                    this.get_distros();
                }
                Continue(true)
            }),
        );

        self.get_distros();
    }

    fn get_distros(&self) {
        let (tx, rx) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);

        std::thread::spawn(move || {
            let distros = collect_online_distros();
            tx.send(distros).expect("Concurrency Issues");
        });

        rx.attach(
            None,
            clone!(@weak self as this => @default-return Continue(false), move |online_distros| {
                if let Ok((amd_distros, arm_distros)) = online_distros {
                    this.load_distros(&this.imp().amd_distros, amd_distros);
                    this.load_distros(&this.imp().arm_distros, arm_distros);
                    this.imp().download_spinner.set_visible(false);
                    this.imp().offline_screen.set_visible(false);
                    this.imp().distros.set_visible(true);
                    this.imp().architecture.set_sensitive(true);
                } else {
                    this.imp().download_spinner.set_visible(false);
                    this.imp().offline_screen.set_visible(true);
                }
                Continue(false)
            }),
        );
    }

    fn load_distros(
        &self,
        target: &TemplateChild<gtk::ListBox>,
        distros: Vec<(String, Option<String>, String)>,
    ) {
        target.remove_all();
        for (name, version, url) in distros {
            let action_row = adw::ActionRow::new();
            action_row.set_title(&name);
            if let Some(subtitle) = version {
                action_row.set_subtitle(&subtitle);
            }
            let next_image = gtk::Image::new();
            next_image.set_icon_name(Some("go-next-symbolic"));
            action_row.add_suffix(&next_image);
            action_row.set_activatable_widget(Some(&next_image));
            action_row.connect_activated(clone!(@weak self as this => move |_| {
                let name = name.clone();
                let url = url.clone();
                spawn!(async move {
                    this.imp().selected_image.replace(Some(DiskImage::Online { url, name }));
                    this.load_stored();
                });
            }));
            target.append(&action_row);
        }
    }

    async fn open_dialog(&self) {
        let filter = gtk::FileFilter::new();
        filter.add_mime_type("application/x-iso9660-image");
        filter.add_mime_type("application/x-raw-disk-image");
        filter.set_name(Some(&gettext("Disk Images")));

        let model = gio::ListStore::new(gtk::FileFilter::static_type());
        model.append(&filter);

        if let Ok(file) = gtk::FileDialog::builder()
            .modal(true)
            .filters(&model)
            .build()
            .open_future(Some(self))
            .await
        {
            let path = file.path().unwrap();
            println!("Selected Disk Image: {:?}", path);

            self.open_file(path).await;
        }
    }

    pub async fn open_file(&self, path: PathBuf) {
        let filename = path.file_name().unwrap().to_str().unwrap().to_owned();

        if !["iso", "img"].contains(&path.extension().unwrap().to_str().unwrap()) {
            self.imp()
                .toast_overlay
                .add_toast(adw::Toast::new(&gettext("File is not a Disk Image")));
            println!("Not a Disk Image: {:?}", path);
            return;
        }

        let size = std::fs::File::open(path.clone())
            .unwrap()
            .metadata()
            .unwrap()
            .len();

        self.imp().selected_image.replace(Some(DiskImage::Local {
            path,
            filename,
            size,
        }));

        self.load_stored();
    }

    fn load_stored(&self) {
        match self.selected_image() {
            DiskImage::Local {
                path: _,
                filename,
                size,
            } => {
                self.imp().name_value_label.set_text(&filename);
                self.imp().size_label.set_text(&get_size_string(size));
            }
            DiskImage::Online { url: _, name } => {
                self.imp().name_value_label.set_text(&name);
                self.imp().size_label.set_text("");
            }
        }

        self.refresh_devices();
    }

    fn refresh_devices(&self) {
        if let Ok(devices) = refresh_devices() {
            self.load_devices(devices, false);
        }
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
