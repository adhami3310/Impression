use gettextrs::gettext;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::time::Instant;
use tokio::{fs::File, io::AsyncWriteExt};

use crate::window::{Compression, DiskImage};

pub async fn refresh_devices() -> udisks::Result<Vec<udisks::Object>> {
    let client = udisks::Client::new().await?;

    let mut drives = vec![];
    for object in client
        .object_manager()
        .get_managed_objects()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(object_path, _)| client.object(object_path).ok())
    {
        let Ok(drive): udisks::Result<udisks::drive::DriveProxy> = object.drive().await else {
            continue;
        };
        if drive
            .connection_bus()
            .await
            .is_ok_and(|bus| bus != "usb" && bus != "sdio")
        {
            continue;
        }

        if let Some(block) = client.block_for_drive(&drive, false).await {
            let object = client.object(block.inner().path().to_owned()).unwrap();
            drives.push(object);
        }
    }

    drives.sort_unstable_by_key(|x| x.object_path().to_string());

    Ok(drives)
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
    destination: udisks::Object,
    status: std::sync::Arc<std::sync::Mutex<FlashStatus>>,
    is_running: Arc<AtomicBool>,
}

impl FlashRequest {
    pub fn new(
        source: DiskImage,
        destination: udisks::Object,
        status: std::sync::Arc<std::sync::Mutex<FlashStatus>>,
        is_running: Arc<AtomicBool>,
    ) -> Self {
        Self {
            source,
            destination,
            status,
            is_running,
        }
    }

    fn set_status(&self, status: FlashStatus) {
        if let Ok(mut lock) = self.status.lock() {
            *lock = status;
        }
    }

    pub async fn perform(self) {
        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let Ok(client) = udisks::Client::new().await else {
            self.set_status(FlashStatus::Done(Some(gettext("Failed to unmount disk"))));
            return;
        };

        let destination_block = self.destination.block().await.unwrap();
        let destination_drive = client.drive_for_block(&destination_block).await.unwrap();

        // Unmount the devices beforehand.
        if let Ok(partition_table) = self.destination.partition_table().await {
            for partition in client
                .partitions(&partition_table)
                .await
                .iter()
                .filter_map(|partition| client.object(partition.inner().path().clone()).ok())
            {
                udisks_unmount(&partition).await.ok();
            }
        }

        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let Ok(file) = udisks_open(&destination_block).await else {
            self.set_status(FlashStatus::Done(Some(gettext("Failed to open disk"))));

            return;
        };

        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let image = match &self.source {
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

                    self.set_status(FlashStatus::Active(FlashPhase::Copy, Progress::Pulse));

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
                            self.set_status(FlashStatus::Done(Some(gettext(
                                "Failed to extract drive",
                            ))));
                            return;
                        }
                    }
                }
            },
            DiskImage::Online { url, name } => {
                let temp_dir = glib::user_cache_dir();

                std::fs::create_dir_all(&temp_dir).expect("cannot create temporary directory");

                let result_path = temp_dir.join(name.to_owned() + ".iso");

                let downloading_path = result_path.clone();

                #[derive(thiserror::Error, Debug)]
                #[error("Error while getting total size")]
                struct TotalSize {}

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
                            self.set_status(FlashStatus::Active(
                                FlashPhase::Download,
                                Progress::Fraction(downloaded as f64 / total_size as f64),
                            ));

                            last_sent = Instant::now();
                        }
                    }

                    anyhow::Ok(downloading_path)
                }
                .await;

                match file {
                    anyhow::Result::Err(_) => {
                        self.set_status(FlashStatus::Done(Some(gettext(
                            "Failed to download image",
                        ))));
                        return;
                    }
                    anyhow::Result::Ok(i) => Ok(File::open(i).await.expect("file where :(")),
                }
            }
        };

        //TODO: we should probably spawn a UDIsks.Job for this operation,
        //but udisks-rs does not support this yet
        FlashRequest::load_file(
            image.expect("where is file :("),
            file,
            |status| self.set_status(status),
            self.is_running.clone(),
        )
        .await;

        destination_block.rescan(HashMap::new()).await.ok();

        let _ = destination_drive.eject(HashMap::new()).await;
    }

    async fn load_file<F: Fn(FlashStatus) + Send>(
        image: File,
        mut target_file: File,
        set_status: F,
        is_running: Arc<AtomicBool>,
    ) {
        let mut last_sent = Instant::now();
        let mut total = 0;

        let size = image.metadata().await.unwrap().len();

        let mut source = tokio::io::BufReader::with_capacity(4 * 1024 * 1024, image);
        let mut target = tokio::io::BufWriter::with_capacity(4 * 1024 * 1024, &mut target_file);

        let mut buf = [0; 1024 * 1024];

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
                set_status(FlashStatus::Done(Some(gettext("Writing to disk failed"))));
                return;
            };

            if stopped() {
                return;
            }

            if last_sent.elapsed() >= Duration::from_millis(250) {
                set_status(FlashStatus::Active(
                    FlashPhase::Copy,
                    Progress::Fraction(total as f64 / size as f64),
                ));
                last_sent = Instant::now();
            }
        }

        target.flush().await.ok();

        let _ = target_file.sync_all().await;

        set_status(FlashStatus::Done(None));
    }
}

async fn udisks_unmount(object: &udisks::Object) -> udisks::Result<()> {
    let filesystem = object.filesystem().await?;
    let err = filesystem
        .unmount(HashMap::from([("force", true.into())]))
        .await;
    if err != Err(udisks::Error::NotMounted) {
        return err;
    }
    Ok(())
}

async fn udisks_open(block: &udisks::block::BlockProxy<'_>) -> udisks::Result<File> {
    let fd: std::os::fd::OwnedFd = block
        .open_device("rw", HashMap::from([("flags", libc::O_SYNC.into())]))
        .await?
        .into();
    Ok(std::fs::File::from(fd).into())
}
