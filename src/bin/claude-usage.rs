#![windows_subsystem = "windows"]

use eframe::egui;

use claude_usage::ui::App;

fn main() -> eframe::Result {
    claude_usage::install_crypto_provider();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Claude Usage Monitor")
            .with_app_id("claude-usage")
            .with_inner_size([560.0, 620.0])
            .with_min_inner_size([360.0, 400.0])
            .with_resizable(true),
        // Remember window size and position (the default; stated so it stays on).
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "Claude Usage Monitor",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
