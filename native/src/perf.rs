//! Native TUI perf diagnostics.
//!
//! Runtime file capture is opt-in with `RUDDER_NATIVE_PERF=1`. Normal samples are
//! aggregated in memory and emitted as periodic summaries; raw NDJSON lines are
//! reserved for threshold breaches and discrete input bursts.

use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hdrhistogram::Histogram;
use serde_json::json;

const PERF_ENV: &str = "RUDDER_NATIVE_PERF";
const PERF_HUD_ENV: &str = "RUDDER_PERF_HUD";
const PERF_PREFIX: &str = "native-perf-";
const PERF_SUFFIX: &str = ".ndjson";
const LEGACY_PERF_FILE: &str = "native-perf.ndjson";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const KEEP_ROTATED: usize = 2;
const MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const SUMMARY_INTERVAL: Duration = Duration::from_secs(60);
const HUD_WINDOW: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct PerfLogger {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    enabled: bool,
    bytes_written: u64,
    last_flush: Instant,
}

#[derive(Debug)]
pub(crate) struct PerfStats {
    enabled: bool,
    hud_enabled: bool,
    last_summary: Instant,
    histograms: HashMap<&'static str, Histogram<u64>>,
    recent: VecDeque<PerfSample>,
}

#[derive(Debug, Clone, Copy)]
struct PerfSample {
    at: Instant,
    metric: &'static str,
    value_us: u64,
}

impl PerfLogger {
    pub(crate) fn new() -> Self {
        let Some(dir) = rudder_home() else {
            return Self::disabled();
        };
        cleanup_old_logs(&dir);

        if !perf_file_enabled() {
            return Self::disabled();
        }

        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("{PERF_PREFIX}{}{PERF_SUFFIX}", std::process::id()));
        rotate_if_needed(&path);
        let bytes_written = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(BufWriter::new);
        Self {
            path,
            writer,
            enabled: true,
            bytes_written,
            last_flush: Instant::now(),
        }
    }

    fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            writer: None,
            enabled: false,
            bytes_written: 0,
            last_flush: Instant::now(),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn log(&mut self, event: &str, fields: serde_json::Value) {
        if !self.enabled {
            return;
        }
        let line = json!({
            "ts_ms": now_ms(),
            "event": event,
            "fields": fields,
        });
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');
        self.write_line(&bytes);
    }

    pub(crate) fn log_duration_over(
        &mut self,
        event: &str,
        duration: Duration,
        threshold: Duration,
        fields: serde_json::Value,
    ) {
        if duration < threshold {
            return;
        }
        self.log_duration_value(event, duration, fields);
    }

    pub(crate) fn log_duration_value(
        &mut self,
        event: &str,
        duration: Duration,
        fields: serde_json::Value,
    ) {
        let mut fields = fields;
        if let Some(object) = fields.as_object_mut() {
            object.insert(
                "duration_us".to_string(),
                serde_json::Value::from(duration.as_micros() as u64),
            );
        }
        self.log(event, fields);
    }

    fn write_line(&mut self, bytes: &[u8]) {
        if self.bytes_written.saturating_add(bytes.len() as u64) > MAX_LOG_BYTES {
            self.rotate();
        }
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        if writer.write_all(bytes).is_ok() {
            self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
        }
        if self.last_flush.elapsed() >= FLUSH_INTERVAL {
            let _ = writer.flush();
            self.last_flush = Instant::now();
        }
    }

    fn rotate(&mut self) {
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.flush();
        }
        rotate_if_needed(&self.path);
        self.writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok()
            .map(BufWriter::new);
        self.bytes_written = self
            .path
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        self.last_flush = Instant::now();
    }
}

impl Drop for PerfLogger {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
    }
}

impl PerfStats {
    pub(crate) fn new(file_enabled: bool) -> Self {
        let hud_enabled = env::var(PERF_HUD_ENV).is_ok_and(|value| value != "0");
        Self {
            enabled: file_enabled || hud_enabled,
            hud_enabled,
            last_summary: Instant::now(),
            histograms: HashMap::new(),
            recent: VecDeque::new(),
        }
    }

    pub(crate) fn record_duration(&mut self, metric: &'static str, duration: Duration) {
        if !self.enabled {
            return;
        }
        let value_us = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        let histogram = self
            .histograms
            .entry(metric)
            .or_insert_with(|| Histogram::new(3).expect("valid histogram precision"));
        let _ = histogram.record(value_us);
        self.recent.push_back(PerfSample {
            at: Instant::now(),
            metric,
            value_us,
        });
        self.prune_recent();
    }

    pub(crate) fn emit_due(&mut self, logger: &mut PerfLogger) {
        if !logger.enabled() || self.last_summary.elapsed() < SUMMARY_INTERVAL {
            return;
        }
        self.last_summary = Instant::now();
        let mut summaries = Vec::new();
        for (metric, histogram) in self.histograms.iter_mut() {
            if histogram.len() == 0 {
                continue;
            }
            summaries.push(json!({
                "metric": metric,
                "count": histogram.len(),
                "p50_us": histogram.value_at_quantile(0.50),
                "p95_us": histogram.value_at_quantile(0.95),
                "p99_us": histogram.value_at_quantile(0.99),
                "max_us": histogram.max(),
            }));
            histogram.reset();
        }
        for summary in summaries {
            logger.log("perf_summary", summary);
        }
    }

    pub(crate) fn hud_line(&mut self, agent_count: usize) -> Option<String> {
        if !self.hud_enabled {
            return None;
        }
        self.prune_recent();
        let mut frame_values = Vec::new();
        let mut drain_values = Vec::new();
        let mut render_values = Vec::new();
        for sample in &self.recent {
            match sample.metric {
                "frame_total" => frame_values.push(sample.value_us),
                "pty_drain_parse" => drain_values.push(sample.value_us),
                "terminal_draw" => render_values.push(sample.value_us),
                _ => {}
            }
        }
        let fps = frame_values.len() as f64 / HUD_WINDOW.as_secs_f64();
        Some(format!(
            "fps {fps:.1}  frame p95 {}us max {}us  draw max {}us  drain max {}us  agents {}",
            percentile(&mut frame_values, 0.95),
            frame_values.into_iter().max().unwrap_or(0),
            render_values.into_iter().max().unwrap_or(0),
            drain_values.into_iter().max().unwrap_or(0),
            agent_count
        ))
    }

    fn prune_recent(&mut self) {
        let cutoff = Instant::now() - HUD_WINDOW;
        while self.recent.front().is_some_and(|sample| sample.at < cutoff) {
            self.recent.pop_front();
        }
    }
}

fn percentile(values: &mut [u64], quantile: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * quantile).round() as usize;
    values[index.min(values.len() - 1)]
}

fn perf_file_enabled() -> bool {
    env::var(PERF_ENV).is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
}

fn rudder_home() -> Option<PathBuf> {
    if let Some(value) = env::var_os("RUDDER_HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    let home = env::var_os("HOME").filter(|value| !value.is_empty())?;
    Some(PathBuf::from(home).join(".rudder"))
}

fn rotate_if_needed(path: &Path) {
    if !path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES)
    {
        return;
    }
    for index in (1..=KEEP_ROTATED).rev() {
        let from = rotated_path(path, index);
        let to = rotated_path(path, index + 1);
        if index == KEEP_ROTATED {
            let _ = fs::remove_file(&from);
        } else if from.exists() {
            let _ = fs::rename(&from, &to);
        }
    }
    let _ = fs::rename(path, rotated_path(path, 1));
}

fn cleanup_old_logs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = SystemTime::now().checked_sub(MAX_AGE);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_perf_log_name(name) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if let (Some(cutoff), Ok(modified)) = (cutoff, metadata.modified()) {
            if modified < cutoff {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn is_perf_log_name(name: &str) -> bool {
    name == LEGACY_PERF_FILE
        || name.starts_with(&format!("{LEGACY_PERF_FILE}."))
        || (name.starts_with(PERF_PREFIX) && name.contains(PERF_SUFFIX))
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(LEGACY_PERF_FILE);
    path.with_file_name(format!("{name}.{index}"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
