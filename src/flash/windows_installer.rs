//! Building a UEFI-bootable Windows installer USB.
//!
//! Windows ISOs can't be raw-copied (no hybrid boot record, and `install.wim`
//! exceeds FAT32's 4 GiB file limit), so this reads the ISO's UDF filesystem
//! with libudf, splits the oversized install image with libwim, and builds a
//! FAT32 filesystem with `fatfs`, all in-process and unmounted, so the sandbox
//! needs no `--filesystem`/`--device` permissions. (A subprocess can't help: it
//! would re-`open()` the device by path and fail `EACCES`; the udisks fd must be
//! used directly.)
//!
//! The image is built in a local scratch file, then cloned to the USB as large
//! sequential blocks: `fatfs`'s many tiny scattered metadata writes are slow
//! sent synchronously to a stick but free in the page cache on local disk.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use log::{error, info};
use terrors::OneOf;

use super::udf::{UdfError, UdfImage};
use super::wim::{self, WimError};

use super::{
    FlashPhase, FlashRequest, FlashStatus, Progress, ProcessStoppedByUser, udisks_open_fd,
    udisks_unmount,
};

/// FAT32 cannot store a single file of 4 GiB or larger. Windows install images
/// (`install.wim`/`install.esd`) regularly exceed this, so they must be split.
const FAT32_MAX_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024 - 1;

#[derive(thiserror::Error, Debug)]
#[error("Installer creation failed: {details:?}")]
pub(super) struct WindowsInstallerFailed {
    details: Option<String>,
}

impl FlashRequest {
    /// Builds a UEFI-bootable Windows installer USB on the destination. Returns
    /// `Ok(false)` without touching the destination for a non-Windows source, so
    /// the caller falls back to a raw write. UEFI only (single GPT + FAT32).
    pub(super) async fn try_flash_windows(
        &self,
        client: &udisks::Client,
        destination_block: &udisks::block::BlockProxy<'_>,
        source_path: &Path,
    ) -> Result<
        bool,
        OneOf<(
            ProcessStoppedByUser,
            std::io::Error,
            udisks::Error,
            WindowsInstallerFailed,
        )>,
    > {
        // `.img`/`.raw` images are always raw-written; only probe ISOs.
        if !is_iso_path(source_path) {
            return Ok(false);
        }

        let work_dir = glib::user_cache_dir().join("impression-windows");

        // Detect + extract. `None` means not a Windows source: leave the drive
        // untouched for the caller's raw-write fallback.
        let Some(source_label) = self
            .extract_windows_iso(source_path, &work_dir)
            .await
            .map_err(OneOf::broaden)?
        else {
            info!("Source is not a Windows installer, falling back to raw write");
            if let Err(e) = std::fs::remove_dir_all(&work_dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                error!("Failed to remove scratch directory, will be ignored: {e}");
            }
            return Ok(false);
        };

        info!("Detected Windows installer, building FAT32 installer USB");

        let result = self
            .build_windows_installer(client, destination_block, &work_dir, &source_label)
            .await;

        // Always clean up the scratch directory, success or failure.
        if let Err(e) = std::fs::remove_dir_all(&work_dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            error!("Failed to remove scratch directory, will be ignored: {e}");
        }

        result.map(|()| true)
    }

    async fn build_windows_installer(
        &self,
        client: &udisks::Client,
        destination_block: &udisks::block::BlockProxy<'_>,
        work_dir: &Path,
        source_label: &str,
    ) -> Result<
        (),
        OneOf<(
            ProcessStoppedByUser,
            std::io::Error,
            udisks::Error,
            WindowsInstallerFailed,
        )>,
    > {
        let label = fat_label(source_label);

        self.split_oversized_install_image(work_dir)
            .await
            .map_err(OneOf::broaden)?;

        // udisks's vfat format zeroes the FAT/root/boot regions, so the later
        // sparse clone can skip holes (they land on zeroed device regions).
        self.set_status(FlashStatus::Active(FlashPhase::Partition, Progress::Pulse));
        self.stopped_running().map_err(OneOf::broaden)?;
        let partition = self
            .prepare_fat32_partition(client, destination_block, &label)
            .await
            .map_err(OneOf::new)?;
        let partition_block = partition.block().await.map_err(OneOf::new)?;
        let partition_size = partition_block.size().await.map_err(OneOf::new)?;

        let image_path = glib::user_cache_dir().join("impression-windows.img");
        let staged = self
            .stage_and_clone(
                work_dir,
                &partition,
                &partition_block,
                &image_path,
                partition_size,
                &label,
            )
            .await;

        if let Err(e) = std::fs::remove_file(&image_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            error!("Failed to remove scratch image, will be ignored: {e}");
        }

        staged
    }

    /// Builds the FAT32 filesystem into `image_path` on local disk, then clones
    /// it onto the freshly prepared `partition`.
    async fn stage_and_clone(
        &self,
        work_dir: &Path,
        partition: &udisks::Object,
        partition_block: &udisks::block::BlockProxy<'_>,
        image_path: &Path,
        partition_size: u64,
        label: &str,
    ) -> Result<
        (),
        OneOf<(
            ProcessStoppedByUser,
            std::io::Error,
            udisks::Error,
            WindowsInstallerFailed,
        )>,
    > {
        self.set_status(FlashStatus::Active(FlashPhase::BuildImage, Progress::Pulse));
        let content_total = self
            .build_fat_image(work_dir, image_path, partition_size, label)
            .await
            .map_err(OneOf::broaden)?;

        // Unmount first: an automounter grabs the freshly formatted partition
        // within a second or two, and writing the raw device while the kernel's
        // FAT driver holds it corrupts the filesystem. The fd is `O_SYNC` so
        // writes commit steadily rather than in bursts big enough to knock a
        // marginal stick off the bus.
        self.ensure_unmounted(partition).await.map_err(OneOf::new)?;
        let device_fd = udisks_open_fd(partition_block, true)
            .await
            .map_err(OneOf::new)?;
        self.clone_image_to_device(image_path, device_fd, content_total)
            .await
            .map_err(OneOf::broaden)?;

        Ok(())
    }

    /// Unmounts `partition` and confirms it stays unmounted, defeating the
    /// desktop automount race: an automounter can mount a freshly formatted
    /// filesystem shortly after it appears, so we unmount, wait for any pending
    /// automount to surface, and repeat until a full cycle sees no mount points.
    async fn ensure_unmounted(&self, partition: &udisks::Object) -> udisks::Result<()> {
        for _ in 0..10 {
            udisks_unmount(partition).await?;
            // Let a pending automount actually fire so we can see and undo it.
            tokio::time::sleep(Duration::from_millis(300)).await;
            if partition.filesystem().await?.mount_points().await?.is_empty() {
                return Ok(());
            }
        }

        error!("Partition kept getting remounted; proceeding to write anyway");
        Ok(())
    }

    /// Extracts the ISO into a fresh `work_dir` if it is a Windows installer,
    /// returning its volume label; `None` means it is not one and the caller
    /// should fall back to a raw write. Blocking: libudf is synchronous, `!Send`.
    async fn extract_windows_iso(
        &self,
        source_path: &Path,
        work_dir: &Path,
    ) -> Result<Option<String>, OneOf<(ProcessStoppedByUser, std::io::Error, WindowsInstallerFailed)>>
    {
        self.set_status(FlashStatus::Active(FlashPhase::Extract, Progress::Pulse));

        let source = source_path.to_owned();
        let work = work_dir.to_owned();
        let is_running = self.is_running.clone();
        let status = self.status.clone();

        let outcome = tokio::task::spawn_blocking(move || -> Result<Option<String>, UdfError> {
            // A non-UDF image (e.g. a Linux ISO9660 ISO) is simply "not Windows".
            let Ok(image) = UdfImage::open(&source) else {
                return Ok(None);
            };
            if !image.has_path("sources/install.wim") && !image.has_path("sources/install.esd") {
                return Ok(None);
            }
            let label = image.volume_label();

            // Fresh scratch directory.
            if let Err(e) = std::fs::remove_dir_all(&work)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                return Err(UdfError::Io(e));
            }
            std::fs::create_dir_all(&work)?;

            image.extract_all(
                &work,
                &mut |done, total| {
                    if let Ok(mut lock) = status.lock() {
                        *lock =
                            FlashStatus::Active(FlashPhase::Extract, Progress::from((done, total)));
                    }
                },
                &mut || !is_running.load(std::sync::atomic::Ordering::SeqCst),
            )?;

            Ok(Some(label.unwrap_or_default()))
        })
        .await;

        match outcome {
            Ok(Ok(label)) => Ok(label),
            Ok(Err(UdfError::Cancelled)) => Err(OneOf::new(ProcessStoppedByUser)),
            Ok(Err(UdfError::Io(e))) => Err(OneOf::new(e)),
            Ok(Err(e)) => Err(OneOf::new(WindowsInstallerFailed {
                details: Some(e.to_string()),
            })),
            Err(join_error) => Err(OneOf::new(WindowsInstallerFailed {
                details: Some(format!("extraction task failed: {join_error}")),
            })),
        }
    }

    /// Splits `sources/install.wim` (or `.esd`) into `.swm` parts in place if it
    /// exceeds the FAT32 file-size limit, then removes the oversized original.
    async fn split_oversized_install_image(
        &self,
        work_dir: &Path,
    ) -> Result<(), OneOf<(ProcessStoppedByUser, std::io::Error, WindowsInstallerFailed)>> {
        let sources_dir = work_dir.join("sources");

        for name in ["install.wim", "install.esd"] {
            let image = sources_dir.join(name);
            let size = match std::fs::metadata(&image) {
                Ok(metadata) => metadata.len(),
                Err(_) => continue,
            };

            if size <= FAT32_MAX_FILE_SIZE {
                continue;
            }

            info!("Splitting {} ({size} bytes) to fit FAT32", image.display());
            self.set_status(FlashStatus::Active(FlashPhase::ProcessImage, Progress::Pulse));

            // libwim's split is blocking and `!Send`, so run it off the executor.
            let input = image.clone();
            let output = sources_dir.join("install.swm");
            let is_running = self.is_running.clone();
            let result =
                tokio::task::spawn_blocking(move || wim::split(&input, &output, &is_running)).await;

            match result {
                Ok(Ok(())) => {}
                Ok(Err(WimError::Cancelled)) => return Err(OneOf::new(ProcessStoppedByUser)),
                Ok(Err(WimError::Failed(message))) => {
                    return Err(OneOf::new(WindowsInstallerFailed {
                        details: Some(message),
                    }));
                }
                Err(join_error) => {
                    return Err(OneOf::new(WindowsInstallerFailed {
                        details: Some(format!("split task failed: {join_error}")),
                    }));
                }
            }

            // Windows Setup loads the `.swm` parts; the original must not remain.
            std::fs::remove_file(&image).map_err(OneOf::new)?;
        }

        Ok(())
    }

    /// Builds the installer's FAT32 filesystem into `image_path`, labelled
    /// `label`, and returns the total file bytes written (for clone progress).
    /// The image is sparse and partition-sized, so cloning it is a 1:1 copy;
    /// source files are deleted as packed to bound peak scratch. Blocking.
    async fn build_fat_image(
        &self,
        work_dir: &Path,
        image_path: &Path,
        partition_size: u64,
        label: &str,
    ) -> Result<u64, OneOf<(ProcessStoppedByUser, std::io::Error, WindowsInstallerFailed)>> {
        let work = work_dir.to_owned();
        let image = image_path.to_owned();
        let status = self.status.clone();
        let is_running = self.is_running.clone();
        let label = fat_label_bytes(label);

        let outcome = tokio::task::spawn_blocking(move || -> Result<u64, WriteError> {
            let content_total = dir_byte_total(&work).map_err(WriteError::Io)?;
            if content_total == 0 {
                return Err(WriteError::Empty);
            }

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&image)
                .map_err(WriteError::Io)?;
            file.set_len(partition_size).map_err(WriteError::Io)?;

            let mut stream = fscommon::BufStream::new(file);
            fatfs::format_volume(
                &mut stream,
                fatfs::FormatVolumeOptions::new()
                    .fat_type(fatfs::FatType::Fat32)
                    .volume_label(label),
            )
            .map_err(WriteError::Fat)?;
            let filesystem =
                fatfs::FileSystem::new(stream, fatfs::FsOptions::new()).map_err(WriteError::Fat)?;

            let mut sink = WriteSink {
                buffer: vec![0u8; 1024 * 1024],
                written: 0,
                total: content_total,
                status,
                is_running,
                delete_packed: true,
            };
            sink.write_dir(&filesystem.root_dir(), &work)?;
            filesystem.unmount().map_err(WriteError::Fat)?;

            Ok(content_total)
        })
        .await;

        finish_write(outcome, "building the installer image")
    }

    /// Clones the staged image onto `device_fd` in large sequential blocks,
    /// skipping sparse regions. Progress is reported over `content_total`.
    /// Blocking.
    async fn clone_image_to_device(
        &self,
        image_path: &Path,
        device_fd: std::os::fd::OwnedFd,
        content_total: u64,
    ) -> Result<(), OneOf<(ProcessStoppedByUser, std::io::Error, WindowsInstallerFailed)>> {
        let image = image_path.to_owned();
        let status = self.status.clone();
        let is_running = self.is_running.clone();

        let outcome = tokio::task::spawn_blocking(move || -> Result<(), WriteError> {
            let mut source = std::fs::File::open(&image).map_err(WriteError::Io)?;
            let source_len = source.metadata().map_err(WriteError::Io)?.len();
            let mut dest = std::fs::File::from(device_fd);

            let mut cloned = 0_u64;
            clone_image(
                &mut source,
                &mut dest,
                source_len,
                &mut |written| {
                    cloned = cloned.saturating_add(written);
                    if let Ok(mut lock) = status.lock() {
                        let done = cloned.min(content_total);
                        *lock = FlashStatus::Active(
                            FlashPhase::Copy,
                            Progress::from((done, content_total)),
                        );
                    }
                },
                &mut || !is_running.load(std::sync::atomic::Ordering::SeqCst),
            )
        })
        .await;

        finish_write(
            outcome,
            "writing to the USB device (it may have disconnected under load; try \
             a different port, cable, or drive)",
        )
    }

    /// Wipes the destination to a GPT disk holding a single FAT32 partition and
    /// returns the new partition's udisks object. The partition and filesystem
    /// are both named `label`.
    async fn prepare_fat32_partition(
        &self,
        client: &udisks::Client,
        destination_block: &udisks::block::BlockProxy<'_>,
        label: &str,
    ) -> udisks::Result<udisks::Object> {
        destination_block.format("gpt", HashMap::new()).await?;

        let partition_table = self.wait_for_partition_table(client).await?;

        let partition_path = partition_table
            .create_partition_and_format(
                0,
                0,
                "",
                label,
                HashMap::new(),
                "vfat",
                HashMap::from([
                    ("label", label.into()),
                    ("update-partition-type", true.into()),
                ]),
            )
            .await?;

        Ok(client.object(partition_path).expect("valid object path"))
    }

    /// Waits for udisks to expose the `PartitionTable` interface on the
    /// destination after `format("gpt")`. Driven by the `InterfacesAdded` signal
    /// so it returns as soon as the table appears, bounded by a timeout.
    async fn wait_for_partition_table(
        &self,
        client: &udisks::Client,
    ) -> udisks::Result<udisks::partitiontable::PartitionTableProxy<'static>> {
        const SETTLE_TIMEOUT: Duration = Duration::from_secs(15);
        const PARTITION_TABLE: &str = "org.freedesktop.UDisks2.PartitionTable";

        // Subscribe before the first check so a table appearing between the check
        // and the wait is not missed.
        let mut interfaces_added = client.object_manager().receive_interfaces_added().await?;

        if let Ok(partition_table) = self.destination.partition_table().await {
            return Ok(partition_table);
        }

        let destination_path = self.destination.object_path().as_str();
        let settled = async {
            while let Some(signal) = futures::StreamExt::next(&mut interfaces_added).await {
                let Ok(args) = signal.args() else { continue };
                if args.object_path.as_str() == destination_path
                    && args
                        .interfaces_and_properties
                        .keys()
                        .any(|interface| interface.as_str() == PARTITION_TABLE)
                {
                    break;
                }
            }
        };

        // Whether the interface arrived or the wait timed out, fetch once more so
        // the real udisks error surfaces if it never appeared.
        let _ = tokio::time::timeout(SETTLE_TIMEOUT, settled).await;
        self.destination.partition_table().await
    }
}

enum WriteError {
    /// The user aborted.
    Cancelled,
    /// The extracted ISO had no files.
    Empty,
    /// Host-side error reading the scratch tree.
    Io(std::io::Error),
    /// Destination-write failure.
    Fat(std::io::Error),
}

/// State for the recursive FAT build.
struct WriteSink {
    /// Reusable copy buffer.
    buffer: Vec<u8>,
    /// Bytes packed so far.
    written: u64,
    /// Total bytes to pack, for progress.
    total: u64,
    status: std::sync::Arc<std::sync::Mutex<FlashStatus>>,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Delete each source file once packed, so scratch never holds two copies.
    delete_packed: bool,
}

impl WriteSink {
    fn cancelled(&self) -> bool {
        !self.is_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn report_progress(&self) {
        if let Ok(mut lock) = self.status.lock() {
            // Building the local image is preparation, not the device write.
            *lock = FlashStatus::Active(
                FlashPhase::BuildImage,
                Progress::from((self.written, self.total)),
            );
        }
    }

    /// Recursively writes `host_dir` into the FAT directory `dir`, polling for
    /// cancellation. With `delete_packed`, each source file is removed once
    /// written so scratch never holds the whole installer twice.
    fn write_dir<T: fatfs::ReadWriteSeek>(
        &mut self,
        dir: &fatfs::Dir<'_, T>,
        host_dir: &Path,
    ) -> Result<(), WriteError> {
        for entry in std::fs::read_dir(host_dir).map_err(WriteError::Io)? {
            if self.cancelled() {
                return Err(WriteError::Cancelled);
            }

            let entry = entry.map_err(WriteError::Io)?;
            let file_name = entry.file_name();
            let name = file_name.to_str().ok_or_else(|| {
                WriteError::Fat(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("non-UTF-8 filename: {}", file_name.to_string_lossy()),
                ))
            })?;
            let file_type = entry.file_type().map_err(WriteError::Io)?;
            let source_path = entry.path();

            if file_type.is_dir() {
                let subdir = dir.create_dir(name).map_err(WriteError::Fat)?;
                self.write_dir(&subdir, &source_path)?;
            } else {
                let mut source = std::fs::File::open(&source_path).map_err(WriteError::Io)?;
                let mut dest = dir.create_file(name).map_err(WriteError::Fat)?;
                dest.truncate().map_err(WriteError::Fat)?;

                loop {
                    if self.cancelled() {
                        return Err(WriteError::Cancelled);
                    }

                    let read = source.read(&mut self.buffer).map_err(WriteError::Io)?;
                    if read == 0 {
                        break;
                    }
                    dest.write_all(&self.buffer[..read]).map_err(WriteError::Fat)?;
                    self.written += read as u64;
                    self.report_progress();
                }

                if self.delete_packed {
                    drop(source);
                    if let Err(remove_error) = std::fs::remove_file(&source_path) {
                        error!("Failed to remove packed source file, will be ignored: {remove_error}");
                    }
                }
            }
        }

        Ok(())
    }
}

/// Block size for the device clone: large and sequential, so the USB sees a few
/// big writes instead of fatfs's metadata churn.
const CLONE_CHUNK: usize = 4 * 1024 * 1024;

/// Maps the outcome of a blocking FAT-build/clone task onto the caller's error
/// set. `fat_context` describes the failing operation for [`WriteError::Fat`]
/// (and a dead task), e.g. "writing to the USB device".
fn finish_write<T>(
    outcome: Result<Result<T, WriteError>, tokio::task::JoinError>,
    fat_context: &str,
) -> Result<T, OneOf<(ProcessStoppedByUser, std::io::Error, WindowsInstallerFailed)>> {
    match outcome {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(WriteError::Cancelled)) => Err(OneOf::new(ProcessStoppedByUser)),
        Ok(Err(WriteError::Io(error))) => Err(OneOf::new(error)),
        Ok(Err(WriteError::Empty)) => Err(OneOf::new(WindowsInstallerFailed {
            details: Some("extracted ISO was empty".to_owned()),
        })),
        Ok(Err(WriteError::Fat(error))) => Err(OneOf::new(WindowsInstallerFailed {
            details: Some(format!("{fat_context}: {error}")),
        })),
        Err(join_error) => Err(OneOf::new(WindowsInstallerFailed {
            details: Some(format!("{fat_context}: task failed: {join_error}")),
        })),
    }
}

/// Reads up to `buffer.len()` bytes, looping over short reads until the buffer
/// is full or EOF. Returns the number of bytes read (0 only at EOF).
fn read_full(source: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Result of looking for the next populated extent in a sparse file: a data
/// range `[start, end)`, no more data (trailing hole), or no `SEEK_DATA` support.
enum NextExtent {
    Data(u64, u64),
    End,
    Unsupported,
}

/// Finds the next populated extent at or after `pos` via `SEEK_DATA`/`SEEK_HOLE`,
/// moving the fd offset as a side effect.
// Offsets fit both `off_t` and `u64`; the i64 to u64 casts run only after a
// non-negative check.
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn next_data_extent(fd: std::os::fd::RawFd, pos: u64, len: u64) -> std::io::Result<NextExtent> {
    if pos >= len {
        return Ok(NextExtent::End);
    }
    // SAFETY: `fd` is a live descriptor; lseek only reads/sets its offset.
    let data = unsafe { libc::lseek(fd, pos as libc::off_t, libc::SEEK_DATA) };
    if data < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENXIO) => Ok(NextExtent::End),
            Some(libc::EINVAL | libc::EOPNOTSUPP) => Ok(NextExtent::Unsupported),
            _ => Err(error),
        };
    }
    let data = data as u64;
    // SAFETY: as above; SEEK_HOLE after valid data returns the extent end or EOF.
    let hole = unsafe { libc::lseek(fd, data as libc::off_t, libc::SEEK_HOLE) };
    let end = if hole < 0 { len } else { (hole as u64).min(len) };
    Ok(NextExtent::Data(data, end.max(data)))
}

/// Clones the populated regions of sparse image `source` onto `dest` at matching
/// offsets, in [`CLONE_CHUNK`] writes. Holes are skipped via `SEEK_DATA`/
/// `SEEK_HOLE`, falling back to skipping all-zero chunks if unsupported.
/// `on_written` reports bytes sent to the device.
// `read` is bounded by `CLONE_CHUNK`, so the `usize` to `i64` seek cast can't wrap.
#[allow(clippy::cast_possible_wrap)]
fn clone_image(
    source: &mut std::fs::File,
    dest: &mut std::fs::File,
    source_len: u64,
    on_written: &mut dyn FnMut(u64),
    should_cancel: &mut dyn FnMut() -> bool,
) -> Result<(), WriteError> {
    let source_fd = source.as_raw_fd();
    let mut buffer = vec![0u8; CLONE_CHUNK];
    let mut pos = 0_u64;
    let mut have_seek_data = true;

    while pos < source_len {
        if should_cancel() {
            return Err(WriteError::Cancelled);
        }

        // The extent to write next. With SEEK_DATA it is a fully-populated range;
        // without it, the remainder of the file (zero chunks skipped per-chunk).
        let (data_start, data_end) = if have_seek_data {
            match next_data_extent(source_fd, pos, source_len).map_err(WriteError::Io)? {
                NextExtent::Data(start, end) => (start, end),
                NextExtent::End => break,
                NextExtent::Unsupported => {
                    have_seek_data = false;
                    (pos, source_len)
                }
            }
        } else {
            (pos, source_len)
        };

        source
            .seek(SeekFrom::Start(data_start))
            .map_err(WriteError::Io)?;
        dest.seek(SeekFrom::Start(data_start))
            .map_err(WriteError::Fat)?;

        let mut offset = data_start;
        while offset < data_end {
            if should_cancel() {
                return Err(WriteError::Cancelled);
            }
            let want = usize::try_from((data_end - offset).min(CLONE_CHUNK as u64))
                .unwrap_or(CLONE_CHUNK);
            let read = read_full(source, &mut buffer[..want]).map_err(WriteError::Io)?;
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            if have_seek_data || chunk.iter().any(|&byte| byte != 0) {
                dest.write_all(chunk).map_err(WriteError::Fat)?;
                on_written(read as u64);
            } else {
                // Fallback path only: leave the (already-zeroed) device region be.
                dest.seek(SeekFrom::Current(read as i64))
                    .map_err(WriteError::Fat)?;
            }
            offset += read as u64;
        }
        pos = data_end;
    }

    // A failure here means the device dropped writes (typically a USB
    // disconnect), so surface it rather than reporting a silent partial flash.
    dest.flush().map_err(WriteError::Fat)?;
    if unsafe { libc::fsync(dest.as_raw_fd()) } != 0 {
        return Err(WriteError::Fat(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Total size in bytes of every regular file under `dir`, recursively.
fn dir_byte_total(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            total += dir_byte_total(&entry.path())?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

/// FAT32 volume label from the source ISO label: uppercase ASCII, max 11 chars,
/// matching what Rufus does. Falls back to `INSTALLER` when nothing remains.
fn fat_label(source: &str) -> String {
    let cleaned: String = source
        .to_ascii_uppercase()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(11)
        .collect();
    if cleaned.is_empty() {
        "INSTALLER".to_owned()
    } else {
        cleaned
    }
}

/// Pads a FAT-sanitized `label` into the fixed 11-byte, space-filled field
/// `fatfs` expects.
fn fat_label_bytes(label: &str) -> [u8; 11] {
    let mut bytes = [b' '; 11];
    for (slot, byte) in bytes.iter_mut().zip(label.bytes().take(11)) {
        *slot = byte;
    }
    bytes
}

/// Heuristic: does this path look like an ISO image worth probing for a Windows
/// installer? `.img`/`.raw` images are always raw-written.
fn is_iso_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains(".iso"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// `clone_image` must reproduce every populated byte at its offset and leave
    /// the holes untouched (whichever path the scratch filesystem takes).
    #[test]
    fn clone_image_copies_data_extents_and_skips_holes() {
        const LEN: u64 = 16 * 1024 * 1024;
        const BLOCK: usize = 1024 * 1024;
        let second = 8 * 1024 * 1024;

        let base = std::env::temp_dir().join(format!("impression-clone-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let src_path = base.join("src.img");
        let dst_path = base.join("dst.img");

        // Sparse source: data at [0, 1 MiB) and [8 MiB, 9 MiB), holes elsewhere.
        {
            let mut src = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&src_path)
                .unwrap();
            src.set_len(LEN).unwrap();
            src.write_all(&vec![0xAB; BLOCK]).unwrap();
            src.seek(SeekFrom::Start(second as u64)).unwrap();
            src.write_all(&vec![0xCD; BLOCK]).unwrap();
            src.sync_all().unwrap();
        }

        // Fresh zeroed destination of the same size (as udisks's vfat format
        // leaves the regions our clone will skip).
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dst_path)
            .unwrap()
            .set_len(LEN)
            .unwrap();

        let mut source = std::fs::File::open(&src_path).unwrap();
        let mut dest = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dst_path)
            .unwrap();
        let mut written = 0_u64;
        clone_image(&mut source, &mut dest, LEN, &mut |bytes| written += bytes, &mut || false)
            .unwrap_or_else(|_| panic!("clone_image failed"));

        let out = std::fs::read(&dst_path).unwrap();
        assert_eq!(out.len() as u64, LEN);
        assert!(out[..BLOCK].iter().all(|&byte| byte == 0xAB), "first extent");
        assert!(
            out[second..second + BLOCK].iter().all(|&byte| byte == 0xCD),
            "second extent",
        );
        assert!(out[BLOCK..second].iter().all(|&byte| byte == 0), "gap stays zero");
        assert!(out[second + BLOCK..].iter().all(|&byte| byte == 0), "tail stays zero");
        assert!(written >= 2 * BLOCK as u64, "both extents reported as written");

        std::fs::remove_dir_all(&base).unwrap();
    }
}
