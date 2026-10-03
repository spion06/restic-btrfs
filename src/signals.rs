//! Deferred termination signals for the consistency window.
//!
//! Between the first pre-hook and the last post-hook the process must not die:
//! a pre-hook may have quiesced a database or service that only the post-hook
//! thaws. While a [`Deferred`] guard is alive SIGINT/SIGTERM/SIGHUP only set a
//! flag; the caller checks it at safe points and winds down after the post-hooks
//! have run.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const DEFERRED: [Signal; 3] = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP];

extern "C" fn on_signal(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// While alive, termination signals are recorded instead of killing the process.
pub struct Deferred {
    previous: Vec<(Signal, SigAction)>,
}

impl Deferred {
    pub fn install() -> Result<Self> {
        INTERRUPTED.store(false, Ordering::SeqCst);
        let action = SigAction::new(SigHandler::Handler(on_signal), SaFlags::empty(), SigSet::empty());
        let mut previous = Vec::new();
        for sig in DEFERRED {
            // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
            let old = unsafe { sigaction(sig, &action) }
                .with_context(|| format!("installing {sig} handler"))?;
            previous.push((sig, old));
        }
        Ok(Self { previous })
    }

    /// Has a deferred signal arrived since [`Deferred::install`]?
    pub fn interrupted(&self) -> bool {
        INTERRUPTED.load(Ordering::SeqCst)
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        for (sig, old) in &self.previous {
            // SAFETY: restores the handler that was installed before us.
            let _ = unsafe { sigaction(*sig, old) };
        }
    }
}
