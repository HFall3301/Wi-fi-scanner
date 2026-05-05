use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime};
use wifi_scan::Wifi;

pub const MAX_WIFI_STRENGTH: f64 = -100.0;
pub const MIN_WIFI_STRENGTH: f64 = -30.0;
pub const DEFAULT_MAX_TIMESTAMP: Duration = Duration::from_secs(300);
pub const DEFAULT_UPDATE_TIME: Duration = Duration::from_millis(100);

pub struct Scanner {
    max_timestamp: Duration,
    snapshots: Vec<ScanSnapshot>,
    history_layers: Vec<Vec<ScanSnapshot>>,
}

#[derive(Clone)]
pub struct WifiSample {
    pub ssid: String,
    pub signal_level: i32,
    pub bssid: String,
    pub channel: u32,
    pub frequency: u32,
}

#[derive(Clone)]
pub struct ScanSnapshot {
    pub timestamp: SystemTime,
    pub data: Vec<WifiSample>,
}

struct NetworkHistory {
    timestamps: Vec<SystemTime>,
    signal_levels: Vec<i32>,
}

impl Scanner {
    pub fn ingest_snapshot(&mut self, mut snapshot: ScanSnapshot) {
        let mut best_by_bssid = HashMap::new();

        for sample in snapshot.data {
            let key = if sample.bssid.is_empty() {
                sample.ssid.clone()
            } else {
                sample.bssid.clone()
            };

            best_by_bssid
                .entry(key)
                .and_modify(|existing: &mut WifiSample| {
                    if sample.signal_level > existing.signal_level {
                        *existing = sample.clone();
                    }
                })
                .or_insert(sample);
        }

        snapshot.data = best_by_bssid.into_values().collect();
        self.snapshots.push(snapshot);
        self.cutoff_history();
    }

    pub fn get_plots_with_history(&self, selected_layer: usize) -> Vec<(String, Vec<[f64; 2]>)> {
        let snapshots = if selected_layer == 0 {
            &self.snapshots
        } else {
            self.history_layers
                .get(selected_layer.saturating_sub(1))
                .unwrap_or(&self.snapshots)
        };

        self.make_plots_from_snapshots(snapshots)
    }

    pub fn capture_history_snapshot(&mut self) {
        if self.snapshots.is_empty() {
            return;
        }

        self.history_layers.push(self.snapshots.clone());
        if self.history_layers.len() > 10 {
            self.history_layers.remove(0);
        }
    }

    pub fn get_history_layers_count(&self) -> usize {
        self.history_layers.len()
    }

    pub fn get_channel_averages(&self) -> Vec<(u32, f64)> {
        let Some(snapshot) = self.snapshots.last() else {
            return Vec::new();
        };

        let mut channel_data: HashMap<u32, Vec<i32>> = HashMap::new();
        for sample in &snapshot.data {
            channel_data
                .entry(sample.channel)
                .or_default()
                .push(sample.signal_level);
        }

        let mut averages = channel_data
            .into_iter()
            .map(|(channel, signals)| {
                let average = signals.iter().sum::<i32>() as f64 / signals.len() as f64;
                (channel, average)
            })
            .collect::<Vec<_>>();
        averages.sort_by_key(|(channel, _)| *channel);
        averages
    }

    pub fn get_best_signal(&self) -> Option<(String, i32)> {
        self.snapshots.last().and_then(|snapshot| {
            snapshot
                .data
                .iter()
                .max_by_key(|sample| sample.signal_level)
                .map(|sample| (sample.ssid.clone(), sample.signal_level))
        })
    }

    pub fn estimate_memory_kb(&self) -> usize {
        let sample_count = self
            .snapshots
            .iter()
            .map(|snapshot| snapshot.data.len())
            .sum::<usize>();
        (sample_count * std::mem::size_of::<WifiSample>()) / 1024
    }

    fn make_plots_from_snapshots(
        &self,
        snapshots: &[ScanSnapshot],
    ) -> Vec<(String, Vec<[f64; 2]>)> {
        let histories = self.make_networks_histories_from_snapshots(snapshots);
        let total_points = histories
            .values()
            .map(|history| history.timestamps.len())
            .sum::<usize>();
        let mut plots = Vec::with_capacity(histories.len());
        let now = SystemTime::now();

        for (ssid, net_history) in histories {
            let plot = if total_points > 500 {
                net_history
                    .bin_by_time(Duration::from_secs(1))
                    .history_to_plot_points(now)
            } else {
                net_history.history_to_plot_points(now)
            };
            plots.push((ssid, plot));
        }

        plots
    }

    pub fn get_snapshots_count(&self) -> usize {
        self.snapshots.len()
    }

    pub fn get_networks_count(&self) -> usize {
        self.make_networks_histories().len() //TODO!(добавить кэширование)
    }

    pub fn get_max_timestamp(&self) -> Duration {
        self.max_timestamp
    }

    pub fn export_to_csv(&self, filename: &str) -> std::io::Result<()> {
        let mut file = File::create(filename)?;
        writeln!(file, "timestamp,ssid,signal,bssid,channel,frequency")?;

        for snapshot in &self.snapshots {
            let timestamp = snapshot
                .timestamp
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs_f64();

            for sample in &snapshot.data {
                writeln!(
                    file,
                    "{},{},{},{},{},{}",
                    timestamp,
                    csv_escape(&sample.ssid),
                    sample.signal_level,
                    csv_escape(&sample.bssid),
                    sample.channel,
                    sample.frequency
                )?;
            }
        }

        Ok(())
    }

    fn cutoff_history(&mut self) {
        let cutoff = SystemTime::now() - self.max_timestamp;
        self.snapshots
            .retain(|snap_shot| snap_shot.timestamp >= cutoff);
    }

    fn make_networks_histories(&self) -> HashMap<String, NetworkHistory> {
        self.make_networks_histories_from_snapshots(&self.snapshots)
    }

    fn make_networks_histories_from_snapshots(
        &self,
        snapshots: &[ScanSnapshot],
    ) -> HashMap<String, NetworkHistory> {
        let mut history_map = HashMap::new();

        for snapshot in snapshots {
            for wifi in &snapshot.data {
                let history =
                    history_map
                        .entry(wifi.ssid.clone())
                        .or_insert_with(|| NetworkHistory {
                            timestamps: Vec::new(),
                            signal_levels: Vec::new(),
                        });

                history.append_history(snapshot.timestamp, wifi.signal_level);
            }
        }

        history_map
    }
}

impl NetworkHistory {
    fn append_history(&mut self, timestamp: SystemTime, signal_level: i32) {
        self.timestamps.push(timestamp);
        self.signal_levels.push(signal_level);
    }

    fn bin_by_time(&self, bin_size: Duration) -> NetworkHistory {
        let Some(&first_timestamp) = self.timestamps.first() else {
            return NetworkHistory {
                timestamps: Vec::new(),
                signal_levels: Vec::new(),
            };
        };

        let mut binned = NetworkHistory {
            timestamps: Vec::new(),
            signal_levels: Vec::new(),
        };
        let mut current_bin_start = first_timestamp;
        let mut bin_signals = Vec::new();

        for (&timestamp, &signal) in self.timestamps.iter().zip(self.signal_levels.iter()) {
            if timestamp <= current_bin_start + bin_size {
                bin_signals.push(signal);
                continue;
            }

            append_average_bin(&mut binned, current_bin_start, &bin_signals);
            current_bin_start = timestamp;
            bin_signals.clear();
            bin_signals.push(signal);
        }

        append_average_bin(&mut binned, current_bin_start, &bin_signals);
        binned
    }

    fn history_to_plot_points(&self, now: SystemTime) -> Vec<[f64; 2]> {
        self.timestamps
            .iter()
            .zip(self.signal_levels.iter())
            .filter_map(|(&timestamp, &signal)| {
                now.duration_since(timestamp)
                    .ok()
                    .map(|elapsed| [-(elapsed.as_secs_f64()), signal as f64])
            })
            .collect()
    }
}

fn append_average_bin(history: &mut NetworkHistory, timestamp: SystemTime, signals: &[i32]) {
    if signals.is_empty() {
        return;
    }

    let average = signals.iter().sum::<i32>() as f64 / signals.len() as f64;
    history.append_history(timestamp, average.round() as i32);
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            max_timestamp: DEFAULT_MAX_TIMESTAMP,
            snapshots: Vec::new(),
            history_layers: Vec::new(),
        }
    }
}

pub fn spawn_scanner_worker(update_time: Duration) -> Receiver<ScanSnapshot> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || scanner_worker_loop(tx, update_time));
    rx
}

fn scanner_worker_loop(tx: Sender<ScanSnapshot>, update_time: Duration) {
    loop {
        if let Some(snapshot) = make_snapshot()
            && tx.send(snapshot).is_err()
        {
            return;
        }
        thread::sleep(update_time);
    }
}
//TODO!(Возвращать соответствующее сообщение,если пользователь не включил вайфай)
fn make_snapshot() -> Option<ScanSnapshot> {
    let wifis: Vec<Wifi> = wifi_scan::scan().ok()?;
    let data = wifis
        .into_iter()
        .filter(|wifi| !wifi.ssid.is_empty())
        .map(|wifi| WifiSample {
            frequency: wifi.get_frequency(),
            channel: wifi.channel,
            bssid: wifi.mac,
            ssid: wifi.ssid,
            signal_level: wifi.signal_level,
        })
        .collect();

    Some(ScanSnapshot {
        timestamp: SystemTime::now(),
        data,
    })
}
