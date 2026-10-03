//! Steelbx policy config: the `containers.conf` schema.
//!
//! TOML, containers-ecosystem format (the key names mirror what
//! podman/container tooling uses, where they exist). A *profile*
//! (`profiles/<name>.conf`) is a complete file in this schema (the
//! `image` key names the image; replace semantics, no merging), and
//! `steelbx create <profile>` selects one. steelbx reads only the
//! profiles — podman's own `containers.conf` is podman's and is
//! applied by podman itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;

use crate::expand::expand_env;
use crate::validate::{
    canonicalize, validate_env, validate_extra_hosts, validate_mount_options, validate_mount_specs,
    validate_ns_value, validate_security_opts, MountDest,
};

/// The config dir: `$HOME/.config/steelbx` (the XDG config location,
/// made steelbx-specific) — where `profiles/` lives, and
/// `$STEELBX_CONFIG_DIR`.
pub fn config_dir_path(home: &Path) -> PathBuf {
    home.join(".config/steelbx")
}

/// The profiles dir: `$HOME/.config/steelbx/profiles`. Absence is a value
/// (no profiles), not an error. Overrides the system dir by name.
pub(crate) fn profiles_dir(home: &Path) -> PathBuf {
    config_dir_path(home).join("profiles")
}

/// The system (shipped) profiles dir: `/etc/steelbx/profiles` — where a
/// distribution installs complete policy profiles; a user profile of the
/// same name overrides it. Absence is a value (no shipped profiles),
/// not an error.
pub(crate) fn system_profiles_dir() -> PathBuf {
    PathBuf::from("/etc/steelbx/profiles")
}

/// The data dir: `$HOME/.local/share/steelbx` (XDG convention) — home
/// for files a box should see (bind-mounted from profiles); policy is
/// in the config dir, data here.
pub fn data_dir(home: &Path) -> PathBuf {
    home.join(".local/share/steelbx")
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
            // The verb is a grammar token (no expansion); every
            // argument expands.
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
/// are stored in their slots; the full pass (`expand_with_env`) does
/// not re-expand them. Returns (image, declared name, final name).
fn pre_expand_with_env(
    cfg: &mut SteelbxConfig,
    caller: &HashMap<String, String>,
    name_flag: Option<&str>,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
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
    Ok((image, declared, final_name))
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
        anyhow::bail!("name: {v:?} is not a valid box name");
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

/// Load `env_files` into a variable map: each file is parsed as
/// `KEY=VALUE` lines (later lines override earlier), later files override
/// earlier files. This map is an expansion *source* only — it is NOT the
/// container env (that is the inline `[env]`). Paths and values expand
/// against the caller env. A missing file or a malformed line is an
/// error naming the file.
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
fn parse_env_file(content: &str, path: &str, out: &mut IndexMap<String, String>) -> Result<()> {
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
        if !crate::validate::is_valid_env_name(k) {
            bail!("{path}:{}: {k:?} is not a valid variable name", i + 1);
        }
        put_env(out, k, v.trim().to_string());
    }
    Ok(())
}

/// The full pass of the load pipeline: expands every string value not
/// yet expanded — the `network` string, each `extra_hosts` element,
/// each `security_opts` element, each `mounts` spec, and the
/// namespace/identity values (`cgroupns`, `ipc`, `pid`, `userns`, `user`,
/// `ulimits`). Keys never expand (an env key is a name, not a value).
/// The `[env]`, `image`, and `name` slots were already expanded by the
/// pre-pass (`pre_expand_with_env`) and are skipped. `box_name` is
/// always defined (the CLI `-n` override, the profile's `name` key, or
/// the generated unique name): it is injected as `STEELBX_BOX_NAME`
/// before the `[env]` values feed the context, so every value —
/// including `[env]` — may reference it, and it is stored in
/// `cfg.name`. Caveat: values that are *host* paths (`mounts` sources,
/// `init` cp sources) must refer to caller/steelbx variables, not
/// container-only `[env]` names.
fn expand_with_env(
    cfg: &mut SteelbxConfig,
    caller: &HashMap<String, String>,
    box_name: &str,
) -> Result<()> {
    // The env_files variables: an expansion source only (not the
    // container env — that is the inline [env]). Later files override
    // earlier; their values expand against the caller env alone.
    let file_env = load_env_files_env(&cfg.env_files, caller)?;
    // The expansion context: the caller env, plus the env_files
    // variables, plus STEELBX_BOX_NAME, plus (the already-expanded)
    // inline [env].
    let mut ctx = caller.clone();
    merge_ctx(&mut ctx, &file_env);
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

/// The `[init]` steps, shape-checked: non-empty, exec requires a
/// command, a `cp` source must exist (re-canonicalized) and the dest a
/// non-empty absolute container path (no spaces).
fn validate_init(init: &mut Option<crate::driver::Init>) -> Result<()> {
    let Some(init) = init else {
        return Ok(());
    };
    if init.is_empty() {
        anyhow::bail!("init: init must not be empty");
    }
    for (i, step) in init.iter_mut().enumerate() {
        match step {
            // An empty *command* is an error; an empty
            // *argument* is a legitimate value ("" is a fine
            // argv element).
            crate::driver::InitStep::Exec(argv) => {
                if argv.is_empty() {
                    anyhow::bail!("init: init[{i}] is an exec step and requires a command");
                }
            }
            // The host source must exist (fail loud, named);
            // the dest is a container path.
            crate::driver::InitStep::Cp { host, dest } => {
                let src_str = host.to_str().unwrap_or("").to_string();
                *host = canonicalize(&src_str).with_context(|| {
                    format!("init: init[{i}] (a cp step) source does not exist: {src_str}")
                })?;
                if dest.is_empty() || !dest.starts_with('/') || dest.contains(' ') {
                    anyhow::bail!(
                        "init: init[{i}] (a cp step) destination must be a non-empty absolute container path"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Default box name (when no name is declared): the image's name
/// component without tag. `localhost/pi-steelbx:latest` → `pi-steelbx`;
/// `registry.fedoraproject.org/fedora:42` → `fedora`.
pub fn default_box_name(image: &str) -> String {
    image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(':')
        .next()
        .unwrap_or(image)
        .to_string()
}

/// Steelbx's policy defaults (the policy layer: CLI > config > image
/// labels).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SteelbxConfig {
    /// The box name (container name) — always defined after load: the
    /// CLI `-n` override (verbatim), else the profile's `name` key (a
    /// fresh random token appended unless `name_unique = false`), else
    /// a generated unique name from the image base. Available to every
    /// profile value as `STEELBX_BOX_NAME`; shape-checked (a valid
    /// podman name).
    #[serde(default)]
    pub name: Option<String>,
    /// Whether a declared `name` gets a fresh random token appended:
    /// unique by default; `false` pins the name verbatim (a taken name
    /// is then an error). Absent = `true`. Only applies when `name` is
    /// present (without `name`, the name is generated from the image
    /// base regardless).
    #[serde(default)]
    pub name_unique: Option<bool>,
    /// The image name (auto-pulled if not local); a profile sets it so
    /// `steelbx create <profile>` is self-contained. CLI `-i` overrides it; absent = `-i` is
    /// required. Expanded like every other string value.
    #[serde(default)]
    pub image: Option<String>,
    /// Podman network (`bridge`, `pasta`, `none`, ...); absent or empty =
    /// podman's default network (no `--network` flag passed).
    #[serde(default)]
    pub network: Option<String>,
    /// `name:address` host entries, rendered as podman `--add-host` args.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// Env overrides, rendered as podman `--env KEY=VALUE` in the order
    /// given (order-preserving); same-key wins over the image's baked-in
    /// ENV. This is the ONLY env passed to the container. Values expand
    /// against the caller env plus `env_files`.
    #[serde(default)]
    pub env: IndexMap<String, String>,
    /// Ordered env files (`KEY=VALUE` per line; `#` comments and a leading
    /// `export ` are allowed), later files override earlier ones. Loaded
    /// as an expansion *source*: their variables can be referenced by
    /// other values (and by `[env]` values), but they are NOT passed to
    /// the container. Paths expand like every other value; a missing
    /// file or a malformed line is an error.
    #[serde(default)]
    pub env_files: Vec<String>,
    /// Runtime env: env NAMES (not values) the box should expose from
    /// the caller env at enter/exec — podman copies each as a bare
    /// `-e NAME` (the value is never argv). Not expanded (a name is a
    /// name, like env keys); shape-checked. Merged with the image's
    /// `com.github.simon3z.steelbx.box.env` label into the box's own
    /// `box.env` label at create.
    #[serde(default)]
    pub runtime_env: Vec<String>,
    /// Security options (`key=value`), rendered as podman `--security-opt`
    /// args (e.g. `label=disable` to turn SELinux labeling off); absent
    /// = podman's default security posture.
    #[serde(default)]
    pub security_opts: Vec<String>,
    /// Podman `--mount` specs, rendered verbatim (e.g.
    /// `type=devpts,destination=/dev/pts`) — the policy mounts, in
    /// addition to the derived bind mounts (shape-checked;
    /// podman interprets the spec).
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Podman `--cgroupns` value (`host`, `private`, ...); absent =
    /// podman's default.
    #[serde(default)]
    pub cgroupns: Option<String>,
    /// Podman `--ipc` value (`host`, `private`, ...); absent = podman's
    /// default.
    #[serde(default)]
    pub ipc: Option<String>,
    /// Podman `--pid` value (`host`, `private`, ...); absent = podman's
    /// default.
    #[serde(default)]
    pub pid: Option<String>,
    /// Podman `--userns` value (`keep-id`, `host`, ...); absent =
    /// podman's default.
    #[serde(default)]
    pub userns: Option<String>,
    /// Podman `--user` value (`name`, `uid`, `name:group`...); absent =
    /// the image's user.
    #[serde(default)]
    pub user: Option<String>,
    /// Podman `--privileged`.
    #[serde(default)]
    pub privileged: bool,
    /// Podman `--no-hosts` (the box keeps the image's `/etc/hosts`).
    #[serde(default)]
    pub no_hosts: bool,
    /// Podman `--ulimit` entries, one `--ulimit` flag each (e.g.
    /// `"host"`, `"nofile=65535:65535"`).
    #[serde(default)]
    pub ulimits: Vec<String>,
    /// The main command argv: replaces the heartbeat
    /// default. Absent (or empty) = no entry: the image's declared
    /// command (if it carries `com.github.simon3z.steelbx.box.cmd`), else the
    /// heartbeat.
    #[serde(default)]
    pub entry: Vec<String>,
    /// Whether `rm` (and `run`'s auto-removal) removes the named
    /// volumes the `mounts` key declares (`type=volume,source=<name>`):
    /// written to the box's `box.volumes` label at create, read back at
    /// `rm` (the profile is not reloaded). Absent = `false` — the
    /// volumes survive `rm` (podman's default). Anonymous (unnamed)
    /// volume mounts are not tracked by the label.
    #[serde(default)]
    pub delete_volumes: bool,
    /// Post-create initialization: `[init]` — `cmd` is an
    /// array of argvs (each inner array = one exec, in order),
    /// Layout override: the container path under
    /// which derived mounts land — precedence over the image's
    /// `WORKDIR` and the `/work` default.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Where the derived mounts are placed inside the box: `basename`
    /// (the default) — under the mount base (`<base>/<basename>`, the
    /// `workdir` override > the image's `WORKDIR` > `/work`) — or
    /// `absolute` — each host path mounted at its own full path. A
    /// keyword: not expanded, shape-checked.
    #[serde(default)]
    pub mount_dest: Option<String>,
    /// Podman `--mount` options appended to each derived bind mount
    /// (e.g. `chown=true`, `ro=true`): podman
    /// interprets them (argv-vector safe); shape-checked (no spaces,
    /// `key=value` tokens, and the keys steelbx itself sets on a derived
    /// mount — `type`, `src`/`source`, `dst`/`destination` — cannot be
    /// re-specified). Most useful with `mount_dest = "absolute"` (e.g.
    /// `chown=true` lets a non-root box user write into a host path it
    /// does not own). Absent = no options (podman's defaults).
    #[serde(default)]
    pub mount_options: Vec<String>,
    /// `user` absent = root; runs right after create, before the box
    /// is handed over.
    #[serde(default)]
    pub init: Option<crate::driver::Init>,
}

impl SteelbxConfig {
    /// Available profile names (the stems of `profiles/*.conf`),
    /// sorted: the user dir and the system (shipped) dir, unioned — a
    /// name present in both appears once (the user profile wins at
    /// load; listing names only).
    pub fn list_profiles() -> Result<Vec<String>> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self::list_profiles_in(&[profiles_dir(&home), system_profiles_dir()])
    }

    /// The profile names across the given dirs, sorted (unioned, no
    /// duplicates).
    fn list_profiles_in(dirs: &[PathBuf]) -> Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for dir in dirs {
            let rd = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("reading profiles dir {}", dir.display()))
                }
            };
            for name in rd
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter_map(Self::profile_name_from_filename)
            {
                names.insert(name);
            }
        }
        Ok(names.into_iter().collect())
    }

    /// A named policy profile: `profiles/<name>.conf` — a complete
    /// `containers.conf` (replace semantics: no merging with the default).
    /// The profile name from a `profiles/` filename (`net-off.conf` →
    /// `net-off`); `None` for other names.
    pub(crate) fn profile_name_from_filename(filename: String) -> Option<String> {
        filename.strip_suffix(".conf").map(str::to_string)
    }

    /// Parse a named policy profile (the user dir first, then the
    /// shipped dir) — TOML only: no expansion, no validation.
    pub fn parse_profile(name: &str) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self::parse_from(&Self::find_profile_path(name, &Self::profile_dirs(&home))?)
    }

    /// The pre-pass of the load pipeline (see `pre_expand_with_env`),
    /// against the process env: returns (image, declared name, final
    /// name).
    pub fn pre_expand(
        cfg: &mut Self,
        name_flag: Option<&str>,
    ) -> Result<(Option<String>, Option<String>, Option<String>)> {
        let caller: HashMap<String, String> = std::env::vars().collect();
        pre_expand_with_env(cfg, &caller, name_flag)
    }

    /// The full pass of the load pipeline (see `expand_with_env`),
    /// against the process env: warn, expand, validate the expanded
    /// shape. `box_name` is always defined (the effective name: the
    /// `-n` flag, the profile's `name` key, or a generated unique
    /// name) and becomes `STEELBX_BOX_NAME`.
    pub fn finish_expand(cfg: &mut Self, box_name: &str) -> Result<()> {
        // Warn before expansion (mounts still have $ references).
        Self::warn_runtime_env_in_mounts(cfg);
        let caller: HashMap<String, String> = std::env::vars().collect();
        expand_with_env(cfg, &caller, box_name)?;
        Self::validate_config(cfg)?;
        Ok(())
    }

    fn profile_dirs(home: &Path) -> [PathBuf; 2] {
        [profiles_dir(home), system_profiles_dir()]
    }

    /// `name_unique`: absent = the default (`true`).
    pub fn name_unique(&self) -> bool {
        self.name_unique.unwrap_or(true)
    }

    /// Find a named profile file: the user dir first, then the shipped
    /// dir — a user profile of the same name overrides the shipped one.
    fn find_profile_path(name: &str, dirs: &[PathBuf]) -> Result<PathBuf> {
        let file = Path::new(name);
        if file.file_name().map(Path::new) != Some(file) {
            bail!("invalid profile name: {name:?} — no path separators, no '..'");
        }
        let path = dirs
            .iter()
            .map(|d| d.join(format!("{name}.conf")))
            .find(|p| p.exists());
        let Some(path) = path else {
            bail!(
                "profile '{name}' not found (looked in: {})\nAvailable profiles: {}",
                dirs.iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                match Self::list_profiles_in(dirs) {
                    Ok(names) if !names.is_empty() => names.join(", "),
                    _ => "none".to_string(),
                }
            );
        };
        Ok(path)
    }

    /// Read and parse a config file (TOML only — no expansion, no
    /// validation).
    fn parse_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading steelbx config {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing steelbx config {}", path.display()))
    }

    /// The load pipeline after parsing: warn, pre-expand, decide the
    /// name (the profile's `name` key, else a generated unique name from
    /// the image's name component — no label lookup here), expand the
    /// rest, validate. `STEELBX_BOX_NAME` is always defined.
    fn finish_load(cfg: &mut Self) -> Result<()> {
        // Warn before expansion (mounts still have $ references).
        Self::warn_runtime_env_in_mounts(cfg);
        let caller: HashMap<String, String> = std::env::vars().collect();
        let (image, _declared, final_name) = pre_expand_with_env(cfg, &caller, None)?;
        // The box name — always defined: the final name (the declared
        // `name`, a fresh random token appended unless
        // `name_unique = false`), else a generated unique name from
        // the image's name component.
        let name = final_name.unwrap_or_else(|| {
            crate::driver::unique_name(&default_box_name(image.as_deref().unwrap_or_default()))
        });
        expand_with_env(cfg, &caller, &name)?;
        Self::validate_config(cfg)?;
        Ok(())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let mut cfg = Self::parse_from(path)?;
        Self::finish_load(&mut cfg)?;
        Ok(cfg)
    }

    /// Warn if a `runtime_env` name is also referenced in a mount spec.
    /// Mounts are rendered at create time; a runtime_env value can only
    /// affect enter/exec sessions, so the mount path is frozen and will
    /// not track the live value.
    fn warn_runtime_env_in_mounts(cfg: &SteelbxConfig) {
        for name in &cfg.runtime_env {
            for (i, m) in cfg.mounts.iter().enumerate() {
                let braced = format!("${{{name}}}");
                let referenced = m.contains(&braced)
                    || m.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .flat_map(|s| s.split('$'))
                        .any(|token| token == name);
                if referenced {
                    eprintln!(
                        "warning: runtime_env {name:?} is referenced in mounts[{i}] — \
                         mount paths are fixed at create time and cannot track \
                         the live runtime value"
                    );
                }
            }
        }
    }
}

/// The namespace/identity values (`cgroupns`, `ipc`, `pid`, `userns`,
/// `user`) and the per-`ulimit` values: podman interprets each, so the
/// shape is checked here.
fn validate_ns_and_ulimits(cfg: &SteelbxConfig) -> Result<()> {
    for (field, slot) in [
        ("cgroupns", &cfg.cgroupns),
        ("ipc", &cfg.ipc),
        ("pid", &cfg.pid),
        ("userns", &cfg.userns),
        ("user", &cfg.user),
    ] {
        if let Some(v) = slot {
            validate_ns_value(field, v)?;
        }
    }
    for u in &cfg.ulimits {
        validate_ns_value("ulimits", u)?;
    }
    Ok(())
}

/// `workdir`: a non-empty absolute container path.
fn validate_workdir(workdir: &Option<String>) -> Result<()> {
    if let Some(w) = workdir {
        if w.is_empty() || !w.starts_with('/') {
            anyhow::bail!("workdir: must be a non-empty absolute container path");
        }
    }
    Ok(())
}

/// `entry`: no empty elements.
fn validate_entry(entry: &[String]) -> Result<()> {
    for (i, e) in entry.iter().enumerate() {
        if e.is_empty() {
            anyhow::bail!("entry: entry[{i}] must not be empty");
        }
    }
    Ok(())
}

/// Trim an optional string value; `""` means "absent", same as omitting
/// the key.
fn trim_or_absent(v: Option<String>) -> Option<String> {
    v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl SteelbxConfig {
    /// Shape validation of a fully expanded config (the `load_from`
    /// pipeline minus file reading and expansion; the `cp` source
    /// exists-checked, nothing else touches the filesystem).
    fn validate_config(cfg: &mut SteelbxConfig) -> Result<()> {
        validate_extra_hosts(&cfg.extra_hosts)?;
        validate_env(&cfg.env)?;
        for (i, n) in cfg.runtime_env.iter().enumerate() {
            if !crate::validate::is_valid_env_name(n) {
                bail!(
                    "runtime_env: runtime_env[{i}] {n:?} is not a valid variable name ([A-Za-z0-9_] only)"
                );
            }
        }
        validate_security_opts(&cfg.security_opts)?;
        validate_mount_specs(&cfg.mounts)?;
        validate_mount_options(&cfg.mount_options)?;
        Self::warn_anonymous_volume_mounts(cfg);
        validate_ns_and_ulimits(cfg)?;
        validate_workdir(&cfg.workdir)?;
        // `name`: the effective box name (always set after load) — a
        // valid podman name (the driver re-checks at create; the
        // profile-level check names the key).
        if let Some(n) = &cfg.name {
            if !crate::driver::is_valid_name(n) {
                anyhow::bail!("name: {n:?} is not a valid box name");
            }
        }
        validate_init(&mut cfg.init)?;
        validate_entry(&cfg.entry)?;
        // `network = ""` means "don't pass a flag", same as omitting it;
        // `image = ""` means "no image declared", same as omitting it.
        cfg.network = trim_or_absent(cfg.network.take());
        cfg.image = trim_or_absent(cfg.image.take());
        cfg.mount_dest = trim_or_absent(cfg.mount_dest.take());
        // `mount_dest` is a keyword (like `workdir` values): shape-checked.
        MountDest::parse(cfg.mount_dest.as_deref())?;
        Ok(())
    }

    /// `delete_volumes`: an unnamed (anonymous) volume mount cannot be
    /// tracked by name — warn (it is not in the `box.volumes` label),
    /// never fail.
    fn warn_anonymous_volume_mounts(cfg: &SteelbxConfig) {
        if !cfg.delete_volumes {
            return;
        }
        for (i, m) in cfg.mounts.iter().enumerate() {
            if crate::validate::is_volume_mount_spec(m)
                && crate::validate::volume_name_from_mount_spec(m).is_none()
            {
                eprintln!(
                    "warning: delete_volumes is set but mounts[{i}] declares a \
                     type=volume mount without a name — it is not tracked \
                     in the box's box.volumes label"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load a named profile from explicit dirs: find, parse, finish —
    /// the test-facing form of the production pipeline, dirs injected.
    fn load_in(name: &str, dirs: &[PathBuf]) -> anyhow::Result<SteelbxConfig> {
        let mut cfg = SteelbxConfig::parse_from(&SteelbxConfig::find_profile_path(name, dirs)?)?;
        SteelbxConfig::finish_load(&mut cfg)?;
        Ok(cfg)
    }

    #[test]
    fn provided_dirs_follow_the_xdg_convention() {
        let home = PathBuf::from("/home/simon");
        assert_eq!(
            config_dir_path(&home),
            PathBuf::from("/home/simon/.config/steelbx")
        );
        assert_eq!(
            data_dir(&home),
            PathBuf::from("/home/simon/.local/share/steelbx")
        );
    }

    #[test]
    fn workdir_override_is_expanded_and_checked() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, "workdir = \"/$$base\"\n").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.workdir.as_deref(), Some("/$base"));
        std::fs::write(&p, "workdir = \"relative\"\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        std::fs::write(&p, "workdir = \"/\"\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_ok());
    }

    #[test]
    fn mount_dest_keyword_is_loaded_and_shape_checked() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, "mount_dest = \"absolute\"\n").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.mount_dest.as_deref(), Some("absolute"));
        // Empty = absent = the default (workdir).
        std::fs::write(&p, "mount_dest = \"  \"\n").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.mount_dest, None);
        // An unknown keyword is an error.
        std::fs::write(&p, "mount_dest = \"wherever\"\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
    }

    #[test]
    fn mount_options_are_loaded_and_shape_checked() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        // Absent = no options.
        std::fs::write(&p, "image = \"img\"\n").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert!(cfg.mount_options.is_empty());
        // Well-formed options are loaded verbatim (a single option and a
        // comma-separated list).
        std::fs::write(
            &p,
            "image = \"img\"\nmount_options = [\"chown=true\", \"ro=true,idmap=true\"]\n",
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(
            cfg.mount_options,
            vec!["chown=true".to_string(), "ro=true,idmap=true".to_string()]
        );
        // A space is an error (hostile input; podman specs have no spaces).
        std::fs::write(&p, "image = \"img\"\nmount_options = [\"chown = true\"]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // Not a key=value token is an error.
        std::fs::write(&p, "image = \"img\"\nmount_options = [\"chown\"]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // A key steelbx sets on a derived mount cannot be re-specified.
        std::fs::write(&p, "image = \"img\"\nmount_options = [\"type=volume\"]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
    }

    #[test]
    fn loads_init_steps_and_expands_their_arguments() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        // Steps: the verb is a grammar token (no expansion); every
        // argument expands. cp steps: host source expanded + must
        // exist, dest an absolute container path.
        std::fs::write(
            &p,
            format!(
                "init = [\n  [\"exec\", \"chown\", \"$$USER:\", \"/home/$$USER\"],\n  [\"cp\", \"{src}\", \"/data/x\"],\n]\n",
                src = t.path().join("f").to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(t.path().join("f"), "x").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(
            cfg.init,
            Some(vec![
                crate::driver::InitStep::Exec(vec![
                    "chown".into(),
                    "$USER:".into(),
                    "/home/$USER".into()
                ]),
                crate::driver::InitStep::Cp {
                    host: t.path().join("f"),
                    dest: "/data/x".into(),
                },
            ])
        );
        // An empty list is an error (an [init] that does nothing).
        std::fs::write(&p, "init = []\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // An exec step with no command is an error.
        std::fs::write(&p, "init = [[\"exec\"]]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // Empty *arguments* are legitimate argv ("" is a fine value).
        std::fs::write(&p, "init = [[\"exec\", \"true\", \"\"]]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_ok());
    }

    #[test]
    fn init_cp_step_is_host_source_and_container_dest() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        // A cp step: the source is expanded and must exist
        // (fail loud, named); the dest is an absolute container path.
        std::fs::write(
            &p,
            format!(
                "init = [[\"cp\", \"{src}\", \"/data/x\"]]\n",
                src = t.path().join("f").to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(t.path().join("f"), "x").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(
            cfg.init,
            Some(vec![crate::driver::InitStep::Cp {
                host: t.path().join("f"),
                dest: "/data/x".into(),
            }])
        );
    }

    #[test]
    fn init_cp_bad_shapes_are_errors() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(t.path().join("f"), "x").unwrap();
        // A missing source fails loudly, naming it.
        std::fs::write(
            &p,
            "init = [[\"cp\", \"/no-such-steelbx-test\", \"/data/x\"]]\n",
        )
        .unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // A relative dest is refused (container paths are absolute).
        std::fs::write(
            &p,
            format!(
                "init = [[\"cp\", \"{s}\", \"data/x\"]]\n",
                s = t.path().join("f").to_string_lossy()
            ),
        )
        .unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // Wrong arity (a cp with three paths) is a grammar error.
        std::fs::write(&p, "init = [[\"cp\", \"a\", \"b\", \"c\"]]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // An unknown verb is a grammar error.
        std::fs::write(&p, "init = [[\"mv\", \"a\", \"b\"]]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
    }
    #[test]
    fn profile_name_from_filename_strips_the_extension() {
        assert_eq!(
            SteelbxConfig::profile_name_from_filename("net-off.conf".to_string()),
            Some("net-off".to_string())
        );
        assert_eq!(
            SteelbxConfig::profile_name_from_filename("notes.txt".to_string()),
            None
        );
    }

    #[test]
    fn profile_names_are_shape_checked() {
        for bad in ["", ".", "..", "a/b", "../x"] {
            assert!(
                SteelbxConfig::parse_profile(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn missing_profile_is_an_error_not_the_default() {
        assert!(SteelbxConfig::parse_profile("no-such-profile-steelbx-test").is_err());
    }

    // Shipped profiles: the system dir is searched after the user dir;
    // a user profile of the same name overrides the shipped one.
    #[test]
    fn shipped_profiles_resolve_with_user_override() {
        let t = tempfile::tempdir().unwrap();
        let user = t.path().join("user");
        let system = t.path().join("system");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        let dirs = vec![user.clone(), system.clone()];
        // The user profile wins over a shipped profile of the same name.
        std::fs::write(system.join("pi-agent.conf"), "network = \"none\"\n").unwrap();
        std::fs::write(
            user.join("pi-agent.conf"),
            "network = \"shipped-test-user\"\n",
        )
        .unwrap();
        let cfg = load_in("pi-agent", &dirs).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("shipped-test-user"));
        // A name only in the system dir resolves to the shipped file.
        std::fs::write(
            system.join("shipped-only.conf"),
            "network = \"shipped-test-sys\"\n",
        )
        .unwrap();
        let cfg = load_in("shipped-only", &dirs).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("shipped-test-sys"));
        // Listing is the union (sorted, no duplicates).
        assert_eq!(
            SteelbxConfig::list_profiles_in(&dirs).unwrap(),
            vec!["pi-agent", "shipped-only"]
        );
        // Absent in both: an error that names the searched dirs.
        let err = load_in("no-such", &dirs).unwrap_err();
        assert!(format!("{err}").contains(&system.display().to_string()));
        // Neither dir existing is fine (no profiles), not an error.
        let empty: Vec<PathBuf> = vec![t.path().join("a"), t.path().join("b")];
        assert_eq!(
            SteelbxConfig::list_profiles_in(&empty).unwrap(),
            Vec::<String>::new()
        );
    }

    // Guard for the working example: the same pipeline as `load_from`
    // (parse, expand, validate), but the expansion runs against a
    // fixed caller env so the test is portable (CI is headless).
    #[test]
    fn examples_pi_agent_profile_is_valid() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/pi-agent.conf"
        ))
        .unwrap();
        expand_against(
            &raw,
            &[
                ("USER", "simon"),
                ("HOME", "/home/simon"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("WAYLAND_DISPLAY", "wayland-0"),
                ("COLORTERM", "truecolor"),
                ("LANG", "C.UTF-8"),
                ("TERM", "xterm-256color"),
            ],
            None,
            "base",
        )
        .unwrap();
    }

    #[test]
    fn examples_toolbox_profile_is_valid() {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/toolbox.conf"
        ))
        .unwrap();
        // SHELL is deliberately unset: the profile's `${SHELL:-/bin/bash}`
        // must fall back. VERSION_ID is supplied in case the host's
        // /etc/os-release lacks it (the example's env_file).
        expand_against(
            &raw,
            &[
                ("USER", "simon"),
                ("HOME", "/home/simon"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("VERSION_ID", "43"),
            ],
            None,
            "base",
        )
        .unwrap();
    }

    #[test]
    fn missing_file_is_the_defaults() {
        let cfg =
            SteelbxConfig::load_from(std::path::Path::new("/nonexistent-steelbx-test")).unwrap();
        assert!(cfg.network.is_none());
        assert!(cfg.extra_hosts.is_empty());
    }

    #[test]
    fn loads_network_and_extra_hosts() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(
            &p,
            r#"
network = "bridge"
extra_hosts = ["llm.example.com:host-gateway"]
"#,
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.network.as_deref(), Some("bridge"));
        assert_eq!(cfg.extra_hosts.len(), 1);
    }

    #[test]
    fn image_key_is_expanded_and_empty_is_absent() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, "image = \"$${RAW}\"").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        // Single-pass: `$$` → `$`, the rest is literal.
        assert_eq!(cfg.image.as_deref(), Some("${RAW}"));
        // Empty (like `network`) is the same as absent.
        std::fs::write(&p, "image = \"  \"").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert!(cfg.image.is_none());
    }

    #[test]
    fn empty_network_is_the_same_as_absent() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, r#"network = "  ""#).unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert!(cfg.network.is_none());
    }

    #[test]
    fn loads_the_documented_example_shape() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(
            &p,
            r#"
extra_hosts = [
  "llm.example.com:host-gateway",
]

[env]
COLORTERM = "truecolor"
LANG = "C.UTF-8"
TERM = "xterm-256color"
"#,
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.extra_hosts.len(), 1);
        assert_eq!(
            cfg.env.get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(cfg.env.len(), 3);
    }

    #[test]
    fn entry_loads_and_expands() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(
            &p,
            r#"
entry = ["init-container", "--uid", "$$UID", "--user", "simon"]
"#,
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(
            cfg.entry,
            vec!["init-container", "--uid", "$UID", "--user", "simon"]
        );

        // An empty element is refused.
        let p2 = t.path().join("bad.conf");
        std::fs::write(&p2, "entry = [\"\"]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p2).is_err());
    }

    #[test]
    fn namespace_and_identity_keys_load() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(
            &p,
            r#"
cgroupns = "host"
ipc = "host"
pid = "host"
userns = "keep-id"
user = "root:root"
privileged = true
no_hosts = true
ulimits = ["host"]
"#,
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.cgroupns.as_deref(), Some("host"));
        assert_eq!(cfg.ipc.as_deref(), Some("host"));
        assert_eq!(cfg.pid.as_deref(), Some("host"));
        assert_eq!(cfg.userns.as_deref(), Some("keep-id"));
        assert_eq!(cfg.user.as_deref(), Some("root:root"));
        assert!(cfg.privileged);
        assert!(cfg.no_hosts);
        assert_eq!(cfg.ulimits, vec!["host"]);
    }

    #[test]
    fn namespace_spaced_value_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let p2 = t.path().join("bad.conf");
        // Shape: a spaced value is refused.
        std::fs::write(&p2, "pid = \"host host\"\n").unwrap();
        assert!(SteelbxConfig::load_from(&p2).is_err());
    }

    // Portable (fixed caller env, the create pipeline minus the process
    // env and the podman inspect): the env-visibility rules are pinned
    // here. `name_flag` is the CLI `-n` override; `base` is the name base
    // for a generated name (used only when no name is declared — the
    // name is then always defined.
    fn expand_against(
        raw: &str,
        caller: &[(&str, &str)],
        name_flag: Option<&str>,
        base: &str,
    ) -> Result<SteelbxConfig> {
        let env: HashMap<String, String> = caller
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut cfg: SteelbxConfig = toml::from_str(raw).unwrap();
        let (_image, _declared, final_name) = pre_expand_with_env(&mut cfg, &env, name_flag)?;
        // The name (as in the create pipeline): the final name (the
        // flag, or the declared name — a fresh token appended unless
        // `name_unique = false`), else a generated name on `base`.
        let name = final_name.unwrap_or_else(|| crate::driver::unique_name(base));
        expand_with_env(&mut cfg, &env, &name)?;
        SteelbxConfig::validate_config(&mut cfg)?;
        Ok(cfg)
    }

    #[test]
    fn env_values_are_visible_to_the_rest_but_not_each_other() {
        // Profile env wins over the caller for other values; a
        // fallback may refer to an env variable (absent from the
        // caller env); an env value set to empty overrides the caller
        // (the fallback then applies).
        let cfg = expand_against(
            r#"
image = "$IMG"
network = "${NET:-none}"
workdir = "${E:-/work}/code"

[env]
IMG = "img-1"
NET = "bridge"
E = ""
"#,
            &[("IMG", "caller-img"), ("E", "caller-e")],
            None,
            "base",
        )
        .unwrap();

        assert_eq!(cfg.image.as_deref(), Some("img-1"));
        assert_eq!(cfg.network.as_deref(), Some("bridge"));
        assert_eq!(cfg.workdir.as_deref(), Some("/work/code"));

        // Env values do NOT reference each other (no order in a
        // map): `$B` with `B` declared only in [env] is unset → error
        // (unless a fallback is given).
        assert!(expand_against(
            r#"
[env]
A = "$B"
B = "b"
"#,
            &[],
            None,
            "base",
        )
        .is_err());
    }

    #[test]
    fn expansion_is_applied_to_every_string_value() {
        // Deterministic: `$$` escapes to a literal `$`, no env needed.
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(
            &p,
            r#"
security_opts = ["label=dis$$able"]
"#,
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.security_opts, vec!["label=dis$able"]);
    }

    /// env_files are an expansion source only (NOT the container env):
    /// `[env]` holds just the inline entries (in order), and env_files
    /// variables resolve other values (later files override earlier).
    #[test]
    fn env_files_are_an_expansion_source_not_the_container_env() {
        let t = tempfile::tempdir().unwrap();
        let f1 = t.path().join("one.env");
        std::fs::write(&f1, "Apple=2\nBanana=3\n").unwrap();
        let f2 = t.path().join("two.env");
        std::fs::write(&f2, "Apple=20\nCherry=4\n").unwrap();
        let profile = t.path().join("containers.conf");
        std::fs::write(
            &profile,
            format!(
                "env_files = [\"{}\", \"{}\"]\nimage = \"$Apple\"\n\n[env]\nEgg = \"$Cherry\"\nDate = \"5\"\n",
                f1.to_str().unwrap(),
                f2.to_str().unwrap()
            ),
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&profile).unwrap();
        // Container env is the inline [env] only, in order (file vars
        // are resolved but not passed as their own keys).
        let order: Vec<&str> = cfg.env.keys().map(String::as_str).collect();
        assert_eq!(order, vec!["Egg", "Date"]);
        assert_eq!(cfg.env.get("Egg").map(String::as_str), Some("4"));
        assert_eq!(cfg.env.get("Date").map(String::as_str), Some("5"));
        // image resolves from the files (f2 overrides f1).
        assert_eq!(cfg.image.as_deref(), Some("20"));
    }

    /// An env file's values are visible to every other value (phase B
    /// expands against the merged env), like the inline `[env]`.
    #[test]
    fn env_file_values_are_visible_to_other_fields() {
        let t = tempfile::tempdir().unwrap();
        let f1 = t.path().join("one.env");
        std::fs::write(&f1, "IMG=my-image\n").unwrap();
        let profile = t.path().join("containers.conf");
        std::fs::write(
            &profile,
            format!(
                "env_files = [\"{}\"]\nimage = \"$IMG\"\n",
                f1.to_str().unwrap()
            ),
        )
        .unwrap();
        let cfg = SteelbxConfig::load_from(&profile).unwrap();
        assert_eq!(cfg.image.as_deref(), Some("my-image"));
    }

    /// `name` + `name_unique = false`: the declared name is pinned
    /// verbatim — expanded once, shape-checked, and available to every
    /// other value (including `[env]`) as `STEELBX_BOX_NAME`.
    #[test]
    fn pinned_name_is_the_steelbx_box_name_variable() {
        let cfg = expand_against(
            r#"
name = "declared-box"
name_unique = false
image = "img-${STEELBX_BOX_NAME}"
mounts = ["type=bind,source=/x/${STEELBX_BOX_NAME},destination=/data"]

[env]
BOX = "$STEELBX_BOX_NAME"
"#,
            &[],
            None,
            "base",
        )
        .unwrap();
        assert_eq!(cfg.name.as_deref(), Some("declared-box"));
        assert_eq!(cfg.image.as_deref(), Some("img-declared-box"));
        assert_eq!(
            cfg.mounts[0],
            "type=bind,source=/x/declared-box,destination=/data"
        );
        assert_eq!(cfg.env.get("BOX").map(String::as_str), Some("declared-box"));
    }

    /// A declared `name` without `name_unique = false`: unique by
    /// default — a fresh random token is appended, and that is
    /// `STEELBX_BOX_NAME`.
    #[test]
    fn declared_name_is_unique_by_default() {
        let cfg = expand_against(
            r#"
name = "declared-box"
image = "img-${STEELBX_BOX_NAME}"
"#,
            &[],
            None,
            "base",
        )
        .unwrap();
        // The name is the declared base plus an 8-hex token.
        let name = cfg.name.as_deref().unwrap();
        assert!(name.starts_with("declared-box-"), "name: {name}");
        assert_eq!(name.len(), "declared-box-".len() + 8);
        assert_eq!(cfg.image.as_deref(), Some(format!("img-{name}").as_str()));
    }

    /// The CLI `-n` override wins over the profile's `name` key: the
    /// effective name (what the box is actually called) is the
    /// `STEELBX_BOX_NAME` value everywhere, and is stored in `name`.
    #[test]
    fn cli_name_override_wins_over_the_profile_name() {
        let cfg = expand_against(
            r#"
name = "declared-box"
image = "img-${STEELBX_BOX_NAME}"
"#,
            &[],
            Some("flag-box"),
            "base",
        )
        .unwrap();
        assert_eq!(cfg.name.as_deref(), Some("flag-box"));
        assert_eq!(cfg.image.as_deref(), Some("img-flag-box"));
    }

    /// No declared name: a generated unique name is decided before the
    /// full pass and is `STEELBX_BOX_NAME` for every value that expands
    /// after it. (`[env]` and `image` expand first, to decide it, so
    /// they cannot reference a generated name — they fail loudly; a
    /// declared name is available to them too.)
    #[test]
    fn generated_name_is_the_steelbx_box_name_variable() {
        let cfg = expand_against(
            r#"
network = "net-${STEELBX_BOX_NAME}"
"#,
            &[],
            None,
            "gen-base",
        )
        .unwrap();
        // The generated name is the base plus an 8-hex token.
        let name = cfg.name.as_deref().unwrap();
        assert!(name.starts_with("gen-base-"), "name: {name}");
        assert_eq!(name.len(), "gen-base-".len() + 8);
        assert_eq!(cfg.network.as_deref(), Some(format!("net-{name}").as_str()));
        // A generated name is not known when `image`/`[env]` expand.
        assert!(expand_against(
            r#"
image = "img-${STEELBX_BOX_NAME}"
"#,
            &[],
            None,
            "gen-base",
        )
        .is_err());
    }

    /// The profile `name` is expanded once and shape-checked: an invalid
    /// podman name is an error naming the key; empty is absent (a
    /// generated name is used instead).
    #[test]
    fn profile_name_is_shape_checked() {
        // Invalid: a space is not a valid podman name.
        assert!(expand_against("name = \"bad name\"\n", &[], None, "base").is_err());
        // Invalid: a literal `$` (from a `$$` escape) is not a valid
        // podman name — the value is expanded exactly once (not
        // re-scanned as a reference) and then shape-checked.
        assert!(expand_against("name = \"a$$b\"\n", &[], None, "base").is_err());
        // Empty is absent (like `network`/`image`): a generated name is
        // used.
        let cfg = expand_against("name = \"  \"\n", &[], None, "gen").unwrap();
        assert!(cfg.name.as_deref().unwrap().starts_with("gen-"));
    }

    /// The create pipeline (`parse_profile` + `pre_expand` +
    /// `finish_expand`): the flag name is the declared name — the
    /// profile values see it as `STEELBX_BOX_NAME`, and the returned
    /// config carries it in `name`. Without the flag, the profile's own
    /// `name` key wins.
    #[test]
    fn create_pipeline_pins_the_declared_name() {
        let t = tempfile::tempdir().unwrap();
        let profiles = t.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(
            profiles.join("p.conf"),
            "image = \"img-${STEELBX_BOX_NAME}\"\n",
        )
        .unwrap();
        let dirs = std::slice::from_ref(&profiles);
        let mut cfg =
            SteelbxConfig::parse_from(&SteelbxConfig::find_profile_path("p", dirs).unwrap())
                .unwrap();
        let (_image, _declared, final_name) =
            SteelbxConfig::pre_expand(&mut cfg, Some("from-flag")).unwrap();
        SteelbxConfig::finish_expand(&mut cfg, final_name.as_deref().unwrap()).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("from-flag"));
        assert_eq!(cfg.image.as_deref(), Some("img-from-flag"));
        // No flag: the profile's own `name` key is the declared name.
        std::fs::write(
            profiles.join("q.conf"),
            "name = \"from-profile\"\nname_unique = false\nimage = \"img-${STEELBX_BOX_NAME}\"\n",
        )
        .unwrap();
        let mut cfg =
            SteelbxConfig::parse_from(&SteelbxConfig::find_profile_path("q", dirs).unwrap())
                .unwrap();
        let (_image, _declared, final_name) = SteelbxConfig::pre_expand(&mut cfg, None).unwrap();
        SteelbxConfig::finish_expand(&mut cfg, final_name.as_deref().unwrap()).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("from-profile"));
        assert_eq!(cfg.image.as_deref(), Some("img-from-profile"));
    }

    /// `runtime_env`: names only — not expanded, shape-checked.
    #[test]
    fn runtime_env_is_names_only() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, "runtime_env = [\"API_KEY\", \"MODEL\"]\n").unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(
            cfg.runtime_env,
            vec!["API_KEY".to_string(), "MODEL".to_string()]
        );
        // Not expanded (a name is a name, like env keys): an env
        // reference is a shape error.
        std::fs::write(&p, "runtime_env = [\"$TARGET\"]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
        // A bad name is an error.
        std::fs::write(&p, "runtime_env = [\"a b\"]\n").unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
    }

    /// A listed env file that is missing is an error (loud), not skipped.
    #[test]
    fn missing_env_file_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let profile = t.path().join("containers.conf");
        std::fs::write(&profile, "env_files = [\"/no-such-steelbx-test.env\"]\n").unwrap();
        assert!(SteelbxConfig::load_from(&profile).is_err());
    }

    /// A malformed env-file line (no `=`, or a bad key) is an error naming
    /// the file and line.
    #[test]
    fn malformed_env_file_line_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let f1 = t.path().join("one.env");
        std::fs::write(&f1, "this line has no equals\n").unwrap();
        let profile = t.path().join("containers.conf");
        std::fs::write(
            &profile,
            format!("env_files = [\"{}\"]\n", f1.to_str().unwrap()),
        )
        .unwrap();
        let err = format!("{}", SteelbxConfig::load_from(&profile).unwrap_err());
        assert!(
            err.contains("KEY=VALUE"),
            "error must name the bad line: {err}"
        );
        let f2 = t.path().join("two.env");
        std::fs::write(&f2, "a-b=1\n").unwrap();
        std::fs::write(
            &profile,
            format!("env_files = [\"{}\"]\n", f2.to_str().unwrap()),
        )
        .unwrap();
        assert!(SteelbxConfig::load_from(&profile).is_err());
    }

    /// The env-file line grammar: `#` comments and blank lines ignored,
    /// optional `export `, whitespace around `=`, an empty value, and
    /// order preserved. Values are raw (not expanded here).
    #[test]
    fn parse_env_file_rules_are_pinned() {
        let mut m: IndexMap<String, String> = IndexMap::new();
        parse_env_file(
            "# a comment\n\nexport Zeta=bar\nAlpha = spaced\nEMPTY=\n  Beta=4\n",
            "f",
            &mut m,
        )
        .unwrap();
        let order: Vec<&str> = m.keys().map(String::as_str).collect();
        assert_eq!(order, vec!["Zeta", "Alpha", "EMPTY", "Beta"]);
        assert_eq!(m.get("Zeta").map(String::as_str), Some("bar"));
        assert_eq!(m.get("Alpha").map(String::as_str), Some("spaced"));
        assert_eq!(m.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(m.get("Beta").map(String::as_str), Some("4"));
        assert!(parse_env_file("no equals here\n", "f", &mut IndexMap::new()).is_err());
        assert!(parse_env_file("a-b=1\n", "f", &mut IndexMap::new()).is_err());
    }

    #[test]
    fn extra_hosts_shape_is_still_checked() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, r#"extra_hosts = ["no-colon"]"#).unwrap();
        assert!(SteelbxConfig::load_from(&p).is_err());
    }

    #[test]
    fn loads_mounts_and_checks_their_shape() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, r#"mounts = ["type=devpts,destination=/dev/pts"]"#).unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.mounts, vec!["type=devpts,destination=/dev/pts"]);

        let p2 = t.path().join("bad.conf");
        std::fs::write(&p2, r#"mounts = ["type=bind,source=/x dest=/y"]"#).unwrap();
        assert!(SteelbxConfig::load_from(&p2).is_err());
    }

    #[test]
    fn loads_security_opts_and_checks_their_shape() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("containers.conf");
        std::fs::write(&p, r#"security_opts = ["label=disable"]"#).unwrap();
        let cfg = SteelbxConfig::load_from(&p).unwrap();
        assert_eq!(cfg.security_opts, vec!["label=disable"]);

        let p2 = t.path().join("bad.conf");
        std::fs::write(&p2, r#"security_opts = ["label"]"#).unwrap();
        assert!(SteelbxConfig::load_from(&p2).is_err());
    }
}
