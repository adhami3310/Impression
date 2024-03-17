use adw::prelude::*;
use dbus_udisks2::DiskDevice;
use glib::clone;

use crate::{get_size_string, window::AppWindow};

pub fn new(
    app: &AppWindow,
    devices: Vec<DiskDevice>,
    selected_device: Option<String>,
) -> Vec<adw::ActionRow> {
    let mut res = Vec::new();

    let mut check_buttons = Vec::new();

    for i in 0..devices.len() {
        let cb = match i {
            0 => gtk::CheckButton::builder(),
            _ => gtk::CheckButton::builder().group(check_buttons.first().unwrap()),
        };
        let cb = cb.valign(gtk::Align::Center).build();

        cb.add_css_class("selection-mode");

        if devices.len() == 1 {
            cb.connect_toggled(clone!(@weak app as this => move |x| {
                x.set_active(true);
                this.set_selected_device_index(Some(0));
            }));
        } else {
            cb.connect_toggled(clone!(@weak app as this => move |x| {
                if x.is_active() {
                    this.set_selected_device_index(Some(i));
                }
            }));
        }
        check_buttons.push(cb);
    }

    for (i, (device, cb)) in devices
        .into_iter()
        .zip(check_buttons.into_iter())
        .enumerate()
    {
        if Some(device.parent.preferred_device.to_str().unwrap().to_owned()) == selected_device
            || selected_device.is_none() && i == 0
        {
            cb.set_active(true);
            app.set_selected_device_index(Some(i));
        }

        let row = adw::ActionRow::builder()
            .title(device_label(&device))
            .subtitle(device_info(&device))
            .activatable_widget(&cb)
            .build();

        row.add_prefix(&cb);
        res.push(row);
    }
    res
}

pub fn device_label(device: &DiskDevice) -> String {
    let parent_id_label = device.parent.id_label.clone();
    let partition_id_label = device
        .partitions
        .iter()
        .filter_map(|b| b.id_label.to_owned())
        .next();
    match parent_id_label.or(partition_id_label) {
        Some(x) => format!("{x} ({} {})", device.drive.vendor, device.drive.model)
            .trim()
            .to_owned(),
        None => format!("{} {}", device.drive.vendor, device.drive.model)
            .trim()
            .to_owned(),
    }
}

fn device_info(device: &DiskDevice) -> String {
    let size = device.drive.size;

    let size_string = get_size_string(size);

    format!(
        "{size_string} ({})",
        device.parent.preferred_device.to_str().unwrap()
    )
}
