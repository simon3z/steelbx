//! Shell completion wiring: the dynamic clap_complete engine (the
//! sourced shell function re-invokes this binary with `COMPLETE=<shell>`;
//! the request is handled and the process exits before any parsing)
//! and the static `completion` subcommand. Self-contained: it needs
//! only `Cli`, the profile list, and the driver's image/box listings.
use clap::CommandFactory;
use clap_complete::aot::{generate, Shell};
use clap_complete::engine::CompletionCandidate;
use clap_complete::CompleteEnv;

use steelbx::config::SteelbxConfig;
use steelbx::driver::Podman;

use crate::Cli;

/// Dynamic completion (clap_complete engine): the sourced shell function
/// re-invokes this binary (`COMPLETE=<shell> ...`); the request is handled
/// and the process exits before any parsing. Must run before stdout writes.
pub fn init_completion() {
    CompleteEnv::with_factory(Cli::command)
        .completer("steelbx")
        .complete();
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
pub fn image_candidates(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let pod = Podman;
    complete(pod.image_names().unwrap_or_default(), current)
}

/// Profile tab completion: the `profiles/*.conf` names, one `read_dir`.
/// Attached to `create`'s `--profile`.
pub fn profile_candidates(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    complete(SteelbxConfig::list_profiles().unwrap_or_default(), current)
}

/// Box-name tab completion: the live boxes, one `podman ps` (no
/// version round-trip). Attached to `enter`/`rm`/`exec` box names.
pub fn box_name_candidates(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let pod = Podman;
    complete(pod.box_names().unwrap_or_default(), current)
}

pub fn cmd_completion(shell: Shell) -> anyhow::Result<()> {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "steelbx", &mut std::io::stdout());
    Ok(())
}
