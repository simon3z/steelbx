# Steel Boxes 🛡️

[![github][badge-github]][link-github]
[![license][badge-license]][link-license]
[![version][badge-version]][link-version]
[![rust-edition][badge-rust]][link-rust]
[![dependencies][badge-deps]][link-cargo]
[![CI][badge-ci]][link-ci]

Isolate workloads from your system in disposable containers with
profiles balancing security and usability.

Run unsupervised workloads — build scripts, data pipelines, experiments,
one-off tooling, AI coding agents (pi, Claude Code, Codex, OpenCode, ...) —
in their own **steel box**: a disposable
podman container whose layout comes from the image, and whose security
policy comes from one small config file. The workload runs *inside* the
box; you work *outside* it — your dotfiles, projects, and credentials
stay on the host, and the mounts you declare are the only surface the
box has.

## Why

Unsupervised workloads do two things:

1. They modify your machine.
2. They talk to external APIs with credentials that live on your machine.

The failure modes are not theoretical: an AI coding agent (pi, Claude
Code, Codex, OpenCode) will `rm` directories you meant to keep, rewrite
dotfiles
you depend on, or exfiltrate secrets to the very endpoints it is
configured with.

A VM gives the strongest isolation, and is the right tool when the
workload is truly adversarial. For most unsupervised workloads — especially
short, frequent, parallel runs — a container is a great compromise:

- **Boot time.** A VM boots in tens of seconds to minutes. A container
  starts in 50–200 ms. In a workflow of *short, frequent, parallel*
  runs, that multiplier is the entire experience.
- **Footprint.** A session needs a *stateless environment*, not a
  machine: the image, a handful of mounts, a bit of env, done.
- **The box IS the credential boundary.** The container is already the
  boundary that matters for credential containment; the VM would be an
  extra layer you pay boot time for.

`steelbx` gives you that box, with the management surface kept small
enough to trust.

## Threat model

| Assumption | Statement |
|---|---|
| A1 | The threat model is **unsupervised workload misbehavior**: code inside the box acting wrongly. We are **not** protecting the workload from a malicious human. |
| A2 | The workload's network egress is a **capability surface**, and what it can reach is a **design variable** — not an accident. |
| A3 | We accept **image supply-chain risk** as out of scope for v0. The workload is expected to be able to `sudo` inside the box anyway (containers are not security boundaries against the workload). |

## When steelbx is the right tool

Steelbx is a thin layer over podman: it does not enforce anything itself —
it compiles a reviewed profile into a pinned podman command and lets the
container runtime do the containing. It is the right choice when:

- you run **your own** unsupervised workloads on **your own** machine (or
  one remote podman host), in short, frequent, parallel runs
- boot time (50–200 ms) and footprint matter more than kernel-level
  enforcement
- you want the entire security surface to be auditable — no daemon, no
  state, no moving parts
- runtime env (bare `-e NAME`) is enough for your credential handling

It is **not** the right choice when the workload is genuinely adversarial
or you need in-box enforcement — per-destination/per-binary egress policy,
Landlock/seccomp, or credentials kept out of the process env. Recall threat
model A3: the workload can `sudo` inside the box. For that class of
workload, use a heavier runtime (e.g. NVIDIA's OpenShell) or a VM.

## How it works

**There is no per-box configuration to author:** the profile's `image`
names the box's layout image (the image's `WORKDIR`), and the mounts you
pass on the CLI (plus the profile's `mounts` key) are all the box has.

```console
$ steelbx create -p default -i localhost/my-workload:latest ~/project
Created box: my-workload-1a2b3c4d
Enter with: steelbx enter my-workload-1a2b3c4d
```

- `create [-p <profile>] [-i <image>] [-n <box-name>] <paths...>` — the
  profile (default: `default`) selects the policy; the image comes
  from the profile's `image` key (overridable with `-i`) and must be
  local (steelbx consumes images, it does not pull them); the box
  name is `-n` when given, otherwise a generated unique name built on
  the image's declared base (its `com.github.simon3z.steelbx.box.name`
  label, else the image's name component without tag), e.g.
  `pi-steelbx-a1b2c3d4`; each `<path>` is bind-mounted at
  `<WORKDIR>/<basename>`; the box is left in `created` state and
  started on its first `enter`

- `run [-p <profile>] [-i <image>] [-n <name>] [-e NAME] <paths...>` —
  a disposable box: creates the box (a generated unique name unless
  `-n`), enters it, and force-removes it on the way out — like
  `podman run --rm`. The profile is a flag (`-p`), so every positional
  is a mount dir; the exit code is the session's (130 if interrupted)

- `enter` / `exec` take the box name and, optionally, `-e NAME`
  runtime env names
- `rm` takes the box name; `--force` kills a running box
- `ps` lists boxes with their live state

The box is a named container carrying three labels (`...box`,
`...box.name`, `...box.env`) — full label semantics are
[below](#shell-completion). Steelbx owns no state of its own: the
labels live on the container, and everything else lives in podman.

### The policy config schema

A profile is one TOML file (absence of a key is the default for that
key). [`examples/default.conf`](examples/default.conf) is the fully
commented reference and also serves as the secure-by-default profile
— copy it to `~/.config/steelbx/profiles/default.conf` to use as-is.

Every string value — `network`, `extra_hosts` elements, `[env]` values
(including values loaded from `env_files`), the `env_files` paths,
`mounts` and `security_opts` entries, and the namespace/identity keys
below — supports `$VAR` and `${VAR}` expansion from the caller's
environment; `$$` is a literal `$`; a reference to an unset variable is
an error that names the variable. Keys never expand. No shell: no
command substitution, ever.

`${IDENT:-default}` falls back to `default` when the variable is
unset *or* empty — a profile can declare a sensible default instead
of failing on machines where the variable is absent. The default is
the text up to the first `}` (no nesting of `${...}` inside it), and
that text itself expands against the same environment (so
`${STEELBX_HOME:-$HOME/steelbx-pi}` resolves `$HOME`; an unset
reference inside it fails loudly, as anywhere else). The expansion
context is layered: `[env]` values
expand against the caller env plus `env_files` (they do *not* reference
each other), and every other value expands against the caller env
*plus* `env_files` *plus* the `[env]` (profile values win) — so
`image = "$IMG"` may name a variable declared in an `env_files` file or
in `[env]`.
Caveat: values that are *host* paths (`mounts` sources, `init` cp
sources) must refer to caller/steelbx variables, not container-only
`[env]` names.

There are three env roles, and they do different things:

| Role | Source | When | Podman |
|---|---|---|---|
| Expansion env | caller env + `env_files` | profile load | none — it only resolves other values |
| Box env | `[env]` | create | `--env KEY=VALUE`: the container's own env, persistent — the only env the container is created with |
| Runtime env | `runtime_env` + the image's `box.env` label | enter/exec | `exec -e NAME` (bare): copies from your shell per session; the value is never argv |

The runtime env is for *live* values (an API key, a model choice):
declare the names once, and each `enter`/`exec` picks up whatever is
set in the shell that runs steelbx — no box recreate to rotate them.
Per-name precedence in a session: your shell (the `-e` copy) > the
box's `[env]` value > the image's ENV. A runtime name unset in your
shell is not an error: the box's own value (if any) survives, and
steelbx prints one info line naming the unset ones. `enter`/`exec`
`-e NAME` adds names for one call.

Steelbx itself provides two variables, set if your environment does
not supply them (override by exporting them); expansion and child
processes see the same values:

- `STEELBX_CONFIG_DIR` — `$HOME/.config/steelbx`: where `profiles/`
  lives.
- `STEELBX_DATA_DIR` — `$HOME/.local/share/steelbx`: home for data
  files a box should see. The mount idiom:

  ```toml
  mounts = ["type=bind,src=$STEELBX_DATA_DIR/shared,dest=/work/shared"]
  ```

  Put files under `$STEELBX_DATA_DIR/shared` and they appear in every
  box created from that config.

Namespace sharing and identity are podman passthrough (shape-checked:
single token, no spaces; podman interprets the value): `cgroupns`,
`ipc`, `pid`, `userns`, `privileged`, `no_hosts`, `ulimits`. Together
they express a toolbox-style box (shared namespaces, host access) —
such a box is a root-adjacent environment, not a sandbox.

`user` overrides the image's official `USER`, rendered as a
create-time `--user` (regular container behavior):
absent, the box runs with the image's `USER`; `enter`/`exec` run as
the container's own user, in its own working directory. A profile
`user` naming a user that `init` creates never yields a usable box —
podman resolves the user against the image before handover, at
create (some podman versions defer the lookup to first start) —
such a box is the image's baked `USER` + `init`, not the profile's.

The main command is `entry` (an argv array, expanded like every
other value); absent, an image carrying the `com.github.simon3z.steelbx.box.cmd`
label runs its own ENTRYPOINT+CMD (the image author's declaration),
else the heartbeat default (`sleep infinity`). Per-deployment
commands belong in the profile; the label is the image's.

`init` — a list of steps, run in the declared order, post-create,
before handover; a failure aborts create. Step forms:

- `"exec"` — `["exec", argv...]`, run as `podman exec -u 0` (init
  configures the box; root, never a shell).
- `"cp"` — `["cp", "host source", "container dest"]`, podman `cp`
  (host to container; the source is expanded and must exist, the
  dest an absolute container path). No mount needed.

Steps run in the order declared — a `cp` may follow the `exec` that
makes its destination exist:

  ```toml
  init = [
    ["exec", "useradd", "-m", "$USER"],
    ["cp", "$STEELBX_DATA_DIR/shared/config", "/home/$USER/.config"],
    ["exec", "chown", "-R", "$USER:", "/home/$USER/.config"],
  ]
  ```

Steelbx reads only the profiles below; podman's own `containers.conf`
is neither read nor re-declared by steelbx.

### Profiles: `~/.config/steelbx/profiles/<name>.conf`

Working examples to copy into `profiles/` and adjust:
`examples/default.conf` (the secure-by-default posture, doubles as
the schema reference), `examples/pi-agent.conf` (a toolbox-style box
with GUI access and session env passthrough), and
`examples/toolbox.conf` (the full toolbox flag set; adjust the mounts
for your host). Each carries its `image` key —
`create -p <profile>` is self-contained; `-i <image>` overrides it for
one create.

`steelbx create -p <profile>` selects `profiles/<name>.conf` (default:
`default`) — a **complete** `containers.conf` with **no merging**
(replace semantics). Profiles resolve in
`~/.config/steelbx/profiles/` first, then in the shipped location
`/etc/steelbx/profiles/` — a distribution can install policy profiles
there, and a user profile of the same name overrides it. Absent
directories = no profiles. The dir listing is your profile list;
tab completion and the unknown-profile error name what exists.

### The pinned baseline

`steelbx create` builds a podman `create` from a **pinned baseline** —
it is *not* a pass-through. The baseline is reviewed code, and the
image + config + CLI add layout and policy on top.

| Input | Behavior |
|---|---|
| `image` (profile) | The image this policy applies to; must be local. Its `WORKDIR` is the mount base (overridable below) — an image with no `WORKDIR` gets the default layout `/work`. |
| `-i` (CLI) | Image override for one create (the profile's `image` is the default); must be local. |
| CLI paths | Each is canonicalized and bind-mounted at `<workdir>/<basename>`; there is no `dest` to declare. |
| `workdir` (config) | The layout override (a container path, expanded): precedence over the image's `WORKDIR` and the `/work` default. Rendered as a create-time `--workdir` and the mount base; `enter`/`exec` run in the container's own working directory. Re-basing where the image's tools expect their files is the profile author's, reviewed, choice. |
| `network` (config) | A podman network value; omitted = podman's default (no flag passed); `"none"` disables. |
| `extra_hosts` (config) | `host:ip` entries or the `host-gateway` keyword — how host services are reached from inside. |
| `env_files` (config) | Ordered env files (`KEY=VALUE` per line; `#` comments, optional `export `); later files override earlier. An expansion *source*: their variables can be referenced by other values (and by `[env]` values), but they are NOT passed to the container. Paths expand like other values; a missing file or a malformed line is an error. |
| `[env]` (config) | The only env the container is created with (rendered in the order given, order-preserving). Keys must be valid env names; values are trusted (it's your box). Same-key wins over the image's ENV. Values expand against the caller env plus `env_files`. |
| `runtime_env` (config) | Runtime env NAMES (not values; shape-checked, not expanded). Merged with the image's `box.env` label into the box's own `box.env` label. At enter/exec, each name set in the caller env is passed as bare `podman exec -e NAME` (the value is never argv); unset names are left alone and named in one info line. |
| `mounts` (config) | Podman `--mount` specs, rendered verbatim — e.g. `type=devpts,destination=/dev/pts` — in addition to the derived bind mounts (podman interprets; shape-checked). |
| `security_opts` (config) | `key=value` entries rendered as podman `--security-opt` (e.g. `label=disable` — SELinux labeling off); omitted = podman's default security posture. |
| `[profile]` (CLI) | Selects a `profiles/<name>.conf` — a complete `containers.conf` (replace semantics, no merging). Defaults to `default`; optional when `-i` provides the image. |
| caps | Not configurable: the baseline is podman's default posture. |

Unknown keys, unparseable config, and invalid `workdir`/`env`/
`runtime_env`/`extra_hosts`/`security_opts`/`mounts`/`init` shapes
are rejected by validation, before anything runs.

## Commands

| Command | Behavior |
|---|---|
| `steelbx create [-p <profile>] [-i <image>] [-n <box-name>] <paths...>` | Create the box (created state; starts on first `enter`). Profile defaults to `default`. Without `-n`, a unique name is generated (base + 8 hex). Tab-completion suggests profiles, marker-labeled images for `-i`, existing box names for `-n`, and directories for the paths |
| `steelbx run [-p <profile>] [-i <image>] [-n <name>] [-e NAME] <paths...>` | Disposable box: create → enter → auto-rm, like `podman run --rm`. The profile is a flag (`-p`) so every positional is a mount dir; the name is `-n` or a generated unique name; the box is force-removed on exit and the exit code is the session's (130 if interrupted). TTY required |
| `steelbx enter <box-name> [-e NAME]` | Start if needed, interactive shell (TTY required); `-e NAME` exposes a caller env var for the session (the box's declared runtime env is always injected) |
| `steelbx exec <box-name> [-e NAME] cmd...` | One-shot command; `-e NAME` as above |
| `steelbx rm <box-name>` | Remove; silent on success, `--force` kills running |
| `steelbx ps` | List boxes: name, state, image, age |

`--verbose` (`-v`): print each podman command as it runs, on stderr —
for reading back exactly what steelbx sent podman.

`enter`/`exec` set the terminal window title to `steelbx <box>`; `run`
sets it to the profile name (the box's own name carries a random unique
suffix, so the title uses the readable profile). Written as an OSC 0
escape when the output is a terminal — never when piped. A shell prompt
that sets its own title (e.g. Fedora's default `PROMPT_COMMAND`)
overwrites it after the first prompt.

### Shell completion

The install path is the *dynamic* one: the sourced function re-invokes
steelbx on each tab, so box names (`enter`, `rm`, `exec`) complete from
the live boxes — and everything else (verbs, flags, values) comes from
the clap definition, so it can never drift from the binary:

```bash
echo "source <(COMPLETE=bash steelbx)" >> ~/.bashrc
```

This needs `steelbx` on your PATH (`install -D target/release/steelbx
~/.local/bin/steelbx`, or wherever you keep binaries). `zsh` works the
same way; `fish` and `elvish` use their own source idiom with the same
`COMPLETE=<shell>` variable.

Labels mark the artifacts (the marker and name values are
conventionally `true`, never read; the env list is read):

- `com.github.simon3z.steelbx.box` — on an image, "this image is a box image" (or a
  toolbox base image: `com.github.containers.toolbox`); `create`'s `-i`
  completion suggests local images carrying either one. On a
  container, the marker: "this is a steelbx box" (write path —
  steelbx sets it at create; its absence keeps a container out of
  `ps`/`enter`/`rm`).
- `com.github.simon3z.steelbx.box.name` — on an image, the declared base
  name for a generated box name (`create` uses it as the prefix when
  `-n` is absent); on a container, a mirror of the name (the
  distribution unit carries its identity). Never read for layout or
  policy.
- `com.github.simon3z.steelbx.box.env` — on an image, the declared runtime env
  names (a comma-separated list — a label is one value; env names
  cannot contain commas); on a container, the box's runtime env (the
  profile's `runtime_env` ∪ the image's, written at create).
  `enter`/`exec` read it and inject each name set in the caller env
  as bare `-e`.

Mark an image at build time:

```dockerfile
LABEL com.github.simon3z.steelbx.box=true
LABEL com.github.simon3z.steelbx.box.name=my-box
```

A toolbox base image (the fedora base a toolbox profile creates from)
instead carries:

```dockerfile
LABEL com.github.containers.toolbox=true
```

Static-only completions (no live box names, no per-tab subprocess) are
still available: `steelbx completion <shell>` (hidden from help; source
its output the usual way). Prefer the dynamic install.

## Design notes

Key decisions:

- **profile-as-distribution** — the profile (`image` + the full
  policy schema) is the distribution unit; layout is
  the image's `WORKDIR`, mounts are derived
- **heartbeat** main process, entry is always `exec`
- host services are reached via `extra_hosts` /
  `host-gateway`
- steelbx owns no state: the labels on each container are the index

## Build

```console
$ cargo build --release
$ sudo -i -g podman -u <user>   # rootless: the podman group owns the store
```

There is no daemon and no required configuration beyond the profile
you create from (`~/.config/steelbx/profiles/<name>.conf`). The CLI
tells you honestly
if it is running against a rootful store.

### Remote mode

Point steelbx at a remote podman host with `STEELBX_CONNECTION`
(e.g. `ssh://myhost`):

```bash
export STEELBX_CONNECTION=ssh://myhost
```

steelbx then drives `podman-remote` instead of `podman`, passing the
connection through as podman's `CONTAINER_HOST` (a `CONTAINER_HOST`
already set by the caller wins). Everything runs on the remote host:
the image must be local *to that host*, and the paths you mount must
exist there — local path validation is skipped in remote mode.

## Project status

| Area | Status |
|---|---|
| Image-as-distribution create (layout from `WORKDIR`, derived mounts) | Done |
| `enter` / `exec` / `ps` / `rm` | Done |
| Profiles (`profiles/<name>.conf`, replace semantics; `image` + full policy schema) | Done |
| Pinned podman baseline | Done |
| Box management (labels, no state) | Done |
| CI: fmt, clippy, tests, cognitive complexity | Done |

[badge-github]: https://img.shields.io/badge/github-simon3z/steelbx-6f57b0.svg?logo=github
[badge-license]: https://img.shields.io/badge/license-Apache_2.0-blue.svg
[badge-version]: https://img.shields.io/badge/version-0.1.0-ff8000.svg
[badge-rust]: https://img.shields.io/badge/rust-edition_2021-steelblue.svg
[badge-deps]: https://img.shields.io/badge/dependencies-6-green.svg
[badge-ci]: https://github.com/simon3z/steelbx/actions/workflows/ci.yml/badge.svg
[link-github]: https://github.com/simon3z/steelbx
[link-license]: LICENSE
[link-version]: Cargo.toml
[link-rust]: https://www.rust-lang.org
[link-cargo]: Cargo.toml
[link-ci]: https://github.com/simon3z/steelbx/actions/workflows/ci.yml
