use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Default, Deserialize)]
pub struct SshConfig {
    /// Enable zlib compression for SSH connections.
    #[serde(default)]
    pub compression: bool,
}

#[derive(Default, Deserialize)]
pub struct Config {
    /// Default program to run (e.g. "emacs -nw"). Overridden by `-- <PROGRAM>` on CLI.
    pub program: Option<String>,
    /// Extra environment variables. Values can reference $MUX_SESSION and other env vars.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// SSH options for remote sessions.
    #[serde(default)]
    pub ssh: SshConfig,
}

impl Config {
    /// Load config from `$XDG_CONFIG_HOME/mux/config.toml` (or `~/.config/mux/config.toml`).
    /// Returns default config if the file doesn't exist.
    pub fn load() -> Self {
        let path = config_path();
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Config::default(),
        };
        match toml::from_str(&contents) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: failed to parse {}: {e}", path.display());
                Config::default()
            }
        }
    }

    /// Resolve the program to run. CLI args take priority, then config, then $SHELL, then /bin/bash.
    pub fn resolve_program(&self, cli_program: &[String]) -> Vec<String> {
        if !cli_program.is_empty() {
            return cli_program.to_vec();
        }
        if let Some(ref prog) = self.program {
            let parts: Vec<String> = prog.split_whitespace().map(String::from).collect();
            if !parts.is_empty() {
                return parts;
            }
        }
        Vec::new() // PtyHandle::spawn will fall back to $SHELL / /bin/bash
    }

    /// Resolve config env vars, expanding `$VAR` and `${VAR}` references.
    /// `MUX_SESSION` is available for expansion.
    pub fn resolve_env(&self, session_name: &str) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| (k.clone(), expand_vars(v, session_name)))
            .collect()
    }
}

fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(dir).join("mux/config.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/mux/config.toml")
    } else {
        PathBuf::from("/etc/mux/config.toml")
    }
}

/// Expand `$VAR` and `${VAR}` references in a string.
/// Checks `MUX_SESSION` first, then the process environment.
fn expand_vars(value: &str, session_name: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' {
            let mut var_name = String::new();
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next(); // skip '{'
                while let Some(&ch) = chars.peek() {
                    if ch == '}' {
                        chars.next();
                        break;
                    }
                    var_name.push(chars.next().unwrap());
                }
            } else {
                while let Some(&ch) = chars.peek() {
                    if ch.is_ascii_alphanumeric() || ch == '_' {
                        var_name.push(chars.next().unwrap());
                    } else {
                        break;
                    }
                }
            }
            if var_name.is_empty() {
                result.push('$');
                if braced {
                    result.push('{');
                    result.push('}');
                }
            } else if var_name == "MUX_SESSION" {
                result.push_str(session_name);
            } else {
                if let Ok(val) = std::env::var(&var_name) {
                    result.push_str(&val);
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}
