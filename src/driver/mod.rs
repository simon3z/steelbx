//! Podman driver: subprocess CLI, machine-readable flags only.
//!
//! Args are built as vectors and passed to argv directly — never through a
//! shell (argument injection). The baseline is in `driver/args.rs`, and
//! only there; the podman-JSON parses are in `driver/parse.rs`.

pub(crate) mod args;
pub(crate) mod parse;

use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

pub use args::{init_exec_args, CreateSpec, Init, InitStep, Mount};

/// `--verbose`: print the podman commands as they run (stderr).
static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Supported podman major versions (fail loud with the range).
fn supported(major: u64) -> bool {
    matches!(major, 5 | 6)
}

/// Hard cap on a single non-interactive podman call: explicit
/// failure, no ambiguity — a wedged podman call must die with a message,
/// it must not hang steelbx silently. Interactive enter/exec are never
/// time-boxed. `timeout` is coreutils, present on the supported Fedora
/// baseline.
const CALL_TIMEOUT_SECS: &str = "120";

/// A detected, version-guarded podman.
pub struct Podman;

/// What `create` reads from the image in one `podman image inspect`
/// (no N+1): the layout (WORKDIR, the image's own field — the mount
/// derivation base: host paths land at `<workdir>/<basename>`) and
/// the optional declared default box name (`com.github.simon3z.steelbx.box.name`).
pub struct ImageMeta {
    /// The image's WORKDIR; `None` ⇒ the default layout `/work`.
    pub workdir: Option<String>,
    /// The declared default box name (optional label).
    pub name: Option<String>,
    /// The image declares its own complete main command
    /// (`com.github.simon3z.steelbx.box.cmd` label, presence) — steelbx appends
    /// nothing.
    pub cmd: bool,
    /// The image's declared runtime env names (the `box.env` label,
    /// comma-separated).
    pub env: Vec<String>,
}

/// Facts for one box (container); `inspect` returns `None` if it is
/// absent.
pub struct BoxInfo {
    /// podman state: "created" or "running".
    pub state: Option<String>,
    /// The box marker's value as recorded at create (label
    /// `com.github.simon3z.steelbx.box`, conventionally `true`); `None` for a
    /// container without the marker — not a steelbx box.
    pub r#box: Option<String>,
    /// The declared runtime env names (the `box.env` label written at
    /// create); enter/exec inject them as bare `-e NAME`.
    pub env: Vec<String>,
}

/// The box marker — presence is the check; the value is
/// conventionally `true` and never read. Two roles, one key:
/// - image: "this is a box image" (identity of the artifact); feeds
///   `create`'s completion only
/// - container: "this is a steelbx box" (the instance's identity)
///   The mapping lives on the object itself (podman's own index);
///   steelbx owns no state.
pub const BOX_MARKER: &str = "com.github.simon3z.steelbx.box";
/// The toolbox marker: on an image, "this image is a toolbox base
/// image" (the toolbox profiles' base); feeds `create`'s `-i`
/// completion only — a toolbox base carries no box layout, so
/// nothing else reads it.
pub const TOOLBOX_MARKER: &str = "com.github.containers.toolbox";
/// The box name label: on an image, the *declared default name* for
/// `create` (optional); on a container, written by `create`, a
/// mirror of the container name. The name is NOT a second identity:
/// box name = container name, always.
pub const BOX_NAME_LABEL: &str = "com.github.simon3z.steelbx.box.name";
/// The runtime env label: one value holding a comma-separated list of
/// env names the box exposes from the caller env at enter/exec (bare
/// `-e NAME`; the value is never argv). Written at create (the profile
/// `runtime_env` ∪ the image's own `box.env`); read back at
/// enter/exec. A label is one value — podman labels cannot repeat a
/// key — so the list is comma-separated (env names cannot contain
/// commas).
pub const BOX_ENV_LABEL: &str = "com.github.simon3z.steelbx.box.env";

/// One `steelbx ps` row: a steelbx box (marker-labelled) with the
/// fields the listing shows — name, state, image, and the human age
/// (podman's "created" column, e.g. "8 hours ago").
pub struct BoxRow {
    pub name: String,
    pub state: String,
    pub image: String,
    pub created: String,
}
impl Podman {
    /// The podman binary name: `podman-remote` in remote mode
    /// (`CONTAINER_HOST` set), `podman` otherwise.
    pub fn binary() -> &'static str {
        if std::env::var_os("CONTAINER_HOST").is_some() {
            "podman-remote"
        } else {
            "podman"
        }
    }
    pub fn detect() -> Result<Self> {
        let bin = Self::binary();
        let out = Command::new(bin)
            .arg("--version")
            .output()
            .with_context(|| format!("'{bin}' not found on PATH"))?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // "podman version X.Y.Z" or "podman-remote version X.Y.Z"
        let version = if let Some(v) = text.strip_prefix("podman-remote version") {
            v.trim().to_string()
        } else if let Some(v) = text.strip_prefix("podman version") {
            v.trim().to_string()
        } else {
            bail!("unexpected '{bin} --version' output: {text}");
        };
        let major: u64 = version
            .split('.')
            .next()
            .unwrap_or("0")
            .parse()
            .context("parsing podman major version")?;
        if !supported(major) {
            bail!("podman {version} is outside the supported range (5.x\u{2013}6.x)");
        }
        Ok(Podman)
    }

    /// The time-boxed podman subprocess (a wedged call must die
    /// with a message, not hang steelbx). Interactive enter/exec never go
    /// through this.
    fn timeout_cmd(args: &[String]) -> Command {
        let bin = Self::binary();
        let mut cmd = Command::new("timeout");
        cmd.args(["-s", "KILL", CALL_TIMEOUT_SECS, bin]).args(args);
        cmd
    }

    /// Runs a podman arg vector (never a shell string), time-boxed, and
    /// returns the output — success or failure alike. A wedged call
    /// (timeout) is `Err`, not a hang.
    fn run_timeboxed(args: &[String]) -> Result<std::process::Output> {
        if VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("podman {}", args.join(" "));
        }
        let out = Self::timeout_cmd(args).output().context("running podman")?;
        if out.status.code() == Some(124) {
            bail!(
                "podman {} timed out after {CALL_TIMEOUT_SECS}s — a podman \
                 process may be stuck (check `ps -ef | grep podman`) and \
                 should be killed before retrying",
                args.join(" ")
            );
        }
        Ok(out)
    }

    /// A time-boxed run: any non-zero exit is an error naming stderr.
    fn run(&self, args: &[String]) -> Result<std::process::Output> {
        let out = Self::run_timeboxed(args)?;
        if !out.status.success() {
            bail!(
                "podman {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(out)
    }

    /// Machine-readable inspect (the parse tests pin the shape); the
    /// expected `None` for "no such container" is a value, not an
    /// error. Any other `podman inspect` failure (a wedged store) is
    /// an error — never reported as absence.
    pub fn inspect(&self, name: &str) -> Result<Option<BoxInfo>> {
        let args = vec!["inspect".to_string(), name.to_string()];
        let out = match self.run(&args) {
            Ok(o) => o,
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("no such container")
                || msg.contains("no such object")
                || msg.contains("not found") {
                    return Ok(None);
                }
                return Err(e);
            }
        };
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).context("parsing 'podman inspect' JSON")?;
        let state = v
            .get(0)
            .and_then(|c| c.get("State"))
            .and_then(|s| s.get("Status"))
            .and_then(|s| s.as_str())
            .map(str::to_string);
        Ok(Some(BoxInfo {
            state,
            r#box: parse::box_marker_from_inspect(&v),
            env: parse::env_names_from_inspect(&v),
        }))
    }

    /// One machine-readable image inspect: layout + declared name.
    pub fn image_meta(&self, image: &str) -> Result<ImageMeta> {
        let args = vec![
            "image".to_string(),
            "inspect".to_string(),
            image.to_string(),
        ];
        let out = match self.run(&args) {
            Ok(o) => o,
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("no such image") || msg.contains("not found") {
                    bail!(
                        "image '{image}' is not local — steelbx consumes images, it \
                         does not pull them: podman pull {image}"
                    )
                }
                return Err(e);
            }
        };
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).context("parsing 'podman image inspect' JSON")?;
        Ok(ImageMeta {
            workdir: parse::workdir_from_image_inspect(&v),
            name: parse::name_label_from_image_inspect(&v),
            cmd: parse::cmd_label_from_image_inspect(&v),
            env: parse::env_names_from_inspect(&v),
        })
    }

    /// Build the box from the image and leave it in `created` state
    /// (lazy start). Any post-create failure rolls back — ordered ops
    /// + rollback, no orphans.
    pub fn create(&self, box_name: &str, spec: &CreateSpec) -> Result<()> {
        let args = Self::create_args(box_name, spec);
        let res = self.run(&args);
        if let Err(e) = res {
            // Roll back only a half-built container that exists: a
            // rejected create (a bad flag) often leaves nothing, and
            // rm-ing a nonexistent name is noise, not cleanup.
            if matches!(self.inspect(box_name), Ok(Some(_))) {
                let _ = self.remove_container(box_name, false);
            }
            return Err(e);
        }
        // Post-create initialization: the declared steps (exec / cp)
        // run in the declared order —
        // exec as root, cp host to container — before the box is
        // handed over. Init implies start (exec needs a running
        // container); a failure rolls the box back — a
        // half-initialized box is not a box.
        if let Some(init) = &spec.init {
            let init_res = self.run_init(box_name, init);
            if let Err(e) = init_res {
                if matches!(self.inspect(box_name), Ok(Some(_))) {
                    // Force: init implied start, so the box may be
                    // running — a plain rm would politely refuse and
                    // leave it behind.
                    let _ = self.remove_container(box_name, true);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// The init steps, in the order declared — a cp may follow the
    /// exec that makes its destination exist (the order is pinned
    /// by the integration test).
    fn run_init(&self, box_name: &str, init: &Init) -> Result<()> {
        // Exec needs a running container: init implies start.
        self.run(&["start".into(), box_name.into()])
            .context("starting the box for init")?;
        for (i, step) in init.iter().enumerate() {
            match step {
                // Exec steps run as root: init configures the box.
                InitStep::Exec(argv) => {
                    let args = init_exec_args(box_name, argv);
                    Self::podman_exec(&format!("init.exec[{i}]"), args)?;
                }
                // Cps are host to container: podman cp.
                InitStep::Cp { host, dest } => {
                    Self::podman_exec(
                        &format!("init.cp[{i}]"),
                        vec![
                            "cp".to_string(),
                            host.to_string_lossy().to_string(),
                            format!("{box_name}:{dest}"),
                        ],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// `rm` never orphans. Single enforcement point, here: without
    /// `force`, a running box is refused — no silent
    /// escalation. Force: kill (SIGKILL, no stop grace — `rm -f`'s
    /// SIGTERM grace was a live-measured 10.4s) then rm. Plain:
    /// rm; a failed graceful rm is retried once with force (wedged box).
    pub fn remove_container(&self, name: &str, force: bool) -> Result<()> {
        let running = self
            .inspect(name)?
            .is_some_and(|i| i.state.as_deref() == Some("running"));
        if !force && running {
            bail!("box '{name}' is running\nForce-remove it: steelbx rm --force {name}");
        }
        if force {
            let _ = self.run(&["kill".into(), name.into()]);
        }

        match self.run(&["rm".into(), name.into()]) {
            Ok(_) => Ok(()),
            Err(e) => {
                eprintln!("warning: 'podman rm {name}' failed ({e}) — retrying with force");
                match self.run(&["rm".into(), "-f".into(), name.into()]) {
                    Ok(_) => Ok(()),
                    Err(e2) => {
                        bail!(
                            "removal of '{name}' failed even with force: {e2}\nManual escape: podman rm -f {name}"
                        )
                    }
                }
            }
        }
    }

    /// `podman exec` with inherited stdio (never time-boxed — it is
    /// interactive); a non-zero exit is an error named by the verb.
    fn podman_exec(label: &str, args: Vec<String>) -> Result<()> {
        if VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("podman {}", args.join(" "));
        }
        let bin = Self::binary();
        let status = Command::new(bin)
            .args(args)
            .status()
            .context(format!("running 'podman exec' ({label})"))?;
        if !status.success() {
            bail!("{label} exited: {status}");
        }
        Ok(())
    }

    /// Start if not running, then interactive exec (entry is always
    /// exec, never attach). `env`: the runtime env NAMES, each passed
    /// bare (podman copies the value from the caller env — the value
    /// is never argv). Stdio is inherited; the exec runs with the
    /// container's own user and working directory (the image's
    /// official `USER`/`WORKDIR`).
    pub fn enter(&self, name: &str, env: &[String]) -> Result<()> {
        self.ensure_running(name)?;
        Self::podman_exec("enter", Self::enter_args(name, env))
    }

    /// One-shot command in the box (starts it if needed), as
    /// the container's own user, in its own working directory. `env`
    /// is the runtime env NAMES (bare `-e NAME`; see `enter`).
    pub fn exec(&self, name: &str, env: &[String], cmd: &[String]) -> Result<()> {
        self.ensure_running(name)?;
        let mut args = vec!["exec".to_string()];
        for e in env {
            args.push("-e".to_string());
            args.push(e.clone());
        }
        args.push(name.to_string());
        args.extend(cmd.iter().cloned());
        Self::podman_exec("exec", args)
    }

    fn ensure_running(&self, name: &str) -> Result<BoxInfo> {
        let info = self
            .inspect(name)?
            .ok_or_else(|| anyhow!("box '{name}' does not exist"))?;
        if info.state.as_deref() != Some("running") {
            self.run(&["start".into(), name.into()])?;
        }
        Ok(info)
    }

    /// List the boxes. One podman call, client-side marker filter (no
    /// N+1). Each row carries the fields `steelbx ps` shows: name,
    /// state, image, and the human age. The name is the container
    /// name: the name you see in `podman ps` is the name you use.
    pub fn boxes(&self) -> Result<Vec<BoxRow>> {
        let args = vec![
            "ps".into(),
            "--all".into(),
            "--format".into(),
            "json".into(),
        ];
        let out = self.run(&args)?;
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).context("parsing 'podman ps' JSON")?;
        Ok(parse::ps_rows_from_json(&v))
    }

    /// Local images marked with `BOX_MARKER` or `TOOLBOX_MARKER`, as
    /// tappable tag references: the `create -i` image tab-completion
    /// path. The label filter is podman-side (`--filter label=`) — one
    /// call per marker, unioned and deduped (an image can carry both),
    /// so the JSON parse needs no label field. Best effort: on error
    /// the shell keeps its default completion.
    pub fn image_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for marker in [BOX_MARKER, TOOLBOX_MARKER] {
            let args = vec![
                "images".into(),
                "--filter".into(),
                format!("label={marker}"),
                "--format".into(),
                "json".into(),
            ];
            let out = self.run(&args)?;
            let v: serde_json::Value =
                serde_json::from_slice(&out.stdout).context("parsing 'podman images' JSON")?;
            for r in parse::image_refs_from_images_json(&v) {
                if seen.insert(r.clone()) {
                    names.push(r);
                }
            }
        }
        Ok(names)
    }

    /// Box names only: the tab-completion path. Same single podman call
    /// as `boxes`, names projected — no version round-trip, so a tab
    /// costs one podman call, not two. Best effort: on error the shell
    /// keeps its default completion.
    pub fn box_names(&self) -> Result<Vec<String>> {
        Ok(self.boxes()?.into_iter().map(|row| row.name).collect())
    }
}

/// Podman container naming: [a-zA-Z0-9] then [a-zA-Z0-9._:/-].
pub fn is_valid_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
}

/// Rootful podman means an escape means root.
pub fn warn_rootful() {
    if current_uid() == 0 {
        eprintln!(
            "warning: running as root — boxes run rootful; a box escape is a root \
             escape. Use rootless podman where possible."
        );
    }
}

fn current_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|l| l.split_whitespace().next())
                .and_then(|u| u.parse::<u32>().ok())
        })
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_names_follow_podman_naming() {
        assert!(is_valid_name("feature-x"));
        assert!(!is_valid_name("-x"));
        assert!(!is_valid_name("has space"));
    }
}
