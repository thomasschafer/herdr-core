//! `~/.config/herdr-core/config.toml`. A missing file means defaults.

use crate::rpc::RpcError;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Default)]
pub struct Config {
    /// The chord that detaches `hc attach`: `ctrl+<key>`, default `ctrl+\`.
    pub detach_key: Option<String>,
    /// Seconds a `wait` runs before it checks in (default 180).
    pub wait_timeout: Option<f64>,
    /// Notes to parent agents and `<hc>` tags on messages between agents, on
    /// or off (default on). Off: hc types nothing on its own.
    pub notes: Option<bool>,
    /// Seconds without a person typing before a note goes in (default 3).
    pub notes_quiet: Option<f64>,
    /// Size of a terminal nobody is typing in, `COLSxROWS` (default 200x120).
    pub headless: Option<String>,
    /// Scrollback kept per live terminal, in MB (default 5).
    pub history_mb: Option<f64>,
    /// Where the core listens (default under $XDG_RUNTIME_DIR). `HC_SOCKET` wins.
    pub socket: Option<String>,
    /// Seconds another machine gets to answer a call (a wait gets its own
    /// timeout on top), and to send anything at all before its line is
    /// given up and dialed again (default 30).
    pub machine_timeout: Option<f64>,
    /// Programs a kept terminal starts again after a loss (default none): if
    /// one led its foreground job when the program was lost, its command line
    /// is typed into the replacement shell.
    #[serde(default)]
    pub restore_commands: Vec<RestoreCommand>,
}

/// A `restore_commands` entry. It is typed into a shell and found through
/// PATH, so it is a plain program name: no path, no option, nothing to quote.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(try_from = "String")]
pub struct RestoreCommand(String);

impl RestoreCommand {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RestoreCommand {
    type Error = String;

    fn try_from(name: String) -> Result<Self, String> {
        let plain = name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
        match plain && name.bytes().any(|b| b.is_ascii_alphanumeric()) && !name.starts_with('-') {
            true => Ok(RestoreCommand(name)),
            false => Err(format!("restore_commands {name:?}: use a program name like \"hx\", not a path")),
        }
    }
}

impl Config {
    pub fn headless(&self) -> Result<(u16, u16), RpcError> {
        let Some(spec) = &self.headless else { return Ok((200, 120)) };
        spec.split_once('x')
            .and_then(|(c, r)| Some((c.trim().parse().ok()?, r.trim().parse().ok()?)))
            .filter(|(c, r): &(u16, u16)| *c > 0 && *r > 0)
            .ok_or_else(|| invalid(format!("headless {spec:?}: use COLSxROWS, like \"200x120\"")))
    }

    pub fn machine_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.machine_timeout.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(30.0))
    }

    pub fn history_bytes(&self) -> usize {
        (self.history_mb.unwrap_or(5.0).max(0.0) * 1024.0 * 1024.0) as usize
    }
}

fn invalid(message: String) -> RpcError {
    RpcError::new("config", message, "fix ~/.config/herdr-core/config.toml")
}

pub fn load(path: &Path) -> Result<Config, RpcError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| invalid(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(invalid(format!("{}: {e}", path.display()))),
    }
}

/// A detach chord: the control byte it sends, and its key code for the
/// kitty and modifyOtherKeys encodings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetachKey {
    pub byte: u8,
    pub code: u32,
}

impl Config {
    pub fn detach_key(&self) -> Result<DetachKey, RpcError> {
        let spec = self.detach_key.as_deref().unwrap_or("ctrl+\\");
        let key = spec.strip_prefix("ctrl+").and_then(|k| {
            let mut chars = k.chars();
            let c = chars.next()?.to_ascii_lowercase();
            (chars.next().is_none() && (c.is_ascii_lowercase() || "@[\\]^_".contains(c))).then_some(c)
        });
        let c = key.ok_or_else(|| {
            invalid(format!("detach_key {spec:?}: use a ctrl chord like \"ctrl+\\\\\" or \"ctrl+]\""))
        })?;
        Ok(DetachKey { byte: c as u8 & 0x1f, code: c as u32 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &str) -> Result<DetachKey, RpcError> {
        Config { detach_key: Some(spec.into()), ..Default::default() }.detach_key()
    }

    #[test]
    fn detach_keys_are_ctrl_chords_only() {
        assert_eq!(Config::default().detach_key().unwrap(), DetachKey { byte: 0x1c, code: 92 });
        assert_eq!(key("ctrl+]").unwrap(), DetachKey { byte: 0x1d, code: 93 });
        assert_eq!(key("ctrl+A").unwrap(), DetachKey { byte: 0x01, code: 97 });
        for bad in ["alt+x", "ctrl+", "ctrl+ab", "ctrl+1", "x"] {
            assert_eq!(key(bad).unwrap_err().kind, "config", "{bad}");
        }
    }

    #[test]
    fn restore_commands_are_plain_program_names() {
        let names = |text: &str| toml::from_str::<Config>(text).map(|c| c.restore_commands);
        assert!(names("").unwrap().is_empty());
        let ok = names("restore_commands = [\"hx\", \"nvim.appimage\", \"my_ed-2\"]").unwrap();
        assert_eq!(ok.iter().map(RestoreCommand::as_str).collect::<Vec<_>>(), ["hx", "nvim.appimage", "my_ed-2"]);
        for bad in ["", "/usr/bin/hx", "bin/hx", "-hx", "..", "h x", "hx;rm", "C:\\hx.exe"] {
            let err = names(&format!("restore_commands = [{bad:?}]")).map(drop).unwrap_err().to_string();
            assert!(err.contains("not a path"), "{bad:?}: {err}");
        }
    }
}
