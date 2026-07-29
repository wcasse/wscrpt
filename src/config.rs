use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

const MAX_LANGUAGE_SERVERS: usize = 32;
const MAX_SERVER_ARGUMENTS: usize = 128;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub tab_width: usize,
    pub insert_spaces: bool,
    pub line_numbers: bool,
    pub mouse: bool,
    pub scroll_margin: usize,
    pub osc52_copy: bool,
    /// When true, Save requests LSP document formatting before writing disk.
    /// Formatting remains explicit (`Esc c f`) when this is false.
    pub format_on_save: bool,
    pub language_servers: Vec<LanguageServerConfig>,
}

/// A language server explicitly authorized by the user's global config.
///
/// Workspace files cannot add executable commands. This keeps merely opening
/// an untrusted checkout from silently selecting a workspace-supplied binary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LanguageServerConfig {
    pub name: String,
    pub extensions: Vec<String>,
    pub language_id: String,
    pub argv: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tab_width: 4,
            insert_spaces: true,
            line_numbers: true,
            mouse: false,
            scroll_margin: 3,
            osc52_copy: true,
            format_on_save: false,
            language_servers: Vec::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<(Self, Option<String>), String> {
        let Some(path) = config_path() else {
            return Ok((Self::default(), None));
        };
        if !path.exists() {
            return Ok((Self::default(), None));
        }
        let source = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let config = Self::parse(&source, &path.display().to_string())?;
        Ok((config, Some(path.display().to_string())))
    }

    pub fn parse(source: &str, source_name: &str) -> Result<Self, String> {
        let mut config: Self =
            toml::from_str(source).map_err(|error| format!("invalid {source_name}: {error}"))?;
        if config.tab_width == 0 || config.tab_width > 16 {
            return Err(format!("{source_name}: tab_width must be 1 through 16"));
        }
        config.scroll_margin = config.scroll_margin.min(20);
        config.validate_language_servers(source_name)?;
        Ok(config)
    }

    pub fn language_server_for(&self, path: &Path) -> Option<&LanguageServerConfig> {
        let extension = path.extension()?.to_str()?;
        self.language_servers.iter().find(|server| {
            server
                .extensions
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(extension))
        })
    }

    fn validate_language_servers(&self, source_name: &str) -> Result<(), String> {
        if self.language_servers.len() > MAX_LANGUAGE_SERVERS {
            return Err(format!(
                "{source_name}: at most {MAX_LANGUAGE_SERVERS} language servers may be configured"
            ));
        }
        let mut names = std::collections::HashSet::new();
        let mut extensions = std::collections::HashSet::new();
        for (index, server) in self.language_servers.iter().enumerate() {
            let location = format!("{source_name}: language_servers[{index}]");
            if server.name.is_empty()
                || server.name.trim() != server.name
                || server.name.chars().any(char::is_control)
            {
                return Err(format!(
                    "{location}.name must be non-empty, trimmed, and contain no controls"
                ));
            }
            if !names.insert(server.name.to_ascii_lowercase()) {
                return Err(format!(
                    "{location}.name duplicates another language server name"
                ));
            }
            if server.language_id.is_empty()
                || server.language_id.chars().any(char::is_control)
                || server.language_id.len() > 128
            {
                return Err(format!(
                    "{location}.language_id must be 1 through 128 bytes with no controls"
                ));
            }
            if server.extensions.is_empty() {
                return Err(format!("{location}.extensions must not be empty"));
            }
            for extension in &server.extensions {
                let normalized = extension.trim_start_matches('.').to_ascii_lowercase();
                if normalized.is_empty()
                    || normalized.len() > 32
                    || normalized.chars().any(|character| {
                        character.is_control()
                            || character.is_whitespace()
                            || matches!(character, '/' | '\\')
                    })
                {
                    return Err(format!(
                        "{location}.extensions contains invalid extension {extension:?}"
                    ));
                }
                if extension.starts_with('.') {
                    return Err(format!(
                        "{location}.extensions must omit the leading dot in {extension:?}"
                    ));
                }
                if !extensions.insert(normalized) {
                    return Err(format!(
                        "{location}.extensions overlaps another configured server"
                    ));
                }
            }
            if server.argv.is_empty() || server.argv.len() > MAX_SERVER_ARGUMENTS {
                return Err(format!(
                    "{location}.argv must contain 1 through {MAX_SERVER_ARGUMENTS} arguments"
                ));
            }
            for (argument_index, argument) in server.argv.iter().enumerate() {
                if argument.is_empty() || argument.contains('\0') {
                    return Err(format!(
                        "{location}.argv[{argument_index}] must be non-empty and contain no NUL"
                    ));
                }
            }
        }
        Ok(())
    }
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(root) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(root).join("wscrpt/config.toml"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/wscrpt/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_selects_an_explicit_language_server() {
        let config = Config::parse(
            r#"
tab_width = 2

[[language_servers]]
name = "rust-analyzer"
extensions = ["rs"]
language_id = "rust"
argv = ["rust-analyzer"]
"#,
            "test config",
        )
        .unwrap();
        let server = config
            .language_server_for(Path::new("src/main.RS"))
            .unwrap();
        assert_eq!(server.name, "rust-analyzer");
        assert_eq!(server.language_id, "rust");
        assert!(config.language_server_for(Path::new("README.md")).is_none());
    }

    #[test]
    fn rejects_ambiguous_extensions_and_shell_style_empty_commands() {
        let ambiguous = r#"
[[language_servers]]
name = "one"
extensions = ["rs"]
language_id = "rust"
argv = ["one"]

[[language_servers]]
name = "two"
extensions = ["RS"]
language_id = "rust"
argv = ["two"]
"#;
        assert!(Config::parse(ambiguous, "test").is_err());

        let empty = r#"
[[language_servers]]
name = "bad"
extensions = ["rs"]
language_id = "rust"
argv = []
"#;
        assert!(Config::parse(empty, "test").is_err());
    }
}
