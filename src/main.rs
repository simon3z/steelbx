use std::io::IsTerminal;

use anyhow::Context;

use clap::{CommandFactory, Parser, Subcommand, ValueHint};
use clap_complete::aot::{generate, Shell};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use clap_complete::CompleteEnv;

use steelbx::config::{self, SteelbxConfig};
use steelbx::driver::{self, CreateSpec, Podman};

#[derive(Parser)]
#[command(
    name = "steelbx",
    version,
    about = "run unsupervised workloads in isolated podman boxes"
)]
struct Cli {
    /// Print the podman commands as they run (stderr)
    #[arg(long, short = 'v')]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a box from a profile (created state; started on first
    /// enter)
    Create {
        /// Policy profile (`profiles/<name>.conf` — a complete
        /// `containers.conf` carrying the image, e.g. `image = "quay.io/..."`; replace semantics, no merging). Resolved
        /// in `~/.config/steelbx/profiles/`, then `/etc/steelbx/profiles/`
        /// (shipped; a user profile of the same name overrides).
        /// Defaults to `default`.
        /// Completes from the profiles dirs.
        #[arg(add = ArgValueCompleter::new(profile_candidates))]
        profile: Option<String>,
        /// Image override for the profile's `image` key (must be local:
        /// steelbx consumes images, it does not pull them). Completes
        /// from local images carrying the `com.github.simon3z.steelbx.box` or
        /// `com.github.containers.toolbox` label.
        #[arg(short = 'i', long = "image", add = ArgValueCompleter::new(image_candidates))]
        image: Option<String>,
        /// Box name (container name). Default: the image's name
        /// component without tag, e.g. `localhost/pi-steelbx:latest`
        /// → `pi-steelbx`. A taken name is refused — pass `-n`
        /// for another. Completes from the live boxes.
        #[arg(short = 'n', long = "name", add = ArgValueCompleter::new(box_name_candidates))]
        box_name: Option<String>,
        /// Host paths, mounted at <WORKDIR>/<basename>
        #[arg(value_hint = ValueHint::DirPath)]
        paths: Vec<String>,
    },
    /// Enter a box: start if needed, then interactive shell
    Enter {
        /// Box name
        #[arg(add = ArgValueCompleter::new(box_name_candidates))]
        box_name: String,
        /// Expose a caller env variable to the box (bare NAME — podman
        /// copies the value from your shell; `NAME=VALUE` is refused).
        /// Repeatable. The box's declared runtime env (`box.env` label)
        /// is always injected as well.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
    },
    /// Remove boxes; fails if running — --force force-deletes
    Rm {
        /// Box names
        #[arg(add = ArgValueCompleter::new(box_name_candidates))]
        box_names: Vec<String>,
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// List boxes: name, state
    Ps,
    /// Run a one-shot command in a box
    Exec {
        /// Box name
        #[arg(add = ArgValueCompleter::new(box_name_candidates))]
        box_name: String,
        /// Expose a caller env variable to the box (bare NAME — podman
        /// copies the value from your shell; `NAME=VALUE` is refused).
        /// Repeatable. The box's declared runtime env (`box.env` label)
        /// is always injected as well.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        cmd: Vec<String>,
    },
    /// Print static shell completions (`bash`, `zsh`, `fish`, `elvish`).
    /// The dynamic install (box-name completion) is `source
    /// <(COMPLETE=bash steelbx)` — see the README.
    #[command(hide = true)]
    Completion {
        /// The shell to print completions for
        shell: Shell,
    },
}

fn main() -> anyhow::Result<()> {
    // Dynamic completion (clap_complete engine): the sourced shell
    // function re-invokes this binary (`COMPLETE=<shell> ...`); the
    // request is handled and the process exits before any parsing. It
    // must run before anything writes to stdout.
    CompleteEnv::with_factory(Cli::command).complete();
    // Steelbx-provided env: the standard locations, set if
    // the caller hasn't overridden them — expansion and child
    // processes see the same values.
    provision_env()?;
    let cli = Cli::parse();
    driver::set_verbose(cli.verbose);
    match &cli.cmd {
        Cmd::Create {
            profile,
            image,
            box_name,
            paths,
        } => cmd_create(
            profile.as_deref().unwrap_or("default"),
            image.as_deref(),
            box_name.as_deref(),
            paths,
        ),
        Cmd::Enter { box_name, env } => cmd_enter(box_name, env),
        Cmd::Rm { box_names, force } => cmd_rm(box_names, *force),
        Cmd::Ps => cmd_ps(),
        Cmd::Exec { box_name, env, cmd } => cmd_exec(box_name, env, cmd),
        Cmd::Completion { shell } => cmd_completion(*shell),
    }
}

fn cmd_create(
    profile: &str,
    image: Option<&str>,
    box_name: Option<&str>,
    paths: &[String],
) -> anyhow::Result<()> {
    let pod = Podman::detect()?;
    if !is_remote() {
        driver::warn_rootful();
    }
    // Policy: the named profile (profiles/<name>.conf) — a complete
    // `containers.conf` (replace semantics, no merging). Loaded before
    // layout: a profile `workdir` overrides the base.
    let cfg = SteelbxConfig::load_profile(profile)?;
    // Image precedence: the `-i` flag > the profile's `image` key.
    let image = resolve_image(image, &cfg, profile)?;
    // One image inspect: layout (WORKDIR, overridable) + declared
    // default name.
    let meta = pod.image_meta(image)?;
    let (workdir, mounts) = create_layout(&cfg, &meta, paths)?;
    let box_name = box_name_from(box_name, image, &meta)?;

    // Refuse if this box exists — create never modifies.
    if let Some(existing) = pod.inspect(&box_name)? {
        anyhow::bail!(
            "box '{box_name}' already exists (state: {})\nRemove it first: steelbx rm {box_name}",
            existing.state.as_deref().unwrap_or("?")
        );
    }
    let spec = CreateSpec {
        image: image.to_string(),
        workdir,
        command: main_command(&cfg, &meta),
        mounts,
        runtime_env: merge_runtime_env(&cfg.runtime_env, &meta.env),
        ..(&cfg).into()
    };
    pod.create(&box_name, &spec)?;
    println!("Created box: {box_name}");
    println!("Enter with: steelbx enter {box_name}");
    Ok(())
}

/// The mount layout: the profile's `workdir` override > the image's
/// WORKDIR > the default `/work`. The override renders as `--workdir`
/// (regular container behavior — enter/exec run in the container's own
/// working directory); the resolved value is the mount base (where
/// derived mounts land).
fn create_layout(
    cfg: &SteelbxConfig,
    meta: &driver::ImageMeta,
    paths: &[String],
) -> anyhow::Result<(Option<String>, Vec<driver::Mount>)> {
    let workdir = cfg
        .workdir
        .clone()
        .map(|w| w.trim_end_matches('/').to_string())
        .filter(|w| !w.is_empty());
    let mount_base = workdir
        .as_deref()
        .or(meta.workdir.as_deref())
        .unwrap_or("/work");
    let mounts = steelbx::validate::derive_mounts(paths, mount_base)?;
    Ok((workdir, mounts))
}

/// Image precedence: the `-i` flag > the profile's `image` key;
/// neither = an error that names both ways out.
fn resolve_image<'a>(
    flag: Option<&'a str>,
    cfg: &'a SteelbxConfig,
    profile: &'a str,
) -> anyhow::Result<&'a str> {
    match flag {
        Some(i) => Ok(i),
        None => cfg.image.as_deref().ok_or_else(|| {
            anyhow::anyhow!("no image: pass -i <image> or set `image` in profile '{profile}'")
        }),
    }
}

/// Name precedence: -n flag > the image's declared name label
/// (`com.github.simon3z.steelbx.box.name`) > the image's name component without tag.
fn box_name_from(
    flag: Option<&str>,
    image: &str,
    meta: &driver::ImageMeta,
) -> anyhow::Result<String> {
    let name = flag
        .map(String::from)
        .or_else(|| meta.name.clone())
        .unwrap_or_else(|| default_box_name(image));
    if !driver::is_valid_name(&name) {
        anyhow::bail!("invalid box name: {name:?} — pass -n <name>");
    }
    Ok(name)
}

/// The runtime env names written onto the box at create (its
/// `box.env` label): the profile's `runtime_env` plus the names the
/// image declares (its `box.env` label) — deduped, order-preserving
/// (the profile first; the image adds what the profile did not).
fn merge_runtime_env(profile: &[String], image: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in profile.iter().chain(image.iter()) {
        if !out.iter().any(|x| x == n) {
            out.push(n.clone());
        }
    }
    out
}

/// The main command: profile `entry` > the image's declared command
/// (`com.github.simon3z.steelbx.box.cmd` label — empty argv = the image's own
/// ENTRYPOINT+CMD) > the heartbeat default. Resolved here — the
/// driver renders what it is given.
fn main_command(cfg: &SteelbxConfig, meta: &driver::ImageMeta) -> Vec<String> {
    if !cfg.entry.is_empty() {
        cfg.entry.clone()
    } else if meta.cmd {
        vec![]
    } else {
        vec!["sleep".to_string(), "infinity".to_string()]
    }
}

/// Set the steelbx-provided env vars if the caller hasn't:
/// `STEELBX_CONFIG_DIR` (where `profiles/` live)
/// and `STEELBX_DATA_DIR` (home for data files a box should see).
///
/// `STEELBX_CONNECTION`: the podman connection (e.g. `ssh://local`);
/// passed to podman as `CONTAINER_HOST` (unless already set).
/// When set, steelbx is in remote mode: local path validation is
/// skipped (paths are on the remote host).
fn provision_env() -> anyhow::Result<()> {
    let home =
        std::env::var_os("HOME").context("HOME is required (steelbx-provided env defaults)")?;
    let home = std::path::PathBuf::from(home);
    if std::env::var_os("STEELBX_CONFIG_DIR").is_none() {
        std::env::set_var("STEELBX_CONFIG_DIR", config::config_dir_path(&home));
    }
    if std::env::var_os("STEELBX_DATA_DIR").is_none() {
        std::env::set_var("STEELBX_DATA_DIR", config::data_dir(&home));
    }
    // Remote mode: pass STEELBX_CONNECTION to podman as CONTAINER_HOST
    // (unless the caller already set it).
    if let Some(conn) = std::env::var_os("STEELBX_CONNECTION") {
        if std::env::var_os("CONTAINER_HOST").is_none() {
            std::env::set_var("CONTAINER_HOST", &conn);
        }
    }
    Ok(())
}

/// Whether steelbx is in remote mode (a podman connection is active).
fn is_remote() -> bool {
    std::env::var_os("CONTAINER_HOST").is_some()
}

/// Default box name (when `-n` is absent): the image's name component
/// without tag. `localhost/pi-steelbx:latest` → `pi-steelbx`;
/// `registry.fedoraproject.org/fedora:42` → `fedora`.
fn default_box_name(image: &str) -> String {
    image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(':')
        .next()
        .unwrap_or(image)
        .to_string()
}

/// The window-title escape for a box session (OSC 0, icon + window
/// title): `ESC ] 0 ; <title> BEL`.
fn title_escape(name: &str) -> Vec<u8> {
    format!("\x1b]0;steelbx {name}\x07").into_bytes()
}

/// Set the terminal window title for the box session, when stdout is
/// a real terminal — never when piped (the escape bytes would pollute
/// captured output).
fn set_window_title(name: &str) {
    if std::io::stdout().is_terminal() {
        let _ = std::io::Write::write_all(&mut std::io::stdout(), &title_escape(name));
    }
}

fn cmd_enter(box_name: &str, env: &[String]) -> anyhow::Result<()> {
    if !std::io::stderr().is_terminal() {
        eprintln!(
            "warning: stderr is not a terminal — 'enter' will fail without \
                 a TTY; use 'exec' for non-interactive commands"
        );
    }
    validate_cli_env(env)?;
    let pod = Podman::detect()?;
    driver::warn_rootful();
    let info = resolve_box(&pod, box_name)?;
    let (inject, unset) = runtime_env_injection(&info.env, env, |n| std::env::var(n).is_ok());
    note_unset_env(box_name, &unset);
    set_window_title(box_name);
    pod.enter(box_name, &inject)?;
    // The box is started; the user is in it.
    Ok(())
}

/// The runtime env names to inject: the box's declared ones (its
/// `box.env` label) plus the CLI `-e` names — deduped, order-
/// preserving — kept to what is SET in the caller env. A bare `-e
/// NAME` copies the value from the caller env; a name that is not
/// set is not passed at all, so the box's own `[env]` value (if any)
/// survives. Returns (inject, unset) — the unset ones get the info
/// line, not an error (runtime env is per-session; absence is a value).
fn runtime_env_injection(
    declared: &[String],
    cli: &[String],
    present: impl Fn(&str) -> bool,
) -> (Vec<String>, Vec<String>) {
    let mut names: Vec<String> = Vec::new();
    for n in declared.iter().chain(cli.iter()) {
        if !names.iter().any(|x| x == n) {
            names.push(n.clone());
        }
    }
    let unset: Vec<String> = names.iter().filter(|n| !present(n)).cloned().collect();
    let inject: Vec<String> = names.into_iter().filter(|n| present(n)).collect();
    (inject, unset)
}

/// One info line per box: the declared runtime env names that are not
/// set in the caller env — left as the box has them (its `[env]` value
/// or unset).
fn note_unset_env(box_name: &str, unset: &[String]) {
    if !unset.is_empty() {
        eprintln!(
            "note: box '{box_name}' runtime env {} not set (left unset this session)",
            unset.join(", ")
        );
    }
}

/// CLI `-e` values: bare env names only (`NAME=VALUE` is refused — a
/// value on the command line is a value in the process list).
fn validate_cli_env(env: &[String]) -> anyhow::Result<()> {
    for n in env {
        if !steelbx::validate::is_valid_env_name(n) {
            anyhow::bail!("-e: {n:?} is not a valid variable name (bare names only)");
        }
    }
    Ok(())
}

fn cmd_rm(box_names: &[String], force: bool) -> anyhow::Result<()> {
    let pod = Podman::detect()?;
    driver::warn_rootful();
    for name in box_names {
        resolve_box(&pod, name)?;
        // The enforcement lives in the driver: plain rm refuses a running box.
        pod.remove_container(name, force)?;
    }
    // Silent on success.
    Ok(())
}

/// The single resolution point: the box must exist and carry the
/// steelbx box marker — no marker, no access.
fn resolve_box(pod: &Podman, name: &str) -> anyhow::Result<driver::BoxInfo> {
    let existing = pod
        .inspect(name)?
        .ok_or_else(|| anyhow::anyhow!("box '{name}' does not exist"))?;
    if existing.r#box.is_none() {
        anyhow::bail!(
            "'{name}' exists but is not a steelbx box (no {} label) — refusing; it is not in steelbx's scope",
            driver::BOX_MARKER
        );
    }
    Ok(existing)
}

fn cmd_ps() -> anyhow::Result<()> {
    let pod = Podman::detect()?;
    // Single call: no N+1. Header always, so an empty list still shows
    // the columns. Widths: the header is the floor, the widest value
    // wins — podman's human ages (e.g. "About an hour ago") and long
    // names must not push the IMAGE column out of alignment.
    let rows = pod.boxes()?;
    let name_w = rows
        .iter()
        .map(|b| b.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let state_w = rows
        .iter()
        .map(|b| b.state.len())
        .max()
        .unwrap_or(0)
        .max("STATE".len());
    let created_w = rows
        .iter()
        .map(|b| b.created.len())
        .max()
        .unwrap_or(0)
        .max("CREATED".len());
    println!(
        "{:<name_w$} {:<state_w$} {:<created_w$} IMAGE",
        "NAME", "STATE", "CREATED"
    );
    for b in &rows {
        println!(
            "{:<name_w$} {:<state_w$} {:<created_w$} {}",
            b.name, b.state, b.created, b.image
        );
    }
    Ok(())
}

fn cmd_exec(box_name: &str, env: &[String], cmd: &[String]) -> anyhow::Result<()> {
    validate_cli_env(env)?;
    let pod = Podman::detect()?;
    driver::warn_rootful();
    let info = resolve_box(&pod, box_name)?;
    let (inject, unset) = runtime_env_injection(&info.env, env, |n| std::env::var(n).is_ok());
    note_unset_env(box_name, &unset);
    set_window_title(box_name);
    pod.exec(box_name, &inject, cmd)?;
    Ok(())
}

/// Best-effort candidates, prefix-filtered: on error the underlying
/// source yields nothing and the shell keeps its default completion.
fn complete(candidates: Vec<String>, current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let current = current.to_str().unwrap_or_default();
    candidates
        .into_iter()
        .filter(|n| n.starts_with(current))
        .map(CompletionCandidate::new)
        .collect()
}

/// Image tab completion: local images marked with `com.github.simon3z.steelbx.box`
/// or `com.github.containers.toolbox`, one `podman images` call per marker
/// (podman-side label filter). Attached to `create`'s `-i` argument.
fn image_candidates(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let pod = Podman;
    complete(pod.image_names().unwrap_or_default(), current)
}

/// Profile tab completion: the `profiles/*.conf` names, one `read_dir`.
/// Attached to `create`'s `--profile`.
fn profile_candidates(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    complete(SteelbxConfig::list_profiles().unwrap_or_default(), current)
}

/// Box-name tab completion: the live boxes, one `podman ps` (no
/// version round-trip). Attached to `enter`/`rm`/`exec` box names.
fn box_name_candidates(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let pod = Podman;
    complete(pod.box_names().unwrap_or_default(), current)
}

fn cmd_completion(shell: Shell) -> anyhow::Result<()> {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "steelbx", &mut std::io::stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::aot::generate_to;

    /// The test-built binary. `CARGO_BIN_EXE_steelbx` is set for test
    /// targets that depend on the bin (integration tests); the bin's
    /// own unit tests sit beside it in `target/<profile>/deps`, so
    /// fall back to that relative path.
    fn binary() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("CARGO_BIN_EXE_steelbx") {
            return p.into();
        }
        let exe = std::env::current_exe().unwrap();
        let profile = exe
            .parent()
            .unwrap()
            .parent()
            .unwrap() // .../deps → .../target/<profile>
            .join(format!("steelbx{}", std::env::consts::EXE_SUFFIX));
        assert!(
            profile.exists(),
            "test-built binary missing: {}",
            profile.display()
        );
        profile
    }

    #[test]
    fn bash_completion_covers_the_verbs() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        generate(Shell::Bash, &mut cmd, "steelbx", &mut buf);
        let s = String::from_utf8_lossy(&buf);
        for verb in ["create", "enter", "exec", "ps", "rm", "completion"] {
            assert!(s.contains(verb), "completion must cover '{verb}'");
        }
    }

    #[test]
    fn image_resolves_flag_over_profile_key() {
        let cfg = SteelbxConfig {
            image: Some("profile-image".into()),
            ..Default::default()
        };
        // The flag wins.
        assert_eq!(
            resolve_image(Some("flag-image"), &cfg, "p").unwrap(),
            "flag-image"
        );
        // Absent flag: the profile's key.
        assert_eq!(resolve_image(None, &cfg, "p").unwrap(), "profile-image");
        // Neither: an error naming both ways out.
        let err = format!(
            "{}",
            resolve_image(None, &SteelbxConfig::default(), "pi-agent").unwrap_err()
        );
        assert!(err.contains("no image"));
        assert!(err.contains("-i"));
        assert!(err.contains("pi-agent"));
    }

    /// The box's `box.env` label: profile first, the image adds what
    /// the profile did not, deduped.
    /// Inject only what is set; the unset ones are returned for the
    /// info line (no failure). Deduped, order-preserving (declared
    /// first, CLI adds what the box did not declare).
    #[test]
    fn runtime_env_injection_is_set_names_only() {
        let (inject, unset) =
            runtime_env_injection(&["A".into(), "B".into()], &["B".into(), "C".into()], |n| {
                n == "A" || n == "C"
            });
        assert_eq!(inject, vec!["A".to_string(), "C".to_string()]);
        assert_eq!(unset, vec!["B".to_string()]);
    }

    #[test]
    fn runtime_env_merges_profile_and_image() {
        assert_eq!(
            merge_runtime_env(&["A".into(), "B".into()], &["B".into(), "C".into()]),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
        assert_eq!(merge_runtime_env(&[], &[]), Vec::<String>::new());
    }

    #[test]
    fn default_box_name_is_the_image_name_component() {
        assert_eq!(
            default_box_name("localhost/pi-steelbx:latest"),
            "pi-steelbx"
        );
        assert_eq!(
            default_box_name("registry.fedoraproject.org/fedora:42"),
            "fedora"
        );
        assert_eq!(default_box_name("pi-steelbx"), "pi-steelbx");
    }

    // The dynamic install (source <(COMPLETE=bash steelbx)) is primary;
    // the AOT verb is a fallback and must stay out of the help output.
    #[test]
    fn completion_subcommand_is_hidden() {
        let cli = Cli::command();
        let comp = cli
            .get_subcommands()
            .into_iter()
            .find(|s| s.get_name() == "completion")
            .expect("completion subcommand exists");
        assert!(
            comp.is_hide_set(),
            "'completion' must be hidden — the dynamic install is primary"
        );
    }

    /// The *dynamic* install: `COMPLETE=bash steelbx` emits the
    /// registration the user sources at shell startup; a syntax error
    /// there is a breakage of their shell. Pin the syntax with `bash -n`.
    #[test]
    fn dynamic_bash_registration_is_valid_bash() {
        let out = std::process::Command::new(binary())
            .env("COMPLETE", "bash")
            .output()
            .expect("running the binary");
        assert!(out.status.success());
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("registration.sh");
        std::fs::write(&p, &out.stdout).unwrap();
        let o = std::process::Command::new("bash")
            .args(["-n", p.to_str().unwrap()])
            .output();
        if let Ok(o) = o {
            assert!(
                o.status.success(),
                "bash -n failed on the dynamic registration: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }

    /// The window-title escape: OSC 0, the box name, terminated by BEL.
    #[test]
    fn title_escape_is_osc0_with_box_name() {
        assert_eq!(title_escape("my-box"), b"\x1b]0;steelbx my-box\x07");
    }

    /// The `-i` completion value path: the union of the two marker
    /// labels, deduped, podman-side filtered — proven against a fake
    /// `podman` shim (no podman needed; one `images` call per marker).
    #[test]
    fn create_image_completion_unions_both_marker_labels() {
        let t = tempfile::tempdir().unwrap();
        let bin = t.path().join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        let shim = bin.join("podman");
        std::fs::write(
            &shim,
            "#!/bin/sh\nif [ \"$1\" = images ]; then\ncase \"$3\" in\nlabel=com.github.simon3z.steelbx.box) printf '[{\"repoTags\":[\"box-a:latest\",\"box-b:latest\"]}]' ;;\nlabel=com.github.containers.toolbox) printf '[{\"repoTags\":[\"tool-a:latest\",\"box-a:latest\"]}]' ;;\nesac\nexit 0\nfi\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The shim path first, coreutils (`timeout`) still reachable.
        let path = format!("{fake}:/usr/bin:/bin", fake = bin.display());
        let out = std::process::Command::new(binary())
            .env("COMPLETE", "bash")
            .env("_CLAP_COMPLETE_INDEX", "4")
            .env("HOME", t.path())
            .env("PATH", path)
            .args(["--", "steelbx", "create", "prof", "-i", ""])
            .output()
            .expect("running the binary");
        let out = String::from_utf8_lossy(&out.stdout).to_string();
        let lines: Vec<&str> = out.lines().collect();
        // box-a carries BOTH labels: deduped to one candidate.
        assert_eq!(
            lines,
            vec!["box-a:latest", "box-b:latest", "tool-a:latest",]
        );
    }

    /// The profile positional completes from the profiles dirs (no
    /// podman involved): the values of `COMPLETE`-driven completion
    /// that do not touch podman.
    #[test]
    fn create_profile_completion_lists_profiles() {
        let t = tempfile::tempdir().unwrap();
        let profiles = t.path().join(".config/steelbx/profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        for name in ["pi-agent.conf", "toolbox.conf"] {
            std::fs::write(profiles.join(name), "image = \"x\"\n").unwrap();
        }
        let out = std::process::Command::new(binary())
            .env("COMPLETE", "bash")
            .env("_CLAP_COMPLETE_INDEX", "2")
            .env("HOME", t.path())
            .args(["--", "steelbx", "create", ""])
            .output()
            .expect("running the binary");
        let out = String::from_utf8_lossy(&out.stdout).to_string();
        let lines: Vec<&str> = out.lines().collect();
        // The engine also offers flags at a value position (bash
        // default completion): the profiles must be among the values.
        for p in ["pi-agent", "toolbox"] {
            assert!(lines.contains(&p), "profile {p:?} must be suggested");
        }
    }

    // The user sources this from a shell startup: a syntax error in it is
    // a breakage of their shell, not of steelbx. Pin the syntax, not just
    // the content — `bash -n` is the cheapest way.
    #[test]
    fn bash_completion_script_is_valid_bash() {
        let t = tempfile::tempdir().unwrap();
        let p = generate_to(Shell::Bash, &mut Cli::command(), "steelbx", t.path())
            .expect("generating the bash script to a file");
        if let Ok(o) = std::process::Command::new("bash")
            .args(["-n", p.to_str().unwrap()])
            .output()
        {
            assert!(
                o.status.success(),
                "bash -n failed on the generated script: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }
}
