use std::ffi::CString;

use adw::prelude::*;
use glib::clone;

use crate::window::ImpressionAppWindow;

async fn refresh_devices(client: &udisks::Client) -> udisks::Result<Vec<udisks::Object>> {
    let mut drives = vec![];
    for object in client
        .object_manager()
        .get_managed_objects()
        .await?
        .into_iter()
        .filter_map(|(object_path, _)| client.object(object_path).ok())
    {
        let Ok(drive): udisks::Result<udisks::drive::DriveProxy> = object.drive().await else {
            continue;
        };
        if !drive.removable().await.unwrap_or(true) {
            continue;
        }

        if let Some(block) = client.block_for_drive(&drive, false).await {
            let Ok(object) = client.object(block.inner().path().to_owned());
            drives.push(object);
        }
    }

    drives.sort_unstable_by_key(|x| x.object_path().to_string());

    Ok(drives)
}

#[derive(Debug, Clone)]
pub struct DeviceMetadata {
    pub object: udisks::Object,
    pub display_string: Option<String>,
    pub info: Option<String>,
    pub label: udisks::Result<String>,
}

async fn device_metadata(client: &udisks::Client, object: &udisks::Object) -> DeviceMetadata {
    DeviceMetadata {
        object: object.clone(),
        display_string: preferred_device_display_string(object).await,
        info: device_info(client, object).await,
        label: device_label(client, object).await,
    }
}

async fn get_devices_metadata(
    client: &udisks::Client,
    devices: &[udisks::Object],
) -> Vec<DeviceMetadata> {
    let mut res = Vec::new();
    for device in devices {
        let metadata = device_metadata(client, device).await;
        res.push(metadata);
    }
    res
}

pub async fn fetch_devices_metadata() -> udisks::Result<Vec<DeviceMetadata>> {
    let client = udisks::Client::new().await?;
    let devices = refresh_devices(&client).await?;
    Ok(get_devices_metadata(&client, &devices).await)
}

pub fn new(
    app: &ImpressionAppWindow,
    devices: &[DeviceMetadata],
    selected_device: Option<&str>,
) -> Vec<adw::ActionRow> {
    let mut check_buttons = Vec::new();

    for device in devices {
        let check_button_builder = check_buttons
            .first()
            .map_or_else(gtk::CheckButton::builder, |first_check_button| {
                gtk::CheckButton::builder().group(first_check_button)
            });
        let check_button = check_button_builder
            .valign(gtk::Align::Center)
            .css_classes(["selection_mode"])
            .build();

        let object_path = device.object.object_path().to_string();
        if devices.len() == 1 {
            check_button.connect_toggled(clone!(
                #[weak(rename_to=this)]
                app,
                move |x| {
                    x.set_active(true);
                    this.set_selected_device_object_path_for_writing(Some(object_path.clone()));
                }
            ));
        } else {
            check_button.connect_toggled(clone!(
                #[weak(rename_to=this)]
                app,
                move |x| {
                    if x.is_active() {
                        this.set_selected_device_object_path_for_writing(Some(object_path.clone()));
                    }
                }
            ));
        }
        check_buttons.push(check_button);
    }

    let mut res = Vec::new();

    for (i, (device, check_button)) in devices.iter().zip(check_buttons.into_iter()).enumerate() {
        if device.display_string.as_ref().is_some_and(|device_name| {
            selected_device.is_some_and(|selected_device_name| device_name == selected_device_name)
        }) || selected_device.is_none() && i == 0
        {
            check_button.set_active(true);
            app.set_selected_device_object_path_for_writing(Some(
                device.object.object_path().to_string(),
            ));
        }

        let row = adw::ActionRow::builder()
            .title(device.label.clone().unwrap_or_default())
            .subtitle(device.info.clone().unwrap_or_default())
            .activatable_widget(&check_button)
            .build();

        row.add_prefix(&check_button);
        res.push(row);
    }

    res
}

pub async fn device_label(
    client: &udisks::Client,
    object: &udisks::Object,
) -> udisks::Result<String> {
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
    Ok(parent_id_label.or(partition_id_label).map_or_else(
        || format!("{vendor} {model}").trim().to_owned(),
        |label| format!("{label} ({vendor} {model})").trim().to_owned(),
    ))
}

async fn device_info(client: &udisks::Client, device: &udisks::Object) -> Option<String> {
    let info = client.object_info(device).await;
    info.one_liner
}

pub async fn preferred_device_display_string(object: &udisks::Object) -> Option<String> {
    let preferred_device = object.block().await.ok()?.preferred_device().await.ok()?;
    Some(
        CString::from_vec_with_nul(preferred_device)
            .ok()?
            .to_str()
            .ok()?
            .to_string(),
    )
}
