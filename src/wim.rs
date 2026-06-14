//! Hand-written FFI to libwim (wimlib) for splitting a Windows install image
//! into FAT32-sized `.swm` parts in-process, without bindgen. Dynamically
//! linked against the system `libwim`.
//!
//! Blocking and `!Send`; call from a blocking context.

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Opaque `WIMStruct` from wimlib.
enum WimStruct {}

/// `wimlib_progress_func_t`: enum return + enum/union/void* args, all int-sized
/// or pointers across the C ABI.
type ProgressFunc =
    extern "C" fn(msg_type: c_int, info: *mut c_void, progctx: *mut c_void) -> c_int;

// Values from wimlib.h.
const WIMLIB_ERR_SUCCESS: c_int = 0;
const WIMLIB_ERR_ABORTED_BY_PROGRESS: c_int = 76;
const WIMLIB_PROGRESS_STATUS_CONTINUE: c_int = 0;
const WIMLIB_PROGRESS_STATUS_ABORT: c_int = 1;

#[link(name = "wim")]
unsafe extern "C" {
    fn wimlib_global_init(init_flags: c_int) -> c_int;
    fn wimlib_open_wim(
        wim_file: *const c_char,
        open_flags: c_int,
        wim_ret: *mut *mut WimStruct,
    ) -> c_int;
    fn wimlib_split(
        wim: *mut WimStruct,
        swm_name: *const c_char,
        part_size: u64,
        write_flags: c_int,
    ) -> c_int;
    fn wimlib_free(wim: *mut WimStruct);
    fn wimlib_register_progress_function(
        wim: *mut WimStruct,
        progfunc: ProgressFunc,
        progctx: *mut c_void,
    );
}

/// Outcome of a split that the caller maps onto its own error set.
pub enum WimError {
    Cancelled,
    Failed(String),
}

/// Splits `input` into `install.swm`, `install2.swm`, ... next to `output`,
/// each under FAT32's 4 GiB-per-file limit. Cancels when `is_running` clears,
/// via wimlib's progress callback.
pub fn split(input: &Path, output: &Path, is_running: &AtomicBool) -> Result<(), WimError> {
    /// Just under FAT32's 4 GiB-per-file limit, for the fewest parts. Matches Rufus.
    const PART_SIZE: u64 = 4094 * 1024 * 1024;

    let input_c =
        CString::new(input.as_os_str().as_bytes()).map_err(|e| WimError::Failed(e.to_string()))?;
    let output_c = CString::new(output.as_os_str().as_bytes())
        .map_err(|e| WimError::Failed(e.to_string()))?;

    // SAFETY: the pointers are valid for the duration of each call, the strings
    // are NUL-terminated, and `is_running` outlives the split, so the context
    // pointer handed to the progress callback stays valid throughout.
    unsafe {
        if wimlib_global_init(0) != WIMLIB_ERR_SUCCESS {
            return Err(WimError::Failed("wimlib_global_init failed".to_owned()));
        }

        let mut wim: *mut WimStruct = std::ptr::null_mut();
        let open_rc = wimlib_open_wim(input_c.as_ptr(), 0, &raw mut wim);
        if open_rc != WIMLIB_ERR_SUCCESS {
            return Err(WimError::Failed(format!(
                "wimlib_open_wim failed (code {open_rc})"
            )));
        }

        wimlib_register_progress_function(
            wim,
            progress_callback,
            std::ptr::from_ref(is_running).cast_mut().cast::<c_void>(),
        );

        let split_rc = wimlib_split(wim, output_c.as_ptr(), PART_SIZE, 0);
        wimlib_free(wim);

        match split_rc {
            WIMLIB_ERR_SUCCESS => Ok(()),
            WIMLIB_ERR_ABORTED_BY_PROGRESS => Err(WimError::Cancelled),
            other => Err(WimError::Failed(format!(
                "wimlib_split failed (code {other})"
            ))),
        }
    }
}

/// Progress callback: aborts the split when the shared flag clears.
// The raw-pointer deref is intrinsic to the C callback ABI; the pointer is the
// `&AtomicBool` we registered and is valid for the call.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
extern "C" fn progress_callback(
    _msg_type: c_int,
    _info: *mut c_void,
    progctx: *mut c_void,
) -> c_int {
    // SAFETY: `progctx` is the `*const AtomicBool` registered in `split`.
    let is_running = unsafe { &*progctx.cast::<AtomicBool>() };
    if is_running.load(Ordering::SeqCst) {
        WIMLIB_PROGRESS_STATUS_CONTINUE
    } else {
        WIMLIB_PROGRESS_STATUS_ABORT
    }
}
