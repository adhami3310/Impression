use std::path::PathBuf;

use adw::prelude::*;
use gettextrs::gettext;
use glib::{clone, timeout_add_seconds_local};
use gtk::gdk;
use gtk::{gio, subclass::prelude::*};
use itertools::Itertools;

use crate::runtime;
use crate::{
    config::APP_ID,
    flash::{FlashPhase, FlashRequest, FlashStatus, Progress, refresh_devices},
    get_size_string,
    online::{DistroRelease, collect_online_distros, get_osinfodb_url},
    spawn,
    widgets::device_list,
};

#[derive(Debug, Clone)]
pub enum Compression {
    Raw,
    Xz,
}

#[derive(Debug, Clone)]
pub enum DiskImage {
    Local {
        path: PathBuf,
        filename: String,
        size: u64,
        compression: Compression,
    },
    Online {
        url: String,
        name: String,
    },
}

mod imp {

    use std::{cell::RefCell, sync::atomic::AtomicBool};

    use crate::{
        config::{APP_ID, PROFILE},
        drag_overlay::DragOverlay,
    };

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
        pub main_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub navigation: TemplateChild<adw::NavigationView>,
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
        #[template_child]
        pub drag_overlay: TemplateChild<DragOverlay>,

        pub selected_device_object_path: RefCell<Option<String>>,
        pub is_running: std::sync::Arc<AtomicBool>,
        pub is_flashing: std::sync::Arc<AtomicBool>,
        pub selected_image: RefCell<Option<DiskImage>>,
        pub available_devices: RefCell<Vec<udisks::Object>>,
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
            klass.bind_template();
            klass.bind_template_instance_callbacks();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for AppWindow {
        fn constructed(&self) {
            self.parent_constructed();

            if PROFILE == "Devel" {
                self.obj().add_css_class("devel");
            }

            let obj = self.obj();
            obj.load_window_size();
            obj.setup_gactions();
        }
    }

    impl WidgetImpl for AppWindow {}
    impl WindowImpl for AppWindow {
        fn close_request(&self) -> glib::Propagation {
            let obj = self.obj();

            if let Err(err) = obj.save_window_size() {
                dbg!("Failed to save window state, {}", &err);
            }

            if self.is_flashing.load(std::sync::atomic::Ordering::SeqCst) {
                obj.cancel_request(true);
                glib::Propagation::Stop
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
        @extends gtk::Widget, gtk::Window,  gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionMap, gio::ActionGroup,
                    gtk::Root, gtk::Native, gtk::ShortcutManager,
                    gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

#[gtk::template_callbacks]
impl AppWindow {
    pub fn new<P: glib::prelude::IsA<gtk::Application>>(app: &P) -> Self {
        let win = glib::Object::builder::<AppWindow>()
            .property("application", app)
            .build();

        win.setup_callbacks();
        win.setup_drop_target();
        win.imp().open_image_button.grab_focus();

        win
    }

    fn setup_gactions(&self) {
        self.add_action_entries([
            gio::ActionEntry::builder("close")
                .activate(clone!(
                    #[weak(rename_to=window)]
                    self,
                    move |_, _, _| {
                        window.close();
                    },
                ))
                .build(),
            gio::ActionEntry::builder("about")
                .activate(clone!(
                    #[weak(rename_to=window)]
                    self,
                    move |_, _, _| {
                        window.show_about();
                    }
                ))
                .build(),
            gio::ActionEntry::builder("open")
                .activate(clone!(
                    #[weak(rename_to=window)]
                    self,
                    move |_, _, _| {
                        spawn!(async move {
                            window.open_dialog().await;
                        });
                    }
                ))
                .build(),
        ]);
    }

    fn setup_drop_target(&self) {
        let drop_target = gtk::DropTarget::builder()
            .name("file-drop-target")
            .actions(gdk::DragAction::COPY)
            .formats(&gdk::ContentFormats::for_type(gdk::FileList::static_type()))
            .build();

        drop_target.connect_drop(clone!(
            #[weak(rename_to=win)]
            self,
            #[upgrade_or_default]
            move |_, value, _, _| {
                if let Ok(file_list) = value.get::<gdk::FileList>()
                    && let Some(input_file) = file_list.files().into_iter().next()
                {
                    spawn!(async move {
                        win.open_file(input_file.path().expect("Must have file path"))
                            .await;
                    });
                    return true;
                }

                false
            }
        ));

        self.imp().drag_overlay.set_drop_target(&drop_target);
    }

    fn cancel_request(&self, close_after: bool) {
        let dialog = adw::AlertDialog::new(
            Some(&gettext("Stop Writing?")),
            Some(&gettext("This might leave the drive in a faulty state")),
        );

        dialog.add_responses(&[
            ("cancel", &gettext("_Cancel")),
            ("stop", &gettext("_Stop Writing")),
        ]);
        dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);

        dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to=this)]
                self,
                move |_, id| {
                    if id == "cancel" {
                        return;
                    }

                    this.imp()
                        .is_running
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    this.imp()
                        .is_flashing
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    if close_after {
                        this.close();
                    } else {
                        this.refresh_devices();
                        this.imp().main_stack.set_visible_child_name("choose");
                    }
                }
            ),
        );

        dialog.present(Some(self));
    }

    #[template_callback]
    async fn flash_dialog(&self) {
        let selected_device = runtime()
            .block_on(async move {
                device_list::preferred_device(&self.selected_device().unwrap()).await
            })
            .unwrap_or_default();

        let flash_dialog = adw::AlertDialog::new(
            Some(&gettext("Erase Drive?")),
            Some(&gettext("You will lose all data stored on {}").replace("{}", &selected_device)),
        );

        flash_dialog.add_response("cancel", &gettext("_Cancel"));
        flash_dialog.add_response("erase", &gettext("_Erase"));
        flash_dialog.set_response_appearance("erase", adw::ResponseAppearance::Destructive);

        flash_dialog.connect_response(
            None,
            clone!(
                #[weak(rename_to=this)]
                self,
                move |_, response_id| {
                    if response_id == "erase" {
                        this.flash();
                    }
                }
            ),
        );

        flash_dialog.present(Some(self));
    }

    fn flash(&self) {
        self.imp().main_stack.set_visible_child_name("status");
        self.imp().stack.set_visible_child_name("flashing");
        self.imp().progress_bar.set_fraction(0.);
        glib::MainContext::default().iteration(true);
        self.imp()
            .is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let selected_image = self.selected_image().unwrap();

        let current_status = std::sync::Arc::<std::sync::Mutex<FlashStatus>>::new(
            std::sync::Mutex::new(FlashStatus::Active(
                match selected_image {
                    DiskImage::Online { url: _, name: _ } => FlashPhase::Download,
                    _ => FlashPhase::Copy,
                },
                Progress::Fraction(0.0),
            )),
        );

        let flash_job = FlashRequest::new(
            selected_image.clone(),
            self.selected_device().unwrap(),
            current_status.clone(),
            self.imp().is_running.clone(),
        );

        if matches!(selected_image, DiskImage::Online { url: _, name: _ }) {
            let flashing_page = &self.imp().flashing_page;
            flashing_page.set_description(Some(&gettext(
                "Writing will begin once the download is completed",
            )));
            flashing_page.set_title(&gettext("Downloading Image"));
            flashing_page.set_icon_name(Some("folder-download-symbolic"));
        } else {
            let flashing_page = &self.imp().flashing_page;
            flashing_page.set_description(Some(&gettext("Do not remove the drive")));
            flashing_page.set_title(&gettext("Writing"));
            flashing_page.set_icon_name(Some("flash-symbolic"));
            self.imp()
                .is_flashing
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        glib::timeout_add_seconds_local(
            1,
            clone!(
                #[weak(rename_to=this)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if !this
                        .imp()
                        .is_running
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        return glib::ControlFlow::Break;
                    }
                    let state = {
                        if let Ok(lock) = current_status.lock() {
                            lock.clone()
                        } else {
                            return glib::ControlFlow::Break;
                        }
                    };
                    match state {
                        FlashStatus::Active(p, x) => {
                            let flashing_page = &this.imp().flashing_page;
                            flashing_page.set_description(Some(&match p {
                                FlashPhase::Download => {
                                    gettext("Writing will begin once the download is completed")
                                }
                                FlashPhase::Copy => gettext("This could take a while"),
                            }));
                            flashing_page.set_title(&match p {
                                FlashPhase::Download => gettext("Downloading Image"),
                                _ => gettext("Writing"),
                            });
                            flashing_page.set_icon_name(Some(match p {
                                FlashPhase::Download => "folder-download-symbolic",
                                _ => "flash-symbolic",
                            }));
                            match x {
                                Progress::Fraction(x) => {
                                    this.imp().progress_bar.set_fraction(x);
                                }
                                Progress::Pulse => {
                                    this.imp().progress_bar.pulse();
                                }
                            }
                            glib::MainContext::default().iteration(true);
                        }
                        FlashStatus::Done(Some(_)) => {
                            this.imp().stack.set_visible_child_name("failure");
                            this.imp()
                                .is_running
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            this.imp()
                                .is_flashing
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            this.send_notification(gettext("Failed to write image"));
                            glib::MainContext::default().iteration(true);
                            return glib::ControlFlow::Break;
                        }
                        FlashStatus::Done(None) => {
                            this.imp().stack.set_visible_child_name("success");
                            this.imp()
                                .is_running
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            this.imp()
                                .is_flashing
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                            this.send_notification(gettext("Image Written"));
                            glib::MainContext::default().iteration(true);
                            return glib::ControlFlow::Break;
                        }
                    };
                    glib::ControlFlow::Continue
                }
            ),
        );

        runtime().spawn(flash_job.perform());
    }

    fn send_notification(&self, message: String) {
        if !self.is_active() {
            runtime().spawn(async move {
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

    fn selected_image(&self) -> Option<DiskImage> {
        self.imp().selected_image.borrow().to_owned()
    }

    fn selected_device_object_path(&self) -> Option<String> {
        self.imp().selected_device_object_path.borrow().clone()
    }

    pub fn set_selected_device_object_path(&self, selected_device_object_path: Option<String>) {
        self.imp()
            .flash_button
            .set_sensitive(selected_device_object_path.is_some());
        self.imp()
            .selected_device_object_path
            .replace(selected_device_object_path);
    }

    fn selected_device(&self) -> Option<udisks::Object> {
        let object_path = self.selected_device_object_path();
        object_path.and_then(|object_path| {
            self.imp()
                .available_devices
                .borrow()
                .clone()
                .into_iter()
                .find(|x| x.object_path().to_string() == object_path)
        })
    }

    fn setup_callbacks(&self) {
        let imp = self.imp();

        imp.open_image_button.connect_activated(clone!(
            #[weak(rename_to=this)]
            self,
            move |_| {
                spawn!(async move {
                    this.open_dialog().await;
                });
            }
        ));

        imp.cancel_button.connect_clicked(clone!(
            #[weak(rename_to=this)]
            self,
            move |_| {
                if this
                    .imp()
                    .is_flashing
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    this.cancel_request(false);
                } else {
                    this.imp()
                        .is_running
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    this.imp().main_stack.set_visible_child_name("choose");
                    this.refresh_devices();
                }
            }
        ));
        imp.done_button.connect_clicked(clone!(
            #[weak(rename_to=this)]
            self,
            move |_| {
                this.imp().available_devices.replace(vec![]);
                this.imp().main_stack.set_visible_child_name("status");
                this.imp().stack.set_visible_child_name("no_devices");
                this.imp().open_image_button.grab_focus();
            }
        ));
        imp.try_again_button.connect_clicked(clone!(
            #[weak(rename_to=this)]
            self,
            move |_| {
                this.refresh_devices();
                this.imp().main_stack.set_visible_child_name("choose");
            }
        ));
        timeout_add_seconds_local(
            2,
            clone!(
                #[weak(rename_to=this)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let main_stack = this.imp().main_stack.visible_child_name().unwrap();
                    let current_stack = this.imp().stack.visible_child_name().unwrap();
                    let current_page = this
                        .imp()
                        .navigation
                        .visible_page()
                        .and_then(|x| x.tag())
                        .map(|x| x.as_str().to_owned());
                    if main_stack == "status" && current_stack == "no_devices"
                        || main_stack == "choose"
                            && matches!(current_page, Some(x) if x == "device_list" || x == "welcome")
                    {
                        this.refresh_devices();
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        self.refresh_devices();

        imp.architecture.connect_selected_notify(clone!(
            #[weak(rename_to=this)]
            self,
            move |a| {
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
            }
        ));

        timeout_add_seconds_local(
            10,
            clone!(
                #[weak(rename_to=this)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let main_stack = this.imp().stack.visible_child_name().unwrap();
                    let current_page = this
                        .imp()
                        .navigation
                        .visible_page()
                        .and_then(|x| x.tag())
                        .map(|x| x.as_str().to_owned());
                    if main_stack == "choose"
                        && matches!(current_page, Some(x) if x == "welcome")
                        && this.imp().offline_screen.is_visible()
                    {
                        this.get_distros();
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        self.get_distros();
    }

    fn get_distros(&self) {
        let Some(downloadable_distros) = self
            .imp()
            .settings
            .value("downloadable-distros")
            .get::<Vec<(String, Option<String>, bool)>>()
        else {
            self.load_distros(&self.imp().amd_distros, vec![]);
            self.load_distros(&self.imp().arm_distros, vec![]);
            self.imp().download_spinner.set_visible(false);
            self.imp().offline_screen.set_visible(false);
            return;
        };

        let (sender, receiver) = tokio::sync::oneshot::channel();

        let distros =
            get_osinfodb_url().and_then(|u| collect_online_distros(&u, &downloadable_distros));
        runtime().spawn(async move { sender.send(distros).expect("Concurrency Issues") });

        glib::spawn_future_local(clone!(
            #[weak(rename_to=this)]
            self,
            async move {
                if let Ok(online_distros) = receiver.await {
                    if let Some((amd_distros, arm_distros)) = online_distros {
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
                }
            }
        ));
    }

    fn load_distros(&self, target: &TemplateChild<gtk::ListBox>, distros: Vec<DistroRelease>) {
        target.remove_all();
        for DistroRelease {
            name, version, url, ..
        } in distros
        {
            let action_row = adw::ActionRow::new();
            action_row.set_title(&name);
            if let Some(subtitle) = version {
                action_row.set_subtitle(&subtitle);
            }
            let next_image = gtk::Image::new();
            next_image.set_icon_name(Some("go-next-symbolic"));
            action_row.add_suffix(&next_image);
            action_row.set_activatable_widget(Some(&next_image));
            action_row.connect_activated(clone!(
                #[weak(rename_to=this)]
                self,
                move |_| {
                    let url = url.clone();
                    let name = name.clone();
                    spawn!(async move {
                        this.imp()
                            .selected_image
                            .replace(Some(DiskImage::Online { url, name }));
                        this.load_stored();
                    });
                }
            ));
            target.append(&action_row);
        }
    }

    async fn open_dialog(&self) {
        let filter = gtk::FileFilter::new();
        filter.add_mime_type("application/x-iso9660-image");
        filter.add_mime_type("application/x-raw-disk-image");
        filter.add_mime_type("application/x-cd-image");
        filter.add_pattern("*.iso");
        filter.add_pattern("*.img");
        filter.add_pattern("*.iso.xz");
        filter.add_pattern("*.img.xz");
        filter.add_pattern("*.raw.xz");
        filter.set_name(Some(&gettext("Disk Images")));

        let model = gio::ListStore::new::<gtk::FileFilter>();
        model.append(&filter);

        if let Ok(file) = gtk::FileDialog::builder()
            .modal(true)
            .filters(&model)
            .default_filter(&filter)
            .build()
            .open_future(Some(self))
            .await
        {
            let path = file.path().unwrap();
            println!("Selected Disk Image: {path:?}");

            self.open_file(path).await;
        }
    }

    pub async fn open_file(&self, path: PathBuf) {
        let filename = path.file_name().unwrap().to_str().unwrap().to_owned();

        if !["iso", "img", "xz"].contains(&path.extension().unwrap().to_str().unwrap()) {
            self.imp()
                .toast_overlay
                .add_toast(adw::Toast::new(&gettext("File is not a Disk Image")));
            println!("Not a Disk Image: {path:?}");
            return;
        }

        let size = std::fs::File::open(path.clone())
            .unwrap()
            .metadata()
            .unwrap()
            .len();

        self.imp().selected_image.replace(Some(DiskImage::Local {
            path: path.clone(),
            filename,
            size,
            compression: {
                if matches!(path.extension(), Some(x) if x == "xz") {
                    Compression::Xz
                } else {
                    Compression::Raw
                }
            },
        }));

        self.load_stored();
    }

    fn load_stored(&self) {
        match self.selected_image().unwrap() {
            DiskImage::Local {
                path: _,
                filename,
                size,
                compression: _,
            } => {
                self.imp().name_value_label.set_text(&filename);
                self.imp().size_label.set_text(&get_size_string(size));
            }
            DiskImage::Online { url: _, name } => {
                self.imp().name_value_label.set_text(&name);
                self.imp().size_label.set_text("");
            }
        }

        self.imp().navigation.push_by_tag("device_list");
    }

    fn refresh_devices(&self) {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        runtime().block_on(async move {
            let devices = refresh_devices().await;
            sender.send(devices).expect("Concurrency Issues");
        });

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                if let Ok(Ok(devices)) = receiver.await {
                    this.load_devices(devices).await;
                }
            }
        ));
    }

    async fn load_devices(&self, devices: Vec<udisks::Object>) {
        let imp = self.imp();

        let current_devices = imp.available_devices.borrow().clone();

        if devices.iter().map(|d| d.object_path()).collect_vec()
            == current_devices
                .iter()
                .map(|d| d.object_path())
                .collect_vec()
            && !devices.is_empty()
        {
            return;
        }

        imp.selected_device_object_path.take();

        let selected_device = if let Some(dev) = self.selected_device() {
            runtime().block_on(async move { device_list::preferred_device(&dev).await })
        } else {
            None
        };

        imp.available_devices_list.remove_all();
        imp.available_devices.replace(devices.clone());

        if devices.is_empty() {
            self.set_selected_device_object_path(None);
            self.imp().stack.set_visible_child_name("no_devices");
            self.imp().main_stack.set_visible_child_name("status");
        } else {
            let devices = runtime()
                .block_on(async move { device_list::new(self, devices, selected_device).await });
            for device in devices {
                imp.available_devices_list.append(&device);
            }
            self.imp().main_stack.set_visible_child_name("choose");
        }
    }

    fn show_about(&self) {
        let developers = ["Khaleel Al-Adhami"];
        let designers = ["Brage Fuglseth https://bragefuglseth.dev"];
        let artists = ["Brage Fuglseth https://bragefuglseth.dev"];

        let about = adw::AboutDialog::from_appdata(
            "/io/gitlab/adhami3310/Impression/io.gitlab.adhami3310.Impression.metainfo.xml",
            Some("3.1.0"),
        );
        about.set_developers(&developers);
        about.set_designers(&designers);
        about.set_artists(&artists);
        about.set_translator_credits(&gettext("translator-credits"));
        about.add_acknowledgement_section(
            Some(&gettext("Code borrowed from")),
            &["Popsicle https://github.com/pop-os/popsicle"],
        );

        about.add_other_app(
            "io.gitlab.adhami3310.Footage",
            // Translators: Metainfo for the app Footage. <https://gitlab.com/adhami3310/Footage>
            &gettext("Footage"),
            // Translators: Metainfo for the app Footage. <https://gitlab.com/adhami3310/Footage>
            &gettext("Polish your videos"),
        );

        about.add_other_app(
            "io.gitlab.adhami3310.Converter",
            // Translators: Metainfo for the app Switcheroo. <https://gitlab.com/adhami3310/Switcheroo>
            &gettext("Switcheroo"),
            // Translators: Metainfo for the app Switcheroo. <https://gitlab.com/adhami3310/Switcheroo>
            &gettext("Convert and manipulate images"),
        );

        about.present(Some(self));
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
