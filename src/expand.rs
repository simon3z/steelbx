//! Caller-env expansion for profile values: `$IDENT`, `${IDENT}`,
//! `${IDENT:-default}`, and `$$`.
//!
//! Two layers:
//! - [`expand_env`]: the pure string engine.
//! - The load pipeline: [`pre_expand`] (decide the box name, expand
//!   `[env]` and `image` with `STEELBX_BOX_NAME` available) and
//!   [`expand_rest`] (expand every remaining field). The env files are
//!   read once and threaded between the two passes — no redundant
//!   re-read.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use indexmap::IndexMap;

use crate::config::SteelbxConfig;
use crate::validate::{canonicalize, is_valid_env_name};

/// Caller-env expansion of a config string value: `$IDENT`,
/// `${IDENT}` expand against the caller's environment; `${IDENT:-default}`
/// falls back to `default` when the variable is unset or empty — the
/// default is the text up to the first `}` (no nesting of `${...}`
/// inside it), and that text itself expands against the same
/// environment; `$$` is a literal `$`; any other `$` is an error.
/// Unset references fail loudly (unless a fallback is given). No shell:
/// an expanded *value* is never re-scanned.
pub(crate) fn expand_env(value: &str, env: &HashMap<String, String>, path: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(d) = rest.find('$') {
        out.push_str(&rest[..d]);
        let (next, val) = expand_token(&rest[d..], env, path)?;
        out.push_str(&val);
        rest = next;
    }
    out.push_str(rest);
    Ok(out)
}

/// One expansion reference: `$IDENT`, `${IDENT}`, `${IDENT:-default}`
/// (the fallback: the variable set *and* non-empty wins, else the
/// default), or `$$`. The default is the text up to the first `}` (no
/// nesting of `${...}` inside it); it is not expanded here —
/// `expand_token` expands it against the caller's environment. The
/// bare form has no fallback: `$IDENT:-x` is the value plus the
/// literal `:-x` (still never an error, still never a shell).
fn parse_expansion_ref<'a>(
    after: &'a str,
    path: &str,
) -> Result<(String, Option<String>, &'a str)> {
    if let Some(inner) = after.strip_prefix('{') {
        let (inner, close) = inner
            .split_once('}')
            .ok_or_else(|| anyhow!("{path}: unterminated ${{...}} reference (no closing }}"))?;
        return match inner.split_once(":-") {
            Some((id, fallback)) => Ok((id.to_string(), Some(fallback.to_string()), close)),
            None => Ok((inner.to_string(), None, close)),
        };
    }
    let id_len = after
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if id_len == 0 {
        bail!("{path}: '$' must introduce $IDENT, ${{IDENT}} (optionally ${{IDENT:-default}}), or $$ (a literal dollar)");
    }
    Ok((after[..id_len].to_string(), None, &after[id_len..]))
}

fn expand_token<'a>(
    token: &'a str,
    env: &HashMap<String, String>,
    path: &str,
) -> Result<(&'a str, String)> {
    let after = &token[1..];
    if let Some(rest) = after.strip_prefix('$') {
        return Ok((rest, "$".to_string()));
    }
    let (ident, default, rest) = parse_expansion_ref(after, path)?;
    if !is_valid_env_name(&ident) {
        bail!("{path}: {ident:?} is not a valid variable name");
    }
    // Set *and* non-empty wins. Empty with no fallback is a value
    // ("", unchanged); a fallback treats empty the same as unset
    // (the `:-` semantics, not POSIX's unset-only `-`).
    let val = match env.get(&ident) {
        Some(v) if !v.is_empty() => v.clone(),
        Some(_) if default.is_none() => String::new(),
        _ => match default {
            Some(d) => expand_env(&d, env, &format!("{path} (default)"))?,
            None => return Err(anyhow!("{path}: references unset variable {ident:?}")),
        },
    };
    Ok((rest, val))
}

// ---------------------------------------------------------------------------
// Load pipeline (moved from config.rs). The env files are read once here
// and threaded between `pre_expand` and `expand_rest` — no redundant
// re-read.
// ---------------------------------------------------------------------------

/// The pre-pass of the load pipeline: expands what the caller needs
/// before the full pass, in the order that decides the box name first.
/// The final name is decided before `[env]` and `image` expand: the
/// CLI `-n` flag (verbatim), else the profile's `name` key (expanded
/// against the caller env plus `env_files`, empty is absent,
/// shape-checked) with a fresh random token appended (unique by
/// default; `name_unique = false` pins it verbatim). When a name is
/// known, it is `STEELBX_BOX_NAME` in the context for `[env]` and
/// `image` — both may reference it. When no name is declared (a
/// generated name), `STEELBX_BOX_NAME` is not yet known here — a
/// reference in `[env]` or `image` fails loudly; every other value
/// gets the generated name in the full pass. The inline `[env]` values
/// expand against the caller env plus `env_files` plus the known name
/// (they do not reference each other); `image` expands against that
/// plus the expanded `[env]` — so `image = "$IMG"` may refer to a
/// variable declared in `env_files` or `[env]`. The profile's `name`
/// key expands against the caller env plus `env_files` (it is decided
/// first, so it cannot reference `[env]` values). The expanded values
/// are stored in their slots; the full pass (`expand_rest`) does
/// not re-expand them. Returns [`crate::config::PreExpand`] (expanded
/// `image`, declared name, final name, `file_env`).
pub(crate) fn pre_expand(
    cfg: &mut SteelbxConfig,
    caller: &HashMap<String, String>,
    name_flag: Option<&str>,
) -> Result<crate::config::PreExpand> {
    let file_env = load_env_files_env(&cfg.env_files, caller)?;
    let mut ctx = caller.clone();
    merge_ctx(&mut ctx, &file_env);
    // The name, decided first: the CLI `-n` flag (verbatim; it wins —
    // the profile key is replaced by it), else the profile's `name`
    // key. A declared name is unique by default: a fresh random token
    // is appended unless `name_unique = false`.
    let declared = match name_flag {
        Some(n) => Some(n.to_string()),
        None => expand_declared_name(cfg, &ctx)?,
    };
    let final_name = declared.as_ref().map(|d| {
        if name_flag.is_some() || !cfg.name_unique() {
            d.clone()
        } else {
            crate::driver::unique_name(d)
        }
    });
    // When known, the final name is STEELBX_BOX_NAME for [env] and
    // image (it is the name the box is actually created as).
    if let Some(n) = &final_name {
        ctx.insert("STEELBX_BOX_NAME".to_string(), n.clone());
    }
    // [env] values, against caller+files+name (they do not reference
    // each other).
    for (k, v) in cfg.env.iter_mut() {
        *v = expand_env(v, &ctx, &format!("env.{k}"))?;
    }
    // Now the inline [env] feeds every other value too.
    for (k, v) in cfg.env.iter() {
        ctx.insert(k.clone(), v.clone());
    }
    // The image key, against the full context (name + [env]).
    let image = cfg
        .image
        .take()
        .map(|v| expand_env(&v, &ctx, "image"))
        .transpose()?;
    cfg.image = image.clone();
    Ok(crate::config::PreExpand {
        image,
        declared,
        final_name,
        file_env,
    })
}

/// The full pass of the load pipeline: expands every string value not
/// yet expanded — the `network` string, each `extra_hosts` element,
/// each `security_opts` element, each `mounts` spec, and the
/// namespace/identity values (`cgroupns`, `ipc`, `pid`, `userns`,
/// `user`, `workdir`, `ulimits`, `entry`, `init`). Keys never expand
/// (an env key is a name, not a value). The `[env]`, `image`, and
/// `name` slots were already expanded by the pre-pass (`pre_expand`)
/// and are skipped. `box_name` is always defined (the CLI `-n`
/// override, the profile's `name` key, or the generated unique name):
/// it is injected as `STEELBX_BOX_NAME` before the `[env]` values
/// feed the context, so every value — including `[env]` — may
/// reference it, and it is stored in `cfg.name`.
///
/// `file_env` is the parsed `env_files` content from [`pre_expand`] —
/// no re-read.
pub(crate) fn expand_rest(
    cfg: &mut SteelbxConfig,
    caller: &HashMap<String, String>,
    box_name: &str,
    file_env: &IndexMap<String, String>,
) -> Result<()> {
    let mut ctx = caller.clone();
    merge_ctx(&mut ctx, file_env);
    ctx.insert("STEELBX_BOX_NAME".to_string(), box_name.to_string());
    for (k, v) in cfg.env.iter() {
        ctx.insert(k.clone(), v.clone());
    }
    cfg.name = Some(box_name.to_string());
    expand_slot(&mut cfg.network, &ctx, "network")?;
    expand_slot(&mut cfg.cgroupns, &ctx, "cgroupns")?;
    expand_slot(&mut cfg.ipc, &ctx, "ipc")?;
    expand_slot(&mut cfg.pid, &ctx, "pid")?;
    expand_slot(&mut cfg.userns, &ctx, "userns")?;
    expand_slot(&mut cfg.user, &ctx, "user")?;
    expand_slot(&mut cfg.workdir, &ctx, "workdir")?;
    for (i, u) in cfg.ulimits.iter_mut().enumerate() {
        *u = expand_env(u, &ctx, &format!("ulimits[{i}]"))?;
    }
    for (i, e) in cfg.entry.iter_mut().enumerate() {
        *e = expand_env(e, &ctx, &format!("entry[{i}]"))?;
    }
    expand_init(&mut cfg.init, &ctx)?;
    for (i, h) in cfg.extra_hosts.iter_mut().enumerate() {
        *h = expand_env(h, &ctx, &format!("extra_hosts[{i}]"))?;
    }
    for (i, o) in cfg.security_opts.iter_mut().enumerate() {
        *o = expand_env(o, &ctx, &format!("security_opts[{i}]"))?;
    }
    for (i, m) in cfg.mounts.iter_mut().enumerate() {
        *m = expand_env(m, &ctx, &format!("mounts[{i}]"))?;
    }
    Ok(())
}

/// Expand the profile's `name` key (caller+files; empty is absent;
/// shape-checked — a valid podman name, before any token is added).
fn expand_declared_name(
    cfg: &mut SteelbxConfig,
    ctx: &HashMap<String, String>,
) -> Result<Option<String>> {
    let Some(v) = cfg.name.take() else {
        return Ok(None);
    };
    let v = expand_env(&v, ctx, "name")?.trim().to_string();
    if v.is_empty() {
        return Ok(None);
    }
    if !crate::driver::is_valid_name(&v) {
        bail!("name: {v:?} is not a valid box name");
    }
    Ok(Some(v))
}

/// Expand one caller-env string value in place.
fn expand_slot(
    slot: &mut Option<String>,
    caller: &HashMap<String, String>,
    path: &str,
) -> Result<()> {
    if let Some(v) = slot.take() {
        *slot = Some(expand_env(&v, caller, path)?);
    }
    Ok(())
}

/// The `[init]` steps, expanded: the verb is a grammar token (no
/// expansion); every argument expands. `cp` steps: the host source is
/// re-canonicalized after expansion, the dest a string.
fn expand_init(
    init: &mut Option<crate::driver::Init>,
    caller: &HashMap<String, String>,
) -> Result<()> {
    let Some(init) = init else {
        return Ok(());
    };
    for (i, step) in init.iter_mut().enumerate() {
        match step {
            crate::driver::InitStep::Exec(argv) => {
                for (j, c) in argv.iter_mut().enumerate() {
                    *c = expand_env(c, caller, &format!("init[{i}][{j}]"))?;
                }
            }
            crate::driver::InitStep::Cp { host, dest } => {
                let s = host.to_string_lossy().to_string();
                let d = dest.clone();
                *host = expand_env(&s, caller, &format!("init[{i}].0")).map(PathBuf::from)?;
                *dest = expand_env(&d, caller, &format!("init[{i}].1"))?;
            }
        }
    }
    Ok(())
}

/// Read the declared `env_files` (`KEY=VALUE` files) into an
/// expansion-source map — the profile's declared values, not the
/// caller's shell. Read-only: it never mutates the caller env, the
/// box env, or `cfg.env`.
fn load_env_files_env(
    paths: &[String],
    caller: &HashMap<String, String>,
) -> Result<IndexMap<String, String>> {
    let mut out = IndexMap::new();
    for raw in paths {
        let path = expand_env(raw, caller, "env_files")?;
        let path = canonicalize(&path)?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading env file {}", path.display()))?;
        let path_str = path.to_string_lossy();
        parse_env_file(&content, path_str.as_ref(), &mut out)?;
    }
    Ok(out)
}

/// Fold `src` into the expansion context `ctx` (keys inserted/updated).
fn merge_ctx(ctx: &mut HashMap<String, String>, src: &IndexMap<String, String>) {
    for (k, v) in src {
        ctx.insert(k.clone(), v.clone());
    }
}

/// Insert `key`/`value` into `out`: a redefined key updates its value in
/// place (keeping its position), a new key appends. This is the
/// `KEY=VALUE` assignment rule the env files and `[env]` share.
fn put_env(out: &mut IndexMap<String, String>, key: &str, value: String) {
    match out.get_mut(key) {
        Some(slot) => *slot = value,
        None => {
            out.insert(key.to_string(), value);
        }
    }
}

/// Parse one env file into `out` (later lines override earlier ones).
/// Blank lines and `#` comments are ignored; a line is `KEY=VALUE` (an
/// optional leading `export ` is stripped, and whitespace around the
/// `=` is tolerated). The key must be a valid env name; the value is
/// everything after the first `=` (may be empty or contain spaces) and is
/// left raw for a later expansion pass.
pub(crate) fn parse_env_file(
    content: &str,
    path: &str,
    out: &mut IndexMap<String, String>,
) -> Result<()> {
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((k, v)) = line.split_once('=') else {
            bail!("{path}:{}: expected KEY=VALUE (got {line:?})", i + 1);
        };
        let k = k.trim();
        if !is_valid_env_name(k) {
            bail!("{path}:{}: {k:?} is not a valid variable name", i + 1);
        }
        put_env(out, k, v.trim().to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SteelbxConfig;
    use crate::driver::InitStep;

    // --- env file parsing ---

    #[test]
    fn env_files_bare_export_is_refused() {
        let mut m = IndexMap::new();
        assert!(parse_env_file("export Zeta", "f", &mut m).is_err());
    }

    #[test]
    fn env_files_hash_comment_and_blank_and_export_forms() {
        let mut m = IndexMap::new();
        parse_env_file(
            "# a comment\n\nexport Zeta=bar\nexport Gamma='g val'\nDelta=last\n",
            "f",
            &mut m,
        )
        .unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m.get("Zeta"), Some(&"bar".to_string()));
        assert_eq!(m.get("Gamma"), Some(&"'g val'".to_string()));
        assert_eq!(m.get("Delta"), Some(&"last".to_string()));
    }

    #[test]
    fn env_files_duplicate_key_keeps_last() {
        let mut m = IndexMap::new();
        parse_env_file("K=a\nK=b\n", "f", &mut m).unwrap();
        assert_eq!(m.get("K"), Some(&"b".to_string()));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn env_files_invalid_key_is_refused() {
        let mut m = IndexMap::new();
        assert!(parse_env_file("=two\n", "f", &mut m).is_err());
        assert!(parse_env_file("A-B=two\n", "f", &mut m).is_err());
    }

    // --- expansion pipeline ---

    #[test]
    fn env_files_feed_expansion_but_not_the_box_env() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("env.conf");
        std::fs::write(&f, "FOO=bar\n").unwrap();
        let mut cfg = SteelbxConfig {
            env_files: vec![f.to_str().unwrap().to_string()],
            network: Some("$FOO".to_string()),
            env: [("FOO".to_string(), "declared".to_string())].into(),
            ..Default::default()
        };

        let caller: HashMap<String, String> = std::env::vars().collect();
        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        expand_rest(&mut cfg, &caller, "box", &pre.file_env).unwrap();
        // The inline [env] feeds the ctx after env_files, so its (already
        // expanded) value shadows the file's FOO for later expansion —
        // same as the old `expand_with_env` order: caller < files < name < env.
        assert_eq!(cfg.network.as_deref(), Some("declared"));
        // The file's FOO is an expansion source, not the box env: the
        // declared [env] FOO=declared is what runs in the box.
        assert_eq!(
            cfg.env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            vec![("FOO", "declared")],
        );
    }

    #[test]
    fn caller_env_expansion_bare_braced_and_default() {
        let mut caller = HashMap::new();
        caller.insert("FOO".to_string(), "bar".to_string());
        let mut cfg = SteelbxConfig {
            network: Some("${FOO}".to_string()),
            cgroupns: Some("$FOO".to_string()),
            ipc: Some("${MISSING:-private}".to_string()),
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        expand_rest(&mut cfg, &caller, "box", &pre.file_env).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("bar"));
        assert_eq!(cfg.cgroupns.as_deref(), Some("bar"));
        assert_eq!(cfg.ipc.as_deref(), Some("private"));
    }

    #[test]
    fn declared_name_is_expanded_and_reused_as_box_name() {
        let mut caller = HashMap::new();
        caller.insert("PREFIX".to_string(), "pi".to_string());
        let mut cfg = SteelbxConfig {
            name: Some("${PREFIX}-agent".to_string()),
            name_unique: Some(false),
            network: Some("$STEELBX_BOX_NAME".to_string()),
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        assert_eq!(pre.declared.as_deref(), Some("pi-agent"));
        assert_eq!(pre.final_name.as_deref(), Some("pi-agent"));
        expand_rest(&mut cfg, &caller, "pi-agent", &pre.file_env).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("pi-agent"));
    }

    #[test]
    fn name_flag_suppresses_declared_name_expansion() {
        let mut caller: HashMap<String, String> = std::env::vars().collect();
        caller.insert("NAME".to_string(), "caller-name".to_string());
        let mut cfg = SteelbxConfig {
            name: Some("$NAME".to_string()),
            network: Some("$STEELBX_BOX_NAME".to_string()),
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, Some("flag-name")).unwrap();
        assert_eq!(pre.final_name.as_deref(), Some("flag-name"));
        expand_rest(&mut cfg, &caller, "flag-name", &pre.file_env).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("flag-name"));
    }

    #[test]
    fn caller_var_used_by_env_is_not_leaked_into_the_box_env() {
        let mut caller = HashMap::new();
        caller.insert("FOO".to_string(), "bar".to_string());
        let mut cfg = SteelbxConfig {
            network: Some("$FOO".to_string()),
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        expand_rest(&mut cfg, &caller, "box", &pre.file_env).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("bar"));
        assert!(cfg.env.is_empty());
    }

    #[test]
    fn init_mount_and_env_expansion_and_declared_runtime_env() {
        let mut caller = HashMap::new();
        caller.insert("WORK".to_string(), "/data".to_string());
        let mut cfg = SteelbxConfig {
            init: Some(vec![
                InitStep::Exec(vec!["setfacl".into(), "-m".into(), "u:me".into()]),
                InitStep::Cp {
                    host: "/data".into(),
                    dest: "/data".into(),
                },
            ]),
            env: [("FOO".to_string(), "bar".to_string())].into(),
            entry: vec!["run".into(), "$FOO".into()],
            network: Some("private".into()),
            ulimits: vec!["nofile=256:256".to_string()],
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        expand_rest(&mut cfg, &caller, "box", &pre.file_env).unwrap();
        assert_init_steps(cfg.init.as_ref().unwrap());
        // entry arg is expanded.
        assert_eq!(cfg.entry, vec!["run".to_string(), "bar".to_string()]);
        // declared runtime env is in cfg.env, and the caller's FOO did not leak in.
        assert_eq!(cfg.env.len(), 1);
        assert_eq!(
            cfg.env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            vec![("FOO", "bar")],
        );
    }

    /// The init steps after expansion: the Cp source/dest are expanded,
    /// the Exec argv untouched.
    fn assert_init_steps(init: &[InitStep]) {
        // init Cp source is expanded.
        match &init[1] {
            InitStep::Cp { host, dest } => {
                assert_eq!(host, &PathBuf::from("/data"));
                assert_eq!(dest, "/data");
            }
            _ => unreachable!(),
        }
        // init Exec args untouched.
        match &init[0] {
            InitStep::Exec(argv) => {
                assert_eq!(argv, &["setfacl", "-m", "u:me"]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn prepass_image_expansion_and_box_name_as_env() {
        let mut caller = HashMap::new();
        caller.insert("IMG".to_string(), "localhost/pi-steelbx:latest".to_string());
        let mut cfg = SteelbxConfig {
            image: Some("$IMG".to_string()),
            name: Some("pi-agent".to_string()),
            name_unique: Some(false),
            env: [("NAME".to_string(), "$STEELBX_BOX_NAME".to_string())].into(),
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        assert_eq!(pre.image.as_deref(), Some("localhost/pi-steelbx:latest"));
        assert_eq!(pre.declared.as_deref(), Some("pi-agent"));
        assert_eq!(pre.final_name.as_deref(), Some("pi-agent"));
        assert_eq!(
            cfg.env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect::<Vec<_>>(),
            vec![("NAME", "pi-agent")],
        );

        // The full pass expands the rest; STEELBX_BOX_NAME is available.
        cfg.network = Some("private".into());
        expand_rest(&mut cfg, &caller, "pi-agent", &pre.file_env).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("private"));
    }

    #[test]
    fn unset_var_in_a_later_field_is_a_loud_error() {
        let caller: HashMap<String, String> = std::env::vars().collect();
        let mut cfg = SteelbxConfig {
            image: Some("localhost/pi-steelbx:latest".to_string()),
            network: Some("$STEELBX_NOPE".to_string()),
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        assert!(expand_rest(&mut cfg, &caller, "box", &pre.file_env).is_err());
    }

    #[test]
    fn empty_expanded_value_is_refused() {
        let caller: HashMap<String, String> = std::env::vars().collect();
        let mut cfg = SteelbxConfig {
            image: Some("localhost/pi-steelbx:latest".to_string()),
            network: Some("$EMPTY_VAR".to_string()),
            ..Default::default()
        };

        let pre = pre_expand(&mut cfg, &caller, None).unwrap();
        assert!(expand_rest(&mut cfg, &caller, "box", &pre.file_env).is_err());
    }
}

// --- original expand_env engine tests (unchanged) ---

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[test]
    fn expansion_bare_and_braced_refs() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());

        assert_eq!(
            expand_env("no dollar signs", &env, "t").unwrap(),
            "no dollar signs"
        );
        assert_eq!(expand_env("$FOO", &env, "t").unwrap(), "bar");
        assert_eq!(expand_env("${FOO}", &env, "t").unwrap(), "bar");
        assert_eq!(
            expand_env("pre-$FOO-post", &env, "t").unwrap(),
            "pre-bar-post"
        );
        assert_eq!(
            expand_env("pre-${FOO}post", &env, "t").unwrap(),
            "pre-barpost"
        );
        assert_eq!(expand_env("$$", &env, "t").unwrap(), "$");
        assert_eq!(expand_env("$$FOO", &env, "t").unwrap(), "$FOO");
        // Expansion is single-pass: a value containing $ is not re-scanned.
        env.insert("RAW".to_string(), "$FOO".to_string());
        assert_eq!(expand_env("$RAW", &env, "t").unwrap(), "$FOO");
    }

    #[test]
    fn expansion_empty_and_fallbacks() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        env.insert("EMPTY".to_string(), String::new());

        // Empty values expand to empty.
        assert_eq!(expand_env("${EMPTY}", &env, "t").unwrap(), "");
        // Fallback `${IDENT:-default}`: unset or empty → the default;
        // set and non-empty → the value.
        assert_eq!(expand_env("${NOFALL:-x}", &env, "t").unwrap(), "x");
        assert_eq!(expand_env("${EMPTY:-x}", &env, "t").unwrap(), "x");
        assert_eq!(expand_env("${FOO:-x}", &env, "t").unwrap(), "bar");
        // The default is the text up to the first `}` (no nesting
        // of `${...}` inside it), and that text itself expands:
        // `$$` inside it is a literal `$`.
        assert_eq!(expand_env("${NOFALL:-a$$b}c}", &env, "t").unwrap(), "a$bc}");
    }

    #[test]
    fn expansion_fallbacks_expand_against_the_caller_env() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/user".to_string());

        // Unset variable → the fallback, which itself expands.
        assert_eq!(
            expand_env("${STEELBX_HOME:-$HOME/steelbx-pi}", &env, "t").unwrap(),
            "/home/user/steelbx-pi"
        );
        // Set and non-empty → the value; the fallback is never touched.
        env.insert("STEELBX_HOME".to_string(), "/box".to_string());
        assert_eq!(
            expand_env("${STEELBX_HOME:-$HOME/steelbx-pi}", &env, "t").unwrap(),
            "/box"
        );
        // An unset reference inside the fallback fails loudly.
        env.remove("STEELBX_HOME");
        assert!(expand_env("${STEELBX_HOME:-$NOPE}", &env, "t").is_err());
        // Pure-literal defaults still work (the toolbox idiom).
        assert_eq!(
            expand_env("${SHELL:-/bin/bash}", &env, "t").unwrap(),
            "/bin/bash"
        );
    }

    #[test]
    fn expansion_bad_dollars_are_errors() {
        let env = HashMap::new();
        // Unset fails loudly.
        assert!(expand_env("$UNSET", &env, "t").is_err());
        // An empty ident is still an error.
        assert!(expand_env("${:-x}", &env, "t").is_err());
        // Anything else with a dollar is an error, not a shell.
        assert!(expand_env("trailing $", &env, "t").is_err());
        assert!(expand_env("$(shell)", &env, "t").is_err());
        assert!(expand_env("${UNCLOSED", &env, "t").is_err());
    }
}
