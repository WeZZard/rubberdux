use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// A logger that writes to a file.
struct FileLogger {
    file: Mutex<File>,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record) {
        let mut file = self.file.lock().unwrap();
        let _ = writeln!(
            file,
            "[{}] {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {
        let mut file = self.file.lock().unwrap();
        let _ = file.flush();
    }
}

/// Initialize file-based log capture for tests.
/// Call once at the start of a test.
pub fn init(log_path: &Path) {
    let file = File::create(log_path).expect("failed to create log file");
    let logger = Box::new(FileLogger {
        file: Mutex::new(file),
    });
    // Only set logger if not already set (to avoid panics in concurrent tests)
    let _ = log::set_boxed_logger(logger);
    log::set_max_level(log::LevelFilter::Debug);
}
