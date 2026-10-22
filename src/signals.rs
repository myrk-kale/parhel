//! SIGINT/SIGTERM/SIGHUP handling: the handler only records the signal; the `add`
//! transaction polls it between steps and rolls back.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn record(signal: libc::c_int) {
    // Only async-signal-safe operations: atomic stores.
    SIGNAL.store(signal, Ordering::SeqCst);
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Install the handlers and return the flag they raise.
#[must_use]
pub fn install() -> &'static AtomicBool {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `record` is async-signal-safe; zeroed sigaction is a valid base.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&raw mut action.sa_mask);
            libc::sigaction(signal, &raw const action, std::ptr::null_mut());
        }
    }
    &INTERRUPTED
}

/// The last signal received, if any.
pub fn received() -> Option<i32> {
    match SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}
