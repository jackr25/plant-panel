//! stderr logger. Under systemd (`JOURNAL_STREAM` set) each line gets a `<N>` syslog
//! priority prefix, which journald turns into the entry's level.

use std::io::Write;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger {
    journal: bool,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        // Dependencies only get to speak up about problems.
        m.level() <= log::max_level()
            && (m.level() <= Level::Warn || m.target().starts_with("plantd"))
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = if self.journal {
            let prio = match r.level() {
                Level::Error => 3,
                Level::Warn => 4,
                Level::Info => 6,
                Level::Debug | Level::Trace => 7,
            };
            writeln!(err, "<{prio}>{}", r.args())
        } else {
            writeln!(err, "{:<5} {}", r.level(), r.args())
        };
    }

    fn flush(&self) {}
}

/// Level from `PLANTD_LOG` (error, warn, info, debug, trace); default info.
pub fn init() {
    let level = std::env::var("PLANTD_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(LevelFilter::Info);
    let journal = std::env::var_os("JOURNAL_STREAM").is_some();
    if log::set_boxed_logger(Box::new(Logger { journal })).is_ok() {
        log::set_max_level(level);
    }
}
