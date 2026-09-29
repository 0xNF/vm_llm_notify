use clap::{CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use ksni::blocking::TrayMethods;
use std::collections::BTreeMap;
use std::io::Read;
use std::{thread, time::Duration};

const APP: &str = env!("CARGO_PKG_NAME");
const ICON: &str = "utilities-terminal";
const FLASH_ICON: &str = "dialog-warning";

/// Precedence: CLI args > $XDG_CONFIG_HOME/vm_llm_notify/settings.conf.json > environment.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Address to listen on for Claude Code hook events
    #[arg(long, env = "VM_LLM_NOTIFY_LISTEN", default_value = "127.0.0.1:9876")]
    listen: String,
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    listen: Option<String>,
}

fn load_config(args: impl IntoIterator<Item = std::ffi::OsString>, file: FileConfig) -> String {
    let m = Cli::command().get_matches_from(args);
    let from_cli = m.value_source("listen") == Some(ValueSource::CommandLine);
    let cli = Cli::from_arg_matches(&m).unwrap_or_else(|e| e.exit());
    match file.listen {
        Some(listen) if !from_cli => listen,
        _ => cli.listen,
    }
}

fn read_config_file() -> Result<FileConfig, String> {
    let Some(path) = xdg::BaseDirectories::with_prefix(APP).find_config_file("settings.conf.json") else {
        return Ok(FileConfig::default());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[derive(Default)]
struct Tray {
    /// session_id -> cwd of sessions waiting for the user
    waiting: BTreeMap<String, String>,
    blink: bool,
    /// URL agents should POST hook events to
    url: String,
}

impl Tray {
    /// Returns whether the event was understood.
    fn handle(&mut self, event: &serde_json::Value) -> bool {
        let (Some(name), Some(id)) = (event["hook_event_name"].as_str(), event["session_id"].as_str()) else {
            return false;
        };
        match name {
            "Stop" | "Notification" => {
                log::info!("{name}: session {id} waiting");
                let cwd = event["cwd"].as_str().unwrap_or(id);
                self.waiting.insert(id.into(), cwd.into());
            }
            // ponytail: any other event from a session means it's busy again (or gone)
            _ => {
                self.waiting.remove(id);
            }
        }
        true
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        APP.into()
    }
    fn title(&self) -> String {
        "Claude Code".into()
    }
    fn icon_name(&self) -> String {
        if self.blink { FLASH_ICON } else { ICON }.into()
    }
    fn status(&self) -> ksni::Status {
        if self.waiting.is_empty() { ksni::Status::Active } else { ksni::Status::NeedsAttention }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let description = if self.waiting.is_empty() {
            "No sessions waiting".into()
        } else {
            self.waiting.values().cloned().collect::<Vec<_>>().join("\n")
        };
        ksni::ToolTip { title: "Claude Code".into(), description, ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.waiting.clear();
        self.blink = false;
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let mut items: Vec<ksni::MenuItem<Self>> = self
            .waiting
            .iter()
            .map(|(id, cwd)| {
                let id = id.clone();
                StandardItem {
                    label: format!("Dismiss {cwd}"),
                    activate: Box::new(move |t: &mut Self| {
                        t.waiting.remove(&id);
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        items.push(ksni::MenuItem::Separator);
        let url = self.url.clone();
        items.push(
            StandardItem {
                label: "Copy address for agent".into(),
                icon_name: "edit-copy".into(),
                activate: Box::new(move |_| copy_to_clipboard(url.clone())),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// `log` backend writing RFC 5424 to the local syslog socket; syslog's own `init` only does RFC 3164.
struct Syslog5424(std::sync::Mutex<syslog::Logger<syslog::LoggerBackend, syslog::Formatter5424>>);

impl log::Log for Syslog5424 {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::max_level()
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        // (MSGID, STRUCTURED-DATA, MSG); the crate types MSGID as u32
        let msg = (0, BTreeMap::new(), r.args().to_string());
        let Ok(mut l) = self.0.lock() else { return };
        let _ = match r.level() {
            log::Level::Error => l.err(msg),
            log::Level::Warn => l.warning(msg),
            log::Level::Info => l.info(msg),
            log::Level::Debug | log::Level::Trace => l.debug(msg),
        };
    }
    fn flush(&self) {}
}

fn init_syslog() -> Result<(), String> {
    let formatter = syslog::Formatter5424 {
        facility: syslog::Facility::LOG_USER,
        hostname: std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|h| h.trim().into()),
        process: APP.into(),
        pid: std::process::id(),
    };
    let logger = syslog::unix(formatter).map_err(|e| e.to_string())?;
    log::set_boxed_logger(Box::new(Syslog5424(logger.into()))).map_err(|e| e.to_string())?;
    log::set_max_level(log::LevelFilter::Info);
    Ok(())
}

/// ponytail: KDE-only via Klipper's D-Bus API, fine for a KDE tray app
fn copy_to_clipboard(text: String) {
    // own thread: don't block ksni's D-Bus dispatch with a nested D-Bus call
    thread::spawn(move || {
        let r = zbus::blocking::Connection::session().and_then(|c| {
            c.call_method(Some("org.kde.klipper"), "/klipper", Some("org.kde.klipper.klipper"), "setClipboardContents", &(text.as_str(),))
        });
        match r {
            Ok(_) => log::info!("copied {text} to clipboard"),
            Err(e) => log::error!("copying to clipboard via Klipper failed: {e}"),
        }
    });
}

fn main() {
    if let Err(e) = init_syslog() {
        eprintln!("syslog unavailable, logging disabled: {e}");
    }
    if let Err(e) = run() {
        log::error!("{e}");
        eprintln!("{APP}: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let addr = load_config(std::env::args_os(), read_config_file()?);
    let tray = Tray { url: format!("http://{addr}"), ..Default::default() }.spawn().map_err(|e| format!("failed to register tray icon: {e}"))?;
    let server = tiny_http::Server::http(&addr).map_err(|e| format!("failed to bind {addr}: {e}"))?;
    log::info!("listening on {addr}");

    let blinker = tray.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(500));
        blinker.update(|t| t.blink = !t.waiting.is_empty() && !t.blink);
    });

    for mut req in server.incoming_requests() {
        let mut body = Vec::new();
        let _ = req.as_reader().take(1 << 20).read_to_end(&mut body);
        let ok = serde_json::from_slice(&body).is_ok_and(|ev| tray.update(|t| t.handle(&ev)).unwrap_or(false));
        if !ok {
            log::warn!("rejected request from {:?}", req.remote_addr());
        }
        let _ = req.respond(tiny_http::Response::empty(if ok { 200 } else { 400 }));
    }
    Ok(())
}

#[test]
fn stop_then_prompt() {
    let mut t = Tray::default();
    let ev = |name: &str| serde_json::json!({"hook_event_name": name, "session_id": "s1", "cwd": "/w"});
    assert!(t.handle(&ev("Stop")));
    assert_eq!(t.waiting.get("s1").map(String::as_str), Some("/w"));
    assert!(t.handle(&ev("UserPromptSubmit")));
    assert!(t.waiting.is_empty());
    assert!(!t.handle(&serde_json::json!({"foo": 1})));
}

#[test]
fn config_precedence() {
    let file = || FileConfig { listen: Some("file:1".into()) };
    let args = |a: &[&str]| a.iter().map(Into::into).collect::<Vec<std::ffi::OsString>>();
    assert_eq!(load_config(args(&[APP, "--listen", "cli:1"]), file()), "cli:1");
    assert_eq!(load_config(args(&[APP]), file()), "file:1");
    assert_eq!(load_config(args(&[APP]), FileConfig::default()), "127.0.0.1:9876");
}
