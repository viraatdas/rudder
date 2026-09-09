#![allow(unused_imports)]
//! Desktop notifications for lifecycle transitions.
//!
//! Workers run inside Rudder's embedded terminal, so a backend's own desktop
//! notification escapes (Claude emits OSC 777 "needs your permission") are
//! consumed by our VT parser and never reach the real terminal. The dashboard
//! therefore emits its own notification at each lifecycle transition, driven
//! by the backend's OWN lifecycle events (the hook signals in `signals.rs`:
//! Claude `Stop`/`StopFailure`/`Notification`, Codex `Stop`/`PermissionRequest`,
//! opencode `session.idle`/`permission.asked`/`session.error`): entering
//! review, a turn failing, and a worker latching a question/permission wait.
//!
//! Two channels, native first:
//! - **OS-native**: `terminal-notifier` or `osascript` on macOS, `notify-send`
//!   on Linux, spawned off-thread. Works in every emulator, including ones with
//!   no notification escape at all (Terminal.app), and is what makes "the
//!   session is done" reach the user reliably.
//! - **OSC 777** through the emulator, used only when no native notifier is on
//!   PATH. Ghostty, kitty and other urxvt-notify-aware emulators render it;
//!   others ignore it silently. Never both: Ghostty would show two.
//!
//! Focus gating: the tab the user is looking at never notifies — they can see
//! the row change color. Focus is tracked via the terminal's focus-report mode
//! (CSI ?1004), flipped by `Event::FocusGained`/`FocusLost` in the event loop.
//! The default is "focused" so a session that never receives focus events
//! (emulator without mode 1004) stays silent rather than spamming.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

static TERMINAL_FOCUSED: AtomicBool = AtomicBool::new(true);

pub(crate) fn set_terminal_focused(focused: bool) {
    TERMINAL_FOCUSED.store(focused, Ordering::Relaxed);
}

pub(crate) fn terminal_focused() -> bool {
    TERMINAL_FOCUSED.load(Ordering::Relaxed)
}

/// Longest title/body we ship: notification centers truncate anyway, and an
/// oversized OSC risks being dropped whole by intermediaries.
const MAX_FIELD_CHARS: usize = 120;

/// Strip everything that could terminate or corrupt the OSC payload: control
/// characters (ESC/BEL end the sequence early) and, for the title, semicolons
/// (the field separator — a `;` in the title would shift the body).
fn sanitize_field(value: &str, strip_semicolons: bool) -> String {
    let mut out = String::with_capacity(value.len().min(MAX_FIELD_CHARS + 3));
    for (count, ch) in value.chars().enumerate() {
        if count >= MAX_FIELD_CHARS {
            out.push('…');
            break;
        }
        if ch.is_control() {
            out.push(' ');
        } else if strip_semicolons && ch == ';' {
            out.push(',');
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// The raw escape sequence for one notification. `ESC ] 777 ; notify ; title ;
/// body BEL`, wrapped in a tmux passthrough envelope when running under tmux
/// (tmux swallows unknown OSCs otherwise; inside the envelope every ESC is
/// doubled per the DCS tmux; protocol).
pub(crate) fn build_notification_sequence(title: &str, body: &str, under_tmux: bool) -> String {
    let title = sanitize_field(title, true);
    let body = sanitize_field(body, false);
    let osc = format!("\x1b]777;notify;{title};{body}\x07");
    if under_tmux {
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else {
        osc
    }
}

/// Rudder's own icon, baked into the binary so the notifier bundle can be
/// built anywhere `RUDDER_HOME` is writable (rendered from site/favicon.svg).
const RUDDER_ICNS: &[u8] = include_bytes!("../../assets/rudder.icns");
const RUDDER_NOTIFIER_BUNDLE_ID: &str = "dev.viraat.rudder.notifier";

/// Which OS-native notifier this machine has. Detected once per process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeNotifier {
    /// macOS, homebrew `terminal-notifier`: clicking the notification can
    /// activate the terminal app, and it shows title / subtitle / message.
    /// The path is the executable to run — ideally inside Rudder's own
    /// renamed copy of the bundle (see `ensure_rudder_notifier_app`), so the
    /// notification carries Rudder's icon and name instead of Terminal's.
    TerminalNotifier(PathBuf),
    /// macOS, always present: `display notification` via AppleScript.
    Osascript(PathBuf),
    /// Linux freedesktop.
    NotifySend(PathBuf),
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The `terminal-notifier.app` bundle behind a `terminal-notifier` executable.
/// Homebrew installs a bash wrapper in `bin/` that execs the bundle's binary;
/// other installs symlink straight into the bundle. Both are handled.
pub(crate) fn terminal_notifier_app_from(bin: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    if let Some(app) = resolved
        .ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
    {
        return Some(app.to_path_buf());
    }
    // A wrapper script: find the quoted bundle path it execs.
    let text = std::fs::read_to_string(&resolved).ok()?;
    let marker = ".app/Contents/MacOS/terminal-notifier";
    let end = text.find(marker)?;
    let start = text[..end].rfind(|c: char| c == '"' || c == '\'' || c.is_whitespace())? + 1;
    let app = PathBuf::from(&text[start..end + 4]);
    app.is_dir().then_some(app)
}

/// Build (or refresh) `<home>/notifier/Rudder.app`: terminal-notifier's own
/// documented way to get a notification that looks like YOUR app — a copy of
/// its bundle with the icon, name and bundle id swapped, ad-hoc signed so macOS
/// accepts it as a notification sender. Returns the executable to run.
///
/// Rebuilt whenever the source bundle, the embedded icon, or the Rudder version
/// changes (a stamp file records all three); otherwise a cheap existence check.
/// macOS asks once, per bundle id, whether "Rudder" may notify — that prompt is
/// the point: it is Rudder asking, not Terminal.
pub(crate) fn ensure_rudder_notifier_app(source_app: &Path, home: &Path) -> Option<PathBuf> {
    use std::process::Command;
    let app = home.join("notifier").join("Rudder.app");
    let exe = app.join("Contents/MacOS/terminal-notifier");
    // The stamp lives NEXT TO the bundle, not inside it: any file added under
    // Contents/ after signing is "a sealed resource missing or invalid" and
    // the notification center silently drops everything the app sends.
    let stamp_path = home.join("notifier").join("Rudder.app.stamp");
    let source_mtime = std::fs::metadata(source_app.join("Contents/Info.plist"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format!(
        "{}|{source_mtime}|{}|{}",
        source_app.display(),
        RUDDER_ICNS.len(),
        env!("CARGO_PKG_VERSION")
    );
    if exe.is_file() && std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str())
    {
        return Some(exe);
    }
    let _ = std::fs::remove_dir_all(&app);
    std::fs::create_dir_all(app.parent()?).ok()?;
    // `cp -R` keeps the bundle layout (symlinks, lproj dirs) that a file-by-file
    // copy would have to reimplement.
    let copied = Command::new("cp")
        .arg("-R")
        .arg(source_app)
        .arg(&app)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !copied || !exe.is_file() {
        let _ = std::fs::remove_dir_all(&app);
        return None;
    }
    let resources = app.join("Contents/Resources");
    std::fs::write(resources.join("Rudder.icns"), RUDDER_ICNS).ok()?;
    let _ = std::fs::remove_file(resources.join("Terminal.icns"));
    let plist = app.join("Contents/Info.plist");
    for (key, value) in [
        ("CFBundleIdentifier", RUDDER_NOTIFIER_BUNDLE_ID),
        ("CFBundleName", "Rudder"),
        ("CFBundleDisplayName", "Rudder"),
        ("CFBundleIconFile", "Rudder"),
    ] {
        let ok = Command::new("plutil")
            .args(["-replace", key, "-string", value])
            .arg(&plist)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&app);
            return None;
        }
    }
    // Editing Resources/Info.plist broke whatever signature the copy carried;
    // an ad-hoc signature is enough for the notification center to trust it.
    let signed = Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(&app)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !signed {
        let _ = std::fs::remove_dir_all(&app);
        return None;
    }
    std::fs::write(&stamp_path, stamp).ok()?;
    Some(exe)
}

pub(crate) fn detect_native_notifier() -> Option<NativeNotifier> {
    if cfg!(target_os = "macos") {
        if let Some(path) = find_on_path("terminal-notifier") {
            // Rudder's own bundle when it can be built; the stock one otherwise.
            let branded = terminal_notifier_app_from(&path)
                .zip(crate::signals::rudder_home())
                .and_then(|(app, home)| ensure_rudder_notifier_app(&app, &home));
            return Some(NativeNotifier::TerminalNotifier(branded.unwrap_or(path)));
        }
        if let Some(path) = find_on_path("osascript") {
            return Some(NativeNotifier::Osascript(path));
        }
        return None;
    }
    find_on_path("notify-send").map(NativeNotifier::NotifySend)
}

fn native_notifier() -> Option<&'static NativeNotifier> {
    static NOTIFIER: std::sync::OnceLock<Option<NativeNotifier>> = std::sync::OnceLock::new();
    NOTIFIER.get_or_init(detect_native_notifier).as_ref()
}

/// One lifecycle notification, in parts, so each channel can lay it out the
/// way it looks best there: terminal-notifier has title / subtitle / message
/// lines; the others get a single title and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DesktopNote {
    /// The repo the dashboard is running in.
    pub(crate) repo: String,
    /// What happened: "✔ ready for review", "needs permission"...
    pub(crate) event: String,
    /// The agent's task, possibly empty.
    pub(crate) task: String,
}

impl DesktopNote {
    /// `Rudder · <repo>` for channels with one title line.
    pub(crate) fn flat_title(&self) -> String {
        format!("Rudder · {}", self.repo)
    }

    /// `<event> — <task>` for channels with one body line.
    pub(crate) fn flat_body(&self) -> String {
        let task = self.task.trim();
        if task.is_empty() {
            self.event.clone()
        } else {
            format!("{} — {task}", self.event)
        }
    }
}

/// The program and argv for one native notification. Text is passed as
/// separate arguments, never interpolated into a script, so a task summary
/// containing quotes cannot break out (osascript reads them from `argv`).
/// `activate` is the bundle id of the terminal app to bring forward on click
/// (macOS sets `__CFBundleIdentifier` on GUI-launched processes).
pub(crate) fn native_notification_command(
    notifier: &NativeNotifier,
    note: &DesktopNote,
    activate: Option<&str>,
) -> (PathBuf, Vec<String>) {
    let title = sanitize_field(&note.flat_title(), false);
    let body = sanitize_field(&note.flat_body(), false);
    match notifier {
        NativeNotifier::TerminalNotifier(path) => {
            // Three lines: the repo up top, the event as the subtitle, the
            // task as the message. The Rudder name is carried by the bundle's
            // icon and app name, so it is not repeated in the title.
            let task = sanitize_field(note.task.trim(), false);
            let event = sanitize_field(&note.event, false);
            let (subtitle, message) = if task.is_empty() {
                (String::new(), event)
            } else {
                (event, task)
            };
            let mut args = vec![
                "-title".to_string(),
                sanitize_field(&note.repo, false),
                "-message".to_string(),
                message,
                "-ignoreDnD".to_string(),
            ];
            if !subtitle.is_empty() {
                args.push("-subtitle".to_string());
                args.push(subtitle);
            }
            if let Some(bundle) = activate.filter(|b| !b.trim().is_empty()) {
                args.push("-activate".to_string());
                args.push(bundle.to_string());
            }
            (path.clone(), args)
        }
        NativeNotifier::Osascript(path) => (
            path.clone(),
            vec![
                "-e".to_string(),
                "on run argv".to_string(),
                "-e".to_string(),
                "display notification (item 2 of argv) with title (item 1 of argv)".to_string(),
                "-e".to_string(),
                "end run".to_string(),
                title,
                body,
            ],
        ),
        NativeNotifier::NotifySend(path) => (
            path.clone(),
            vec!["-a".to_string(), "Rudder".to_string(), title, body],
        ),
    }
}

/// Emit one desktop notification. No-op when disabled (`/notify off`), when
/// this tab currently has focus (the user can already see the dashboard), or
/// in tests. Native notifier when one exists, else OSC 777 through the outer
/// terminal. The native process runs on a throwaway thread so the poll loop
/// never blocks on it (osascript can take a few hundred ms) and the child is
/// always reaped.
pub(crate) fn notify_desktop(note: DesktopNote) {
    if cfg!(test) {
        return;
    }
    if !desktop_notifications_enabled() {
        return;
    }
    if terminal_focused() {
        return;
    }
    if native_notifier_available() {
        let activate = std::env::var("__CFBundleIdentifier").ok();
        // Detection (and, the first time, building the Rudder.app bundle:
        // cp + plutil + codesign) happens on the thread too, so the poll loop
        // never pays for it.
        std::thread::spawn(move || {
            let Some(notifier) = native_notifier() else {
                return;
            };
            let (program, args) = native_notification_command(notifier, &note, activate.as_deref());
            let _ = std::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        });
        return;
    }
    let seq = build_notification_sequence(
        &note.flat_title(),
        &note.flat_body(),
        std::env::var_os("TMUX").is_some(),
    );
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Cheap PATH-only check for the channel decision; the full detection (which
/// may build the bundle) runs off-thread in `native_notifier`.
fn native_notifier_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if cfg!(target_os = "macos") {
            find_on_path("terminal-notifier").is_some() || find_on_path("osascript").is_some()
        } else {
            find_on_path("notify-send").is_some()
        }
    })
}

fn repo_display_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "rudder".to_string())
}

/// One lifecycle notification for a run. The repo name distinguishes tabs when
/// several dashboards are open; the task tells agents apart.
pub(crate) fn notify_run(run: &AgentRun, event: &str) {
    notify_desktop(DesktopNote {
        repo: repo_display_name(&run.cwd),
        event: event.to_string(),
        task: run.task_summary.trim().to_string(),
    });
}
