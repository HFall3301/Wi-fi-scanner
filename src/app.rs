use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, SystemTime};
use eframe::Frame;
use egui::Ui;
use egui_plot::{Line, Legend, Plot, PlotPoints};
use wifi_scan::Wifi;

pub struct ScannerApp {
    last_update: SystemTime,
    max_timestamp: Duration,
    update_time: Duration,
    snap_shots: Vec<SnapShot>,
    last_plot_update: SystemTime,
}

impl Default for ScannerApp {
    fn default() -> Self {
        ScannerApp {
            last_update: SystemTime::now() - Duration::from_secs(10),
            max_timestamp: Duration::from_secs(300),
            update_time: Duration::from_millis(500),
            snap_shots: Vec::new(),
            last_plot_update: SystemTime::UNIX_EPOCH,
        }
    }
}

struct NetworkHistory {
    ssid: String,
    timestamps: Vec<SystemTime>,
    signal_levels: Vec<i32>,
}

impl NetworkHistory {
    fn append_history(&mut self, timestamp: SystemTime, signal_level: i32) {
        self.timestamps.push(timestamp);
        self.signal_levels.push(signal_level);
    }
}

struct SnapShot {
    timestamp: SystemTime,
    data: Vec<Wifi>,
}

impl ScannerApp {
    const MAX_WIFI_STRENGTH: f64 = -100.0;
    const MIN_WIFI_STRENGTH: f64 = -30.0;
    fn make_snapshot(&mut self) {
        self.last_update = SystemTime::now();

        let wifis = match wifi_scan::scan() {
            Ok(wifis) => wifis,
            Err(_) => return,
        };

        self.snap_shots.push(SnapShot {
            timestamp: self.last_update,
            data: wifis,
        });

        self.clear_history();
    }

    fn clear_history(&mut self) {
        let cutoff = SystemTime::now() - self.max_timestamp;
        self.snap_shots.retain(|snap_shot| {
            snap_shot.timestamp >= cutoff
        });
    }

    fn update_plots(&mut self) -> HashMap<String, NetworkHistory> {
        let now = SystemTime::now();
        self.last_plot_update = now;

        let mut history_map = HashMap::new();

        for snapshot in &self.snap_shots {
            for wifi in &snapshot.data {
                let history = history_map
                    .entry(wifi.ssid.clone())
                    .or_insert_with(|| NetworkHistory {
                        ssid: wifi.ssid.clone(),
                        timestamps: Vec::new(),
                        signal_levels: Vec::new(),
                    });

                history.append_history(snapshot.timestamp, wifi.signal_level);
            }
        }

        history_map
    }

    fn history_to_plot_points(history: &NetworkHistory, now: SystemTime) -> PlotPoints {
        let points: Vec<[f64; 2]> = history.timestamps
            .iter()
            .zip(history.signal_levels.iter())
            .filter_map(|(&timestamp, &signal)| {
                now.duration_since(timestamp)
                    .ok()
                    .map(|elapsed| [
                        -(elapsed.as_secs_f64()),
                        signal as f64
                    ])
            })
            .collect();

        PlotPoints::new(points)
    }
    //todo!(переписать этот метод без использования негарантированной структуры DefaultHasher)
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
}
impl eframe::App for ScannerApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut Frame) {
        let should_scan = self.last_update.elapsed().unwrap_or(Duration::ZERO) >= self.update_time;

        if should_scan {
            self.make_snapshot();
        }

        ui.ctx().request_repaint_after(self.update_time);

        let network_histories = self.update_plots();
        let now = SystemTime::now();

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Сетей: {}", network_histories.len()));
                ui.label(format!("Кадров сканирования: {}", self.snap_shots.len()));
            });

            Plot::new("wifi_signals")
                .x_axis_label("Время (секунд тому)")
                .y_axis_label("Сила сигнала (dBm)")
                .legend(Legend::default())
                .include_x(-(self.max_timestamp.as_secs() as f64))
                .include_x(0.0)
                .include_y(Self::MIN_WIFI_STRENGTH)
                .include_y(Self::MAX_WIFI_STRENGTH)
                .show(ui, |plot_ui| {
                    for history in network_histories.values() {
                        if history.timestamps.len() >= 2 {
                            let points = Self::history_to_plot_points(&history, now);
                            let color = Self::ssid_to_color32(&history.ssid);

                            plot_ui.line(
                                Line::new(history.ssid.clone(), points)
                                    .color(color)
                                    .width(1.5)
                            );
                        }
                    }
                });
        });
    }
}