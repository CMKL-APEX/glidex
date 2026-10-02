//! Linux-PAM, loaded at run time.
//!
//! Why `libloading` instead of linking: build hosts and CI have only the
//! runtime `libpam.so.0`, no headers or `libpam.so` dev symlink. The few
//! types and functions glidex-authd needs are declared here from
//! `<security/pam_appl.h>` / `<security/_pam_types.h>`.
//!
//! All of the crate's `unsafe` lives in this module.

use crate::authenticator::{AuthFailure, Authenticator};
use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

pub const DEFAULT_LIBRARY: &str = "libpam.so.0";

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
const PAM_SILENT: c_int = 0x8000;
const PAM_DISALLOW_NULL_AUTHTOK: c_int = 0x0001;
const PAM_MAX_NUM_MSG: c_int = 32;

/// Opaque `pam_handle_t`.
#[repr(C)]
struct PamHandle {
    _private: [u8; 0],
}

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

type ConvFn = unsafe extern "C" fn(
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

type PamStartFn =
    unsafe extern "C" fn(*const c_char, *const c_char, *const PamConv, *mut *mut PamHandle) -> c_int;
type PamFlagsFn = unsafe extern "C" fn(*mut PamHandle, c_int) -> c_int;
type PamEndFn = unsafe extern "C" fn(*mut PamHandle, c_int) -> c_int;
type PamStrerrorFn = unsafe extern "C" fn(*mut PamHandle, c_int) -> *const c_char;

#[derive(Debug, thiserror::Error)]
#[error("cannot load {library}: {message}")]
pub struct LoadError {
    pub library: String,
    pub message: String,
}

/// `libpam.so.0` and the entry points glidex-authd calls.
pub struct Pam {
    start: PamStartFn,
    authenticate: PamFlagsFn,
    acct_mgmt: PamFlagsFn,
    end: PamEndFn,
    strerror: PamStrerrorFn,
    // Keeps the function pointers above valid; dropped last.
    _lib: libloading::Library,
}

impl std::fmt::Debug for Pam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pam")
    }
}

impl Pam {
    /// Load the system's `libpam.so.0`.
    pub fn load_default() -> Result<Self, LoadError> {
        Self::load(Path::new(DEFAULT_LIBRARY))
    }

    /// Load PAM from `library` (a path, or a soname for the dynamic loader).
    pub fn load(library: &Path) -> Result<Self, LoadError> {
        let err = |what: &str, e: libloading::Error| LoadError {
            library: library.display().to_string(),
            message: format!("{what}: {e}"),
        };
        // SAFETY: loading libpam runs its ELF initializers, which only set up
        // internal state; it has no other load-time side effects.
        let lib = unsafe { libloading::Library::new(library) }.map_err(|e| err("dlopen", e))?;
        // SAFETY: each symbol is declared with the signature from
        // <security/pam_appl.h>. The copied fn pointers stay valid while `lib`
        // is loaded, and `lib` lives in the same struct.
        unsafe {
            macro_rules! sym {
                ($ty:ty, $name:literal) => {
                    *lib.get::<$ty>($name).map_err(|e| err($name.to_str().unwrap_or("?"), e))?
                };
            }
            let start = sym!(PamStartFn, c"pam_start");
            let authenticate = sym!(PamFlagsFn, c"pam_authenticate");
            let acct_mgmt = sym!(PamFlagsFn, c"pam_acct_mgmt");
            let end = sym!(PamEndFn, c"pam_end");
            let strerror = sym!(PamStrerrorFn, c"pam_strerror");
            Ok(Pam {
                start,
                authenticate,
                acct_mgmt,
                end,
                strerror,
                _lib: lib,
            })
        }
    }

    fn describe(&self, pamh: *mut PamHandle, code: c_int) -> String {
        // SAFETY: pam_strerror accepts any code and returns a static string
        // (or NULL); `pamh` is a live handle from pam_start.
        let s = unsafe { (self.strerror)(pamh, code) };
        if s.is_null() {
            return format!("PAM error {code}");
        }
        // SAFETY: non-NULL pam_strerror results are NUL-terminated.
        format!("{} ({code})", unsafe { CStr::from_ptr(s) }.to_string_lossy())
    }
}

/// What the conversation function answers with.
struct ConvData {
    user: CString,
    password: Zeroizing<Vec<u8>>, // NUL-terminated
}

/// `strdup` a NUL-terminated byte string with `malloc`, as PAM frees it.
fn malloc_copy(bytes_with_nul: &[u8]) -> *mut c_char {
    // SAFETY: malloc of a non-zero size; the copy stays within both buffers.
    unsafe {
        let p = libc::malloc(bytes_with_nul.len()) as *mut c_char;
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(bytes_with_nul.as_ptr() as *const c_char, p, bytes_with_nul.len());
        }
        p
    }
}

/// Free a response array built by [`conversation`] after a failure,
/// wiping each answer first (one of them may be the password).
///
/// SAFETY: `resp` is a `calloc`'d array of `n` responses whose `resp`
/// fields are NULL or `malloc`'d NUL-terminated strings.
unsafe fn free_responses(resp: *mut PamResponse, n: usize) {
    for i in 0..n {
        let r = &mut *resp.add(i);
        if !r.resp.is_null() {
            let len = libc::strlen(r.resp);
            std::slice::from_raw_parts_mut(r.resp as *mut u8, len).zeroize();
            libc::free(r.resp as *mut c_void);
            r.resp = std::ptr::null_mut();
        }
    }
    libc::free(resp as *mut c_void);
}

/// The PAM conversation: answers password prompts with the password and
/// echoed prompts with the user name; informational messages get no answer.
///
/// SAFETY: called by libpam with Linux-PAM's convention (`msg` is an array
/// of `num_msg` pointers) and `appdata_ptr` pointing at the `ConvData` that
/// [`PamAuthenticator::authenticate`] keeps alive for the whole PAM
/// transaction.
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    let run = || {
        if num_msg <= 0 || num_msg > PAM_MAX_NUM_MSG || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
            return PAM_CONV_ERR;
        }
        let n = num_msg as usize;
        let data = &*(appdata_ptr as *const ConvData);
        let out = libc::calloc(n, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
        if out.is_null() {
            return PAM_BUF_ERR;
        }
        for i in 0..n {
            let m = *msg.add(i);
            if m.is_null() {
                free_responses(out, n);
                return PAM_CONV_ERR;
            }
            let answer = match (*m).msg_style {
                PAM_PROMPT_ECHO_OFF => Some(data.password.as_slice()),
                PAM_PROMPT_ECHO_ON => Some(data.user.as_bytes_with_nul()),
                PAM_ERROR_MSG | PAM_TEXT_INFO => {
                    if !(*m).msg.is_null() {
                        let text = CStr::from_ptr((*m).msg).to_string_lossy();
                        tracing::debug!(message = %text, "PAM message");
                    }
                    None
                }
                _ => {
                    free_responses(out, n);
                    return PAM_CONV_ERR;
                }
            };
            if let Some(bytes) = answer {
                let p = malloc_copy(bytes);
                if p.is_null() {
                    free_responses(out, n);
                    return PAM_BUF_ERR;
                }
                (*out.add(i)).resp = p;
            }
        }
        // libpam owns `out` and every `resp` in it from here on.
        *resp = out;
        PAM_SUCCESS
    };
    // Unwinding across the C boundary is undefined; nothing above should
    // panic, but fail the conversation rather than abort if it does.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or(PAM_CONV_ERR)
}

/// [`Authenticator`] backed by the host's PAM stack:
/// `pam_start(service)`, `pam_authenticate`, `pam_acct_mgmt`, `pam_end`.
#[derive(Debug)]
pub struct PamAuthenticator {
    pam: Pam,
}

impl PamAuthenticator {
    pub fn new(pam: Pam) -> Self {
        Self { pam }
    }
}

impl Authenticator for PamAuthenticator {
    fn authenticate(&self, service: &str, user: &str, password: &str) -> Result<(), AuthFailure> {
        let service = CString::new(service).map_err(|_| AuthFailure::Internal("service name contains NUL".into()))?;
        let user_c = CString::new(user).map_err(|_| AuthFailure::Denied("user name contains NUL".into()))?;
        if password.as_bytes().contains(&0) {
            return Err(AuthFailure::Denied("password contains NUL".into()));
        }
        let mut pw = Zeroizing::new(Vec::with_capacity(password.len() + 1));
        pw.extend_from_slice(password.as_bytes());
        pw.push(0);
        let data = ConvData {
            user: user_c.clone(),
            password: pw,
        };
        let conv = PamConv {
            conv: conversation,
            appdata_ptr: &data as *const ConvData as *mut c_void,
        };
        let mut pamh: *mut PamHandle = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated strings and a pam_conv that outlive the
        // transaction (`data` and `conv` are dropped after pam_end below).
        let rc = unsafe { (self.pam.start)(service.as_ptr(), user_c.as_ptr(), &conv, &mut pamh) };
        if rc != PAM_SUCCESS || pamh.is_null() {
            if !pamh.is_null() {
                // SAFETY: a handle returned by pam_start is ended exactly once.
                unsafe { (self.pam.end)(pamh, rc) };
            }
            return Err(AuthFailure::Internal(format!("pam_start failed ({rc})")));
        }
        let flags = PAM_SILENT | PAM_DISALLOW_NULL_AUTHTOK;
        // SAFETY: `pamh` is live until pam_end.
        let mut rc = unsafe { (self.pam.authenticate)(pamh, flags) };
        let mut step = "pam_authenticate";
        if rc == PAM_SUCCESS {
            // SAFETY: as above.
            rc = unsafe { (self.pam.acct_mgmt)(pamh, flags) };
            step = "pam_acct_mgmt";
        }
        let result = if rc == PAM_SUCCESS {
            Ok(())
        } else {
            Err(AuthFailure::Denied(format!("{step}: {}", self.pam.describe(pamh, rc))))
        };
        // SAFETY: ends the handle exactly once; libpam frees it.
        unsafe { (self.pam.end)(pamh, rc) };
        // `conv` and `data` (with the password, zeroized) drop only here.
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_library_is_a_clean_error() {
        let e = Pam::load(Path::new("/nonexistent/libpam-glidex-test.so.0")).unwrap_err();
        assert!(e.to_string().contains("/nonexistent/libpam-glidex-test.so.0"), "{e}");
    }

    #[test]
    fn library_without_pam_symbols_is_a_clean_error() {
        let e = Pam::load(Path::new("libc.so.6")).unwrap_err();
        assert!(e.message.contains("pam_start"), "{e}");
    }

    #[test]
    fn loads_system_libpam_when_present() {
        if !Path::new("/usr/lib/x86_64-linux-gnu/libpam.so.0").exists() && !Path::new("/usr/lib64/libpam.so.0").exists() {
            return;
        }
        Pam::load_default().expect("libpam.so.0 should load");
    }

    fn call_conv(data: &ConvData, styles: &[c_int]) -> (c_int, Vec<Option<Vec<u8>>>) {
        let texts: Vec<CString> = styles.iter().map(|_| CString::new("Prompt: ").unwrap()).collect();
        let msgs: Vec<PamMessage> = styles
            .iter()
            .zip(&texts)
            .map(|(s, t)| PamMessage { msg_style: *s, msg: t.as_ptr() })
            .collect();
        let mut ptrs: Vec<*const PamMessage> = msgs.iter().map(|m| m as *const PamMessage).collect();
        let mut out: *mut PamResponse = std::ptr::null_mut();
        // SAFETY: same contract libpam follows; we free the result like libpam.
        unsafe {
            let rc = conversation(
                ptrs.len() as c_int,
                ptrs.as_mut_ptr(),
                &mut out,
                data as *const ConvData as *mut c_void,
            );
            if rc != PAM_SUCCESS {
                return (rc, vec![]);
            }
            let mut answers = vec![];
            for i in 0..styles.len() {
                let r = &*out.add(i);
                answers.push(if r.resp.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr(r.resp).to_bytes().to_vec())
                });
            }
            free_responses(out, styles.len());
            (rc, answers)
        }
    }

    #[test]
    fn conversation_answers_prompts() {
        let data = ConvData {
            user: CString::new("alice").unwrap(),
            password: Zeroizing::new(b"s3cret\0".to_vec()),
        };
        let (rc, answers) = call_conv(&data, &[PAM_TEXT_INFO, PAM_PROMPT_ECHO_ON, PAM_PROMPT_ECHO_OFF, PAM_ERROR_MSG]);
        assert_eq!(rc, PAM_SUCCESS);
        assert_eq!(answers, vec![None, Some(b"alice".to_vec()), Some(b"s3cret".to_vec()), None]);
        let (rc, _) = call_conv(&data, &[PAM_PROMPT_ECHO_OFF, 99]);
        assert_eq!(rc, PAM_CONV_ERR);
        let (rc, _) = call_conv(&data, &[]);
        assert_eq!(rc, PAM_CONV_ERR);
    }
}
