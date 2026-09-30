//! The validator is the single enforcement point.
//! Config and CLI paths are hostile input — every path is canonicalized
//! before any check (the raw string is never trusted).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;

use crate::driver::Mount;

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

/// Where the derived mounts land inside the box (the profile
/// `mount_dest` key).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MountDest {
    /// Under the mount base: `<base>/<basename>` — the default. The
    /// base is the profile `workdir` override, else the image's own
    /// WORKDIR, else `/work`.
    Basename,
    /// At the host path itself: each mount lands at its own full
    /// (canonical) path.
    Absolute,
}

impl MountDest {
    /// Parse the `mount_dest` keyword; absent (or empty) is the default
    /// `Basename`.
    pub fn parse(v: Option<&str>) -> Result<Self> {
        match v {
            None | Some("") => Ok(Self::Basename),
            Some("basename") => Ok(Self::Basename),
            Some("absolute") => Ok(Self::Absolute),
            Some(v) => bail!("mount_dest: {v:?} must be 'basename' or 'absolute'"),
        }
    }
}

/// Mount derivation (image-as-distribution): each host path from the
/// CLI is mounted per the `MountDest` policy — `<workdir>/<basename>`
/// under the mount base, or at the host path itself.
pub fn derive_mounts(paths: &[String], workdir: &str, dest: &MountDest) -> Result<Vec<Mount>> {
    paths
        .iter()
        .map(|p| {
            let host = canonicalize(p)?;
            let dest = match dest {
                MountDest::Absolute => host.to_string_lossy().to_string(),
                MountDest::Basename => {
                    let base = host
                        .file_name()
                        .and_then(|f| f.to_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("cannot derive a mount name from '{p}' (no basename)")
                        })?
                        .to_string();
                    format!("{}/{}", workdir.trim_end_matches('/'), base)
                }
            };
            Ok(Mount { host, dest })
        })
        .collect::<Result<Vec<_>>>()
}

/// Podman `--mount` specs from the config `mounts` key — passthrough:
/// podman is the interpreter (argv-vector, never a shell). Shape is
/// checked (hostile input); the spec is trusted.
pub(crate) fn validate_mount_specs(specs: &[String]) -> Result<()> {
    for s in specs {
        if s.trim().is_empty() || s.contains(' ') {
            bail!("mounts: {s:?} must be a podman --mount spec (no spaces)");
        }
    }
    Ok(())
}

/// Options appended to each derived bind mount (the config
/// `mount_options` key) — podman interprets them (argv-vector safe).
/// Shape-checked as hostile input: no spaces, each element is a
/// comma-separated list of `key=value` tokens, and the keys steelbx
/// itself sets on a derived mount (`type`, `src`/`source`, `dst`/
/// `destination`) cannot be re-specified (they would clash with the
/// auto-built `type=bind,src=…,dst=…`).
pub fn validate_mount_options(opts: &[String]) -> Result<()> {
    for o in opts {
        let o = o.trim();
        if o.is_empty() {
            bail!("mount_options: an option must not be empty");
        }
        if o.contains(' ') {
            bail!("mount_options: {o:?} must not contain spaces");
        }
        for seg in o.split(',') {
            let (k, v) = seg.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("mount_options: {seg:?} must be a key=value option")
            })?;
            if k.is_empty() || v.is_empty() {
                bail!("mount_options: {seg:?} must be a non-empty key=value option");
            }
            if matches!(k, "type" | "src" | "source" | "dst" | "destination") {
                bail!(
                    "mount_options: {k:?} is set by steelbx on a derived mount \
                     and cannot be overridden"
                );
            }
        }
    }
    Ok(())
}

/// Whether a podman `--mount` spec (a comma-separated `key=value`
/// list) declares a volume mount (`type=volume`).
pub fn is_volume_mount_spec(spec: &str) -> bool {
    spec.split(',').any(|p| {
        p.split_once('=')
            .is_some_and(|(k, v)| k == "type" && v == "volume")
    })
}

/// The volume name a podman `--mount` spec declares: the `source`
/// (or `name`) key, only for `type=volume` specs. `None` for
/// non-volume specs and for anonymous (unnamed) volume mounts.
pub fn volume_name_from_mount_spec(spec: &str) -> Option<String> {
    if !is_volume_mount_spec(spec) {
        return None;
    }
    spec.split(',').find_map(|part| {
        part.split_once('=').and_then(|(k, v)| {
            if (k == "source" || k == "name") && !v.is_empty() {
                Some(v.to_string())
            } else {
                None
            }
        })
    })
}

/// The named volumes across the `mounts` specs, in declaration order,
/// deduped. Anonymous (unnamed) volume mounts have no name and
/// contribute nothing.
pub fn volume_names_from_specs(specs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in specs {
        if let Some(n) = volume_name_from_mount_spec(s) {
            if !out.iter().any(|x| x == &n) {
                out.push(n);
            }
        }
    }
    out
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
    fn mount_dest_basename_is_workdir_join_basename() {
        let t = tempfile::tempdir().unwrap();
        let proj = t.path().join("foo");
        std::fs::create_dir(&proj).unwrap();
        let mounts = derive_mounts(
            &[proj.to_str().unwrap().to_string()],
            "/root/.pi",
            &MountDest::Basename,
        )
        .unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].dest, "/root/.pi/foo");
    }

    #[test]
    fn mount_dest_absolute_keeps_the_full_path() {
        let t = tempfile::tempdir().unwrap();
        let proj = t.path().join("foo");
        std::fs::create_dir(&proj).unwrap();
        let mounts = derive_mounts(
            &[proj.to_str().unwrap().to_string()],
            "/root/.pi",
            &MountDest::Absolute,
        )
        .unwrap();
        assert_eq!(
            mounts[0].dest,
            proj.canonicalize().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn mount_dest_keyword_is_shape_checked() {
        assert_eq!(MountDest::parse(None).unwrap(), MountDest::Basename);
        assert_eq!(MountDest::parse(Some("")).unwrap(), MountDest::Basename);
        assert_eq!(
            MountDest::parse(Some("basename")).unwrap(),
            MountDest::Basename
        );
        assert_eq!(
            MountDest::parse(Some("absolute")).unwrap(),
            MountDest::Absolute
        );
        assert!(MountDest::parse(Some("wherever")).is_err());
    }

    #[test]
    fn missing_host_path_fails() {
        let mounts = derive_mounts(
            &["/nonexistent-steelbx-test".to_string()],
            "/work",
            &MountDest::Basename,
        );
        assert!(mounts.is_err());
    }

    #[test]
    fn tilde_paths_are_expanded() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return; // no HOME to test against
        };
        let dir = home.join("steelbx-test-foo");
        std::fs::create_dir_all(&dir).unwrap();
        let mounts = derive_mounts(
            &["~/steelbx-test-foo".to_string()],
            "/work",
            &MountDest::Basename,
        )
        .unwrap();
        assert_eq!(mounts[0].host, dir.canonicalize().unwrap());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn mount_specs_shape_is_checked_value_is_trusted() {
        assert!(validate_mount_specs(&["type=devpts,destination=/dev/pts".to_string()]).is_ok());
        assert!(validate_mount_specs(&[]).is_ok());
        for bad in ["", "type=bind,source=/x dest=/y"] {
            assert!(
                validate_mount_specs(&[bad.to_string()]).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn mount_options_are_shape_checked() {
        // Well-formed options (single or comma-separated) pass.
        assert!(validate_mount_options(&["chown=true".to_string()]).is_ok());
        assert!(validate_mount_options(&["ro=true,idmap=true".to_string()]).is_ok());
        assert!(validate_mount_options(&[]).is_ok());
        // Empty, a space, a bare key, an empty key, or an empty value are
        // rejected.
        for bad in ["", "chown = true", "chown", "=true", "chown="] {
            assert!(
                validate_mount_options(&[bad.to_string()]).is_err(),
                "{bad:?} should be rejected"
            );
        }
        // A key steelbx sets on a derived mount cannot be re-specified.
        for bad in [
            "type=volume",
            "src=/x",
            "source=/x",
            "dst=/y",
            "destination=/y",
        ] {
            assert!(
                validate_mount_options(&[bad.to_string()]).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn volume_names_are_extracted_from_mount_specs() {
        // source (the primary form) and name (the alternate) are both
        // read; declaration order is kept; duplicates collapse.
        assert_eq!(
            volume_names_from_specs(&[
                "type=volume,source=steelbx-it-vol,destination=/data".to_string(),
                "type=volume,name=steelbx-it-vol,destination=/data2".to_string(),
                "type=volume,source=other,destination=/x".to_string(),
            ]),
            vec!["steelbx-it-vol".to_string(), "other".to_string()]
        );
        // Non-volume specs contribute nothing; an unnamed (anonymous)
        // volume contributes nothing either.
        assert_eq!(
            volume_names_from_specs(&[
                "type=bind,src=/a,dst=/b".to_string(),
                "type=devpts,destination=/dev/pts".to_string(),
                "type=volume,destination=/data".to_string(),
            ]),
            Vec::<String>::new()
        );
        assert!(is_volume_mount_spec("type=volume,source=v,destination=/d"));
        assert!(!is_volume_mount_spec("type=bind,src=/a,dst=/b"));
        // An empty source is treated as unnamed.
        assert_eq!(
            volume_name_from_mount_spec("type=volume,source=,destination=/d"),
            None
        );
    }

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
