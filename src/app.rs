//! egui application shell for the Wi-Fi scanner.
//!
//! This module owns presentation state: filters, visible series, interaction
//! state, and the small amount of persisted UI configuration. The scan data
//! model and aggregation live in [`crate::scanner`].

use eframe::{CreationContext, Frame, Storage};
use egui::{RichText, Ui};
use egui_plot::{Line, Plot, PlotPoints, PlotUi};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::scanner::{
    DEFAULT_SCAN_INTERVAL, PlotSeries, SIGNAL_CEILING_DBM, SIGNAL_FLOOR_DBM, ScanSnapshot, Scanner,
    spawn_scanner_worker,
};
const STORAGE_SSID_FILTER: &str = "ssid_filter";
const STORAGE_SHOW_HEATMAP: &str = "show_heatmap";
const STORAGE_PAUSED: &str = "paused";
const EXPORT_FILENAME: &str = "wifi_scan_export.csv";
const MIN_PLOT_WINDOW_SECS: f64 = 5.0;
const MIN_SIGNAL_WINDOW_DBM: f64 = 10.0;
const SERIES_PANEL_WIDTH: f32 = 180.0;
const MIN_PLOT_HEIGHT: f32 = 300.0;

/// Native egui app that visualizes Wi-Fi scan snapshots.
pub struct ScannerApp {
    scanner: Scanner,
    receiver: Receiver<ScanSnapshot>,
    settings: AppSettings,
    interaction: PlotInteraction,
    frame_stats: FrameStats,
    export_status: Option<String>,
}

#[derive(Default)]
struct AppSettings {
    ssid_filter: String,
    show_heatmap: bool,
    paused: bool,
}

#[derive(Default)]
struct PlotInteraction {
    hovered_point: Option<HoveredPoint>,
    selected_ssid: Option<String>,
    hidden_ssids: HashSet<String>,
}

struct FrameStats {
    last_frame_time: Duration,
    frame_times: Vec<f32>,
}

impl ScannerApp {
    /// Create the app and restore persisted UI settings when storage is enabled.
    pub fn new(cc: &CreationContext<'_>) -> Self {
        let mut app = Self::default();

        if let Some(storage) = cc.storage {
            app.settings.ssid_filter = storage.get_string(STORAGE_SSID_FILTER).unwrap_or_default();
            app.settings.show_heatmap = parse_stored_bool(storage, STORAGE_SHOW_HEATMAP);
            app.settings.paused = parse_stored_bool(storage, STORAGE_PAUSED);
        }

        app
    }

    fn apply_new_snapshots(&mut self) {
        while let Ok(mut snapshot) = self.receiver.try_recv() {
            if self.settings.paused {
                continue;
            }

            self.scanner.ingest_snapshot(snapshot);
        }
    }

    fn export_csv(&mut self) {
        self.export_status = Some(match self.scanner.export_to_csv(EXPORT_FILENAME) {
            Ok(()) => format!("Exported to {EXPORT_FILENAME}"),
            Err(error) => format!("Export failed: {error}"),
        });
    }

    fn show_summary(&self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            metric(ui, "Networks", self.scanner.networks_count());
            metric(ui, "Snapshots", self.scanner.snapshots_count());
            metric(ui, "Memory KB", self.scanner.estimated_memory_kb());
            ui.label(format!(
                "Frame: {:.1} ms",
                self.frame_stats.last_frame_time.as_secs_f32() * 1000.0
            ));

            if let Some(best) = self.scanner.best_signal() {
                ui.separator();
                ui.colored_label(
                    egui::Color32::GOLD,
                    format!("Best: {} ({} dBm)", best.ssid, best.signal_level),
                );
            }
        });
    }

    fn show_filter_bar(&mut self, ui: &mut Ui) {
        let previous_filter = self.settings.ssid_filter.clone();

        ui.horizontal_wrapped(|ui| {
            ui.label("SSID filter");
            ui.add_sized(
                [220.0, 22.0],
                egui::TextEdit::singleline(&mut self.settings.ssid_filter)
                    .hint_text("substring"),
            );

            if ui.button("Clear").clicked() {
                self.settings.ssid_filter.clear();
            }

            if ui.button("Export CSV").clicked() {
                self.export_csv();
            }

            if let Some(status) = &self.export_status {
                ui.label(status);
            }
        });
    }

    fn show_scan_controls(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.settings.paused, "Pause");
            ui.checkbox(&mut self.settings.show_heatmap, "Heatmap");
        });
    }

    fn show_heatmap(&self, ui: &mut Ui) {
        if !self.settings.show_heatmap {
            return;
        }

        let averages = self.scanner.channel_averages();
        if averages.is_empty() {
            return;
        }

        egui::CollapsingHeader::new("Channel heatmap")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("channel_heatmap")
                    .num_columns(4)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for (index, average) in averages.iter().enumerate() {
                            ui.label(format!("Ch {}", average.channel));
                            ui.colored_label(
                                signal_color(average.signal_level),
                                format!("{:.1} dBm", average.signal_level),
                            );

                            if index % 2 == 1 {
                                ui.end_row();
                            }
                        }
                    });
            });
    }

    fn show_signal_plot(&mut self, ui: &mut Ui) {
        let series = self
            .scanner
            .plot_series()
            .into_iter()
            .filter(|s| {
                self.settings.ssid_filter.is_empty()
                    || s.ssid
                    .to_lowercase()
                    .contains(&self.settings.ssid_filter.to_lowercase())
            })
            .collect::<Vec<_>>();
        self.prune_hidden_series(&series);
        let visible_series = series
            .iter()
            .filter(|item| !self.interaction.hidden_ssids.contains(&item.ssid))
            .collect::<Vec<_>>();
        let selected_ssid = self.interaction.selected_ssid.clone();
        let spacing = ui.spacing().item_spacing.x;
        let plot_height = ui.available_height().max(MIN_PLOT_HEIGHT);

        ui.horizontal(|ui| {
            let plot_width = (ui.available_width() - SERIES_PANEL_WIDTH - spacing).max(1.0);
            let plot_response = ui
                .allocate_ui_with_layout(
                    egui::vec2(plot_width, plot_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        Plot::new("wifi_signals")
                            .width(plot_width)
                            .height(plot_height)
                            .x_axis_label("Time (seconds ago)")
                            .y_axis_label("Signal strength (dBm)")
                            .default_x_bounds(
                                -(self.scanner.max_history_age().as_secs() as f64),
                                0.0,
                            )
                            .default_y_bounds(SIGNAL_FLOOR_DBM, SIGNAL_CEILING_DBM)
                            .allow_zoom(true)
                            .allow_drag(true)
                            .allow_scroll(true)
                            .show(ui, |plot_ui| {
                                let mut normal_series = visible_series
                                    .iter()
                                    .copied()
                                    .filter(|item| selected_ssid.as_ref() != Some(&item.ssid))
                                    .collect::<Vec<_>>();
                                let selected_series = visible_series
                                    .iter()
                                    .copied()
                                    .filter(|item| selected_ssid.as_ref() == Some(&item.ssid));
                                normal_series.extend(selected_series);

                                for item in normal_series {
                                    let is_selected = selected_ssid.as_ref() == Some(&item.ssid);
                                    let points = PlotPoints::new(item.points.clone());

                                    plot_ui.line(
                                        Line::new(&item.ssid, points)
                                            .color(if is_selected {
                                                egui::Color32::WHITE
                                            } else {
                                                ssid_color(&item.ssid)
                                            })
                                            .width(if is_selected { 3.0 } else { 1.5 }),
                                    );
                                }

                                clamp_plot_bounds(plot_ui, self.scanner.max_history_age());

                                let hovered = plot_ui.pointer_coordinate().and_then(|coord| {
                                    nearest_point(&visible_series, coord.x, coord.y)
                                });
                                let clicked_ssid = if plot_ui.response().clicked() {
                                    hovered.as_ref().map(|point| point.ssid.clone())
                                } else {
                                    None
                                };

                                (hovered, clicked_ssid)
                            })
                    },
                )
                .inner;

            let (hovered, clicked_ssid) = plot_response.inner;
            self.interaction.hovered_point = plot_response
                .response
                .hovered()
                .then_some(hovered)
                .flatten();

            if let Some(ssid) = clicked_ssid {
                self.interaction.selected_ssid = Some(ssid);
            }

            if let Some(point) = self.interaction.hovered_point.clone() {
                plot_response.response.on_hover_ui_at_pointer(|ui| {
                    ui.label(format!("SSID: {}", point.ssid));
                    ui.label(format!("Signal: {} dBm", point.signal));
                    ui.label(format!("Time: {:.1}s ago", -point.x));
                    ui.label(format!("Value: {:.1}", point.y));
                });
            }

            self.show_series_legend(ui, &series, plot_height);
        });
    }

    fn show_series_legend(&mut self, ui: &mut Ui, series: &[PlotSeries], height: f32) {
        ui.allocate_ui_with_layout(
            egui::vec2(SERIES_PANEL_WIDTH, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(SERIES_PANEL_WIDTH);
                ui.label(RichText::new("Series").strong());
                egui::ScrollArea::vertical()
                    .max_height(height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for item in series {
                            let mut visible = !self.interaction.hidden_ssids.contains(&item.ssid);
                            let color = ssid_color(&item.ssid);

                            ui.horizontal(|ui| {
                                ui.colored_label(color, "--");
                                if ui.checkbox(&mut visible, &item.ssid).changed() {
                                    if visible {
                                        self.interaction.hidden_ssids.remove(&item.ssid);
                                    } else {
                                        self.interaction.hidden_ssids.insert(item.ssid.clone());
                                        if self.interaction.selected_ssid.as_ref()
                                            == Some(&item.ssid)
                                        {
                                            self.interaction.selected_ssid = None;
                                        }
                                    }
                                }
                            });
                        }
                    });
            },
        );
    }

    fn prune_hidden_series(&mut self, series: &[PlotSeries]) {
        let known_ssids = series
            .iter()
            .map(|item| item.ssid.as_str())
            .collect::<HashSet<_>>();
        self.interaction
            .hidden_ssids
            .retain(|ssid| known_ssids.contains(ssid.as_str()));
    }
}

#[derive(Clone)]
struct HoveredPoint {
    ssid: String,
    x: f64,
    y: f64,
    signal: i32,
}

fn clamp_plot_bounds(plot_ui: &mut PlotUi<'_>, max_history_age: Duration) {
    let max_history = max_history_age.as_secs_f64();
    let bounds = plot_ui.plot_bounds();
    let min = bounds.min();
    let max = bounds.max();

    let x_width = (max[0] - min[0]).clamp(MIN_PLOT_WINDOW_SECS, max_history);
    let x_center = ((min[0] + max[0]) / 2.0).clamp(-max_history + x_width / 2.0, -x_width / 2.0);
    plot_ui.set_plot_bounds_x((x_center - x_width / 2.0)..=(x_center + x_width / 2.0));

    let y_full_height = SIGNAL_CEILING_DBM - SIGNAL_FLOOR_DBM;
    let y_height = (max[1] - min[1]).clamp(MIN_SIGNAL_WINDOW_DBM, y_full_height);
    let y_center = ((min[1] + max[1]) / 2.0).clamp(
        SIGNAL_FLOOR_DBM + y_height / 2.0,
        SIGNAL_CEILING_DBM - y_height / 2.0,
    );
    plot_ui.set_plot_bounds_y((y_center - y_height / 2.0)..=(y_center + y_height / 2.0));
}
impl Default for ScannerApp {
    fn default() -> Self {
        Self {
            scanner: Scanner::default(),
            receiver: spawn_scanner_worker(DEFAULT_SCAN_INTERVAL),
            settings: AppSettings::default(),
            interaction: PlotInteraction::default(),
            frame_stats: FrameStats::default(),
            export_status: None,
        }
    }
}

impl eframe::App for ScannerApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut Frame) {
        let frame_start = Instant::now();
        self.apply_new_snapshots();

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.vertical(|ui| {
                ui.heading("Wi-Fi Scanner");
                self.show_summary(ui);
                ui.separator();
                self.show_filter_bar(ui);
                self.show_scan_controls(ui);
                self.show_heatmap(ui);
                ui.separator();
                self.show_signal_plot(ui);
            });
        });

        self.frame_stats.record(frame_start.elapsed());
        ui.ctx().request_repaint_after(DEFAULT_SCAN_INTERVAL);
    }

    fn save(&mut self, storage: &mut dyn Storage) {
        storage.set_string(STORAGE_SSID_FILTER, self.settings.ssid_filter.clone());
        storage.set_string(STORAGE_SHOW_HEATMAP, self.settings.show_heatmap.to_string());
        storage.set_string(STORAGE_PAUSED, self.settings.paused.to_string());
    }
}

impl FrameStats {
    fn record(&mut self, elapsed: Duration) {
        self.last_frame_time = elapsed;
        self.frame_times.push(elapsed.as_secs_f32());

        if self.frame_times.len() > 100 {
            self.frame_times.remove(0);
        }
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            last_frame_time: Duration::ZERO,
            frame_times: Vec::new(),
        }
    }
}

fn metric(ui: &mut Ui, label: &str, value: usize) {
    ui.label(RichText::new(format!("{label}: {value}")).monospace());
}

fn nearest_point(series: &[&PlotSeries], pointer_x: f64, pointer_y: f64) -> Option<HoveredPoint> {
    series
        .iter()
        .flat_map(|item| {
            item.points.iter().map(move |point| {
                let dx = point[0] - pointer_x;
                let dy = (point[1] - pointer_y) / 10.0;
                let distance = dx * dx + dy * dy;
                (
                    distance,
                    HoveredPoint {
                        ssid: item.ssid.clone(),
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

fn ssid_color(ssid: &str) -> egui::Color32 {
    let hash = stable_hash64(ssid.as_bytes());
    let hue = (hash as f32) / (u64::MAX as f32);

    egui::Color32::from_rgb(
        (hue * 255.0) as u8,
        ((hue + 0.33) % 1.0 * 255.0) as u8,
        ((hue + 0.66) % 1.0 * 255.0) as u8,
    )
}

fn signal_color(signal_level: f64) -> egui::Color32 {
    let normalized = ((signal_level - SIGNAL_FLOOR_DBM) / (SIGNAL_CEILING_DBM - SIGNAL_FLOOR_DBM))
        .clamp(0.0, 1.0);

    egui::Color32::from_rgb(
        (255.0 * (1.0 - normalized)) as u8,
        (255.0 * normalized) as u8,
        0,
    )
}

fn stable_hash64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn parse_stored_bool(storage: &dyn Storage, key: &str) -> bool {
    storage
        .get_string(key)
        .and_then(|value| value.parse().ok())
        .unwrap_or(false)
}
