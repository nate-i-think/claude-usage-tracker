#![windows_subsystem = "windows"]

use eframe::egui;

struct App;

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.label("loading…");
    }
}

fn main() -> eframe::Result {
    claude_usage::install_crypto_provider();
    eframe::run_native(
        "Claude Usage Monitor",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(App))),
    )
}
