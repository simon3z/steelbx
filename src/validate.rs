//! The validator is the single enforcement point.
//! Config and CLI paths are hostile input — every path is canonicalized
//! before any check (the raw string is never trusted).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;

/// Expands a leading `~` against $HOME.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    if path == "~" {
        home
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Canonicalize (resolve symlinks). Fails if the path does not exist.
/// In remote mode (`CONTAINER_HOST` set), the path is on the remote
/// host — skip the filesystem check and trust it.
pub(crate) fn canonicalize(path: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(path);
    if crate::driver::Podman::is_remote() {
        // Remote: the path is on the remote host; trust it.
        Ok(expanded)
    } else {
        std::fs::canonicalize(&expanded).with_context(|| format!("resolving path '{path}'"))
    }
}

/// Namespace/identity values (`cgroupns`, `ipc`, `pid`, `userns`, `user`)
/// and `ulimits` elements: a single token, no spaces (hostile
/// input); podman is the interpreter (the value is trusted — argv-
/// vector safe).
pub(crate) fn validate_ns_value(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.contains(' ') {
        bail!("{field}: {value:?} must be a single token (no spaces)");
    }
    Ok(())
}

/// Env overrides: the name must be letters/digits/underscore (the value
/// is passed verbatim to podman `--env`, argv-vector safe).
pub(crate) fn validate_env(env: &IndexMap<String, String>) -> Result<()> {
    for k in env.keys() {
        if !is_valid_env_name(k) {
            bail!("env: {k:?} is not a valid variable name ([A-Za-z0-9_] only)");
        }
    }
    Ok(())
}

pub fn is_valid_env_name(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Security options: `key=value` (podman `--security-opt` shape — e.g.
/// `label=disable`); the value is podman's to interpret. Shape-checked,
/// like `extra_hosts` (hostile input).
pub(crate) fn validate_security_opts(opts: &[String]) -> Result<()> {
    for o in opts {
        let (k, v) = o
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("security_opts: {o:?} must be 'key=value'"))?;
        if k.trim().is_empty() || v.trim().is_empty() || o.contains(' ') {
            bail!("security_opts: {o:?} must be 'key=value' (no spaces)");
        }
    }
    Ok(())
}

/// Extra hosts: `name:address`; the address is podman's to resolve (IP,
/// or the `host-gateway` keyword) — we only check the shape, never the
/// value.
pub(crate) fn validate_extra_hosts(extra_hosts: &[String]) -> Result<()> {
    for h in extra_hosts {
        let (name, addr) = h
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("extra_hosts: {h:?} must be 'name:address'"))?;
        if name.trim().is_empty() || addr.trim().is_empty() || h.contains(' ') {
            bail!("extra_hosts: {h:?} must be 'name:address' (no spaces)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ns_values_are_single_tokens() {
        assert!(validate_ns_value("pid", "host").is_ok());
        assert!(validate_ns_value("user", "root:root").is_ok());
        assert!(validate_ns_value("ulimits", "host").is_ok());
        for bad in ["", "  ", "host host", "host host"] {
            assert!(
                validate_ns_value("pid", bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn env_names_are_shape_checked() {
        let mut good = IndexMap::new();
        good.insert("TERM".to_string(), "xterm-256color".to_string());
        assert!(validate_env(&good).is_ok());

        let mut bad = IndexMap::new();
        bad.insert("with space".to_string(), String::new());
        assert!(validate_env(&bad).is_err());
    }

    #[test]
    fn security_opts_shape_is_checked_value_is_trusted() {
        assert!(validate_security_opts(&["label=disable".to_string()]).is_ok());
        assert!(validate_security_opts(&[]).is_ok());
        for bad in ["label", "=disable", "label=", "label = disable"] {
            assert!(
                validate_security_opts(&[bad.to_string()]).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn extra_hosts_shape_is_checked_value_is_trusted() {
        assert!(validate_extra_hosts(&["llm.example.com:host-gateway".to_string()]).is_ok());
        for bad in ["no-colon", ":1.2.3.4", "a b:1.2.3.4", "name:"] {
            assert!(
                validate_extra_hosts(&[bad.to_string()]).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }
}
