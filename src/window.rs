use std::path::PathBuf;

use adw::prelude::*;
use gettextrs::gettext;
use glib::{clone, timeout_add_seconds_local};
use gtk::gdk;
use gtk::{gio, subclass::prelude::*};
use log::{error, info, warn};

use crate::config::APP_ID;
use crate::runtime;
use crate::{
    flash::{FlashPhase, FlashRequest, FlashStatus, Progress},
    get_size_string,
    online::{DistroRelease, collect_online_distros, get_osinfo_db_url},
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
        #[template_child]
        pub help_overlay: TemplateChild<adw::ShortcutsDialog>,

        pub selected_device_object_path_for_writing: RefCell<Option<String>>,
        pub selected_image_file_for_reading: RefCell<Option<DiskImage>>,
        pub available_devices: RefCell<Vec<device_list::DeviceMetadata>>,

        pub is_running: std::sync::Arc<AtomicBool>,

        #[derivative(Default(value = "gio::Settings::new(APP_ID)"))]
        pub settings: gio::Settings,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AppWindow {
        const NAME: &'static str = "AppWindow";
        type Type = super::ImpressionAppWindow;
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
                error!("Failed to save window state, {}", &err);
            }

            if obj.is_running() {
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
    pub struct ImpressionAppWindow(ObjectSubclass<imp::AppWindow>)
        @extends gtk::Widget, gtk::Window,  gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionMap, gio::ActionGroup,
                    gtk::Root, gtk::Native, gtk::ShortcutManager,
                    gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

#[gtk::template_callbacks]
impl ImpressionAppWindow {
    pub fn new<P: glib::prelude::IsA<gtk::Application>>(app: &P) -> Self {
        let win = glib::Object::builder::<ImpressionAppWindow>()
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
            gio::ActionEntry::builder("show-help-overlay")
                .activate(clone!(
                    #[weak(rename_to=window)]
                    self,
                    move |_, _, _| {
                        window.imp().help_overlay.present(Some(&window));
                    }
                ))
                .build(),
            gio::ActionEntry::builder("open")
                .activate(clone!(
                    #[weak(rename_to=window)]
                    self,
                    move |_, _, _| {
                        glib::spawn_future_local(async move {
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
                    && let Some(input_file) = file_list.files().first()
                {
                    win.open_file(input_file);
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

                    this.set_is_running(false);
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
    fn flash_dialog(&self) {
        let Some(selected_device) = self.selected_device_for_writing() else {
            warn!("No device selected");
            return;
        };

        let Some(selected_disk_image) = self.selected_image_file_for_reading() else {
            warn!("No disk image selected");
            return;
        };

        let selected_device_display_string = selected_device.display_string.unwrap_or_default();

        let flash_dialog = adw::AlertDialog::new(
            Some(&gettext("Erase Drive?")),
            Some(
                &gettext("You will lose all data stored on {}")
                    .replace("{}", &selected_device_display_string),
            ),
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
                        this.flash(&selected_device.object, &selected_disk_image);
                    }
                }
            ),
        );

        flash_dialog.present(Some(self));
    }

    fn flash(&self, device_for_writing: &udisks::Object, disk_image_for_reading: &DiskImage) {
        self.imp().main_stack.set_visible_child_name("status");
        self.imp().stack.set_visible_child_name("flashing");
        self.imp().progress_bar.set_fraction(0.);
        glib::MainContext::default().iteration(true);
        self.set_is_running(true);

        let current_status = std::sync::Arc::<std::sync::Mutex<FlashStatus>>::new(
            std::sync::Mutex::new(FlashStatus::Active(
                match disk_image_for_reading {
                    DiskImage::Online { url: _, name: _ } => FlashPhase::Download,
                    DiskImage::Local { .. } => FlashPhase::Copy,
                },
                Progress::Fraction(0.0),
            )),
        );

        let flash_job = FlashRequest::new(
            disk_image_for_reading.clone(),
            device_for_writing.clone(),
            current_status.clone(),
            self.imp().is_running.clone(),
        );

        let flashing_page = &self.imp().flashing_page;
        if matches!(
            disk_image_for_reading,
            DiskImage::Online { url: _, name: _ }
        ) {
            flashing_page.set_description(Some(&gettext(
                "Writing will begin once the download is completed",
            )));
            flashing_page.set_title(&gettext("Downloading Image"));
            flashing_page.set_icon_name(Some("folder-download-symbolic"));
        } else {
            flashing_page.set_description(Some(&gettext("Do not remove the drive")));
            flashing_page.set_title(&gettext("Writing"));
            flashing_page.set_icon_name(Some("flash-symbolic"));
        }
        glib::timeout_add_seconds_local(
            1,
            clone!(
                #[weak(rename_to=this)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if !this.is_running() {
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
                                FlashPhase::Copy => gettext("Writing"),
                            });
                            flashing_page.set_icon_name(Some(match p {
                                FlashPhase::Download => "folder-download-symbolic",
                                FlashPhase::Copy => "flash-symbolic",
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
                            this.set_is_running(false);
                            this.send_notification(gettext("Failed to write image"));
                            glib::MainContext::default().iteration(true);
                            return glib::ControlFlow::Break;
                        }
                        FlashStatus::Done(None) => {
                            this.imp().stack.set_visible_child_name("success");
                            this.set_is_running(false);
                            this.send_notification(gettext("Image Written"));
                            glib::MainContext::default().iteration(true);
                            return glib::ControlFlow::Break;
                        }
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        runtime().spawn(flash_job.perform());
    }

    fn send_notification(&self, message: String) {
        if !self.is_active() {
            runtime().spawn(async move {
                send_notification(Some(&message)).await;
            });
        }
    }

    fn selected_image_file_for_reading(&self) -> Option<DiskImage> {
        self.imp()
            .selected_image_file_for_reading
            .borrow()
            .to_owned()
    }

    fn selected_device_object_path_for_writing(&self) -> Option<String> {
        self.imp()
            .selected_device_object_path_for_writing
            .borrow()
            .clone()
    }

    pub fn set_selected_device_object_path_for_writing(
        &self,
        selected_device_object_path: Option<String>,
    ) {
        self.imp()
            .flash_button
            .set_sensitive(selected_device_object_path.is_some());
        self.imp()
            .selected_device_object_path_for_writing
            .replace(selected_device_object_path);
    }

    fn set_is_running(&self, is_running: bool) {
        self.imp()
            .is_running
            .store(is_running, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_running(&self) -> bool {
        self.imp()
            .is_running
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn selected_device_for_writing(&self) -> Option<device_list::DeviceMetadata> {
        let object_path = self.selected_device_object_path_for_writing();
        object_path.and_then(|object_path| {
            self.imp()
                .available_devices
                .borrow()
                .iter()
                .find(|x| x.object.object_path().to_string() == object_path)
                .cloned()
        })
    }

    #[template_callback]
    fn cancel_clicked(&self) {
        if self.is_running() {
            self.cancel_request(false);
        } else {
            warn!("Cancel button clicked while not running, how did we get here?");
            self.imp().main_stack.set_visible_child_name("choose");
            self.refresh_devices();
        }
    }

    #[template_callback]
    fn done_clicked(&self) {
        let window = self.imp();
        window.available_devices.replace(vec![]);
        window.main_stack.set_visible_child_name("status");
        window.stack.set_visible_child_name("no_devices");
        window.open_image_button.grab_focus();
    }

    #[template_callback]
    fn try_again_clicked(&self) {
        self.refresh_devices();
        self.imp().main_stack.set_visible_child_name("choose");
    }

    #[template_callback]
    fn on_architecture_changed(&self) {
        let imp = self.imp();

        match imp.architecture.selected() {
            0 => {
                imp.amd_distros.set_visible(true);
                imp.arm_distros.set_visible(false);
            }
            1 => {
                imp.amd_distros.set_visible(false);
                imp.arm_distros.set_visible(true);
            }
            _ => {}
        }
    }

    fn setup_callbacks(&self) {
        timeout_add_seconds_local(
            2,
            clone!(
                #[weak(rename_to=this)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let main_stack = this.imp().main_stack.visible_child_name();
                    let current_stack = this.imp().stack.visible_child_name();
                    let current_page = this
                        .imp()
                        .navigation
                        .visible_page()
                        .and_then(|x| x.tag())
                        .map(|x| x.as_str().to_owned());
                    if matches!(main_stack.as_deref(), Some("status"))
                        && matches!(current_stack.as_deref(), Some("no_devices"))
                        || matches!(main_stack.as_deref(), Some("choose"))
                            && matches!(current_page.as_deref(), Some("device_list" | "welcome"))
                    {
                        this.refresh_devices();
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        self.refresh_devices();

        timeout_add_seconds_local(
            10,
            clone!(
                #[weak(rename_to=this)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    let main_stack = this.imp().stack.visible_child_name();
                    let current_page = this
                        .imp()
                        .navigation
                        .visible_page()
                        .and_then(|x| x.tag())
                        .map(|x| x.as_str().to_owned());
                    if matches!(main_stack.as_deref(), Some("choose"))
                        && matches!(current_page.as_deref(), Some("welcome"))
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

        runtime().spawn(async move {
            if let Some(osinfo_db_url) = get_osinfo_db_url().await {
                let distros = collect_online_distros(&osinfo_db_url, &downloadable_distros).await;
                sender.send(distros).expect("Concurrency Issues");
            } else {
                sender.send(None).expect("Concurrency Issues");
            }
        });

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
                    this.imp()
                        .selected_image_file_for_reading
                        .replace(Some(DiskImage::Online { url, name }));
                    this.load_stored();
                }
            ));
            target.append(&action_row);
        }
    }

    #[template_callback]
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

        match gtk::FileDialog::builder()
            .modal(true)
            .filters(&model)
            .default_filter(&filter)
            .build()
            .open_future(Some(self))
            .await
        {
            Ok(file) => self.open_file(&file),
            Err(e) => {
                error!("Failed to open file dialog: {e}");
            }
        }
    }

    pub fn open_file(&self, file: &gio::File) {
        let Some(path) = file.path() else {
            error!("Failed to get file path for {file:?}");
            return;
        };

        info!("Selected file: {}", path.display());

        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| ["iso", "img", "xz"].contains(&extension))
        {
            self.imp()
                .toast_overlay
                .add_toast(adw::Toast::new(&gettext("File is not a Disk Image")));
            error!("Not a Disk Image: {}", path.display());
            return;
        }

        self.imp()
            .selected_image_file_for_reading
            .replace(Some(DiskImage::Local {
                path: path.clone(),
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
        match self.selected_image_file_for_reading() {
            Some(DiskImage::Local {
                path,
                compression: _,
            }) => {
                self.imp().name_value_label.set_text(
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default(),
                );
                self.imp()
                    .size_label
                    .set_text(&match std::fs::metadata(path) {
                        Ok(metadata) => get_size_string(metadata.len()),
                        Err(e) => {
                            error!("Failed to get file metadata: {e}");
                            String::new()
                        }
                    });
            }
            Some(DiskImage::Online { url: _, name }) => {
                self.imp().name_value_label.set_text(&name);
                self.imp().size_label.set_text("");
            }
            None => {
                warn!("No disk image selected");
                return;
            }
        }

        self.imp().navigation.push_by_tag("device_list");
    }

    fn refresh_devices(&self) {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        runtime().block_on(async move {
            let devices = device_list::fetch_devices_metadata().await;
            sender.send(devices).expect("Concurrency Issues");
        });

        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                if let Ok(Ok(devices)) = receiver.await {
                    this.load_devices_into_ui(&devices);
                }
            }
        ));
    }

    fn load_devices_into_ui(&self, devices: &[device_list::DeviceMetadata]) {
        let imp = self.imp();

        let current_devices = imp.available_devices.borrow().clone();

        if devices
            .iter()
            .map(|d| d.object.object_path().to_string())
            .collect::<Vec<_>>()
            == current_devices
                .iter()
                .map(|d| d.object.object_path().to_string())
                .collect::<Vec<_>>()
            && !devices.is_empty()
        {
            return;
        }

        imp.selected_device_object_path_for_writing.take();

        let selected_device = self
            .selected_device_for_writing()
            .and_then(|dev| dev.display_string);

        imp.available_devices_list.remove_all();
        imp.available_devices.replace(devices.to_vec());

        if devices.is_empty() {
            self.set_selected_device_object_path_for_writing(None);
            self.imp().stack.set_visible_child_name("no_devices");
            self.imp().main_stack.set_visible_child_name("status");
        } else {
            let devices = device_list::new(self, devices, selected_device.as_deref());
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
            Some(crate::config::VERSION),
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

impl SettingsStore for ImpressionAppWindow {
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

async fn send_notification(message: Option<&str>) {
    let proxy = match ashpd::desktop::notification::NotificationProxy::new().await {
        Ok(proxy) => proxy,
        Err(e) => {
            error!("Failed to create notification proxy: {e}");
            return;
        }
    };
    if let Err(e) = proxy
        .add_notification(
            APP_ID,
            ashpd::desktop::notification::Notification::new(&gettext("Impression"))
                .body(message)
                .priority(Some(ashpd::desktop::notification::Priority::Normal)),
        )
        .await
    {
        error!("Failed to send notification: {e}");
    }
}
