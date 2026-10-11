//! Toast delivery on Windows: the app's notification identity and the
//! notifier thread.
//!
//! [`register`] (once, in `main` before `run_native`) writes the per-user
//! key `HKCU\Software\Classes\AppUserModelId\ClaudeUsageMonitor` with the
//! display name and an icon, and tags the process with that app id, so
//! toasts show under "Claude Usage Monitor". When that fails the toasts use
//! PowerShell's app id instead (they then read "Windows PowerShell").
//!
//! [`spawn_notifier`] owns toast creation on its own thread (`Toast` is not
//! `Send`), fed by a channel, so the UI never waits on WinRT.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

use crate::analytics::alerts::AlertCopy;
use crate::paths::Paths;

use super::tray;

/// The app id the toasts are sent under.
pub const AUMID: &str = "ClaudeUsageMonitor";
/// The name Windows shows on the toasts and in its notification settings.
pub const DISPLAY_NAME: &str = "Claude Usage Monitor";
/// The toast icon, written into the data directory.
pub const ICON_FILE: &str = "app-icon.png";
/// The toast icon's size in pixels.
pub const ICON_SIZE: u32 = 256;

/// Which identity toasts are sent under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppId {
    /// Our registered [`AUMID`].
    Registered,
    /// PowerShell's, which needs no registration.
    Fallback,
}

/// Called when a toast is clicked (while the app runs).
pub type OnClick = Arc<dyn Fn() + Send + Sync>;

/// The registry key under `HKEY_CURRENT_USER` that names our app id.
pub fn aumid_key_path() -> String {
    format!("Software\\Classes\\AppUserModelId\\{AUMID}")
}

/// The REG_SZ values of [`aumid_key_path`]: the display name, and the icon
/// when there is one.
pub fn aumid_values(icon: Option<&Path>) -> Vec<(&'static str, String)> {
    let mut values = vec![("DisplayName", DISPLAY_NAME.to_owned())];
    if let Some(icon) = icon {
        values.push(("IconUri", icon.display().to_string()));
    }
    values
}

/// Where the toast icon is written.
pub fn icon_path(paths: &Paths) -> PathBuf {
    paths.data_dir().join(ICON_FILE)
}

/// The app icon ([`tray::render_app_icon_rgba`]) as a PNG.
pub fn app_icon_png() -> Vec<u8> {
    let rgba = tray::render_app_icon_rgba(ICON_SIZE);
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, ICON_SIZE, ICON_SIZE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // Writing into a Vec with a matching buffer size cannot fail.
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&rgba).expect("png data");
    writer.finish().expect("png finish");
    out
}

/// Write the toast icon into the data directory unless it is already there
/// as is; returns its path.
pub fn write_icon(paths: &Paths) -> io::Result<PathBuf> {
    let path = icon_path(paths);
    let png = app_icon_png();
    if std::fs::read(&path).ok().as_deref() != Some(png.as_slice()) {
        std::fs::create_dir_all(paths.data_dir())?;
        std::fs::write(&path, png)?;
    }
    Ok(path)
}

/// Register our toast identity and tag the process with it; on failure,
/// log once to the crash log and use the fallback identity.
pub fn register(paths: &Paths) -> AppId {
    match imp::register(paths) {
        Ok(()) => AppId::Registered,
        Err(e) => {
            let _ = crate::crash::append(
                paths,
                chrono::Local::now(),
                &format!("Notifications use the PowerShell app id: {e}"),
            );
            AppId::Fallback
        }
    }
}

/// Start the notifier thread; each [`AlertCopy`] sent becomes one toast.
/// `on_click` runs (on a system thread) when a toast is clicked while the
/// app runs.
pub fn spawn_notifier(app: AppId, on_click: OnClick) -> Sender<AlertCopy> {
    let (tx, rx) = channel::<AlertCopy>();
    let spawned = std::thread::Builder::new()
        .name("notifier".to_owned())
        .spawn(move || {
            for copy in rx {
                imp::show(app, &copy, on_click.clone());
            }
        });
    // No thread: sends fail quietly, nothing is shown.
    drop(spawned);
    tx
}

#[cfg(windows)]
mod imp {
    use tauri_winrt_notification::{Duration, Toast};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    use super::{aumid_key_path, aumid_values, write_icon, AlertCopy, AppId, OnClick, AUMID};
    use crate::paths::Paths;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn register(paths: &Paths) -> Result<(), String> {
        // Without an icon the toasts still work, with a generic glyph.
        let icon = write_icon(paths).ok();
        write_key(&aumid_key_path(), &aumid_values(icon.as_deref()))
            .map_err(|code| format!("registering the app id failed (error {code})"))?;
        let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(wide(AUMID).as_ptr()) };
        if hr < 0 {
            return Err(format!("tagging the process failed (HRESULT {hr:#010x})"));
        }
        Ok(())
    }

    /// Create (or open) `HKCU\<sub>` and set each REG_SZ value.
    fn write_key(sub: &str, values: &[(&str, String)]) -> Result<(), u32> {
        let sub = wide(sub);
        let mut hkey: HKEY = std::ptr::null_mut();
        let err = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                sub.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            )
        };
        if err != ERROR_SUCCESS {
            return Err(err);
        }
        let mut result = Ok(());
        for (name, value) in values {
            let (name, value) = (wide(name), wide(value));
            let err = unsafe {
                RegSetValueExW(
                    hkey,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                )
            };
            if err != ERROR_SUCCESS {
                result = Err(err);
                break;
            }
        }
        unsafe { RegCloseKey(hkey) };
        result
    }

    pub fn show(app: AppId, copy: &AlertCopy, on_click: OnClick) {
        let id = match app {
            AppId::Registered => AUMID,
            AppId::Fallback => Toast::POWERSHELL_APP_ID,
        };
        // A failed toast is dropped: alerts are best effort and must not
        // nag with dialogs.
        let _ = Toast::new(id)
            .title(&copy.title)
            .text1(&copy.body)
            .duration(Duration::Short)
            .on_activated(move |_| {
                on_click();
                Ok(())
            })
            .show();
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{AlertCopy, AppId, OnClick};
    use crate::paths::Paths;

    pub fn register(_paths: &Paths) -> Result<(), String> {
        Err("toasts are Windows-only".to_owned())
    }

    pub fn show(_app: AppId, _copy: &AlertCopy, _on_click: OnClick) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_key_and_values() {
        assert_eq!(
            aumid_key_path(),
            r"Software\Classes\AppUserModelId\ClaudeUsageMonitor"
        );
        assert_eq!(
            aumid_values(None),
            [("DisplayName", "Claude Usage Monitor".to_owned())]
        );
        let icon = Path::new(r"C:\Users\x\AppData\Roaming\claude_usage_tui\app-icon.png");
        assert_eq!(
            aumid_values(Some(icon)),
            [
                ("DisplayName", "Claude Usage Monitor".to_owned()),
                ("IconUri", icon.display().to_string()),
            ]
        );
    }

    #[test]
    fn app_icon_png_decodes_to_the_rendered_icon() {
        let png = app_icon_png();
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (ICON_SIZE, ICON_SIZE));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(buf, tray::render_app_icon_rgba(ICON_SIZE));
    }

    #[test]
    fn write_icon_writes_once_and_keeps_an_identical_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_data_dir(dir.path().join("data"));
        let path = write_icon(&paths).unwrap();
        assert_eq!(path, dir.path().join("data").join(ICON_FILE));
        assert_eq!(std::fs::read(&path).unwrap(), app_icon_png());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_icon(&paths).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        // A stale icon is replaced.
        std::fs::write(&path, b"old").unwrap();
        write_icon(&paths).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), app_icon_png());
    }
}
