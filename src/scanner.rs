//! Wi-Fi scan ingestion, aggregation, and export.
//!
//! The UI owns a [`Scanner`] and feeds it snapshots received from the worker
//! thread. This module deliberately keeps egui-specific types out of the data
//! model so scanning logic can be tested and evolved independently.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime};
use wifi_scan::Wifi;

/// Weakest visible RSSI value shown on the signal plot.
pub const SIGNAL_FLOOR_DBM: f64 = -100.0;
/// Strongest visible RSSI value shown on the signal plot.
pub const SIGNAL_CEILING_DBM: f64 = -30.0;
/// How long live scan samples are retained.
pub const DEFAULT_MAX_HISTORY_AGE: Duration = Duration::from_secs(300);
/// Default delay between worker scan attempts.
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_millis(100);
/// Maximum number of point samples drawn before per-network binning is applied.
pub const MAX_RAW_PLOT_POINTS: usize = 500;
/// Size of the time buckets used when plot data grows large.
pub const PLOT_BIN_SIZE: Duration = Duration::from_secs(1);
/// Accumulates Wi-Fi snapshots and provides analysis-friendly projections.
pub struct Scanner {
    max_history_age: Duration,
    snapshots: Vec<ScanSnapshot>,
}

/// One network observation returned by the scan backend.
#[derive(Clone)]
pub struct WifiSample {
    /// User-facing network name. Hidden networks are filtered before ingestion.
    pub ssid: String,
    /// RSSI signal level in dBm.
    pub signal_level: i32,
    /// Access point MAC address. `wifi_scan` names this field `mac`.
    pub bssid: String,
    /// Wi-Fi channel number. `0` means unknown.
    pub channel: u32,
    /// Derived center frequency in MHz. `0` means unknown.
    pub frequency: u32,
}

/// A point-in-time scan result.
#[derive(Clone)]
pub struct ScanSnapshot {
    /// Wall-clock timestamp when the scan completed.
    pub timestamp: SystemTime,
    /// Samples observed in this scan.
    pub data: Vec<WifiSample>,
}

/// Plot-ready time series for one SSID.
pub struct PlotSeries {
    /// SSID represented by this line.
    pub ssid: String,
    /// `[seconds_ago, signal_dbm]` points.
    pub points: Vec<[f64; 2]>,
}

/// Average signal level for one channel in the latest live snapshot.
pub struct ChannelAverage {
    /// Wi-Fi channel number.
    pub channel: u32,
    /// Average RSSI in dBm.
    pub signal_level: f64,
}

/// Best candidate in the latest live snapshot.
pub struct BestSignal {
    /// SSID with the strongest RSSI.
    pub ssid: String,
    /// Strongest RSSI in dBm.
    pub signal_level: i32,
}

#[derive(Default)]
struct NetworkHistory {
    timestamps: Vec<SystemTime>,
    signal_levels: Vec<i32>,
}

impl Scanner {
    /// Insert a snapshot, deduplicating AP observations by BSSID first.
    ///
    /// When the backend reports duplicate AP records, only the strongest sample
    /// for each BSSID is kept. If a platform does not expose BSSID, SSID is used
    /// as a fallback key.
    pub fn ingest_snapshot(&mut self, mut snapshot: ScanSnapshot) {
        snapshot.data = deduplicate_samples(snapshot.data);
        self.snapshots.push(snapshot);
        self.cutoff_expired_snapshots();
    }

    /// Build plot series from the live scan window.
    pub fn plot_series(&self) -> Vec<PlotSeries> {
        self.plot_series_from_snapshots(&self.snapshots)
    }

    /// Average RSSI by channel for the latest live snapshot.
    pub fn channel_averages(&self) -> Vec<ChannelAverage> {
        let Some(snapshot) = self.snapshots.last() else {
            return Vec::new();
        };

        let mut samples_by_channel: HashMap<u32, Vec<i32>> = HashMap::new();
        for sample in &snapshot.data {
            samples_by_channel
                .entry(sample.channel)
                .or_default()
                .push(sample.signal_level);
        }

        let mut averages = samples_by_channel
            .into_iter()
            .map(|(channel, signals)| ChannelAverage {
                channel,
                signal_level: average_signal(&signals),
            })
            .collect::<Vec<_>>();
        averages.sort_by_key(|average| average.channel);
        averages
    }

    /// Strongest network observed in the latest live snapshot.
    pub fn best_signal(&self) -> Option<BestSignal> {
        self.snapshots.last().and_then(|snapshot| {
            snapshot
                .data
                .iter()
                .max_by_key(|sample| sample.signal_level)
                .map(|sample| BestSignal {
                    ssid: sample.ssid.clone(),
                    signal_level: sample.signal_level,
                })
        })
    }

    /// Approximate retained live sample storage.
    pub fn estimated_memory_kb(&self) -> usize {
        (sample_count(&self.snapshots) * std::mem::size_of::<WifiSample>()) / 1024
    }

    /// Number of live snapshots currently retained.
    pub fn snapshots_count(&self) -> usize {
        self.snapshots.len()
    }

    /// Number of distinct SSIDs in the live history window.
    pub fn networks_count(&self) -> usize {
        self.network_histories_from_snapshots(&self.snapshots).len()
    }

    /// Current live retention window.
    pub fn max_history_age(&self) -> Duration {
        self.max_history_age
    }

    /// Export live snapshots to a CSV file.
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

    fn plot_series_from_snapshots(&self, snapshots: &[ScanSnapshot]) -> Vec<PlotSeries> {
        let histories = self.network_histories_from_snapshots(snapshots);
        let total_points = histories
            .values()
            .map(|history| history.timestamps.len())
            .sum::<usize>();
        let now = SystemTime::now();

        let mut series = histories
            .into_iter()
            .map(|(ssid, history)| {
                let history = if total_points > MAX_RAW_PLOT_POINTS {
                    history.bin_by_time(PLOT_BIN_SIZE)
                } else {
                    history
                };

                PlotSeries {
                    ssid,
                    points: history.to_plot_points(now),
                }
            })
            .collect::<Vec<_>>();
        series.sort_by(|left, right| left.ssid.cmp(&right.ssid));
        series
    }

    fn cutoff_expired_snapshots(&mut self) {
        let cutoff = SystemTime::now() - self.max_history_age;
        self.snapshots
            .retain(|snapshot| snapshot.timestamp >= cutoff);
    }

    fn network_histories_from_snapshots(
        &self,
        snapshots: &[ScanSnapshot],
    ) -> HashMap<String, NetworkHistory> {
        let mut history_map = HashMap::new();

        for snapshot in snapshots {
            for sample in &snapshot.data {
                history_map
                    .entry(sample.ssid.clone())
                    .or_insert_with(NetworkHistory::default)
                    .append(snapshot.timestamp, sample.signal_level);
            }
        }

        history_map
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            max_history_age: DEFAULT_MAX_HISTORY_AGE,
            snapshots: Vec::new(),
        }
    }
}

impl NetworkHistory {
    fn append(&mut self, timestamp: SystemTime, signal_level: i32) {
        self.timestamps.push(timestamp);
        self.signal_levels.push(signal_level);
    }

    fn bin_by_time(&self, bin_size: Duration) -> NetworkHistory {
        let Some(&first_timestamp) = self.timestamps.first() else {
            return Self::default();
        };

        let mut binned = NetworkHistory::default();
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

    fn to_plot_points(&self, now: SystemTime) -> Vec<[f64; 2]> {
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

/// Start a background scanner loop and return its snapshot receiver.
pub fn spawn_scanner_worker(scan_interval: Duration) -> Receiver<ScanSnapshot> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || scanner_worker_loop(tx, scan_interval));
    rx
}

fn scanner_worker_loop(tx: Sender<ScanSnapshot>, scan_interval: Duration) {
    loop {
        if let Some(snapshot) = make_snapshot()
            && tx.send(snapshot).is_err()
        {
            return;
        }
        thread::sleep(scan_interval);
    }
}

fn make_snapshot() -> Option<ScanSnapshot> {
    let wifis: Vec<Wifi> = wifi_scan::scan().ok()?;
    let data = wifis
        .into_iter()
        .filter(|wifi| !wifi.is_hidden())
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

fn deduplicate_samples(samples: Vec<WifiSample>) -> Vec<WifiSample> {
    let mut best_by_bssid = HashMap::new();

    for sample in samples {
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

    best_by_bssid.into_values().collect()
}

fn append_average_bin(history: &mut NetworkHistory, timestamp: SystemTime, signals: &[i32]) {
    if !signals.is_empty() {
        history.append(timestamp, average_signal(signals).round() as i32);
    }
}

fn average_signal(signals: &[i32]) -> f64 {
    signals.iter().sum::<i32>() as f64 / signals.len() as f64
}

fn sample_count(snapshots: &[ScanSnapshot]) -> usize {
    snapshots
        .iter()
        .map(|snapshot| snapshot.data.len())
        .sum::<usize>()
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
