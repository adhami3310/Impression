use dbus::arg::{OwnedFd, RefArg, Variant};
use dbus::blocking::{Connection, Proxy};
use dbus_udisks2::{DiskDevice, Disks, UDisks2};
use gettextrs::gettext;
use itertools::Itertools;
use std::collections::HashMap;
use std::os::unix::io::FromRawFd;
use std::process::Stdio;
use std::str;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::time::Instant;

use crate::window::{Compression, DiskImage};

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
    Download,
    Copy,
}

#[derive(Clone, Debug, PartialEq)]

pub enum Progress {
    Fraction(f64),
    Pulse,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FlashStatus {
    Active(FlashPhase, Progress),
    Done(Option<String>),
}

pub struct FlashRequest {
    source: DiskImage,
    destination: DiskDevice,
    sender: async_channel::Sender<FlashStatus>,
    is_running: Arc<AtomicBool>,
}

impl FlashRequest {
    pub fn new(
        source: DiskImage,
        destination: DiskDevice,
        sender: async_channel::Sender<FlashStatus>,
        is_running: Arc<AtomicBool>,
    ) -> Self {
        Self {
            source,
            destination,
            sender,
            is_running,
        }
    }

    pub async fn perform(self) {
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

        let target_path = device.parent.path;

        let Ok(file) = udisks_open(&target_path) else {
            self.sender
                .send(FlashStatus::Done(Some(gettext("Failed to open disk"))))
                .await
                .expect("Concurrency Issues");

            return;
        };

        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let image = match source {
            DiskImage::Local {
                path,
                filename: _,
                size: _,
                compression,
            } => match compression {
                Compression::Raw => File::open(path).await,
                Compression::Xz => {
                    let temp_dir = glib::user_cache_dir();

                    std::fs::create_dir_all(&temp_dir).expect("cannot create temporary directory");

                    let result_path = temp_dir.join(
                        path.file_stem()
                            .and_then(|x| x.to_str())
                            .unwrap_or("disk_image.iso"),
                    );

                    let result_file = File::create(&result_path)
                        .await
                        .expect("cannot create uncompressed file");

                    self.sender
                        .send(FlashStatus::Active(FlashPhase::Copy, Progress::Pulse))
                        .await
                        .expect("concurrency issues");

                    match tokio::process::Command::new("xzcat")
                        .arg(path)
                        .arg("-k")
                        .arg("-T0")
                        .stdout(Stdio::from(result_file.into_std().await))
                        .status()
                        .await
                    {
                        Ok(x) if x.success() => File::open(&result_path).await,
                        _ => {
                            self.sender
                                .send(FlashStatus::Done(Some(gettext("Failed to extract drive"))))
                                .await
                                .expect("concurrency issues");
                            return;
                        }
                    }
                }
            },
            DiskImage::Online { url, name } => {
                let temp_dir = glib::user_cache_dir();

                std::fs::create_dir_all(&temp_dir).expect("cannot create temporary directory");

                let result_path = temp_dir.join(name + ".iso");

                let downloading_path = result_path.clone();

                #[derive(thiserror::Error, Debug)]
                #[error("Error while getting total size")]
                struct TotalSize {}

                let downloading_sender = self.sender.clone();

                let file = async {
                    let mut file = File::create(downloading_path.clone()).await?;

                    let res = reqwest::get(url).await?;

                    let total_size = res.content_length().ok_or(TotalSize {})?;
                    let mut downloaded: u64 = 0;
                    let mut stream = res.bytes_stream();

                    let mut last_sent = Instant::now();

                    while let Some(Ok(chunk)) = futures::StreamExt::next(&mut stream).await {
                        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
                        downloaded = std::cmp::min(downloaded + (chunk.len() as u64), total_size);

                        if last_sent.elapsed() >= Duration::from_millis(250) {
                            downloading_sender
                                .send(FlashStatus::Active(
                                    FlashPhase::Download,
                                    Progress::Fraction(downloaded as f64 / total_size as f64),
                                ))
                                .await
                                .expect("Concurrency Issues");

                            last_sent = Instant::now();
                        }
                    }

                    anyhow::Ok(downloading_path)
                }
                .await;

                match file {
                    anyhow::Result::Err(_) => {
                        self.sender
                            .send(FlashStatus::Done(Some(gettext("Failed to download image"))))
                            .await
                            .expect("Concurrency Issues");

                        return;
                    }
                    anyhow::Result::Ok(i) => Ok(File::open(i).await.expect("file where :(")),
                }
            }
        };

        FlashRequest::load_file(
            image.expect("where is file :("),
            file,
            &self.sender,
            self.is_running.clone(),
        )
        .await;

        let _ = udisks_eject(&device.drive.path);
    }

    async fn load_file(
        image: File,
        target_file: File,
        sender: &async_channel::Sender<FlashStatus>,
        is_running: Arc<AtomicBool>,
    ) {
        let mut last_sent = Instant::now();
        let mut total = 0;

        let size = image.metadata().await.unwrap().len();

        let mut source = tokio::io::BufReader::with_capacity(128 * 1024, image);
        let mut target = tokio::io::BufWriter::with_capacity(128 * 1024, target_file);

        let mut buf = [0; 64 * 1024];

        let stopped = || !is_running.load(std::sync::atomic::Ordering::SeqCst);

        while let Ok(x) = tokio::io::AsyncReadExt::read(&mut source, &mut buf).await {
            if stopped() {
                return;
            }
            if x == 0 {
                break;
            }
            total += x;
            if tokio::io::AsyncWriteExt::write_all(&mut target, &buf[..x])
                .await
                .is_err()
            {
                sender
                    .send(FlashStatus::Done(Some(gettext("Writing to disk failed"))))
                    .await
                    .expect("Concurrency failed");
                return;
            };

            if stopped() {
                return;
            }

            if last_sent.elapsed() >= Duration::from_millis(250) {
                sender
                    .send(FlashStatus::Active(
                        FlashPhase::Copy,
                        Progress::Fraction(total as f64 / size as f64),
                    ))
                    .await
                    .expect("Concurrency failed");
                last_sent = Instant::now();
            }
        }
        sender
            .send(FlashStatus::Done(None))
            .await
            .expect("Concurrency failed");
    }
}

fn udisks_eject(dbus_path: &str) -> Result<(), ()> {
    let connection = Connection::new_system().map_err(|_| ())?;

    let dbus_path = ::dbus::strings::Path::new(dbus_path).map_err(|_| ())?;

    let proxy = Proxy::new(
        "org.freedesktop.UDisks2",
        dbus_path,
        Duration::new(25, 0),
        &connection,
    );

    let options = UDisksOptions::new();
    let res: Result<(), _> =
        proxy.method_call("org.freedesktop.UDisks2.Drive", "Eject", (options,));

    if res.is_err() {
        return Err(());
    }

    Ok(())
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
