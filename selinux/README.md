# steelbx_selinux

SELinux module providing `steelbx_wayland_t`: a pre-canned container domain
for steelbx boxes that run host-facing Wayland tools. A box opts in with:

```
security_opts = ["label=type:steelbx_wayland_t"]
```

Built with `./selinux-build.sh` → `selinux/steelbx_selinux.pp`, shipped in the
RPM (`steelbx.spec` `%build` runs the script).

## Layout of `steelbx_selinux.te`

The file is deliberately split so the shared part and the steelbx-specific
part are trivially separable:

**SECTION 1 — `container_t` mirror (literal copy).** Every line corresponds 1:1
to a cited source line, in source order, with the single mechanical
substitution `container_t` → `steelbx_wayland_t` / `container` →
`steelbx_wayland`. It is the same interface-call list the distro sources use
for `container_t`:

| Source (version pinned in the `.te` header) | What it contributes |
|---|---|
| `virt.te:1847` (selinux-policy contrib/virt) — `virt_sandbox_domain_template(container)` | declares the type + base sandbox attributes (`svirt_sandbox_domain`, `domain`, `corenet_unlabeled_type`, `process_user_target`, `mlsrangetrans`, `mcs_constrained_type`, `kernel_system_state_reader`, `can_dump_kernel`, `can_receive_kernel_messages`, `syslog_client_type`) |
| `container.te:960` — `container_manage_files_template(container, container)` | full `container_file_t` management (template body vendored in `selinux/vendor/container_manage_files.if` because container-selinux does not install `container.if` as a header) |
| `container.te:963` | `container_domain`/`container_net_domain`/`container_user_domain` attributes — every attribute-scoped container-selinux grant (53 `self:*` grants, `container_ro_file_t` read/exec, ...) applies automatically and is **not** restated |
| `container.te:1017, 1111, 1116, 1118, 1141, 1144, 1148, 1237` | interface calls (`dev_mounton_sysfs`, `domain_user_exemption_target`, `virt_sandbox_net_domain`, `logging_send_syslog_msg`, `corenet_unconfined`, `virt_default_capabilities`, `kernel_read_messages`, `logging_send_audit_msgs`) |
| `container.te:1134-1135, 1423, 1426, 1515, 1516` | the type-specific `allow` rules (sys_admin caps, `proc_t:filesystem remount`, `container_var_run_t:dir`, xserver misc devices) |

Notes:
- The source wraps the sys_admin and audit grants in `tunable_policy(...)`
  with booleans declared inside the container-selinux module; this module
  cannot reference those booleans, so the grants are written
  unconditionally (source defaults are on).
- The source's `optional_policy(virt_default_capabilities(...))` is likewise
  written unconditionally.

**SECTION 2 — steelbx additions.** Only what is not in the `container_t`
source:
- `container_runtime_t` → `steelbx_wayland_t:process transition` (crun)
  and the conmon/crun stdin/stderr fifo rules;
- the audit-derived `self:cap_userns` set = the `container_domain` set
  (`container-te:1187`) **plus `fsetid`**;
- the `steelbx_wayland_access` permission group (host Wayland access; each
  rule cites the audit denial that motivated it);
- the host-side `system_dbusd_t` `setenforce` grant via `can_setenforce`.

## How the build expands the interface calls

`selinux-build.sh` replicates the distro's `support/Makefile.devel` pipeline:

1. **Stage 1** m4-preprocesses every installed `.if` under
   `/usr/share/selinux/devel/include/` plus the vendored files in
   `selinux/vendor/` (currently `container_manage_files.if`) into
   `all_interfaces.conf` (`divert(-1)` wrapper, `iferror.m4` guard,
   `dollarsstar` sed — the exact distro recipe). The file contains a
   literal `define(...)` for every interface function.
2. **Stage 2** runs `m4` over the support `.spt` files +
   `all_interfaces.conf` + the module `.te`; processing
   `all_interfaces.conf` re-executes the defines, so SECTION 1's interface
   calls expand exactly as they do in the distro build.
3. `checkmodule -m` → `semodule_package`.

Because SECTION 1 calls the same interfaces as the source, base-policy
interface changes (e.g. `kernel_read_all_proc` gaining a grant) are picked
up automatically on rebuild — only `container-te` itself needs syncing.

## Getting the source

```
dnf download --source container-selinux selinux-policy
rpm2cpio container-selinux-*.src.rpm | cpio -idm
tar xf v*.tar.gz                       # -> container-selinux-<ver>/
rpm2cpio selinux-policy-*.src.rpm | cpio -idm
tar xf selinux-policy-*.tar.*          # -> policy/modules/...
```

## Updating when container-selinux upgrades

1. Fetch the new source (above); note the version
   (`rpm -qp container-selinux-*.src.rpm`).
2. Extract `container_t`'s definition and diff it against SECTION 1:
   ```
   grep -n 'container_t' container-selinux-*/container.te | grep -v '^\s*#'
   grep -n -A25 'template(`virt_sandbox_domain_template`' selinux-policy-*/policy/modules/contrib/virt.if
   ```
   Apply the changed lines with the `container_t` → `steelbx_wayland_t`
   substitution. If `container_manage_files_template` changed, re-vendor its
   body from the new `container.if`.
3. Bump the version line in the `.te` header.
4. Rebuild and reload:
   ```
   ./selinux-build.sh
   sudo semodule -r steelbx_selinux
   sudo semodule -i selinux/steelbx_selinux.pp
   ```
5. Retest the box with `label=type:steelbx_wayland_t`; check
   `/var/log/audit/audit.log` for denials and add them to SECTION 2.
