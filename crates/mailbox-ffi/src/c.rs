//! The C ABI the SwiftUI app links (§3).
//!
//! Strings crossing this boundary are UTF-8, heap-allocated with the Rust
//! allocator, and freed by [`mailbox_string_free`]. A NULL `args_json` is an
//! empty object. Failures are JSON documents, never a bare English sentence
//! (§6.4) — the same contract the CLI prints to stderr.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use mailbox_api::{ErrorCode, ErrorResponse};

use crate::Mailbox;

fn c_str<'a>(p: *const c_char) -> Result<&'a str, String> {
    if p.is_null() {
        return Ok("");
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|e| format!("argument is not UTF-8: {e}"))
}

fn to_c_string(s: impl Into<String>) -> *mut c_char {
    let s = s.into().replace('\0', "");
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

fn error_json(code: ErrorCode, message: impl Into<String>) -> *mut c_char {
    let body = ErrorResponse::new(code, message);
    match serde_json::to_string(&body) {
        Ok(s) => to_c_string(s),
        Err(_) => to_c_string(
            "{\"error\":{\"code\":\"failed\",\"message\":\"json\",\"retryable\":false}}",
        ),
    }
}

/// Opens a mailbox rooted at `config_path` / `state_dir`.
///
/// # Safety
///
/// `config_path` and `state_dir` must be valid C strings or NULL. `error_out`,
/// if non-NULL, receives a heap string the caller frees with
/// [`mailbox_string_free`].
#[no_mangle]
pub unsafe extern "C" fn mailbox_open(
    config_path: *const c_char,
    state_dir: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut Mailbox {
    if !error_out.is_null() {
        unsafe { *error_out = ptr::null_mut() };
    }
    let config = match c_str(config_path) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            write_error(error_out, "config_path is required");
            return ptr::null_mut();
        }
        Err(e) => {
            write_error(error_out, e);
            return ptr::null_mut();
        }
    };
    let state = match c_str(state_dir) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            write_error(error_out, "state_dir is required");
            return ptr::null_mut();
        }
        Err(e) => {
            write_error(error_out, e);
            return ptr::null_mut();
        }
    };
    match Mailbox::open(config, state) {
        Ok(mb) => Box::into_raw(Box::new(mb)),
        Err(e) => {
            write_error(error_out, e.to_string());
            ptr::null_mut()
        }
    }
}

fn write_error(error_out: *mut *mut c_char, message: impl Into<String>) {
    if !error_out.is_null() {
        unsafe { *error_out = to_c_string(message.into()) };
    }
}

/// # Safety
///
/// `handle` must be a pointer from [`mailbox_open`], or NULL.
#[no_mangle]
pub unsafe extern "C" fn mailbox_close(handle: *mut Mailbox) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// # Safety
///
/// `handle` must be a live pointer from [`mailbox_open`]. `method` must be a
/// valid C string. `args_json` may be NULL.
#[no_mangle]
pub unsafe extern "C" fn mailbox_exec(
    handle: *mut Mailbox,
    method: *const c_char,
    args_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        return error_json(ErrorCode::Usage, "mailbox handle is null");
    }
    let method = match c_str(method) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return error_json(ErrorCode::Usage, "method is required"),
        Err(e) => return error_json(ErrorCode::Usage, e),
    };
    let args = match c_str(args_json) {
        Ok(s) => s,
        Err(e) => return error_json(ErrorCode::Usage, e),
    };
    let mb = unsafe { &*handle };
    match mb.exec(method, args) {
        Ok(s) => to_c_string(s),
        Err(e) => match serde_json::to_string(&e) {
            Ok(s) => to_c_string(s),
            Err(_) => error_json(ErrorCode::Failed, "could not serialise the error"),
        },
    }
}

/// # Safety
///
/// `s` must be a pointer from this ABI, or NULL.
#[no_mangle]
pub unsafe extern "C" fn mailbox_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}
