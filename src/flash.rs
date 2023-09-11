use dbus::arg::{OwnedFd, RefArg, Variant};
use dbus::blocking::{Connection, Proxy};
use dbus_udisks2::{DiskDevice, Disks, UDisks2};
use itertools::Itertools;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::str;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crate::task::Task;

type UDisksOptions = HashMap<&'static str, Variant<Box<dyn RefArg>>>;

pub fn refresh_devices() -> Result<Vec<DiskDevice>, ()> {
    let udisks = UDisks2::new().map_err(|_| ())?;
    let devices = Disks::new(&udisks).devices;
    let devices = devices
        .into_iter()
        .filter(|d| d.drive.connection_bus == "usb" || d.drive.connection_bus == "sdio")
        .filter(|d| d.parent.size != 0)
        .sorted_by_key(|d| d.drive.id.clone())
        .collect_vec();
    Ok(devices)
}

#[derive(Clone, Debug, PartialEq)]
pub enum FlashPhase {
    Copy,
    Read,
    Validate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FlashStatus {
    Active(FlashPhase, f64),
    Done(Option<String>),
}

pub struct FlashRequest {
    source: PathBuf,
    destination: DiskDevice,
    sender: glib::Sender<FlashStatus>,
    is_running: Arc<AtomicBool>,
}

impl FlashRequest {
    pub fn new(
        source: PathBuf,
        destination: DiskDevice,
        sender: glib::Sender<FlashStatus>,
        is_running: Arc<AtomicBool>,
    ) -> Self {
        Self {
            source,
            destination,
            sender,
            is_running,
        }
    }

    pub fn perform(self) {
        self.sender
            .send(FlashStatus::Active(FlashPhase::Copy, 0.0))
            .expect("Concurrency Issues");

        let source = self.source;
        let device = self.destination;

        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        // Unmount the devices beforehand.
        udisks_unmount(&device.parent.path).ok();
        for partition in &device.partitions {
            udisks_unmount(&partition.path).ok();
        }

        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let Ok(file) = udisks_open(&device.parent.path) else {
            self.sender
                .send(FlashStatus::Done(Some("Failed to open".to_string())))
                .expect("Concurrency Issues");

            return;
        };

        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let mut bucket = [0u8; 64 * 1024];

        let Ok(image) = std::fs::File::open(source) else {
            self.sender
                .send(FlashStatus::Done(Some("Failed to open image".to_string())))
                .expect("Concurrency Issues");

            return;
        };

        let mut task = Task::new(image.into(), &self.sender, self.is_running.clone(), false);
        task.subscribe(file.into());

        let Ok(_) = futures::executor::block_on(task.process(&mut bucket)) else {
            self.sender
                .send(FlashStatus::Done(Some("Failed to open image".to_string())))
                .expect("Concurrency Issues");

            return;
        };
    }
}

fn udisks_unmount(dbus_path: &str) -> Result<(), ()> {
    let connection = Connection::new_system().map_err(|_| ())?;

    let dbus_path = ::dbus::strings::Path::new(dbus_path).map_err(|_| ())?;

    let proxy = Proxy::new(
        "org.freedesktop.UDisks2",
        dbus_path,
        Duration::new(25, 0),
        &connection,
    );

    let mut options = UDisksOptions::new();
    options.insert("force", Variant(Box::new(true)));
    let res: Result<(), _> =
        proxy.method_call("org.freedesktop.UDisks2.Filesystem", "Unmount", (options,));

    if let Err(err) = res {
        if err.name() != Some("org.freedesktop.UDisks2.Error.NotMounted") {
            return Err(());
        }
    }

    Ok(())
}

fn udisks_open(dbus_path: &str) -> Result<File, ()> {
    let connection = Connection::new_system().map_err(|_| ())?;

    let dbus_path = ::dbus::strings::Path::new(dbus_path).map_err(|_| ())?;

    let proxy = Proxy::new(
        "org.freedesktop.UDisks2",
        &dbus_path,
        Duration::new(25, 0),
        &connection,
    );

    let mut options = UDisksOptions::new();
    options.insert("flags", Variant(Box::new(libc::O_SYNC)));
    let res: (OwnedFd,) = proxy
        .method_call(
            "org.freedesktop.UDisks2.Block",
            "OpenDevice",
            ("rw", options),
        )
        .map_err(|_| ())?;

    Ok(unsafe { File::from_raw_fd(res.0.into_fd()) })
}
