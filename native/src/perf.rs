//! Temporary native TUI perf diagnostics for the scroll-latency pass.
//!
//! Enabled by default unless `RUDDER_NATIVE_PERF=0`. Remove this module or make it
//! opt-in once the worker/review scroll investigation is complete.

use std::{
    env, fs,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::json;

const PERF_ENV: &str = "RUDDER_NATIVE_PERF";
const PERF_FILE: &str = "native-perf.ndjson";
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const KEEP_ROTATED: usize = 3;
const MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

pub(crate) struct PerfLogger {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    enabled: bool,
}

impl PerfLogger {
    pub(crate) fn new() -> Self {
        let enabled = if cfg!(test) {
            env::var(PERF_ENV).is_ok_and(|value| value != "0")
        } else {
            !env::var(PERF_ENV).is_ok_and(|value| value == "0")
        };
        let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
            return Self::disabled();
        };
        let dir = home.join(".rudder");
        let path = dir.join(PERF_FILE);
        if !enabled {
            return Self {
                path,
                writer: None,
                enabled: false,
            };
        }
        let _ = fs::create_dir_all(&dir);
        cleanup_old_logs(&dir);
        rotate_if_needed(&path);
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
        }
    }

    fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            writer: None,
            enabled: false,
        }
    }

    pub(crate) fn log(&mut self, event: &str, fields: serde_json::Value) {
        if !self.enabled {
            return;
        }
        if self
            .path
            .metadata()
            .is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES)
        {
            self.writer = None;
            rotate_if_needed(&self.path);
            self.writer = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok()
                .map(BufWriter::new);
        }
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        let line = json!({
            "ts_ms": now_ms(),
            "event": event,
            "fields": fields,
        });
        let _ = serde_json::to_writer(&mut *writer, &line);
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
    }

    pub(crate) fn log_duration(
        &mut self,
        event: &str,
        started: Instant,
        fields: serde_json::Value,
    ) {
        let mut fields = fields;
        if let Some(object) = fields.as_object_mut() {
            object.insert(
                "duration_us".to_string(),
                serde_json::Value::from(started.elapsed().as_micros() as u64),
            );
        }
        self.log(event, fields);
    }
}

fn rotate_if_needed(path: &PathBuf) {
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

fn cleanup_old_logs(dir: &PathBuf) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = SystemTime::now().checked_sub(MAX_AGE);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name != PERF_FILE && !name.starts_with(&format!("{PERF_FILE}.")) {
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

fn rotated_path(path: &PathBuf, index: usize) -> PathBuf {
    path.with_file_name(format!("{PERF_FILE}.{index}"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
