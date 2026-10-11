#![windows_subsystem = "windows"]

use claude_usage::paths::Paths;
use claude_usage::ui::{window, App};

fn main() -> eframe::Result {
    // No console under windows_subsystem: make panics and startup failures
    // visible (crash.log in the data dir, plus a message box on Windows).
    claude_usage::crash::install_panic_hook(Paths::platform_default());
    claude_usage::install_crypto_provider();
    // The first window gets our own saved geometry; eframe's window
    // persistence is off (see ui::window).
    let settings = window::load_settings_early();
    let viewport = window::viewport_builder(&settings, window::PlaceBy::for_platform());
    let ours = viewport.clone();
    let options = eframe::NativeOptions {
        viewport,
        persist_window: false,
        // eframe still applies a stale "window" key from older builds (until
        // App::logic removes it); our geometry wins.
        window_builder: Some(Box::new(move |b| window::keep_our_geometry(b, &ours))),
        ..Default::default()
    };
    let result = eframe::run_native(
        window::TITLE,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    );
    if let Err(e) = &result {
        claude_usage::crash::report(
            &Paths::platform_default(),
            &format!("Claude Usage Monitor could not start: {e}"),
        );
    }
    result
}
