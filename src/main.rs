use crate::app::ScannerApp;

mod app;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default(),
        ..Default::default()
    };

    eframe::run_native(
        "Wi-fi scanner",
        options,
        Box::new(|_cc| Ok(Box::new(ScannerApp::default()))),
    )
}
