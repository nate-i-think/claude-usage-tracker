//! Making GUI crashes visible. The GUI binary runs without a console
//! (`windows_subsystem = "windows"`), so a panic or a startup failure would
//! otherwise exit silently. Each one is appended to `crash.log` in the data
//! directory and, on Windows, shown in a message box.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

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

/// Route panics to [`report`] (with a backtrace in the log), then to the
/// previous hook.
pub fn install_panic_hook(paths: Paths) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let _ = append(
            &paths,
            Local::now(),
            &format!("panic in thread '{name}': {info}\n\nbacktrace:\n{backtrace}"),
        );
        show(&format!(
            "Claude Usage Monitor crashed: {info}\n\nDetails were written to {}",
            paths.crash_log_file().display()
        ));
        previous(info);
    }));
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
}
