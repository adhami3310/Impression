use gettextrs::gettext;
use log::{error, info};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::time::Instant;
use tokio::{fs::File, io::AsyncWriteExt};

use crate::window::{Compression, DiskImage};

#[derive(Clone, Debug)]
pub enum FlashPhase {
    Download,
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
#[error("Error while getting total size")]
struct TotalSize;

#[derive(thiserror::Error, Debug)]
#[error("Error during xz extraction: {details:?}")]
struct XzExtractionError {
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
        downloading_path: std::path::PathBuf,
        url: &str,
    ) -> anyhow::Result<File> {
        let mut file = File::create(downloading_path.clone()).await?;

        let res = reqwest::get(url).await?;

        let total_size = res.content_length().ok_or(TotalSize)?;
        let mut downloaded: u64 = 0;
        let mut stream = res.bytes_stream();

        let mut last_sent = Instant::now();

        while let Some(Ok(chunk)) = futures::StreamExt::next(&mut stream).await {
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            downloaded = std::cmp::min(downloaded + (chunk.len() as u64), total_size);

            if last_sent.elapsed() >= Duration::from_millis(250) {
                self.set_status(FlashStatus::Active(
                    FlashPhase::Download,
                    Progress::from((downloaded, total_size)),
                ));

                last_sent = Instant::now();
            }
        }

        Ok(file)
    }

    async fn extract_xz_image(
        &self,
        input_path: &std::path::Path,
        output_path: &std::path::Path,
    ) -> anyhow::Result<File> {
        let output_file = File::create(&output_path).await?;

        self.set_status(FlashStatus::Active(FlashPhase::Copy, Progress::Pulse));

        let mut extract_process = tokio::process::Command::new("xzcat")
            .arg(input_path)
            .arg("-k")
            .arg("-T0")
            .stdout(Stdio::from(output_file.into_std().await))
            .stderr(Stdio::piped())
            .spawn()?;

        let stderr = extract_process.stderr.take();

        match extract_process.wait().await? {
            x if x.success() => Ok(File::open(&output_path).await?),
            _ => Err(XzExtractionError {
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
            }
            .into()),
        }
    }

    pub async fn perform(self) {
        if let Err(e) = self.perform_job().await {
            error!("Flash operation failed: {e}");
            self.set_status(FlashStatus::Done(Some(e.to_string())));
        }
    }

    fn stopped_running(&self) -> bool {
        !self.is_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn get_source_file_from_image(&self) -> anyhow::Result<File> {
        match &self.source {
            DiskImage::Local { path, compression } => match compression {
                Compression::Raw => Ok(File::open(path).await?),
                Compression::Xz => {
                    let temp_dir = glib::user_cache_dir();

                    std::fs::create_dir_all(&temp_dir)?;

                    let result_path = temp_dir.join(
                        path.file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or("disk_image.iso"),
                    );

                    self.set_status(FlashStatus::Active(FlashPhase::Copy, Progress::Pulse));

                    self.extract_xz_image(path, &result_path).await
                }
            },
            DiskImage::Online { url, name } => {
                let temp_dir = glib::user_cache_dir();

                std::fs::create_dir_all(&temp_dir)?;

                let temporary_download_path = temp_dir.join(name.to_owned() + ".iso");

                self.download_file(temporary_download_path, url).await
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
            udisks_unmount(&partition).await.ok();
        }

        Ok(())
    }

    async fn perform_job(&self) -> anyhow::Result<()> {
        if self.stopped_running() {
            info!("Flash operation was cancelled before starting");
            return Ok(());
        }

        let client = udisks::Client::new().await?;

        let destination_block = self.destination.block().await?;

        let destination_drive = client.drive_for_block(&destination_block).await?;

        let _ = self.unmount_partitions(&client).await;

        if self.stopped_running() {
            info!("Flash operation was cancelled after unmounting partitions, but before flashing");
            return Ok(());
        }

        let destination_file = udisks_open(&destination_block).await?;

        let source_image = self.get_source_file_from_image().await?;

        if self.stopped_running() {
            info!(
                "Flash operation was cancelled after preparing source image, but before flashing"
            );
            return Ok(());
        }

        //TODO: we should probably spawn a UDIsks.Job for this operation,
        //but udisks-rs does not support this yet
        Self::load_file(
            source_image,
            destination_file,
            |status| self.set_status(status),
            self.is_running.clone(),
        )
        .await;

        let _ = destination_block.rescan(HashMap::new()).await;

        let _ = destination_drive.eject(HashMap::new()).await;

        Ok(())
    }

    async fn load_file<F: Fn(FlashStatus) + Send>(
        image: File,
        mut target_file: File,
        set_status: F,
        is_running: Arc<AtomicBool>,
    ) {
        let mut last_sent = Instant::now();
        let mut total = 0_u64;

        let size = match image.metadata().await {
            Ok(meta) => meta.len(),
            Err(e) => {
                error!("Failed to get image metadata: {e}");
                0
            }
        };

        let mut source = tokio::io::BufReader::with_capacity(1024 * 1024, image);
        let mut target = tokio::io::BufWriter::with_capacity(1024 * 1024, &mut target_file);

        let mut buf = vec![0; 256 * 1024].into_boxed_slice();

        let stopped = || !is_running.load(std::sync::atomic::Ordering::SeqCst);

        while let Ok(x) = tokio::io::AsyncReadExt::read(&mut source, &mut buf).await {
            if stopped() {
                return;
            }
            if x == 0 {
                break;
            }
            total += x as u64;
            if tokio::io::AsyncWriteExt::write_all(&mut target, &buf[..x])
                .await
                .is_err()
            {
                set_status(FlashStatus::Done(Some(gettext("Writing to disk failed"))));
                return;
            }

            if stopped() {
                return;
            }

            if last_sent.elapsed() >= Duration::from_millis(250) {
                set_status(FlashStatus::Active(
                    FlashPhase::Copy,
                    Progress::from((total, size)),
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
