use eframe::{CreationContext, Frame, Storage};
use egui::Ui;
use egui_plot::{Legend, Line, Plot, PlotPoints};
use std::cmp::Ordering;
use std::hash::Hasher;
use std::hash::{DefaultHasher, Hash};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::scanner::{
    DEFAULT_UPDATE_TIME, MAX_WIFI_STRENGTH, MIN_WIFI_STRENGTH, ScanSnapshot, Scanner,
    spawn_scanner_worker,
};

pub struct ScannerApp {
    scanner: Scanner,
    receiver: Receiver<ScanSnapshot>,
    hovered_point: Option<HoveredPoint>,
    selected_ssid: Option<String>,
    ssid_filter: String,
    export_status: Option<String>,
    history_layer: usize,
    auto_capture: bool,
    capture_timer: f32,
    show_heatmap: bool,
    paused: bool,
    last_frame_time: Duration,
    frame_times: Vec<f32>,
}

#[derive(Clone)]
struct HoveredPoint {
    ssid: String,
    x: f64,
    y: f64,
    signal: i32,
}

impl ScannerApp {
    const STORAGE_SSID_FILTER: &'static str = "ssid_filter";
    const STORAGE_AUTO_CAPTURE: &'static str = "auto_capture";
    const STORAGE_SHOW_HEATMAP: &'static str = "show_heatmap";
    const STORAGE_PAUSED: &'static str = "paused";

    pub fn new(cc: &CreationContext<'_>) -> Self {
        let mut app = Self::default();

        if let Some(storage) = cc.storage {
            app.ssid_filter = storage
                .get_string(Self::STORAGE_SSID_FILTER)
                .unwrap_or_default();
            app.auto_capture = parse_stored_bool(storage, Self::STORAGE_AUTO_CAPTURE);
            app.show_heatmap = parse_stored_bool(storage, Self::STORAGE_SHOW_HEATMAP);
            app.paused = parse_stored_bool(storage, Self::STORAGE_PAUSED);
        }

        app
    }

    //todo!(Rewrite without relying on DefaultHasher's unspecified output.)
    fn ssid_to_color32(ssid: &str) -> egui::Color32 {
        let mut hasher = DefaultHasher::new();
        ssid.hash(&mut hasher);
        let hash = hasher.finish();
        let hue = (hash as f32) / (u64::MAX as f32);

        egui::Color32::from_rgb(
            (hue * 255.0) as u8,
            ((hue + 0.33) % 1.0 * 255.0) as u8,
            ((hue + 0.66) % 1.0 * 255.0) as u8,
        )
    }

    fn apply_new_snapshots(&mut self) {
        while let Ok(mut snapshot) = self.receiver.try_recv() {
            if self.paused {
                continue;
            }

            if !self.ssid_filter.is_empty() {
                let filter = self.ssid_filter.to_lowercase();
                snapshot
                    .data
                    .retain(|sample| sample.ssid.to_lowercase().contains(&filter));
            }

            self.scanner.ingest_snapshot(snapshot);
        }
    }

    fn nearest_point(
        plots: &[(String, Vec<[f64; 2]>)],
        pointer_x: f64,
        pointer_y: f64,
    ) -> Option<HoveredPoint> {
        plots
            .iter()
            .flat_map(|(ssid, points)| {
                points.iter().map(move |point| {
                    let dx = point[0] - pointer_x;
                    let dy = (point[1] - pointer_y) / 10.0;
                    let distance = dx * dx + dy * dy;
                    (
                        distance,
                        HoveredPoint {
                            ssid: ssid.clone(),
                            x: point[0],
                            y: point[1],
                            signal: point[1].round() as i32,
                        },
                    )
                })
            })
            .min_by(|(left, _), (right, _)| left.partial_cmp(right).unwrap_or(Ordering::Equal))
            .map(|(_, point)| point)
    }

    fn export_csv(&mut self) {
        let filename = "wifi_scan_export.csv";
        self.export_status = Some(match self.scanner.export_to_csv(filename) {
            Ok(()) => format!("Exported to {filename}"),
            Err(error) => format!("Export failed: {error}"),
        });
    }

    fn update_auto_capture(&mut self, ui: &Ui) {
        if !self.auto_capture {
            return;
        }

        self.capture_timer += ui.ctx().input(|input| input.unstable_dt);
        if self.capture_timer >= 5.0 {
            self.scanner.capture_history_snapshot();
            self.capture_timer = 0.0;
        }
    }

    fn draw_history_controls(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("History:");
            let max_layer = self.scanner.get_history_layers_count();
            if self.history_layer > max_layer {
                self.history_layer = max_layer;
            }

            ui.add(egui::Slider::new(&mut self.history_layer, 0..=max_layer).text("layer"));

            if ui.button("Capture now").clicked() {
                self.scanner.capture_history_snapshot();
                self.history_layer = self.scanner.get_history_layers_count();
            }

            ui.checkbox(&mut self.auto_capture, "Auto");
            ui.checkbox(&mut self.paused, "Pause scanning");
            ui.checkbox(&mut self.show_heatmap, "Heatmap");
        });
    }

    fn draw_summary(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("Networks: {}", self.scanner.get_networks_count()));
            ui.label(format!("Snapshots: {}", self.scanner.get_snapshots_count()));
            ui.label(format!(
                "Frame: {:.1} ms",
                self.last_frame_time.as_secs_f32() * 1000.0
            ));
            ui.label(format!("Memory: ~{} KB", self.scanner.estimate_memory_kb()));

            if let Some((ssid, signal)) = self.scanner.get_best_signal() {
                ui.colored_label(egui::Color32::GOLD, format!("Best: {ssid} ({signal} dBm)"));
            }
        });
    }

    fn draw_heatmap(&self, ui: &mut Ui) {
        if !self.show_heatmap {
            return;
        }

        let averages = self.scanner.get_channel_averages();
        if averages.is_empty() {
            return;
        }

        ui.separator();
        egui::Grid::new("channel_heatmap")
            .num_columns(4)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for (index, (channel, average)) in averages.iter().enumerate() {
                    let normalized = ((*average - MAX_WIFI_STRENGTH)
                        / (MIN_WIFI_STRENGTH - MAX_WIFI_STRENGTH))
                        .clamp(0.0, 1.0);
                    let color = egui::Color32::from_rgb(
                        (255.0 * normalized) as u8,
                        (255.0 * (1.0 - normalized)) as u8,
                        0,
                    );

                    ui.label(format!("Ch {channel}"));
                    ui.colored_label(color, format!("{average:.1} dBm"));

                    if index % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
        ui.separator();
    }

    fn record_frame_time(&mut self, elapsed: Duration) {
        self.last_frame_time = elapsed;
        self.frame_times.push(elapsed.as_secs_f32());

        if self.frame_times.len() > 100 {
            self.frame_times.remove(0);
        }
    }
}

impl Default for ScannerApp {
    fn default() -> Self {
        Self {
            scanner: Scanner::default(),
            receiver: spawn_scanner_worker(DEFAULT_UPDATE_TIME),
            hovered_point: None,
            selected_ssid: None,
            ssid_filter: String::new(),
            export_status: None,
            history_layer: 0,
            auto_capture: false,
            capture_timer: 0.0,
            show_heatmap: false,
            paused: false,
            last_frame_time: Duration::ZERO,
            frame_times: Vec::new(),
        }
    }
}

impl eframe::App for ScannerApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut Frame) {
        let frame_start = Instant::now();
        self.apply_new_snapshots();

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.update_auto_capture(ui);
            self.draw_summary(ui);

            ui.horizontal(|ui| {
                ui.label("Filter SSID:");
                ui.text_edit_singleline(&mut self.ssid_filter);

                if ui.button("Clear").clicked() {
                    self.ssid_filter.clear();
                }

                if ui.button("Export CSV").clicked() {
                    self.export_csv();
                }

                if let Some(status) = &self.export_status {
                    ui.label(status);
                }
            });

            self.draw_history_controls(ui);
            self.draw_heatmap(ui);

            let plots = self.scanner.get_plots_with_history(self.history_layer);
            let selected_ssid = self.selected_ssid.clone();
            let plot_response = Plot::new("wifi_signals")
                .x_axis_label("Time (seconds ago)")
                .y_axis_label("Signal strength (dBm)")
                .legend(Legend::default())
                .include_x(-(self.scanner.get_max_timestamp().as_secs() as f64))
                .include_x(0.0)
                .include_y(MIN_WIFI_STRENGTH)
                .include_y(MAX_WIFI_STRENGTH)
                .allow_zoom(true)
                .allow_drag(true)
                .allow_scroll(true)
                .show(ui, |plot_ui| {
                    for (ssid, points) in &plots {
                        let color = Self::ssid_to_color32(ssid);
                        let is_selected = selected_ssid.as_ref() == Some(ssid);
                        let points = PlotPoints::new(points.clone());

                        plot_ui.line(
                            Line::new(ssid, points)
                                .color(if is_selected {
                                    egui::Color32::WHITE
                                } else {
                                    color
                                })
                                .width(if is_selected { 3.0 } else { 1.5 }),
                        );
                    }

                    let hovered = plot_ui
                        .pointer_coordinate()
                        .and_then(|coord| Self::nearest_point(&plots, coord.x, coord.y));
                    let clicked_ssid = if plot_ui.response().clicked() {
                        hovered.as_ref().map(|point| point.ssid.clone())
                    } else {
                        None
                    };

                    (hovered, clicked_ssid)
                });

            let (hovered, clicked_ssid) = plot_response.inner;
            self.hovered_point = if plot_response.response.hovered() {
                hovered
            } else {
                None
            };

            if let Some(ssid) = clicked_ssid {
                self.selected_ssid = Some(ssid);
            }

            if let Some(point) = self.hovered_point.clone() {
                plot_response.response.on_hover_ui_at_pointer(|ui| {
                    ui.label(format!("SSID: {}", point.ssid));
                    ui.label(format!("Signal: {} dBm", point.signal));
                    ui.label(format!("Time: {:.1}s ago", -point.x));
                    ui.label(format!("Value: {:.1}", point.y));
                });
            }
        });

        self.record_frame_time(frame_start.elapsed());
        ui.ctx().request_repaint_after(DEFAULT_UPDATE_TIME);
    }

    fn save(&mut self, storage: &mut dyn Storage) {
        storage.set_string(Self::STORAGE_SSID_FILTER, self.ssid_filter.clone());
        storage.set_string(Self::STORAGE_AUTO_CAPTURE, self.auto_capture.to_string());
        storage.set_string(Self::STORAGE_SHOW_HEATMAP, self.show_heatmap.to_string());
        storage.set_string(Self::STORAGE_PAUSED, self.paused.to_string());
    }
}

fn parse_stored_bool(storage: &dyn Storage, key: &str) -> bool {
    storage
        .get_string(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(false)
}
