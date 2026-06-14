//! Hand-written FFI to libcdio's `libudf` for reading the UDF filesystem on
//! Windows install ISOs, without `libcdio-sys` (no bindgen at build time).
//! Dynamically linked against the system `libudf`. Windows media is UDF-only
//! because `install.wim` exceeds 4 GiB, so `libarchive` cannot read it.
//!
//! Blocking and `!Send` (raw pointers): call from a blocking context, never
//! across `.await`.
//!
//! Ownership: `udf_open` returns a `udf_t*` freed by `udf_close`; `udf_get_root`
//! and `udf_fopen` return owned dirents freed by `udf_dirent_free`. A dirent
//! from `udf_opendir` (or the root) is a content-iteration handle that
//! `udf_readdir` advances and frees once it returns NULL. `udf_opendir` rejects
//! the root, so the root is iterated with `udf_readdir` directly.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Opaque `udf_t` from libcdio.
enum UdfT {}
/// Opaque `udf_dirent_t` from libcdio. Held only by pointer; the prototypes'
/// `const` doesn't affect the ABI, so every parameter is `*mut` to match calls.
enum UdfDirent {}

#[link(name = "udf")]
unsafe extern "C" {
    fn udf_open(psz_path: *const c_char) -> *mut UdfT;
    fn udf_close(p_udf: *mut UdfT) -> bool;
    fn udf_get_root(p_udf: *mut UdfT, b_any_partition: bool, i_partition: u16) -> *mut UdfDirent;
    fn udf_get_volume_id(p_udf: *mut UdfT, psz_volid: *mut c_char, i_volid: c_uint) -> c_int;
    fn udf_fopen(p_udf_root: *mut UdfDirent, psz_name: *const c_char) -> *mut UdfDirent;
    fn udf_dirent_free(p_udf_dirent: *mut UdfDirent) -> bool;
    fn udf_get_filename(p_udf_dirent: *mut UdfDirent) -> *const c_char;
    fn udf_get_file_length(p_udf_dirent: *mut UdfDirent) -> u64;
    fn udf_setpos(p_udf_dirent: *mut UdfDirent, offset: libc::off_t) -> bool;
    fn udf_read_block(p_udf_dirent: *mut UdfDirent, buf: *mut c_void, count: usize) -> isize;
    fn udf_readdir(p_udf_dirent: *mut UdfDirent) -> *mut UdfDirent;
    fn udf_opendir(p_udf_dirent: *mut UdfDirent) -> *mut UdfDirent;
    fn udf_is_dir(p_udf_dirent: *mut UdfDirent) -> bool;
}

/// UDF logical block size for the media we handle.
const UDF_BLOCKSIZE: usize = 2048;
/// Read granularity when streaming a file out (1 MiB).
const CHUNK_BLOCKS: usize = 512;

#[derive(thiserror::Error, Debug)]
pub enum UdfError {
    #[error("could not open UDF image: {0}")]
    Open(PathBuf),
    #[error("UDF read failed: {0}")]
    Read(String),
    #[error("operation was cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// An opened UDF image.
pub struct UdfImage {
    raw: *mut UdfT,
}

impl UdfImage {
    pub fn open(path: &Path) -> Result<Self, UdfError> {
        let c_path = CString::new(path.to_str().ok_or_else(|| UdfError::Open(path.to_owned()))?)
            .map_err(|_| UdfError::Open(path.to_owned()))?;
        // SAFETY: `c_path` is a valid NUL-terminated string for this call.
        let raw = unsafe { udf_open(c_path.as_ptr()) };
        if raw.is_null() {
            return Err(UdfError::Open(path.to_owned()));
        }
        Ok(Self { raw })
    }

    /// The image's volume identifier (latin-1), or `None` if absent. Used to
    /// label the installer USB the way the source ISO is labelled.
    pub fn volume_label(&self) -> Option<String> {
        let mut buffer = [0u8; 128];
        // SAFETY: `self.raw` is live; the buffer holds 128 bytes for the call.
        let written =
            unsafe { udf_get_volume_id(self.raw, buffer.as_mut_ptr().cast::<c_char>(), 128) };
        let len = usize::try_from(written).unwrap_or(0).min(buffer.len());
        let label: String = buffer[..len]
            .iter()
            .take_while(|&&byte| byte != 0)
            .map(|&byte| char::from(byte))
            .collect();
        let label = label.trim().to_owned();
        (!label.is_empty()).then_some(label)
    }

    /// The root directory as a content-iteration handle (owned).
    fn root(&self) -> Result<*mut UdfDirent, UdfError> {
        // SAFETY: `self.raw` is a live udf_t; b_any_partition=true, partition 0.
        let raw = unsafe { udf_get_root(self.raw, true, 0) };
        if raw.is_null() {
            Err(UdfError::Read("udf_get_root returned NULL".to_owned()))
        } else {
            Ok(raw)
        }
    }

    /// Whether `rel` (a `/`-separated, case-sensitive path) exists.
    pub fn has_path(&self, rel: &str) -> bool {
        matches!(self.open_file(rel), Ok(Some(_)))
    }

    /// Open a file by path, or `None` if it does not exist.
    pub fn open_file(&self, rel: &str) -> Result<Option<UdfFile<'_>>, UdfError> {
        let root = self.root()?;
        let c_rel = CString::new(rel).map_err(|_| UdfError::Read(format!("invalid path: {rel}")))?;
        // SAFETY: `root` is live; `udf_fopen` copies the path and does not take
        // ownership of `root`, which we free immediately after.
        let raw = unsafe {
            let file = udf_fopen(root, c_rel.as_ptr());
            udf_dirent_free(root);
            file
        };
        Ok((!raw.is_null()).then_some(UdfFile {
            raw,
            _image: std::marker::PhantomData,
        }))
    }

    /// Every entry in the image (files and directories), recursively.
    pub fn list(&self) -> Result<Vec<Entry>, UdfError> {
        let root = self.root()?;
        let mut entries = Vec::new();
        // `root` is a content handle; `collect_handle` reads it and frees it.
        // SAFETY: `root` is a live content-iteration dirent.
        unsafe { collect_handle(root, "", &mut entries) };
        Ok(entries)
    }

    /// Extract the entire image into `dest`, recreating its directory tree.
    /// `on_progress(done, total)` reports cumulative file bytes; `should_cancel`
    /// returning `true` aborts with [`UdfError::Cancelled`].
    pub fn extract_all(
        &self,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, u64),
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> Result<(), UdfError> {
        let entries = self.list()?;
        let total: u64 = entries.iter().filter(|entry| !entry.is_dir).map(|entry| entry.len).sum();

        for directory in entries.iter().filter(|entry| entry.is_dir) {
            std::fs::create_dir_all(dest.join(&directory.path))?;
        }

        let mut done = 0_u64;
        for file_entry in entries.iter().filter(|entry| !entry.is_dir) {
            if should_cancel() {
                return Err(UdfError::Cancelled);
            }

            let target = dest.join(&file_entry.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let file = self
                .open_file(&file_entry.path)?
                .ok_or_else(|| UdfError::Read(format!("file vanished: {}", file_entry.path)))?;

            let mut output = std::io::BufWriter::new(std::fs::File::create(&target)?);
            let base = done;
            file.read_to(
                &mut output,
                &mut |file_bytes| on_progress(base + file_bytes, total),
                should_cancel,
            )?;
            output.flush()?;
            done += file_entry.len;
        }

        Ok(())
    }
}

impl Drop for UdfImage {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `udf_open` and is closed exactly once.
        unsafe {
            udf_close(self.raw);
        }
    }
}

/// A listed entry.
pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub len: u64,
}

/// An open file within a [`UdfImage`].
pub struct UdfFile<'a> {
    raw: *mut UdfDirent,
    _image: std::marker::PhantomData<&'a UdfImage>,
}

impl UdfFile<'_> {
    pub fn len(&self) -> u64 {
        // SAFETY: `self.raw` is live.
        unsafe { udf_get_file_length(self.raw) }
    }

    /// Stream the whole file to `out`. `udf_read_block` returns at most one
    /// extent per call and advances the image position, so we loop until the
    /// declared length is reached (the multi-extent path a >4 GiB `install.wim`
    /// exercises).
    // The u64 to usize casts below are bounded: `want_blocks <= CHUNK_BLOCKS`
    // and `take <= want_blocks * UDF_BLOCKSIZE <= buffer.len()`.
    #[allow(clippy::cast_possible_truncation)]
    pub fn read_to(
        &self,
        out: &mut impl Write,
        on_progress: &mut dyn FnMut(u64),
        should_cancel: &mut dyn FnMut() -> bool,
    ) -> Result<u64, UdfError> {
        let total = self.len();
        // SAFETY: offset 0 is block-aligned and within the file.
        unsafe {
            udf_setpos(self.raw, 0);
        }

        let mut buffer = vec![0u8; CHUNK_BLOCKS * UDF_BLOCKSIZE];
        let mut written = 0_u64;

        while written < total {
            if should_cancel() {
                return Err(UdfError::Cancelled);
            }

            let remaining = total - written;
            // Bounded by CHUNK_BLOCKS, so this fits a usize on any target.
            let want_blocks = remaining
                .div_ceil(UDF_BLOCKSIZE as u64)
                .min(CHUNK_BLOCKS as u64) as usize;

            // SAFETY: `buffer` holds CHUNK_BLOCKS * UDF_BLOCKSIZE bytes and
            // `want_blocks <= CHUNK_BLOCKS`.
            let read = unsafe {
                udf_read_block(self.raw, buffer.as_mut_ptr().cast::<c_void>(), want_blocks)
            };
            let read = match usize::try_from(read) {
                Ok(bytes) if bytes > 0 => bytes,
                _ => {
                    return Err(UdfError::Read(format!(
                        "udf_read_block returned {read} at offset {written}"
                    )));
                }
            };

            // The final extent is block-padded; never write past the real length.
            let take = (read as u64).min(remaining) as usize;
            out.write_all(&buffer[..take])?;
            written += take as u64;
            on_progress(written);
        }

        Ok(written)
    }
}

impl Drop for UdfFile<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `udf_fopen` and is freed exactly once.
        unsafe {
            udf_dirent_free(self.raw);
        }
    }
}

/// Recursively collect a directory content-handle into `out`. `udf_readdir`
/// advances and ultimately frees `handle`, so ownership is transferred in.
///
/// SAFETY: `handle` must be a live content-iteration dirent.
unsafe fn collect_handle(handle: *mut UdfDirent, prefix: &str, out: &mut Vec<Entry>) {
    loop {
        // Advances and returns `handle`, or returns NULL and frees it.
        let entry = unsafe { udf_readdir(handle) };
        if entry.is_null() {
            break;
        }

        let name = unsafe { filename(entry) };
        // Skip the self/parent entry (empty file identifier).
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }

        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let is_dir = unsafe { udf_is_dir(entry) };
        let len = unsafe { udf_get_file_length(entry) };
        out.push(Entry {
            path: path.clone(),
            is_dir,
            len,
        });

        if is_dir {
            // `entry` (== `handle`) now carries a FID, so `udf_opendir` accepts
            // it and returns a fresh content handle for the subdirectory.
            let child = unsafe { udf_opendir(entry) };
            if !child.is_null() {
                unsafe { collect_handle(child, &path, out) };
            }
        }
    }
}

/// SAFETY: `dirent` must be live.
unsafe fn filename(dirent: *mut UdfDirent) -> String {
    let ptr = unsafe { udf_get_filename(dirent) };
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
    }
}
