//! Configuration shared by the compositor and the shell.
//!
//! Both binaries read the same file. Keeping the schema in one crate is what
//! stops the panel's idea of the accent colour from drifting away from the
//! compositor's idea of the focus-ring colour.

use serde::{Deserialize, Serialize};

/// Top-level configuration, deserialized from `~/.config/huginn/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub layout: LayoutConfig,
    pub commands: CommandsConfig,
}

/// Programs the desktop launches on the user's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandsConfig {
    /// Terminal spawned by the compositor's spawn binding.
    pub terminal: String,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            // RavenLinux ships its own terminal, so that is what the desktop
            // opens. Naming a third-party terminal here would make the default
            // desktop depend on software the distro does not control.
            terminal: "raven-terminal".to_owned(),
        }
    }
}

/// Tiling parameters. Mirrors the fields of `huginn_core::layout::Columns`;
/// `huginn-comp` maps between them so that `huginn-core` stays free of serde.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutConfig {
    /// One of the names reported by `huginn_core::layout::Layout::name`.
    pub default: String,
    pub gap: i32,
    pub master_ratio: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            default: "columns".to_owned(),
            gap: 8,
            master_ratio: 0.5,
        }
    }
}

/// Failure to load a configuration file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string(&cfg).expect("default config is serializable");
        let back: Config = toml::from_str(&text).expect("and deserializable");
        assert_eq!(cfg, back);
    }

    #[test]
    fn an_empty_file_yields_the_defaults() {
        assert_eq!(toml::from_str::<Config>("").expect("empty is valid"), Config::default());
    }

    #[test]
    fn a_partial_file_fills_in_the_rest() {
        let cfg: Config = toml::from_str("[layout]\ngap = 20\n").expect("partial is valid");
        assert_eq!(cfg.layout.gap, 20);
        assert_eq!(cfg.layout.master_ratio, LayoutConfig::default().master_ratio);
    }

    #[test]
    fn the_default_terminal_is_ravens_own() {
        assert_eq!(Config::default().commands.terminal, "raven-terminal");
    }

    #[test]
    fn a_typo_is_rejected_rather_than_silently_ignored() {
        // deny_unknown_fields: a misspelled key should be a visible error, not
        // a setting that mysteriously has no effect.
        let err = toml::from_str::<Config>("[layout]\ngaps = 20\n");
        assert!(err.is_err(), "unknown key was accepted");
    }
}
