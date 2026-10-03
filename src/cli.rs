use clap::{Parser, Subcommand, ValueHint};
use clap_complete::aot::Shell;
use clap_complete::engine::ArgValueCompleter;

#[derive(Parser)]
#[command(
    name = "steelbx",
    version,
    about = "run unsupervised workloads in isolated podman boxes"
)]
pub struct Cli {
    /// Print the podman commands as they run (stderr)
    #[arg(long, short = 'v')]
    pub verbose: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Create a box from a profile (created state; started on first
    /// enter)
    Create {
        /// Policy profile (`profiles/<name>.conf` — a complete
        /// `containers.conf` carrying the image, e.g. `image = "quay.io/..."`; replace semantics, no merging). Resolved
        /// in `~/.config/steelbx/profiles/`, then `/etc/steelbx/profiles/`
        /// (shipped; a user profile of the same name overrides).
        /// Defaults to `default`.
        /// Completes from the profiles dirs.
        #[arg(short = 'p', long = "profile", add = ArgValueCompleter::new(crate::complete::profile_candidates))]
        profile: Option<String>,
        /// Image override for the profile's `image` key (auto-pulled
        /// if not local). Completes from local images carrying the
        /// `com.github.simon3z.steelbx.box` or
        /// `com.github.containers.toolbox` label.
        #[arg(short = 'i', long = "image", add = ArgValueCompleter::new(crate::complete::image_candidates))]
        image: Option<String>,
        /// Box name (container name), pinned verbatim (no random
        /// token). Precedence: this flag > the profile's `name` key
        /// (unique by default: the key + a random token, unless
        /// `name_unique = false`) > a generated unique name. The
        /// effective name is available to profile values as
        /// STEELBX_BOX_NAME. A taken pinned name is refused.
        /// Completes from the live boxes.
        #[arg(short = 'n', long = "name", add = ArgValueCompleter::new(crate::complete::box_name_candidates))]
        box_name: Option<String>,
        /// Host paths, mounted at <WORKDIR>/<basename> (the profile key
        /// mount_dest = absolute mounts each at its own full path)
        #[arg(value_hint = ValueHint::DirPath)]
        paths: Vec<String>,
    },
    /// Disposable box: create → enter → auto-rm (like `podman run
    /// --rm`). The profile is a FLAG (not positional) so every
    /// positional is a mount dir — no profile/path ambiguity.
    Run {
        /// Policy profile. Defaults to `default`. Completes from the
        /// profiles dirs.
        #[arg(short = 'p', long = "profile", add = ArgValueCompleter::new(crate::complete::profile_candidates))]
        profile: Option<String>,
        /// Image override (auto-pulled if not local). Completes from
        /// marked local images.
        #[arg(short = 'i', long = "image", add = ArgValueCompleter::new(crate::complete::image_candidates))]
        image: Option<String>,
        /// Box name to pin, verbatim (no random token). Precedence:
        /// this flag, then the profile's `name` key (unique by
        /// default), then a generated unique name. Profile values
        /// reference the effective name (STEELBX_BOX_NAME is the
        /// expansion variable).
        #[arg(short = 'n', long = "name", add = ArgValueCompleter::new(crate::complete::box_name_candidates))]
        box_name: Option<String>,
        /// Expose a caller env variable to the session (bare NAME).
        /// Repeatable. The box's declared runtime env is always
        /// injected.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        /// Host paths, mounted at <WORKDIR>/<basename> (the profile key
        /// mount_dest = absolute mounts each at its own full path)
        #[arg(value_hint = ValueHint::DirPath)]
        paths: Vec<String>,
    },
    /// Enter a box: start if needed, then interactive shell
    Enter {
        /// Box name
        #[arg(add = ArgValueCompleter::new(crate::complete::box_name_candidates))]
        box_name: String,
        /// Expose a caller env variable to the box (bare NAME — podman
        /// copies the value from your shell; `NAME=VALUE` is refused).
        /// Repeatable. The box's declared runtime env (`box.env` label)
        /// is always injected as well.
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
    },
    /// Remove boxes; fails if running — --force force-deletes;
    /// --volumes removes the box's volumes too
    Rm {
        /// Box names
        #[arg(add = ArgValueCompleter::new(crate::complete::box_name_candidates))]
        box_names: Vec<String>,
        #[arg(long, short = 'f')]
        force: bool,
        /// Remove the box's volumes too: every `type=volume` mount the
        /// box carried. Without it, only the volumes the profile
        /// declared under `delete_volumes` (the `box.volumes` label) are
        /// removed. In-use volumes (another live box still mounts them)
        /// survive, named in the error.
        #[arg(long, short = 'V')]
        volumes: bool,
    },
    /// List boxes: name, state
    Ps,
    /// Run a one-shot command in a box
    Exec {
        /// Box name
        #[arg(add = ArgValueCompleter::new(crate::complete::box_name_candidates))]
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
