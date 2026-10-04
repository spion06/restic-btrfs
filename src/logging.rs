//! Routes the `log` records that rustic_core emits to stderr.
//!
//! rustic_core reports things like unreadable files and skipped entries only
//! through `log`; without a logger they vanish. Warnings and errors are printed in
//! rbtrfs' usual `rbtrfs: warning:` form and counted.

use std::sync::atomic::{AtomicUsize, Ordering};

use log::{Level, LevelFilter, Log, Metadata, Record};

static WARNINGS: AtomicUsize = AtomicUsize::new(0);
static VANISHED: AtomicUsize = AtomicUsize::new(0);
static UNREADABLE: AtomicUsize = AtomicUsize::new(0);

/// Why the backup engine left something out, from its warning text. rustic_core
/// reports every skipped entry as `ignoring error: ...`.
#[derive(Debug, PartialEq, Eq)]
enum Skip {
    /// The file was gone by the time it was read: normal for a live directory.
    Vanished,
    /// Anything else (permissions, I/O errors, ...): data is missing from the backup.
    Unreadable,
}

fn classify(message: &str) -> Option<Skip> {
    let rest = message.strip_prefix("ignoring error")?;
    Some(
        if rest.contains("No such file or directory") || rest.contains("os error 2)") {
            Skip::Vanished
        } else {
            Skip::Unreadable
        },
    )
}

struct Logger;

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= Level::Warn
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        WARNINGS.fetch_add(1, Ordering::Relaxed);
        match classify(&r.args().to_string()) {
            Some(Skip::Vanished) => VANISHED.fetch_add(1, Ordering::Relaxed),
            Some(Skip::Unreadable) => UNREADABLE.fetch_add(1, Ordering::Relaxed),
            None => 0,
        };
        let label = if r.level() == Level::Error {
            "error"
        } else {
            "warning"
        };
        eprintln!("rbtrfs: {label}: {}", r.args());
    }

    fn flush(&self) {}
}

/// Install the logger. Safe to call more than once.
pub fn init() {
    let _ = log::set_logger(&Logger);
    log::set_max_level(LevelFilter::Warn);
}

/// Warnings and errors logged so far by the backup engine.
pub fn warnings() -> usize {
    WARNINGS.load(Ordering::Relaxed)
}

/// Entries skipped because they disappeared while the backup ran.
pub fn vanished() -> usize {
    VANISHED.load(Ordering::Relaxed)
}

/// Entries skipped for any other reason: they are missing from the backup.
pub fn unreadable() -> usize {
    UNREADABLE.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    #[test]
    fn skipped_entries_are_told_apart() {
        use super::{classify, Skip};
        assert_eq!(
            classify("ignoring error: No such file or directory (os error 2)"),
            Some(Skip::Vanished)
        );
        assert_eq!(
            classify("ignoring error: Permission denied (os error 13)"),
            Some(Skip::Unreadable)
        );
        assert_eq!(classify("error determining backup size"), None);
    }

    #[test]
    fn warnings_from_the_backup_engine_are_counted() {
        super::init();
        let before = super::warnings();
        log::warn!("something odd");
        log::info!("not counted");
        assert_eq!(super::warnings(), before + 1);
    }
}
