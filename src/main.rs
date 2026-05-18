//! Native entry point for the Wi-Fi scanner.

mod app;
mod scanner;

use crate::app::ScannerApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default(),
        ..Default::default()
    };

    eframe::run_native(
        "Wi-fi scanner",
        options,
        Box::new(|cc| Ok(Box::new(ScannerApp::new(cc)))),
    )
}
