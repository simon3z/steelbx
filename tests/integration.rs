//! G1 vertical slice (FR-002, FR-004, FR-005, FR-006, FR-007, FR-008,
//! FR-009, FR-010).
//!
//! Uses podman to actually create/exec/remove a box from a local image.
//! Skipped when podman is not installed (CI runs it on fedora 44 with
//! the podman that release ships, single-threaded — the suite shares
//! one podman store).

fn podman_available() -> bool {
    std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Runs `podman <args>`; a failure becomes an error naming the verb.
fn podman_ok(args: &[&str]) -> anyhow::Result<()> {
    let o = std::process::Command::new("podman")
        .args(args)
        .output()
        .unwrap();
    if o.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "podman {} failed: {}",
            args[0],
            String::from_utf8_lossy(&o.stderr)
        ))
    }
}

/// Pulls the base image once per process so every test can rely on it
/// being local: `image_meta` and `inspect` consume images and do not
/// pull, and the parallel first-pulls otherwise race.
fn ensure_base_image_local(image: &str) {
    use std::sync::{Mutex, OnceLock};
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = GUARD.get_or_init(|| Mutex::new(()));
    let _g = guard.lock().unwrap();
    let exists = std::process::Command::new("podman")
        .args(["image", "exists", image])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        let _ = podman_ok(&["pull", image]);
    }
}

/// FR-005/FR-008: removal of a running box requires force, and the
/// forced removal leaves no box.
fn force_remove(pod: &steelbx::driver::Podman, name: &str) {
    let err = pod.remove_container(name, false).unwrap_err();
    assert!(
        format!("{err}").to_string().contains("force"),
        "rm of a running box must require force"
    );
    pod.remove_container(name, true).unwrap();
    assert!(pod.inspect(name).unwrap().is_none());
}

/// The g2 scratch container name (the committed test image is built
/// from it; nothing runs).
const LBL_SCRATCH: &str = "steelbx-it-lbl-scratch";

/// The pinned `create` args for the ADR-020 test image: WORKDIR +
/// marker + cmd label + a CMD.
const LBL_CREATE_ARGS: &[&str] = &[
    "create",
    "--name",
    LBL_SCRATCH,
    "--workdir",
    "/tmp",
    "--label",
    "com.github.simon3z.steelbx.box=true",
    "--label",
    "com.github.simon3z.steelbx.box.cmd=true",
    "registry.fedoraproject.org/fedora:42",
    "sleep",
    "infinity",
];

/// Drops the g2 scratch container and test image.
fn cleanup_lbl_image(img: &str) {
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", LBL_SCRATCH])
        .output();
    let _ = std::process::Command::new("podman")
        .args(["rmi", "-f", img])
        .output();
}

/// Builds the ADR-020 test image: committed from a created container
/// (nothing runs); the scratch container is dropped on failure.
fn build_declared_cmd_image(img: &str) -> anyhow::Result<()> {
    if let Err(e) = podman_ok(LBL_CREATE_ARGS) {
        cleanup_lbl_image(img);
        return Err(e);
    }
    if let Err(e) = podman_ok(&["commit", LBL_SCRATCH, img]) {
        cleanup_lbl_image(img);
        return Err(e);
    }
    Ok(())
}

/// FR-007/FR-003: create the `steelbx-it` test box (a mount at
/// `<WORKDIR>/proj`); the box exists, a second create is refused.
/// Returns the host tempdir (kept alive for the box's lifetime — the
/// bind source must outlive the box) and the host-side `proj` dir.
fn create_test_box(
    pod: &steelbx::driver::Podman,
    image: &str,
    workdir: &str,
) -> (tempfile::TempDir, String) {
    let work = tempfile::tempdir().unwrap();
    let proj = work.path().join("proj");
    std::fs::create_dir(&proj).unwrap();
    let mounts =
        steelbx::validate::derive_mounts(&[proj.to_str().unwrap().to_string()], workdir).unwrap();
    let spec = steelbx::driver::CreateSpec {
        image: image.to_string(),
        mounts,
        command: vec!["sleep".to_string(), "infinity".to_string()],
        ..Default::default()
    };
    pod.create("steelbx-it", &spec).unwrap();
    assert!(pod.inspect("steelbx-it").unwrap().is_some());
    assert!(
        pod.create("steelbx-it", &spec).is_err(),
        "second create must be refused"
    );
    (work, proj.to_str().unwrap().to_string())
}

/// FR-009: exec surfaces the command's exit code; the marker lands on
/// the host mount (FR-004, ADR-007: the box is started on demand).
fn exec_roundtrip(pod: &steelbx::driver::Podman, workdir: &str, proj: &str) {
    let marker = format!("{}/proj/marker", workdir.trim_end_matches('/'));
    let res = pod.exec("steelbx-it", &[], &["touch".to_string(), marker]);
    if let Err(e) = res {
        // Diagnostics: dump the store so a vanished box is visible in
        // the CI log.
        let ps = std::process::Command::new("podman")
            .args(["ps", "--all", "--format", "{{.Names}} {{.State}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        panic!("exec failed: {e}\npodman ps --all:\n{ps}");
    }
    assert!(
        std::path::Path::new(proj).join("marker").exists(),
        "marker must exist on the host mount"
    );
    pod.exec(
        "steelbx-it",
        &[],
        &["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
    )
    .unwrap_err();
}

/// The g4 spec: a mount at `<WORKDIR>/data` (never at `WORKDIR`
/// itself — the base image's `WORKDIR` is `/`, and a bind there would
/// shadow the whole rootfs), one init `exec` step that touches a
/// marker, host network.
fn init_marker_spec(workdir: &str, host: &std::path::Path) -> steelbx::driver::CreateSpec {
    let dest = format!("{workdir}/data");
    steelbx::driver::CreateSpec {
        image: "registry.fedoraproject.org/fedora:42".to_string(),
        command: vec!["sleep".to_string(), "infinity".to_string()],
        mounts: vec![steelbx::driver::Mount {
            host: host.to_path_buf(),
            dest: dest.clone(),
        }],
        init: Some(vec![steelbx::driver::InitStep::Exec(vec![
            "touch".to_string(),
            format!("{dest}/steelbx-init-ok"),
        ])]),
        network: Some("host".to_string()),
        ..Default::default()
    }
}

/// The g7 spec: a `cp` step then an `exec` step that checks the copied
/// file and touches a marker — the step order is the test. The mount
/// lands at `<WORKDIR>/data` (never at `WORKDIR` itself — see
/// `init_marker_spec`).
fn files_spec(
    workdir: &str,
    host: &std::path::Path,
    src: &std::path::Path,
) -> steelbx::driver::CreateSpec {
    let dest = format!("{workdir}/data");
    steelbx::driver::CreateSpec {
        image: "registry.fedoraproject.org/fedora:42".to_string(),
        command: vec!["sleep".to_string(), "infinity".to_string()],
        mounts: vec![steelbx::driver::Mount {
            host: host.to_path_buf(),
            dest: dest.clone(),
        }],
        init: Some(vec![
            steelbx::driver::InitStep::Cp {
                host: src.to_path_buf(),
                dest: format!("{dest}/copied"),
            },
            // The step order is the test: a cp before the exec that
            // checks it.
            steelbx::driver::InitStep::Exec(vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("test -f {dest}/copied && touch {dest}/steelbx-files-ok"),
            ]),
        ]),
        network: Some("host".to_string()),
        ..Default::default()
    }
}

/// The g8 exec env round-trip: with the name exposed (bare `-e NAME`),
/// podman copies the caller env's value into the box; without `-e`,
/// the exec env is the container's own (the test process's export does
/// not leak in).
fn exec_env_roundtrip(pod: &steelbx::driver::Podman) {
    let name = "steelbx-it-re";
    pod.exec(
        name,
        &["STEELBX_IT_RUNTIME_ENV".to_string()],
        &[
            "sh".to_string(),
            "-c".to_string(),
            "test \"${STEELBX_IT_RUNTIME_ENV:-}\" = from-host".to_string(),
        ],
    )
    .unwrap();
    pod.exec(
        name,
        &[],
        &[
            "sh".to_string(),
            "-c".to_string(),
            "test -z \"${STEELBX_IT_RUNTIME_ENV:-}\"".to_string(),
        ],
    )
    .unwrap();
}

/// The g7 completion assertions: each marker-labeled image is listed,
/// the unlabeled one never, and the dual-labeled one appears once.
/// Locally built images come back prefixed with `localhost/`.
fn assert_completion_lists(names: &[String]) {
    let names: Vec<&str> = names
        .iter()
        .map(|n| n.strip_prefix("localhost/").unwrap_or(n.as_str()))
        .collect();
    for img in [
        "steelbx-it-mk-box",
        "steelbx-it-mk-tb",
        "steelbx-it-mk-both",
    ] {
        assert!(
            names.iter().any(|n| n.starts_with(img)),
            "{img} (marker-labeled) must be listed; got {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n.starts_with("steelbx-it-mk-plain")),
        "an unlabeled image must not be listed: {names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|n| n.starts_with("steelbx-it-mk-both"))
            .count(),
        1,
        "an image carrying both markers must appear once: {names:?}"
    );
}

/// (image, marker labels) — the third carries BOTH (dedup), the
/// fourth none (must never appear).
const MARKER_IMAGES: [(&str, &[&str]); 4] = [
    (
        "steelbx-it-mk-box",
        &["com.github.simon3z.steelbx.box=true"],
    ),
    ("steelbx-it-mk-tb", &["com.github.containers.toolbox=true"]),
    (
        "steelbx-it-mk-both",
        &[
            "com.github.simon3z.steelbx.box=true",
            "com.github.containers.toolbox=true",
        ],
    ),
    ("steelbx-it-mk-plain", &[]),
];

/// Drops the g7 scratch container and the committed test images.
fn cleanup_labeled_images(built: &Vec<String>, scratch: &str) {
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", scratch])
        .output();
    for img in built {
        let _ = std::process::Command::new("podman")
            .args(["rmi", "-f", img])
            .output();
    }
}

/// Commits one marker-labeled test image from the scratch container
/// (options before the image: podman's create grammar).
fn build_labeled_image(
    img: &str,
    labels: &[&str],
    scratch: &str,
    built: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut args: Vec<&str> = vec!["create", "--name", scratch];
    for l in labels {
        args.extend(["--label", l]);
    }
    args.extend(["registry.fedoraproject.org/fedora:42", "sleep", "infinity"]);
    if let Err(e) = podman_ok(&args) {
        cleanup_labeled_images(built, scratch);
        return Err(e);
    }
    if let Err(e) = podman_ok(&["commit", scratch, img]) {
        cleanup_labeled_images(built, scratch);
        return Err(e);
    }
    // The scratch is reusable: drop it so the next build can re-create
    // the same name.
    let _ = podman_ok(&["rm", "-f", scratch]);
    built.push(img.to_string());
    Ok(())
}

/// ADR-020: an image that declares its own command
/// (`com.github.simon3z.steelbx.box.cmd` label + a CMD) is created with no command
/// override — podman runs the image's own ENTRYPOINT+CMD. Skipped when
/// podman is unavailable.
#[test]
fn g2_image_declared_command() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    let img = "steelbx-it-lbl:latest";
    ensure_base_image_local("registry.fedoraproject.org/fedora:42");

    // Build the test image: WORKDIR + marker + cmd label + a CMD,
    // committed from a created container (nothing runs).
    build_declared_cmd_image(img).unwrap();

    // The declared name/cmd round-trip through the driver's inspect.
    let meta = match pod.image_meta(img) {
        Ok(m) => m,
        Err(e) => {
            cleanup_lbl_image(img);
            panic!("image inspect failed: {e}");
        }
    };
    assert!(meta.cmd, "the cmd label must be readable");
    assert_eq!(meta.workdir.as_deref(), Some("/tmp"));

    // Create from it with an EMPTY command: podman runs the image's
    // own CMD (no steelbx override).
    let spec = steelbx::driver::CreateSpec {
        image: img.to_string(),
        ..Default::default()
    };
    if let Err(e) = pod.create("steelbx-it-cmd", &spec) {
        cleanup_lbl_image(img);
        panic!("create from the declared-command image failed: {e}");
    }
    assert!(pod.inspect("steelbx-it-cmd").unwrap().is_some());

    pod.remove_container("steelbx-it-cmd", true).unwrap();
    cleanup_lbl_image(img);
}

/// ADR-019/020 acceptance: podman accepts the toolbox flag set
/// (shared namespaces, privileged, no-hosts, ulimits, a user, and a
/// mount spec) in one create; create-only, the box is never started.
/// Propagation flags are not an accepted `--mount` option in podman
/// 5.x, so they are not pinned here. It also pins the `--user`/
/// `--workdir` overrides (ADR-022/017 revised): podman `create`
/// accepts a `--workdir` naming a directory that does not exist in
/// the image (an override may name one that `[init]` makes later).
#[test]
fn g3_toolbox_flag_set_is_accepted_by_podman() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    ensure_base_image_local("registry.fedoraproject.org/fedora:42");
    let nodir = "/steelbx-nodir-test";
    let spec = steelbx::driver::CreateSpec {
        image: "registry.fedoraproject.org/fedora:42".to_string(),
        command: vec!["sleep".to_string(), "infinity".to_string()],
        network: Some("host".to_string()),
        security_opts: vec!["label=disable".to_string()],
        mount_specs: vec!["type=bind,source=/tmp,destination=/tmp".to_string()],
        cgroupns: Some("host".to_string()),
        ipc: Some("host".to_string()),
        pid: Some("host".to_string()),
        userns: None, // keep-id is rootless-only; CI may be rootful
        user: Some("root".to_string()),
        workdir: Some(nodir.to_string()),
        privileged: true,
        no_hosts: true,
        ulimits: vec!["host".to_string()],
        ..Default::default()
    };
    // The g3 acceptance: podman `create` accepts the flag set —
    // including the `--workdir` override for a directory that does
    // not exist in the image (an override may name one that
    // `[init]` makes later).
    if let Err(e) = pod.create("steelbx-it-toolbox", &spec) {
        panic!("podman refused the toolbox flag set: {e}");
    }
    assert!(pod.inspect("steelbx-it-toolbox").unwrap().is_some());
    pod.remove_container("steelbx-it-toolbox", true).unwrap();
}

#[test]
fn g1_create_exec_rm() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let image = "registry.fedoraproject.org/fedora:42";
    let pod = steelbx::driver::Podman::detect().unwrap();
    ensure_base_image_local(image);

    // Layout is the image's: mount lands at <WORKDIR>/<basename>.
    let meta = match pod.image_meta(image) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let Some(workdir) = meta.workdir else {
        eprintln!("skipping: image has no WORKDIR");
        return;
    };

    // FR-007/FR-003: the box is created, a second create is refused.
    let (work, proj) = create_test_box(&pod, image, &workdir);
    // FR-009: exec surfaces the exit code; the marker lands on the
    // host mount (the box is started on demand, FR-004, ADR-007).
    exec_roundtrip(&pod, &workdir, &proj);
    // FR-005/FR-008: removal of a running box requires force.
    force_remove(&pod, "steelbx-it");
    // The box is gone: the bind source can go with the tempdir.
    drop(work);
}

/// ADR-021 acceptance: init commands run as exec -u 0 (root) right
/// after create — live-measured: a box must be initialized before it
/// is handed over. Skipped when podman is unavailable.
#[test]
fn g4_init_commands_run_as_root_after_create() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    ensure_base_image_local("registry.fedoraproject.org/fedora:42");
    let meta = pod
        .image_meta("registry.fedoraproject.org/fedora:42")
        .unwrap();
    let workdir = match meta.workdir {
        Some(w) => w,
        None => {
            eprintln!("skipping: fedora:42 has no WORKDIR");
            return;
        }
    };
    let proj = tempfile::TempDir::new().unwrap();
    let marker = proj.path().join("steelbx-init-ok");
    let spec = init_marker_spec(&workdir, proj.path());
    pod.create("steelbx-it-init", &spec).unwrap();
    assert!(
        marker.exists(),
        "the init command (exec -u 0) must have run"
    );
    // B5: the box is running after init — a plain rm must refuse.
    force_remove(&pod, "steelbx-it-init");
}

/// ADR-024 order: files are copied BEFORE the cmd execs — an init
/// command may depend on a copied file. Skipped when podman is
/// unavailable.
#[test]
fn g7_files_are_copied_before_init_cmds() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    ensure_base_image_local("registry.fedoraproject.org/fedora:42");
    let meta = pod
        .image_meta("registry.fedoraproject.org/fedora:42")
        .unwrap();
    let workdir = match meta.workdir {
        Some(w) => w,
        None => {
            eprintln!("skipping: fedora:42 has no WORKDIR");
            return;
        }
    };
    let proj = tempfile::TempDir::new().unwrap();
    // The copied file: a named temp file on the host, declared as a
    // files pair; the init command tests for it and touches a marker.
    let src = tempfile::NamedTempFile::new().unwrap();
    let marker = proj.path().join("steelbx-files-ok");
    let spec = files_spec(&workdir, proj.path(), src.path());
    // If the copy did not precede the command, the test -f fails, the
    // init aborts, and this create errors — the order is the test.
    pod.create("steelbx-it-files", &spec).unwrap();
    assert!(
        marker.exists(),
        "the init command must have seen the copied file"
    );
    force_remove(&pod, "steelbx-it-files");
}

/// ADR-021 guarantee: a failing init aborts create AND rolls the box
/// back — no half-initialized boxes. Skipped when podman is
/// unavailable. (Also exercises the rollback path on a RUNNING box:
/// init implied start, so the rollback must force-remove it.)
#[test]
fn g5_failed_init_rolls_the_box_back() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    ensure_base_image_local("registry.fedoraproject.org/fedora:42");
    let spec = steelbx::driver::CreateSpec {
        image: "registry.fedoraproject.org/fedora:42".to_string(),
        command: vec!["sleep".to_string(), "infinity".to_string()],
        init: Some(vec![steelbx::driver::InitStep::Exec(vec![
            "false".to_string()
        ])]),
        ..Default::default()
    };
    if pod.create("steelbx-it-initfail", &spec).is_ok() {
        panic!("a failing init must abort create");
    }
    assert!(
        pod.inspect("steelbx-it-initfail").unwrap().is_none(),
        "a failed init must roll the box back"
    );
}

/// ADR-022 (revised) guarantee, measured: the `user` override is
/// podman `--user` at create — podman resolves the user against the
/// image before the box is usable, so a profile `user` naming a user
/// the image lacks never yields a usable box: create fails, or (where
/// the podman version defers the lookup) the box fails to start.
/// Skipped when podman is unavailable.
#[test]
fn g6_user_override_is_unresolvable() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    let image = "registry.fedoraproject.org/fedora:42";
    ensure_base_image_local(image);
    let spec = steelbx::driver::CreateSpec {
        image: image.to_string(),
        command: vec!["sleep".to_string(), "infinity".to_string()],
        user: Some("steelbx-no-such-user".to_string()),
        ..Default::default()
    };
    match pod.create("steelbx-it-u", &spec) {
        Err(_) => {
            // Resolved at create: no box is left behind.
            assert!(
                pod.inspect("steelbx-it-u").unwrap().is_none(),
                "a refused create must leave no box"
            );
        }
        Ok(_) => {
            // Deferred resolution: the box must fail to start (the
            // user is unresolvable), and the half-created box is
            // cleaned up.
            pod.exec("steelbx-it-u", &[], &["true".to_string()])
                .unwrap_err();
            pod.remove_container("steelbx-it-u", true).unwrap();
            assert!(
                pod.inspect("steelbx-it-u").unwrap().is_none(),
                "cleanup must leave no box"
            );
        }
    }
}

/// Runtime env: the box's `box.env` label round-trips through
/// `inspect`, and a bare `-e NAME` on exec copies the value from the
/// caller env (never argv); without `-e`, the exec env is the
/// container's own (the caller's export does not leak in). Skipped
/// when podman is unavailable.
#[test]
fn g8_runtime_env_copies_from_the_caller_env() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    ensure_base_image_local("registry.fedoraproject.org/fedora:42");
    // Export into this test process: podman's own env gets it.
    std::env::set_var("STEELBX_IT_RUNTIME_ENV", "from-host");
    let spec = steelbx::driver::CreateSpec {
        image: "registry.fedoraproject.org/fedora:42".to_string(),
        command: vec!["sleep".to_string(), "infinity".to_string()],
        runtime_env: vec!["STEELBX_IT_RUNTIME_ENV".to_string()],
        ..Default::default()
    };
    if let Err(e) = pod.create("steelbx-it-re", &spec) {
        let _ = pod.remove_container("steelbx-it-re", true);
        panic!("create with runtime_env failed: {e}");
    }
    // The label round-trips through inspect.
    let info = pod.inspect("steelbx-it-re").unwrap().unwrap();
    assert_eq!(info.env, vec!["STEELBX_IT_RUNTIME_ENV".to_string()]);

    // Bare `-e NAME` copies the caller env's value; without `-e` the
    // exec env is the container's own.
    exec_env_roundtrip(&pod);
    pod.remove_container("steelbx-it-re", true).unwrap();
}

/// `create -i` completion is a union of the two marker labels
/// (`com.github.simon3z.steelbx.box`, `com.github.containers.toolbox`), podman-side filtered,
/// and an image carrying both appears once. Skipped when podman is
/// unavailable.
#[test]
fn g7_image_completion_lists_both_markers() {
    if !podman_available() {
        eprintln!("skipping: podman not available");
        return;
    }
    let pod = steelbx::driver::Podman::detect().unwrap();
    ensure_base_image_local("registry.fedoraproject.org/fedora:42");
    let scratch = "steelbx-it-mk-scratch";
    let mut built: Vec<String> = vec![];
    for (img, labels) in MARKER_IMAGES {
        build_labeled_image(img, labels, scratch, &mut built).unwrap();
    }

    let names = match pod.image_names() {
        Ok(n) => n,
        Err(e) => {
            cleanup_labeled_images(&built, scratch);
            panic!("image_names failed: {e}");
        }
    };
    assert_completion_lists(&names);
    cleanup_labeled_images(&built, scratch);
}
