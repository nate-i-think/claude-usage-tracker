//! Making GUI crashes visible. The GUI binary runs without a console
//! (`windows_subsystem = "windows"`), so a panic or a startup failure would
//! otherwise exit silently. Each one is appended to `crash.log` in the data
//! directory and, on Windows, a panic that will take the app down is shown
//! in a message box. Panics inside [`catch_recoverable`] (the poller's guarded
//! regions) are logged only: the app keeps running.

use std::cell::Cell;
use std::fmt::Display;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::panic::{catch_unwind, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Local};

use crate::paths::Paths;

/// One crash-log entry: a local timestamp line, then the message.
pub fn format_entry(at: DateTime<Local>, message: &str) -> String {
    format!(
        "=== {} ===\n{}\n\n",
        at.format("%Y-%m-%d %H:%M:%S%.3f %:z"),
        message.trim_end()
    )
}

/// Append `message` to the crash log; returns the log's path.
pub fn append(paths: &Paths, at: DateTime<Local>, message: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(paths.data_dir())?;
    let file = paths.crash_log_file();
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)?
        .write_all(format_entry(at, message).as_bytes())?;
    Ok(file)
}

/// Log `message` and tell the user (a message box on Windows; stderr
/// elsewhere). Best effort: never panics.
pub fn report(paths: &Paths, message: &str) {
    let logged = append(paths, Local::now(), message);
    let text = match &logged {
        Ok(file) => format!("{message}\n\nDetails were written to {}", file.display()),
        Err(e) => format!("{message}\n\n(Could not write the crash log: {e})"),
    };
    show(&text);
}

/// Route panics to the crash log (with a backtrace), show a message box for
/// those that will take the app down, then call the previous hook.
pub fn install_panic_hook(paths: Paths) {
    static SHOWING: AtomicBool = AtomicBool::new(false);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        on_panic(
            &paths,
            info,
            &std::backtrace::Backtrace::force_capture(),
            &|text| show_once(&SHOWING, text, &show),
        );
        previous(info);
    }));
}

thread_local! {
    /// How many [`catch_recoverable`] regions this thread is inside.
    static RECOVERABLE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// `catch_unwind` that marks the region as recoverable, so a panic inside it
/// is logged but does not show the "crashed" box (the caller handles it and
/// the app keeps running). Use this instead of a bare `catch_unwind`.
pub fn catch_recoverable<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    struct Depth;
    impl Drop for Depth {
        fn drop(&mut self) {
            RECOVERABLE_DEPTH.with(|d| d.set(d.get() - 1));
        }
    }
    RECOVERABLE_DEPTH.with(|d| d.set(d.get() + 1));
    let _depth = Depth;
    catch_unwind(AssertUnwindSafe(f))
}

/// True inside a [`catch_recoverable`] region on this thread. The panic hook
/// runs on the panicking thread before unwinding, so it sees this.
pub fn is_recoverable() -> bool {
    RECOVERABLE_DEPTH.with(Cell::get) > 0
}

/// The hook body: always log; box only for panics that will not be caught.
/// `backtrace` is a parameter so tests can avoid real symbol resolution,
/// which holds std's global backtrace lock and stalls other panicking threads.
fn on_panic(paths: &Paths, info: &PanicHookInfo<'_>, backtrace: &dyn Display, show: &dyn Fn(&str)) {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("unnamed");
    let _ = append(
        paths,
        Local::now(),
        &format!("panic in thread '{name}': {info}\n\nbacktrace:\n{backtrace}"),
    );
    if let Some(text) = box_message(is_recoverable(), &info.to_string(), &paths.crash_log_file()) {
        show(&text);
    }
}

/// The message box text for a panic, or `None` when the panic is caught and
/// the app keeps running.
fn box_message(recoverable: bool, info: &str, log: &Path) -> Option<String> {
    (!recoverable).then(|| {
        format!(
            "Claude Usage Monitor crashed: {info}\n\nDetails were written to {}",
            log.display()
        )
    })
}

/// Call `show` unless a box guarded by `showing` is already up (a second
/// panic while the box is open would otherwise open another, or abort).
fn show_once(showing: &AtomicBool, text: &str, show: &dyn Fn(&str)) {
    if showing.swap(true, Ordering::SeqCst) {
        return;
    }
    show(text);
    showing.store(false, Ordering::SeqCst);
}

#[cfg(windows)]
fn show(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide("Claude Usage Monitor"));
    // SAFETY: both pointers are NUL-terminated UTF-16 buffers that outlive
    // the call; a null owner window is allowed.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show(text: &str) {
    eprintln!("{text}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::{Arc, Mutex};

    #[test]
    fn entries_are_timestamped_and_appended() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(tmp.path().join("data"));
        let at = Local.with_ymd_and_hms(2026, 10, 11, 9, 5, 7).unwrap();
        let file = append(&paths, at, "first\n").unwrap();
        append(&paths, at, "second").unwrap();
        assert_eq!(file, paths.crash_log_file());
        let text = fs::read_to_string(file).unwrap();
        assert!(text.starts_with("=== 2026-10-11 09:05:07.000 "), "{text}");
        assert_eq!(text.matches("=== 2026-10-11").count(), 2);
        assert!(text.contains("\nfirst\n\n===") && text.ends_with("\nsecond\n\n"));
    }

    #[test]
    fn only_unrecoverable_panics_get_a_box() {
        let log = Path::new("C:/data/crash.log");
        assert_eq!(box_message(true, "boom", log), None);
        let text = box_message(false, "boom", log).unwrap();
        assert!(
            text.contains("boom") && text.contains("crash.log"),
            "{text}"
        );
    }

    #[test]
    fn recoverable_flag_is_scoped_to_the_guarded_region() {
        assert!(!is_recoverable());
        let inner = catch_recoverable(|| {
            let nested = catch_recoverable(is_recoverable).unwrap();
            (nested, is_recoverable())
        });
        assert_eq!(inner.unwrap(), (true, true));
        assert!(!is_recoverable());
        let caught = catch_recoverable(|| -> () { panic!("guarded") });
        assert!(caught.is_err());
        assert!(!is_recoverable(), "flag reset after a caught panic");
    }

    #[test]
    fn box_is_not_reentrant() {
        let showing = AtomicBool::new(false);
        let calls = Cell::new(0);
        show_once(&showing, "outer", &|_| {
            calls.set(calls.get() + 1);
            // A second panic while the box is up must not open another.
            show_once(&showing, "inner", &|_| calls.set(calls.get() + 10));
        });
        assert_eq!(calls.get(), 1);
        // Once the box is closed, a later fatal panic can show one again.
        show_once(&showing, "later", &|_| calls.set(calls.get() + 100));
        assert_eq!(calls.get(), 101);
    }

    /// Runs the real hook body (logging plus box decision) with a recording
    /// box, for panics on one uniquely named thread only; every other panic
    /// in the test process goes to the previous hook unchanged.
    #[test]
    fn guarded_panic_is_logged_without_a_box() {
        const THREAD: &str = "crash-hook-test";
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(tmp.path().join("data"));
        let shown = Arc::new(Mutex::new(Vec::<String>::new()));
        let previous: Arc<dyn Fn(&PanicHookInfo<'_>) + Send + Sync> =
            Arc::from(std::panic::take_hook());
        {
            let (paths, shown, previous) = (paths.clone(), shown.clone(), previous.clone());
            std::panic::set_hook(Box::new(move |info| {
                if std::thread::current().name() == Some(THREAD) {
                    let shown = &shown;
                    on_panic(&paths, info, &"<test backtrace>", &|text| {
                        shown.lock().unwrap().push(text.into())
                    });
                } else {
                    previous(info);
                }
            }));
        }
        let result = std::thread::Builder::new()
            .name(THREAD.into())
            .spawn(|| {
                let caught = catch_recoverable(|| -> () { panic!("recovered-marker") });
                assert!(caught.is_err());
                panic!("fatal-marker");
            })
            .unwrap()
            .join();
        std::panic::set_hook(Box::new(move |info| previous(info)));

        assert!(result.is_err());
        let log = fs::read_to_string(paths.crash_log_file()).unwrap();
        assert!(log.contains("panic in thread 'crash-hook-test'"), "{log}");
        assert!(log.contains("recovered-marker") && log.contains("fatal-marker"));
        assert!(log.contains("backtrace:\n<test backtrace>"), "{log}");
        let shown = shown.lock().unwrap();
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].contains("fatal-marker"), "{shown:?}");
    }
}
