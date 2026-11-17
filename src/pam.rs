//! Minimal PAM authentication via direct FFI to libpam.
//!
//! Only `pam_authenticate` is used, the same as swaylock. Each attempt opens a
//! fresh PAM transaction, so modules like pam_faillock see every attempt.

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::ptr;
use zeroize::Zeroizing;

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

const PAM_MAX_NUM_MSG: c_int = 32;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const PamConv,
        pamh: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(pamh: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut c_void, pam_status: c_int) -> c_int;
    fn pam_strerror(pamh: *mut c_void, errnum: c_int) -> *const c_char;
}

/// Passed to the conversation function through `appdata_ptr`.
struct ConvData {
    /// NUL-terminated password.
    password: *const c_char,
    /// PAM_ERROR_MSG / PAM_TEXT_INFO texts, e.g. pam_faillock's
    /// "The account is locked due to 3 failed logins."
    messages: Vec<String>,
}

/// Answers every prompt with the password and collects info / error messages.
extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    if num_msg <= 0 || num_msg > PAM_MAX_NUM_MSG || msg.is_null() || resp.is_null() {
        return PAM_CONV_ERR;
    }
    let n = num_msg as usize;
    if appdata_ptr.is_null() {
        return PAM_CONV_ERR;
    }
    let data = unsafe { &mut *(appdata_ptr as *mut ConvData) };
    let password = data.password;

    // PAM takes ownership of this array and of every `resp` string in it.
    let replies =
        unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) } as *mut PamResponse;
    if replies.is_null() {
        return PAM_BUF_ERR;
    }

    let free_all = |upto: usize| unsafe {
        for j in 0..upto {
            let r = (*replies.add(j)).resp;
            if !r.is_null() {
                libc::explicit_bzero(r as *mut c_void, libc::strlen(r));
                libc::free(r as *mut c_void);
            }
        }
        libc::free(replies as *mut c_void);
    };

    for i in 0..n {
        // Linux-PAM: `msg` is an array of pointers.
        let m = unsafe { *msg.add(i) };
        if m.is_null() {
            free_all(i);
            return PAM_CONV_ERR;
        }
        match unsafe { (*m).msg_style } {
            PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                let dup = unsafe { libc::strdup(password) };
                if dup.is_null() {
                    free_all(i);
                    return PAM_BUF_ERR;
                }
                unsafe { (*replies.add(i)).resp = dup };
            }
            PAM_ERROR_MSG | PAM_TEXT_INFO => {
                let text = unsafe { (*m).msg };
                if !text.is_null() {
                    let t = unsafe { CStr::from_ptr(text) }.to_string_lossy().trim().to_string();
                    if !t.is_empty() {
                        data.messages.push(t);
                    }
                }
            }
            _ => {
                free_all(i);
                return PAM_CONV_ERR;
            }
        }
    }

    unsafe { *resp = replies };
    PAM_SUCCESS
}

#[derive(Debug)]
pub enum AuthError {
    /// PAM said no. `messages` holds anything modules told the user
    /// (empty for a plain wrong password).
    Denied { reason: String, messages: Vec<String> },
    /// Could not even start PAM.
    System(String),
}

pub fn authenticate(service: &CStr, user: &CStr, password: &str) -> Result<(), AuthError> {
    // NUL-terminated copy that is wiped on drop.
    let mut buf: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::with_capacity(password.len() + 1));
    buf.extend_from_slice(password.as_bytes());
    if buf.contains(&0) {
        return Err(AuthError::Denied {
            reason: "password contains NUL byte".into(),
            messages: Vec::new(),
        });
    }
    buf.push(0);

    let mut data = ConvData { password: buf.as_ptr() as *const c_char, messages: Vec::new() };
    let conv = PamConv {
        conv: conversation,
        appdata_ptr: &mut data as *mut ConvData as *mut c_void,
    };

    let mut handle: *mut c_void = ptr::null_mut();
    let rc = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut handle) };
    if rc != PAM_SUCCESS || handle.is_null() {
        return Err(AuthError::System(format!("pam_start failed ({rc})")));
    }

    let rc = unsafe { pam_authenticate(handle, 0) };
    let result = if rc == PAM_SUCCESS {
        Ok(())
    } else {
        let msg = unsafe {
            let s = pam_strerror(handle, rc);
            if s.is_null() {
                format!("PAM error {rc}")
            } else {
                CStr::from_ptr(s).to_string_lossy().into_owned()
            }
        };
        Err(AuthError::Denied { reason: msg, messages: std::mem::take(&mut data.messages) })
    };
    unsafe { pam_end(handle, rc) };
    result
}

/// Name of the user running this process (from the passwd db, not $USER).
pub fn current_user() -> Option<CString> {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return None;
        }
        Some(CStr::from_ptr((*pw).pw_name).to_owned())
    }
}

/// Refuse to run if the PAM service file is missing: libpam would silently
/// fall back to the "other" service, which usually denies everything and
/// would leave the user unable to unlock.
pub fn service_exists(service: &str) -> bool {
    if service.contains('/') {
        return false;
    }
    ["/etc/pam.d", "/usr/lib/pam.d", "/lib/pam.d"]
        .iter()
        .any(|d| std::path::Path::new(d).join(service).is_file())
}
