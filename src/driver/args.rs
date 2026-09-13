//! Podman argv building: args are vectors, never shell strings.
//!
//! The baseline flag set lives here, and only here.

use std::path::PathBuf;

use indexmap::IndexMap;

use super::Podman;

/// One derived bind mount: canonical host path → box destination
/// (`<workdir>/<basename>`, from the image's WORKDIR).
pub struct Mount {
    /// Canonical host path.
    pub host: PathBuf,
    /// Path inside the box.
    pub dest: String,
}

/// A list of init steps: grammar checked at
/// deserialize (the tag and the arity); policy (host exists, dest
/// absolute) is validated by the config layer.
#[derive(Debug, Clone, PartialEq)]
pub enum InitStep {
    Exec(Vec<String>),
    Cp { host: PathBuf, dest: String },
}

/// `init` is a list of steps (validated: non-empty).
pub type Init = Vec<InitStep>;

impl<'de> serde::Deserialize<'de> for InitStep {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_seq(InitStepVisitor)
            .map_err(|e| serde::de::Error::custom(format!("an init step: {e}")))
    }
}

struct InitStepVisitor;

impl<'de> serde::de::Visitor<'de> for InitStepVisitor {
    type Value = InitStep;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an init step: [\"exec\", ...] or [\"cp\", source, dest]")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<InitStep, A::Error> {
        let verb: String = seq.next_element()?.ok_or_else(|| {
            serde::de::Error::custom("an init step is [\"exec\", ...] or [\"cp\", source, dest]")
        })?;
        match verb.as_str() {
            "exec" => {
                let mut argv = Vec::new();
                while let Some(s) = seq.next_element()? {
                    argv.push(s);
                }
                Ok(InitStep::Exec(argv))
            }
            "cp" => {
                let host: String = seq.next_element()?.ok_or_else(|| {
                    serde::de::Error::custom("a cp step is [\"cp\", source, dest]")
                })?;
                let dest: String = seq.next_element()?.ok_or_else(|| {
                    serde::de::Error::custom("a cp step is [\"cp\", source, dest]")
                })?;
                if seq.next_element::<String>()?.is_some() {
                    return Err(serde::de::Error::custom(
                        "a cp step is exactly [\"cp\", source, dest]",
                    ));
                }
                Ok(InitStep::Cp {
                    host: PathBuf::from(host),
                    dest,
                })
            }
            other => Err(serde::de::Error::custom(format!(
                "an init step verb must be \"exec\" or \"cp\" (got {other:?})"
            ))),
        }
    }
}

/// The argv of an init `exec` step: `podman exec -u 0
/// -w / <box> <argv...>` — root (init configures the box), cwd pinned
/// to `/` (a `workdir` override may name a directory init itself
/// makes, so init's cwd is the container root, not the declared
/// layout).
pub fn init_exec_args(box_name: &str, argv: &[String]) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "-u".to_string(),
        "0".to_string(),
        "-w".to_string(),
        "/".to_string(),
    ];
    args.push(box_name.to_string());
    args.extend(argv.iter().cloned());
    args
}

/// Everything `create` renders into podman args: the image and the
/// policy (network, extra_hosts) merged from config + labels.
#[derive(Default)]
pub struct CreateSpec {
    pub image: String,
    /// The `WORKDIR` override (the profile `workdir`), rendered as
    /// podman `--workdir`; absent ⇒ the image's own `WORKDIR`.
    pub workdir: Option<String>,
    pub mounts: Vec<Mount>,
    pub network: Option<String>,
    pub extra_hosts: Vec<String>,
    /// Env overrides (podman `--env KEY=VALUE`); same-key wins over the
    /// image's baked-in ENV.
    pub env: IndexMap<String, String>,
    /// The runtime env NAMES (the merged profile `runtime_env` + the
    /// image's `box.env`), rendered as the `box.env` container label;
    /// read back at enter/exec and injected as bare `-e NAME`.
    pub runtime_env: Vec<String>,
    /// Security options (`key=value`), rendered as podman
    /// `--security-opt VALUE` (e.g. `label=disable`); absent means
    /// podman's default security posture.
    pub security_opts: Vec<String>,
    /// Post-create initialization: run right after create,
    /// before the box is handed over; each command is an exec as the
    /// declared user (default root), not the main process.
    pub init: Option<Init>,
    /// Podman `--mount` specs from the config `mounts` key — passthrough
    /// (podman interprets), in addition to the derived bind mounts.
    pub mount_specs: Vec<String>,
    /// The main command argv, rendered after the image:
    /// a profile `entry`, the image's own command (empty — the
    /// `com.github.simon3z.steelbx.box.cmd` label), or the heartbeat default
    /// (`sleep infinity`).
    pub command: Vec<String>,
    /// Namespace/identity passthrough (podman interprets the values):
    /// `--cgroupns`, `--ipc`, `--pid`, `--userns`, `--user`, and
    /// `--ulimit` per element. Absent = podman's default for each.
    pub cgroupns: Option<String>,
    pub ipc: Option<String>,
    pub pid: Option<String>,
    pub userns: Option<String>,
    /// The `USER` override (the profile `user`), rendered as podman
    /// `--user`; absent ⇒ the image's own `USER`.
    pub user: Option<String>,
    pub privileged: bool,
    pub no_hosts: bool,
    pub ulimits: Vec<String>,
}

impl From<&crate::config::SteelbxConfig> for CreateSpec {
    /// The policy fields, verbatim from the loaded config; the
    /// layout fields (`image`, `workdir`, `mounts`, `command`) are
    /// resolved by the caller and filled in separately.
    fn from(cfg: &crate::config::SteelbxConfig) -> Self {
        Self {
            image: String::new(),
            workdir: None,
            mounts: vec![],
            command: vec![],
            network: cfg.network.clone(),
            extra_hosts: cfg.extra_hosts.clone(),
            env: cfg.env.clone(),
            // The runtime env is a merge (profile + the image's own
            // declaration), resolved by the caller after the image
            // inspect; from the config alone it is just the profile's.
            runtime_env: cfg.runtime_env.clone(),
            security_opts: cfg.security_opts.clone(),
            mount_specs: cfg.mounts.clone(),
            init: cfg.init.clone(),
            cgroupns: cfg.cgroupns.clone(),
            ipc: cfg.ipc.clone(),
            pid: cfg.pid.clone(),
            userns: cfg.userns.clone(),
            user: cfg.user.clone(),
            privileged: cfg.privileged,
            no_hosts: cfg.no_hosts,
            ulimits: cfg.ulimits.clone(),
        }
    }
}

impl Podman {
    /// The pinned podman baseline. The main process is the heartbeat —
    /// `sleep infinity` requires coreutils in the image.
    /// The hostname is pinned to the box name: the identity inside the
    /// box is the box name (no second identity).
    /// Networking is the policy layer's call: a `network` value becomes a
    /// `--network` flag; absent means podman's default.
    fn flag(args: &mut Vec<String>, flag: &str, value: &str) {
        args.push(flag.to_string());
        args.push(value.to_string());
    }

    pub fn create_args(box_name: &str, spec: &CreateSpec) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            box_name.into(),
            // The box name is the hostname (podman's default is the
            // same — pinned explicitly): 'hostname' inside the box
            // answers which box you are in (no second identity).
            "--hostname".into(),
            box_name.into(),
            // Podman's _podman_init as PID 1: signal forwarding and
            // zombie reaping without an image dependency.
            "--init".into(),
        ];
        Self::mount_flags(&mut args, spec);
        Self::value_flags(&mut args, spec);
        Self::identity_flags(&mut args, spec);
        Self::label_flags(&mut args, box_name, spec);
        Self::tail_flags(&mut args, spec);
        args
    }

    /// The `--mount` flags (the derived bind mounts, then the
    /// passthrough `mount_specs`), the `--network` flag, and the
    /// `--add-host` entries: the layout and policy strings, in
    /// declaration order.
    fn mount_flags(args: &mut Vec<String>, spec: &CreateSpec) {
        for m in &spec.mounts {
            Self::flag(
                args,
                "--mount",
                &format!("type=bind,src={},dst={}", m.host.to_string_lossy(), m.dest),
            );
        }
        if let Some(net) = &spec.network {
            Self::flag(args, "--network", net);
        }
        for m in &spec.mount_specs {
            // Passthrough podman `--mount` spec (the config `mounts`
            // key; podman is the interpreter — argv-vector safe).
            Self::flag(args, "--mount", m);
        }
        for h in &spec.extra_hosts {
            // `name:address` — podman resolves the address (e.g. the
            // `host-gateway` keyword) against the chosen network.
            Self::flag(args, "--add-host", h);
        }
    }

    /// The env overrides (`--env KEY=VALUE`, in declaration order) and
    /// the security options (`--security-opt`); podman interprets
    /// both.
    fn value_flags(args: &mut Vec<String>, spec: &CreateSpec) {
        for (k, v) in &spec.env {
            // Podman `--env KEY=VALUE`: same-key overrides the image ENV.
            Self::flag(args, "--env", &format!("{k}={v}"));
        }
        for opt in &spec.security_opts {
            // Podman `--security-opt key=value` (e.g. `label=disable`);
            // off by default — the config decides.
            Self::flag(args, "--security-opt", opt);
        }
    }

    /// Namespace/identity passthrough: the config decides, podman
    /// interprets the value. `user` is the `USER` override:
    /// absent ⇒ the image's own `USER` (regular container behavior).
    fn identity_flags(args: &mut Vec<String>, spec: &CreateSpec) {
        for (flag, slot) in [
            ("--cgroupns", &spec.cgroupns),
            ("--ipc", &spec.ipc),
            ("--pid", &spec.pid),
            ("--userns", &spec.userns),
            ("--user", &spec.user),
        ] {
            if let Some(v) = slot {
                Self::flag(args, flag, v);
            }
        }
        for u in &spec.ulimits {
            Self::flag(args, "--ulimit", u);
        }
        if spec.privileged {
            args.push("--privileged".into());
        }
        if spec.no_hosts {
            args.push("--no-hosts".into());
        }
    }

    /// The box mapping is labels on the container (podman's
    /// own index); steelbx owns no state. Marker + name (mirror of
    /// the container name). The runtime env names: one label,
    /// comma-separated (a label is one value; podman labels cannot
    /// repeat a key). Absent ⇒ no label: the box declares no
    /// runtime env.
    fn label_flags(args: &mut Vec<String>, box_name: &str, spec: &CreateSpec) {
        Self::flag(args, "--label", &format!("{}=true", super::BOX_MARKER));
        Self::flag(
            args,
            "--label",
            &format!("{}={box_name}", super::BOX_NAME_LABEL),
        );
        if !spec.runtime_env.is_empty() {
            Self::flag(
                args,
                "--label",
                &format!("{}={}", super::BOX_ENV_LABEL, spec.runtime_env.join(",")),
            );
        }
    }

    /// The `WORKDIR` override (a create flag; absent ⇒ the image's
    /// own `WORKDIR`), the image, and the main command: already
    /// resolved (profile `entry` > the image's declared command > the
    /// heartbeat). Empty = the image's own ENTRYPOINT+CMD.
    fn tail_flags(args: &mut Vec<String>, spec: &CreateSpec) {
        if let Some(w) = &spec.workdir {
            Self::flag(args, "--workdir", w);
        }
        args.push(spec.image.clone());
        args.extend(spec.command.iter().cloned());
    }

    /// The exec command `enter` runs in the box: interactive (-it),
    /// non-login bash. Non-login → `~/.bashrc` is read; interactive is
    /// what makes it so. (The old `/bin/sh` ran bash in POSIX mode, which
    /// skips `.bashrc` entirely.) A login shell (`bash -l`) would read
    /// profiles instead — not what an interactive human shell expects.
    /// The exec passes no `-u`/`-w`: podman
    /// runs it as the container's own user, in its own working
    /// directory — the image's official `USER`/`WORKDIR`, or the
    /// profile's `--user`/`--workdir` overrides.
    /// The runtime env names are passed BARE (`-e NAME`): podman
    /// copies the value from the caller env at exec time — the value
    /// itself is never argv, so a secret is never on the command
    /// line. A name absent from the caller env is not passed at all
    /// (the box's own `[env]` value, if any, survives — see the
    /// caller's filter).
    pub fn enter_args(name: &str, env: &[String]) -> Vec<String> {
        let mut args = vec!["exec".into(), "--interactive".into(), "--tty".into()];
        for e in env {
            args.push("-e".into());
            args.push(e.clone());
        }
        args.push(name.to_string());
        args.push("/bin/bash".into());
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_exec_argv_is_root_pinned_to_workdir_root() {
        let args = init_exec_args(
            "boxname",
            &["useradd".to_string(), "-m".to_string(), "simon".to_string()],
        );
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "-u".to_string(),
                "0".to_string(),
                "-w".to_string(),
                "/".to_string(),
                "boxname".to_string(),
                "useradd".to_string(),
                "-m".to_string(),
                "simon".to_string(),
            ]
        );
    }

    fn spec() -> CreateSpec {
        CreateSpec {
            image: "img".into(),
            mounts: vec![Mount {
                host: PathBuf::from("/work"),
                dest: "/root/.pi/foo".into(),
            }],
            command: vec!["sleep".into(), "infinity".into()],
            ..Default::default()
        }
    }

    fn toolbox_like_spec() -> CreateSpec {
        let mut s = spec();
        s.cgroupns = Some("host".into());
        s.ipc = Some("host".into());
        s.pid = Some("host".into());
        s.userns = Some("keep-id".into());
        s.user = Some("root:root".into());
        s.privileged = true;
        s.no_hosts = true;
        s.ulimits = vec!["host".into()];
        s
    }

    #[test]
    fn namespace_and_identity_flags_render_when_declared() {
        let a = Podman::create_args("tb", &toolbox_like_spec());
        for (flag, value) in [
            ("--cgroupns", "host"),
            ("--ipc", "host"),
            ("--pid", "host"),
            ("--userns", "keep-id"),
            ("--user", "root:root"),
            ("--ulimit", "host"),
        ] {
            let i = a.iter().position(|x| x == flag).unwrap();
            assert_eq!(a[i + 1], value);
        }
        assert!(a.contains(&"--privileged".to_string()));
        assert!(a.contains(&"--no-hosts".to_string()));

        // Absent keys: no flags (podman's defaults stay podman's —
        // the image's official USER/WORKDIR apply).
        let a = Podman::create_args("tb", &spec());
        for flag in [
            "--cgroupns",
            "--ipc",
            "--pid",
            "--userns",
            "--user",
            "--workdir",
            "--ulimit",
            "--privileged",
            "--no-hosts",
        ] {
            assert!(!a.contains(&flag.to_string()), "'{flag}' must be absent");
        }
    }

    #[test]
    fn baseline_pins_security_defaults() {
        let a = Podman::create_args("pi", &spec());
        // Podman's default posture: the baseline carries no security
        // opts, and the old hardening (cap-drop/no-new-privileges) is
        // gone (it broke package installs).
        assert!(!a.contains(&"label=disable".to_string()));
        assert!(!a.contains(&"--security-opt".to_string()));
        assert!(!a.contains(&"--cap-drop".to_string()));
        assert!(!a.contains(&"no-new-privileges".to_string()));
        // Podman's init binary as PID 1 (signal forwarding, zombie
        // reaping) — always pinned, no image dependency.
        assert!(a.contains(&"--init".to_string()));
        // Heartbeat main process.
        assert_eq!(a[a.len() - 2], "sleep");
        assert_eq!(a.last().unwrap(), "infinity");
    }

    #[test]
    fn create_renders_the_workdir_override_flag() {
        // Absent: no flag — the image's own WORKDIR applies.
        let a = Podman::create_args("tb", &spec());
        assert!(!a.contains(&"--workdir".to_string()));
        // Override (the profile `workdir`): a create flag.
        let mut s = spec();
        s.workdir = Some("/home/simon".into());
        let a = Podman::create_args("tb", &s);
        let i = a.iter().position(|x| x == "--workdir").unwrap();
        assert_eq!(a[i + 1], "/home/simon");
    }

    // The command is already resolved in the spec. An empty
    // command means "the image's own ENTRYPOINT+CMD" — the image is
    // the last arg.
    #[test]
    fn command_rendering_is_resolved() {
        let a = Podman::create_args("pi", &spec());
        assert_eq!(a[a.len() - 2], "sleep");

        let mut s = spec();
        s.command = vec![];
        let a = Podman::create_args("pi", &s);
        assert_eq!(a.last().unwrap(), "img");

        let mut s = spec();
        s.command = vec!["init-container".into(), "--uid".into(), "1000".into()];
        let a = Podman::create_args("pi", &s);
        assert_eq!(a[a.len() - 3], "init-container");
        assert_eq!(a[a.len() - 2], "--uid");
        assert_eq!(a.last().unwrap(), "1000");
    }

    #[test]
    fn mount_specs_rendered_as_mount_args() {
        let mut s = spec();
        s.mount_specs = vec!["type=devpts,destination=/dev/pts".into()];
        let a = Podman::create_args("pi", &s);
        let i = a
            .iter()
            .position(|x| x == "type=devpts,destination=/dev/pts")
            .unwrap();
        assert_eq!(a[i - 1], "--mount");

        let a = Podman::create_args("pi", &spec());
        assert!(!a.iter().any(|x| *x == "type=devpts,destination=/dev/pts"));
    }

    #[test]
    fn security_opts_rendered_as_security_opt_args() {
        let mut s = spec();
        s.security_opts = vec!["label=disable".into()];
        let a = Podman::create_args("pi", &s);
        let i = a.iter().position(|x| x == "--security-opt").unwrap();
        assert_eq!(a[i + 1], "label=disable");

        let a = Podman::create_args("pi", &spec());
        assert!(!a.iter().any(|x| x == "--security-opt"));
    }

    #[test]
    fn bind_mounts_are_rendered_as_bind_mounts() {
        let a = Podman::create_args("pi", &spec());
        let i = a
            .iter()
            .position(|x| x == "type=bind,src=/work,dst=/root/.pi/foo")
            .unwrap();
        assert_eq!(a[i - 1], "--mount");
    }

    #[test]
    fn hostname_is_the_box_name() {
        let a = Podman::create_args("pi", &spec());
        let i = a.iter().position(|x| x == "--hostname").unwrap();
        assert_eq!(a[i + 1], "pi");
    }

    #[test]
    fn network_flag_only_when_declared() {
        let a = Podman::create_args("pi", &spec());
        assert!(!a.iter().any(|x| x == "--network"));

        let mut s = spec();
        s.network = Some("bridge".into());
        let a = Podman::create_args("pi", &s);
        let i = a.iter().position(|x| x == "--network").unwrap();
        assert_eq!(a[i + 1], "bridge");
    }

    #[test]
    fn extra_hosts_rendered_as_add_host_args() {
        let mut s = spec();
        s.extra_hosts = vec!["llm.example.com:host-gateway".into()];
        let a = Podman::create_args("pi", &s);
        let i = a.iter().position(|x| x == "--add-host").unwrap();
        assert_eq!(a[i + 1], "llm.example.com:host-gateway");
    }

    #[test]
    fn env_overrides_are_rendered_as_env_args() {
        let mut s = spec();
        s.env
            .insert("TERM".to_string(), "xterm-256color".to_string());
        let a = Podman::create_args("pi", &s);
        let i = a.iter().position(|x| x == "--env").unwrap();
        assert_eq!(a[i + 1], "TERM=xterm-256color");

        let a = Podman::create_args("pi", &spec());
        assert!(!a.iter().any(|x| x == "--env"));
    }

    /// Env flags render in insertion order (order-preserving): a
    /// redefined key updates in place, and a new key appends.
    #[test]
    fn env_rendered_in_insertion_order() {
        let mut s = spec();
        s.env.insert("SECOND".to_string(), "2".to_string());
        s.env.insert("FIRST".to_string(), "1".to_string());
        s.env.insert("SECOND".to_string(), "2b".to_string());
        let a = Podman::create_args("pi", &s);
        let second = a.iter().position(|x| x == "SECOND=2b").unwrap();
        let first = a.iter().position(|x| x == "FIRST=1").unwrap();
        // SECOND was inserted first, so it renders before FIRST even
        // though FIRST was inserted before SECOND's second write.
        assert!(second < first);
    }

    #[test]
    fn create_carries_the_box_labels() {
        let a = Podman::create_args("pi", &spec());
        // Marker + name: the name mirrors the container name.
        let i = a.iter().position(|x| x == "--label").unwrap();
        assert_eq!(a[i + 1], "com.github.simon3z.steelbx.box=true");
        assert_eq!(a[i + 3], "com.github.simon3z.steelbx.box.name=pi");
    }

    /// The `box.env` label: rendered only when declared, one
    /// comma-separated value (podman labels cannot repeat a key).
    #[test]
    fn create_renders_the_box_env_label_when_declared() {
        let a = Podman::create_args("pi", &spec());
        assert!(!a
            .iter()
            .any(|x| { x.starts_with("com.github.simon3z.steelbx.box.env=") }));

        let mut s = spec();
        s.runtime_env = vec!["API_KEY".to_string(), "MODEL".to_string()];
        let a = Podman::create_args("pi", &s);
        let i = a
            .iter()
            .position(|x| x == "com.github.simon3z.steelbx.box.env=API_KEY,MODEL")
            .unwrap();
        assert_eq!(a[i - 1], "--label");
    }

    /// Runtime env names: bare `-e NAME` (never `NAME=VALUE` — the
    /// value is copied from the caller env, not argv), before the box
    /// name.
    #[test]
    fn enter_args_pass_runtime_env_bare() {
        let a = Podman::enter_args("b1", &["API_KEY".to_string(), "MODEL".to_string()]);
        let e1 = a.iter().position(|x| x == "-e").unwrap();
        assert_eq!(a[e1 + 1], "API_KEY");
        // Never `NAME=VALUE`: the value is not argv.
        assert!(!a.iter().any(|x| x.contains('=')));
        // The env flags come before the box name.
        assert!(e1 < a.iter().position(|x| x == "b1").unwrap());
    }

    #[test]
    fn enter_is_interactive_nonlogin_bash() {
        let a = Podman::enter_args("b1", &[]);
        // Interactive + TTY, so bash reads ~/.bashrc.
        assert!(a.contains(&"--interactive".to_string()));
        assert!(a.contains(&"--tty".to_string()));
        assert_eq!(a.last().unwrap(), "/bin/bash");
        // Non-login: no -l (a login shell reads profiles, not .bashrc).
        assert!(!a.iter().any(|x| x == "-l" || x == "--login"));
        // Regular container behavior: no
        // -u/-w — the container's own user and working directory.
        assert!(!a.contains(&"-u".to_string()));
        assert!(!a.contains(&"-w".to_string()));
    }
}
