//! Redline's settings, kept in ~/.redline/config.json. Missing keys take their defaults, so the
//! file can be edited by hand or left out altogether.

use std::path::PathBuf;

use serde_json::{Value, json};

#[derive(Clone)]
pub struct Config {
    /// The project the agent works in. Without one, turns are only saved for the MCP bridge.
    pub project: Option<PathBuf>,
    /// Which agent to start on each turn: "off", "codex", "claude" or "custom".
    pub agent: String,
    /// For "custom": a shell command. It gets REDLINE_PROMPT, REDLINE_IMAGE, REDLINE_TURN and
    /// REDLINE_PROJECT in its environment and runs in the project folder.
    pub agent_command: String,
    pub pen_key: String,
    pub send_key: String,
    pub clear_key: String,
    pub note_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            project: None,
            agent: "off".into(),
            agent_command: String::new(),
            pen_key: "ctrl+alt+d".into(),
            send_key: "ctrl+alt+s".into(),
            clear_key: "ctrl+alt+c".into(),
            note_key: "ctrl+alt+n".into(),
        }
    }
}

/// Where Redline keeps its settings, turns and speech model: ~/.redline, or $REDLINE_HOME.
pub fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("REDLINE_HOME") { return PathBuf::from(home); }
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".redline")
}

fn file() -> PathBuf {
    home().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        let mut config = Config::default();
        let Ok(text) = std::fs::read_to_string(file()) else { return config };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            eprintln!("redline: {} is not valid JSON; using defaults", file().display());
            return config;
        };
        let text_of = |key: &str| value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        config.project = text_of("project").map(PathBuf::from);
        if let Some(agent) = text_of("agent") { config.agent = agent; }
        if let Some(command) = text_of("agent_command") { config.agent_command = command; }
        let keys = value.get("hotkeys");
        let key_of = |name: &str| keys.and_then(|k| k.get(name)).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        if let Some(key) = key_of("pen") { config.pen_key = key; }
        if let Some(key) = key_of("send") { config.send_key = key; }
        if let Some(key) = key_of("clear") { config.clear_key = key; }
        if let Some(key) = key_of("note") { config.note_key = key; }
        config
    }

    pub fn save(&self) {
        let value = json!({
            "project": self.project.as_ref().map(|p| p.display().to_string()),
            "agent": self.agent,
            "agent_command": self.agent_command,
            "hotkeys": { "pen": self.pen_key, "send": self.send_key, "clear": self.clear_key, "note": self.note_key },
        });
        let _ = std::fs::create_dir_all(home());
        if let Err(e) = std::fs::write(file(), serde_json::to_string_pretty(&value).unwrap_or_default() + "\n") {
            eprintln!("redline: could not save settings: {e}");
        }
    }
}
