use log::{error, info};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use terrors::OneOf;
use tokio::time::Instant;
use tokio::{fs::File, io::AsyncWriteExt};

mod udf;
mod wim;
mod windows_installer;

use crate::window::{Compression, DiskImage};
use windows_installer::WindowsInstallerFailed;

#[derive(Clone, Debug)]
pub enum FlashPhase {
    /// Downloading an online source image.
    Download,
    /// Decompressing an `.xz` source, or extracting a Windows ISO's contents.
    Extract,
    /// Splitting an oversized Windows install image into FAT32-sized parts.
    ProcessImage,
    /// Partitioning and formatting the destination (Windows installer path).
    Partition,
    /// Building the Windows installer's FAT32 filesystem into a local image.
    BuildImage,
    /// Writing bytes to the destination device.
    Copy,
}

#[derive(Clone, Debug)]

pub enum Progress {
    Fraction(f64),
    Pulse,
}

impl From<(u64, u64)> for Progress {
    fn from(value: (u64, u64)) -> Self {
        let (nominator, denominator) = value;
        if denominator == 0 {
            Self::Pulse
        } else {
            Self::Fraction(nominator as f64 / denominator as f64)
        }
    }
}

#[derive(Clone, Debug)]
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

#[derive(thiserror::Error, Debug)]
#[error("Process was stopped by the user")]
struct ProcessStoppedByUser;

#[derive(thiserror::Error, Debug)]
#[error("Total size could not be determined")]
struct TotalSizeCouldNotBeDetermined;

#[derive(thiserror::Error, Debug)]
#[error("XZ extraction failed: {details:?}")]
struct XzExtractionFailed {
    details: Option<String>,
}

impl FlashRequest {
    pub const fn new(
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

    async fn download_file(
        &self,
        downloading_path: &std::path::PathBuf,
        url: &url::Url,
    ) -> Result<
        File,
        OneOf<(
            ProcessStoppedByUser,
            TotalSizeCouldNotBeDetermined,
            std::io::Error,
            reqwest::Error,
        )>,
    > {
        let mut file = File::create(downloading_path).await.map_err(OneOf::new)?;

        let res = reqwest::get(url.to_owned()).await.map_err(OneOf::new)?;

        let total_size = res
            .content_length()
            .ok_or_else(|| OneOf::new(TotalSizeCouldNotBeDetermined))?;
        let mut downloaded: u64 = 0;
        let mut stream = res.bytes_stream();

        let mut last_sent = Instant::now();

        while let Some(Ok(chunk)) = futures::StreamExt::next(&mut stream).await {
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(OneOf::new)?;
            downloaded = std::cmp::min(downloaded + (chunk.len() as u64), total_size);

            if last_sent.elapsed() >= Duration::from_millis(250) {
                self.set_status(FlashStatus::Active(
                    FlashPhase::Download,
                    Progress::from((downloaded, total_size)),
                ));

                last_sent = Instant::now();
            }

            self.stopped_running().map_err(OneOf::broaden)?;
        }

        file.flush().await.map_err(OneOf::new)?;

        file.sync_all().await.map_err(OneOf::new)?;

        File::open(downloading_path).await.map_err(OneOf::new)
    }

    async fn extract_xz_image(
        &self,
        input_path: &std::path::Path,
        output_path: &std::path::Path,
    ) -> Result<File, OneOf<(XzExtractionFailed, std::io::Error)>> {
        let output_file = File::create(&output_path).await.map_err(OneOf::new)?;

        self.set_status(FlashStatus::Active(FlashPhase::Extract, Progress::Pulse));

        let mut extract_process = tokio::process::Command::new("xzcat")
            .arg(input_path)
            .arg("-k")
            .arg("-T0")
            .stdout(Stdio::from(output_file.into_std().await))
            .stderr(Stdio::piped())
            .spawn()
            .map_err(OneOf::new)?;

        let stderr = extract_process.stderr.take();

        match extract_process.wait().await.map_err(OneOf::new)? {
            x if x.success() => Ok(File::open(&output_path).await.map_err(OneOf::new)?),
            _ => Err(OneOf::new(XzExtractionFailed {
                details: match stderr {
                    Some(mut stderr) => {
                        let mut err_output = String::new();
                        tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut err_output)
                            .await
                            .ok();
                        Some(err_output)
                    }
                    None => None,
                },
            })),
        }
    }

    pub async fn perform(self) {
        match self.perform_job().await {
            Ok(()) => {
                self.set_status(FlashStatus::Done(None));
            }
            Err(e) => {
                if let Err(e) = e.narrow::<ProcessStoppedByUser, _>() {
                    error!("Flashing process failed: {e}");
                    self.set_status(FlashStatus::Done(Some(e.to_string())));
                }
            }
        }
    }

    fn stopped_running(&self) -> Result<(), OneOf<(ProcessStoppedByUser,)>> {
        if self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err(OneOf::new(ProcessStoppedByUser))
        }
    }

    /// Resolves the source image to an on-disk path, downloading or
    /// decompressing as needed. Returns a path (not an open handle) so the
    /// caller can either open it for a raw write or read it in-process for the
    /// Windows installer path.
    async fn prepared_source_path(
        &self,
    ) -> Result<
        PathBuf,
        OneOf<(
            ProcessStoppedByUser,
            std::io::Error,
            reqwest::Error,
            XzExtractionFailed,
            TotalSizeCouldNotBeDetermined,
        )>,
    > {
        match &self.source {
            DiskImage::Local { path, compression } => match compression {
                Compression::Raw => Ok(path.clone()),
                Compression::Xz => {
                    let temp_dir = glib::user_cache_dir();

                    std::fs::create_dir_all(&temp_dir).map_err(OneOf::new)?;

                    let result_path = temp_dir.join(
                        path.file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or("disk_image.iso"),
                    );

                    self.set_status(FlashStatus::Active(FlashPhase::Extract, Progress::Pulse));

                    self.extract_xz_image(path, &result_path)
                        .await
                        .map_err(OneOf::broaden)?;

                    Ok(result_path)
                }
            },
            DiskImage::Online {
                url, download_path, ..
            } => {
                self.download_file(download_path, url)
                    .await
                    .map_err(OneOf::broaden)?;

                Ok(download_path.clone())
            }
        }
    }

    async fn unmount_partitions(&self, client: &udisks::Client) -> Result<(), udisks::Error> {
        let partition_table = self.destination.partition_table().await?;

        for partition in client
            .partitions(&partition_table)
            .await
            .iter()
            .filter_map(|partition| client.object(partition.inner().path().clone()).ok())
        {
            if let Err(e) = udisks_unmount(&partition).await {
                error!(
                    "Failed to unmount partition {:?}, this will be ignored: {e}",
                    partition.object_path()
                );
            }
        }

        Ok(())
    }

    async fn perform_job(
        &self,
    ) -> Result<
        (),
        OneOf<(
            ProcessStoppedByUser,
            std::io::Error,
            reqwest::Error,
            udisks::Error,
            XzExtractionFailed,
            TotalSizeCouldNotBeDetermined,
            WindowsInstallerFailed,
        )>,
    > {
        self.stopped_running().map_err(OneOf::broaden)?;

        info!(
            "Flashing {:?} to {:?}",
            self.source,
            self.destination.object_path()
        );

        let client = udisks::Client::new().await.map_err(OneOf::new)?;

        let destination_block = self.destination.block().await.map_err(OneOf::new)?;

        let destination_drive = client
            .drive_for_block(&destination_block)
            .await
            .map_err(OneOf::new)?;
        if let Err(e) = self.unmount_partitions(&client).await {
            error!("Error unmounting partitions, will be ignored: {e}");
        }

        self.stopped_running().map_err(OneOf::broaden)?;

        let source_path = self.prepared_source_path().await.map_err(OneOf::broaden)?;

        info!("Source: {}", source_path.display());

        self.stopped_running().map_err(OneOf::broaden)?;

        // Windows ISOs cannot simply be raw-copied: they carry no hybrid boot
        // record and ship an `install.wim` larger than FAT32's 4 GiB file
        // limit. `try_flash_windows` builds a bootable FAT32 USB for them, and
        // returns false for anything else so we fall back to a raw byte copy.
        let handled_as_windows = self
            .try_flash_windows(&client, &destination_block, &source_path)
            .await
            .map_err(OneOf::broaden)?;

        if !handled_as_windows {
            let destination_file = udisks_open(&destination_block).await.map_err(OneOf::new)?;

            info!("Destination: {destination_file:?}");

            let source_image = File::open(&source_path).await.map_err(OneOf::new)?;

            //TODO: we should probably spawn a UDIsks.Job for this operation,
            //but udisks-rs does not support this yet
            Self::load_file(
                source_image,
                destination_file,
                |status| self.set_status(status),
                self.is_running.clone(),
            )
            .await
            .map_err(OneOf::broaden)?;
        }

        if let Err(e) = destination_block.rescan(HashMap::new()).await {
            error!("Error rescanning block device, will be ignored: {e}");
        }

        if let Err(e) = destination_drive.eject(HashMap::new()).await {
            error!("Error ejecting drive, will be ignored: {e}");
        }

        info!("Flashing completed successfully");

        Ok(())
    }

    async fn load_file<F: Fn(FlashStatus) + Send>(
        image: File,
        mut target_file: File,
        set_status: F,
        is_running: Arc<AtomicBool>,
    ) -> Result<(), OneOf<(std::io::Error, ProcessStoppedByUser)>> {
        let mut last_set = Instant::now();
        let mut total = 0_u64;

        let size = match image.metadata().await {
            Ok(meta) => meta.len(),
            Err(e) => {
                error!("Failed to get image metadata: {e}");
                0
            }
        };

        info!("Writing file {image:?} ({size} bytes)");

        let mut source = tokio::io::BufReader::with_capacity(1024 * 1024, image);
        let mut target = tokio::io::BufWriter::with_capacity(1024 * 1024, &mut target_file);

        let mut buf = vec![0; 256 * 1024].into_boxed_slice();

        loop {
            let x = tokio::io::AsyncReadExt::read(&mut source, &mut buf)
                .await
                .map_err(OneOf::new)?;

            if x == 0 {
                break;
            }

            total += x as u64;

            tokio::io::AsyncWriteExt::write_all(&mut target, &buf[..x])
                .await
                .map_err(OneOf::new)?;

            if !is_running.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(OneOf::new(ProcessStoppedByUser));
            }

            if last_set.elapsed() >= Duration::from_millis(250) {
                set_status(FlashStatus::Active(
                    FlashPhase::Copy,
                    Progress::from((total, size)),
                ));
                last_set = Instant::now();
            }
        }

        // A flush/sync failure here means the device dropped writes (typically a
        // USB disconnect), so fail honestly rather than report a corrupt write as
        // success.
        target.flush().await.map_err(OneOf::new)?;
        target_file.sync_all().await.map_err(OneOf::new)?;

        Ok(())
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

/// Opens the block device read-write through udisks. When `sync` is set, writes
/// are issued synchronously (`O_SYNC`): each commits to the device before the
/// next, avoiding the large buffered write bursts that can knock a marginal USB
/// drive off the bus. Used by both the raw write and the Windows installer copy.
async fn udisks_open_fd(
    block: &udisks::block::BlockProxy<'_>,
    sync: bool,
) -> udisks::Result<std::os::fd::OwnedFd> {
    let flags = if sync { libc::O_SYNC } else { 0 };
    Ok(block
        .open_device("rw", HashMap::from([("flags", flags.into())]))
        .await?
        .into())
}

async fn udisks_open(block: &udisks::block::BlockProxy<'_>) -> udisks::Result<File> {
    let fd = udisks_open_fd(block, true).await?;
    Ok(std::fs::File::from(fd).into())
}
