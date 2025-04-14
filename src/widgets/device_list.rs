use std::ffi::CString;

use adw::prelude::*;
use glib::clone;

use crate::window::AppWindow;

pub async fn new(
    app: &AppWindow,
    devices: Vec<udisks::Object>,
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
            cb.connect_toggled(clone!(
                #[weak(rename_to=this)]
                app,
                move |x| {
                    x.set_active(true);
                    this.set_selected_device_index(Some(0));
                }
            ));
        } else {
            cb.connect_toggled(clone!(
                #[weak(rename_to=this)]
                app,
                move |x| {
                    if x.is_active() {
                        this.set_selected_device_index(Some(i));
                    }
                }
            ));
        }
        check_buttons.push(cb);
    }

    for (i, (device, cb)) in devices
        .into_iter()
        .zip(check_buttons.into_iter())
        .enumerate()
    {
        if preferred_device(&device)
            .await
            .is_some_and(|dev| Some(dev) == selected_device)
            || selected_device.is_none() && i == 0
        {
            cb.set_active(true);
            app.set_selected_device_index(Some(i));
        }

        let info = device_info(&device).await;
        let row = adw::ActionRow::builder()
            .title(device_label(&device).await.unwrap_or_default())
            .subtitle(&info)
            .activatable_widget(&cb)
            .build();

        row.add_prefix(&cb);
        res.push(row);
    }
    res
}

pub async fn device_label(object: &udisks::Object) -> udisks::Result<String> {
    let client = udisks::Client::new().await?;
    let block = object.block().await?;
    let parent_id_label = block.id_label().await.ok();
    let mut partition_id_label = None;

    if let Ok(partition_table) = object.partition_table().await {
        for partition in client
            .partitions(&partition_table)
            .await
            .iter()
            .filter_map(|partition| client.object(partition.inner().path().clone()).ok())
        {
            let Ok(partition) = partition.partition().await else {
                continue;
            };
            partition_id_label = partition.name().await.ok();
            break;
        }
    }
    let drive = client.drive_for_block(&block).await?;
    let vendor = drive.vendor().await?;
    let model = drive.model().await?;
    Ok(match parent_id_label.or(partition_id_label) {
        Some(label) => format!("{label} ({} {})", vendor, model).trim().to_owned(),
        None => format!("{} {}", vendor, model).trim().to_owned(),
    })
}

async fn device_info(device: &udisks::Object) -> String {
    let client = udisks::Client::new().await.unwrap();
    let info = client.object_info(&device).await;
    info.one_liner.unwrap_or_default()
}

pub async fn preferred_device(object: &udisks::Object) -> Option<String> {
    let preferred_device = object.block().await.ok()?.preferred_device().await.ok()?;
    Some(
        CString::from_vec_with_nul(preferred_device)
            .ok()?
            .to_str()
            .ok()?
            .to_string(),
    )
}
