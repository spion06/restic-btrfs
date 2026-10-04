//! Routes the `log` records that rustic_core emits to stderr.
//!
//! rustic_core reports things like unreadable files and skipped entries only
//! through `log`; without a logger they vanish. Warnings and errors are printed in
//! rbtrfs' usual `rbtrfs: warning:` form and counted.

use std::sync::atomic::{AtomicUsize, Ordering};

use log::{Level, LevelFilter, Log, Metadata, Record};

static WARNINGS: AtomicUsize = AtomicUsize::new(0);

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

#[cfg(test)]
mod tests {
    #[test]
    fn warnings_from_the_backup_engine_are_counted() {
        super::init();
        let before = super::warnings();
        log::warn!("something odd");
        log::info!("not counted");
        assert_eq!(super::warnings(), before + 1);
    }
}
