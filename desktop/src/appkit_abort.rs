//! What AppKit said before it took the process, in the log rather than nowhere.
//!
//! **An uncaught Objective-C exception aborts, and this app is the daemon.** It
//! takes the window, the host and every session with it, and the Rust panic hook
//! never runs because nothing panicked — so `orchd.log` ends mid-line with no
//! record that anything went wrong. #14 is the report that shape produces: a
//! person who can say what they clicked and nothing else, and a maintainer with
//! one AppKit selector's worth of guesswork.
//!
//! The handler cannot prevent the abort and does not try. AppKit calls it, we
//! write one line naming the exception and its reason, and the process goes down
//! exactly as it would have. **What that buys is a named cause in the file the
//! app already tells people to send** — and the drag guard in `main.rs` is the
//! proof that it is worth having: that one took reading tao's vendored source to
//! find, because the crash itself said nothing at all.
//!
//! **The one `unsafe` boundary is named here**, which is this workspace's rule for
//! opting out of `unsafe_code = deny`: `NSSetUncaughtExceptionHandler` takes a
//! bare function pointer, the handler receives a raw `NSException *`, and chaining
//! to a previously installed handler means calling through a pointer whose type
//! only the C declaration knows. Everything read off the exception —
//! `name`, `reason`, `callStackSymbols` — is safe in `objc2-foundation`.
#![allow(unsafe_code)]

use objc2_foundation::{NSException, NSSetUncaughtExceptionHandler, NSUncaughtExceptionHandler};
use std::sync::atomic::{AtomicPtr, Ordering};

/// The C signature AppKit calls the handler with: `void (*)(NSException *)`.
///
/// Spelled out because `objc2-foundation` types the slot as an opaque
/// `NSUncaughtExceptionHandler` (a `c_void`), so both the install and the chain
/// below are casts and the real shape has to be written down once.
type Handler = extern "C-unwind" fn(*mut NSException);

/// Whoever held the slot before us, called once we have had our say.
///
/// **Chained rather than replaced.** Something else in this process may already
/// want to know — a crash reporter, a framework — and a diagnostic that silences
/// another diagnostic is a poor trade.
static PREVIOUS: AtomicPtr<NSUncaughtExceptionHandler> = AtomicPtr::new(std::ptr::null_mut());

/// Install the handler. Call once, early, after the logger exists.
pub(crate) fn log_uncaught_exceptions() {
    // Safe in `objc2-foundation`: it reads a global slot and returns either null
    // or whatever was stored there. What is *not* safe is calling through it, and
    // that is at the bottom of `log_and_pass_on` with its own note.
    let previous = objc2_foundation::NSGetUncaughtExceptionHandler();
    PREVIOUS.store(previous, Ordering::SeqCst);
    let ours: Handler = log_and_pass_on;
    // SAFETY: the argument must be a valid pointer or null. It is a `'static`
    // function pointer of the declared signature, cast to the opaque slot type.
    unsafe { NSSetUncaughtExceptionHandler(ours as *mut NSUncaughtExceptionHandler) };
    tracing::debug!("an uncaught AppKit exception will now name itself in the log");
}

/// Write the exception down, then let whoever held the slot before us have it.
///
/// **No allocation the log does not already make, and no unwinding.** This runs on
/// the way to `abort`, so it does the least that gets the sentence into the file:
/// `logging::LogFile` opens the file per write and buffers nothing, which is why
/// a line written from here survives the process ending a moment later.
extern "C-unwind" fn log_and_pass_on(exception: *mut NSException) {
    // SAFETY: AppKit passes the exception it is about to die on, which is a live
    // `NSException` for the duration of this call. Borrowed, never retained past
    // it. A null would be `@throw nil`, which is legal and says nothing.
    if let Some(e) = unsafe { exception.as_ref() } {
        let reason = e.reason().map(|r| r.to_string()).unwrap_or_default();
        /* The stack is the half that names *our* frame rather than AppKit's, and
        it is the difference between "an invalid argument somewhere" and the
        selector that raised. Joined onto one line: a log this app ships is read
        by whoever is pasting it into an issue, not tailed live. */
        let stack: Vec<String> = e.callStackSymbols().iter().map(|s| s.to_string()).collect();
        tracing::error!(
            "an uncaught AppKit exception is taking the process down: {} — {reason}\n{}",
            e.name(),
            stack.join("\n")
        );
    } else {
        tracing::error!("an uncaught AppKit exception with no object is taking the process down");
    }
    let previous = PREVIOUS.load(Ordering::SeqCst);
    if !previous.is_null() {
        // SAFETY: non-null here means `NSGetUncaughtExceptionHandler` returned it,
        // so it is a handler AppKit stored and has the signature the C header
        // declares. Calling it is what not installing ours would have done.
        let previous: Handler = unsafe { std::mem::transmute(previous) };
        previous(exception);
    }
}
