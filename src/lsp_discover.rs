//! PATH-only language-server discovery helpers.
//!
//! Discovery never launches a language server and never reads workspace files
//! for executables. It only inspects `PATH` for well-known server binaries so
//! `wscrpt --health` and the default config comments can tell the operator what is
//! already installed on the remote host.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// One PATH-discovered language-server candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredLanguageServer {
    pub name: &'static str,
    pub language_id: &'static str,
    pub extensions: &'static [&'static str],
    pub executable: PathBuf,
    pub argv0: &'static str,
}

/// Well-known language servers the health probe may report when present.
pub const KNOWN_LANGUAGE_SERVERS: &[KnownLanguageServer] = &[
    KnownLanguageServer {
        name: "rust-analyzer",
        language_id: "rust",
        extensions: &["rs"],
        argv0: "rust-analyzer",
    },
    KnownLanguageServer {
        name: "pyright",
        language_id: "python",
        extensions: &["py", "pyi"],
        argv0: "pyright-langserver",
    },
    KnownLanguageServer {
        name: "typescript-language-server",
        language_id: "typescript",
        extensions: &["ts", "tsx", "js", "jsx"],
        argv0: "typescript-language-server",
    },
    KnownLanguageServer {
        name: "gopls",
        language_id: "go",
        extensions: &["go"],
        argv0: "gopls",
    },
    KnownLanguageServer {
        name: "clangd",
        language_id: "cpp",
        extensions: &["c", "h", "cc", "cpp", "hpp"],
        argv0: "clangd",
    },
    KnownLanguageServer {
        name: "lua-language-server",
        language_id: "lua",
        extensions: &["lua"],
        argv0: "lua-language-server",
    },
    KnownLanguageServer {
        name: "bash-language-server",
        language_id: "shellscript",
        extensions: &["sh", "bash"],
        argv0: "bash-language-server",
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnownLanguageServer {
    pub name: &'static str,
    pub language_id: &'static str,
    pub extensions: &'static [&'static str],
    pub argv0: &'static str,
}

/// Resolve a command name to an executable regular file on `PATH`.
///
/// Absolute paths are accepted only when they already name an executable file.
/// This never executes the candidate.
pub fn resolve_executable(command: &str) -> Option<PathBuf> {
    if command.is_empty() || command.contains('\0') {
        return None;
    }
    let path = Path::new(command);
    if path.is_absolute() || command.contains('/') || command.contains('\\') {
        return executable_file(path);
    }
    let path_var = env::var_os("PATH")?;
    for directory in env::split_paths(&path_var) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(command);
        if let Some(resolved) = executable_file(&candidate) {
            return Some(resolved);
        }
    }
    None
}

/// Scan `PATH` for known language-server executables.
pub fn discover_language_servers() -> Vec<DiscoveredLanguageServer> {
    let mut found = Vec::new();
    for known in KNOWN_LANGUAGE_SERVERS {
        if let Some(executable) = resolve_executable(known.argv0) {
            found.push(DiscoveredLanguageServer {
                name: known.name,
                language_id: known.language_id,
                extensions: known.extensions,
                executable,
                argv0: known.argv0,
            });
        }
    }
    found
}

/// Format TOML snippets the operator can paste into the global config.
pub fn suggested_config_snippets(servers: &[DiscoveredLanguageServer]) -> String {
    if servers.is_empty() {
        return "# No well-known language servers found on PATH.\n".to_owned();
    }
    let mut text =
        String::from("# Discovered on PATH (not auto-enabled). Uncomment to authorize:\n");
    for server in servers {
        text.push_str(&format!(
            "# [[language_servers]]\n# name = \"{}\"\n# extensions = [{}]\n# language_id = \"{}\"\n# argv = [\"{}\"]\n",
            server.name,
            server
                .extensions
                .iter()
                .map(|extension| format!("\"{extension}\""))
                .collect::<Vec<_>>()
                .join(", "),
            server.language_id,
            server.argv0
        ));
        if server.argv0 == "typescript-language-server" {
            text.push_str("# # argv = [\"typescript-language-server\", \"--stdio\"]\n");
        }
        if server.argv0 == "pyright-langserver" {
            text.push_str("# # argv = [\"pyright-langserver\", \"--stdio\"]\n");
        }
        if server.argv0 == "bash-language-server" {
            text.push_str("# # argv = [\"bash-language-server\", \"start\"]\n");
        }
        text.push('\n');
    }
    text
}

fn executable_file(path: &Path) -> Option<PathBuf> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_executable_finds_sh_on_unix() {
        #[cfg(unix)]
        {
            let resolved = resolve_executable("sh");
            assert!(resolved.is_some(), "expected sh on PATH");
        }
    }

    #[test]
    fn empty_command_is_rejected() {
        assert!(resolve_executable("").is_none());
    }
}
