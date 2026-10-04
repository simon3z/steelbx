%bcond_without check

Name:           steelbx
Version:        0.1.1
# Release: computed by rpm-build.sh (--define release N = commits since
# the last tag + 1); defaults to 1 when release is not defined.
Release:        %{?release}%{!?release:1}%{?dist}
Summary:        Run unsupervised workloads in isolated containers with security profiles
# Cumulative SPDX expression (project AND union of all vendored crate
# licenses); verified by rpm-build.sh (license_expression) against the
# Cargo.toml license fields of the vendored crates.
License:        Apache-2.0 AND Apache-2.0 WITH LLVM-exception AND LGPL-2.1-or-later AND MIT AND Unicode-3.0 AND Unlicense
URL:            https://github.com/simon3z/steelbx
# Source0: git archive of the project
# Source1: vendored dependencies (cargo vendor) + .cargo/config
Source0:        %{name}-%{version}.tar.xz
Source1:        %{name}-%{version}-vendor.tar.xz

ExclusiveArch:  %{rust_arches}

Requires:       podman
BuildRequires:  rust
# rust-packaging (cargo-rpm-macros) provides the vendor-manifest macro
# and the cargo_vendor fileattr hook: the shipped cargo-vendor.txt
# becomes Provides: bundled(crate(<name>)) = <version> per vendored crate.
BuildRequires:  rust-packaging
# The lib target (src/lib.rs) exists only so tests/integration.rs can drive
# the internals; it is not shipped. cargo-rpm-macros would otherwise copy
# the crate into %%crate_instdir and create a -devel subpackage, which an
# application package does not need.
%define cargo_install_lib 0
# For the selinux subpackage (module compiled during %build)
BuildRequires:  m4
BuildRequires:  checkpolicy
BuildRequires:  selinux-policy-devel
# Bundled(crate(<name>) = <version> provides: the cargo_vendor fileattr
# hook (cargo-rpm-macros) emits them from the shipped cargo-vendor.txt
# (see "Bundled Dependencies" in the Fedora Rust Packaging Guidelines).

%description
%{summary}.

%package selinux
Summary:        SELinux module for steelbx boxes using host session resources
# semodule (-i/-r in the scriptlets below)
Requires:       policycoreutils

%description selinux
SELinux module for the steelbx container runner. It defines
steelbx_wayland_t, a pre-canned container-side domain that a
profile selects with
  security_opts = ["label=type:steelbx_wayland_t"]
which makes podman run the container's processes as that type
instead of the default container_t.

The domain is a literal mirror of container_t from container-selinux
2.251.0: the same interface calls in the same order, so the
attribute-scoped container grants (container_domain's self:* set,
container_ro_file_t read/exec, ...) and any future container-selinux
interface changes apply as-is, and only the type name is substituted.
On top of the mirror, a permission group (steelbx_wayland_access)
grants the host Wayland resources steelbx profiles bind-mount into
the box (/run/user/<UID> wayland sockets, host config dirs), and a
host-side grant lets system_dbusd_t toggle SELinux enforcement.
%prep
# -a 1: extract the vendor tarball (Source1) on top of Source0
%autosetup -a 1
# %%cargo_prep: writes .cargo/config.toml itself (offline +
# vendored-sources dir), removes Cargo.lock, defines [profile.rpm]
%cargo_prep -v vendor

%build
# %%cargo_build: cargo build --profile rpm -Z avoid-dev-deps + smp flags;
# offline mode comes from the .cargo/config.toml that %%cargo_prep wrote.
%cargo_build
# Write cargo-vendor.txt (one "name vX.Y.Z" line per crate); shipped via
# %%license, the cargo_vendor fileattr hook turns it into
# bundled(crate(<name>) = <version> provides.
%cargo_vendor_manifest

# Build the SELinux module for the selinux subpackage.
# selinux-build.sh replicates the distro header build pipeline
# (m4 -> checkmodule -> semodule_package); the distro Makefile in
# /usr/share/selinux/devel resolves HEADERDIR incorrectly in some
# environments.
./selinux-build.sh selinux/steelbx_selinux.te steelbx_selinux.pp

# Stage the license files of the bundled crates under unique names
# (crate name prefix, to avoid basename clashes) for %%license packaging.
mkdir -p bundled-licenses
for crate in vendor/*/; do
    c=$(basename "$crate")
    ls "$crate" | grep -iE '^(licen|copying)' | while read -r f; do
        cp "$crate$f" "bundled-licenses/${c}-$f"
    done
done

%install
# %%cargo_install: cargo install --profile rpm --no-track --path .
export PATH="%{buildroot}%{_bindir}:$PATH"
%cargo_install
install -Dm 0644 steelbx_selinux.pp %{buildroot}%{_datadir}/selinux/packages/steelbx_selinux.pp
gzip -9 man/steelbx.1
install -Dm 0644 man/steelbx.1.gz %{buildroot}%{_mandir}/man1/steelbx.1.gz
mkdir -p %{buildroot}/etc/steelbx/profiles
for f in examples/*.conf; do
    install -m 0644 "$f" %{buildroot}/etc/steelbx/profiles/
done
# Dynamic bash completion: the source-tree file sources the binary's
# own registration at load time (clap_complete dynamic protocol), so
# the shipped file can never drift out of sync with the binary.
install -Dm 0644 completions/steelbx \
    %{buildroot}%{_datadir}/bash-completion/completions/steelbx

# Ship the bundled crates' license files to the package license directory
# (cargo-vendor.txt is copied there by the %%license scriptlet itself).
mkdir -p %{buildroot}%{_defaultlicensedir}/%{name}
for f in bundled-licenses/*; do
    install -m 0644 "$f" %{buildroot}%{_defaultlicensedir}/%{name}/
done

%check
%if %{with check}
# %%cargo_test (as shipped) runs ALL test targets; the integration suite
# (tests/integration.rs) needs a working podman, which the build
# container lacks. Run the same lib + bins set that ci.sh runs.
%{__cargo} test %{__cargo_common_opts} --profile rpm --lib --bins --no-fail-fast
%endif

# No rpm macro exists for loading SELinux modules; these follow the
# distro convention (hand-written shell, cf. container-selinux):
# $1-guards, SELINUXTYPE from /etc/selinux/config, semodule -n -X 200,
# and '|| :' so policy-load problems never fail the rpm transaction.
%post selinux
# $1 = number of package instances after install; 1 = fresh install.
if [ $1 -eq 1 ] && [ -d /sys/fs/selinux ]; then
    . /etc/selinux/config
    semodule -n -s ${SELINUXTYPE} -X 200 -i \
        %{_datadir}/selinux/packages/steelbx_selinux.pp || :
else
    echo "The SELinux module is not being loaded now (SELinux disabled"
    echo "or upgrade). Load it with:"
    echo "  semodule -i %{_datadir}/selinux/packages/steelbx_selinux.pp"
fi

%postun selinux
# $1 = number of package instances left after removal; 0 = last one.
if [ $1 -eq 0 ] && [ -d /sys/fs/selinux ]; then
    . /etc/selinux/config
    semodule -n -s ${SELINUXTYPE} -X 200 -r steelbx_selinux || :
fi

%files selinux
%license LICENSE
%{_datadir}/selinux/packages/steelbx_selinux.pp

%files
%license LICENSE
%license cargo-vendor.txt
%license bundled-licenses/*
%doc README.md
%{_bindir}/steelbx
%{_mandir}/man1/steelbx.1.gz
%{_datadir}/bash-completion/completions/steelbx
%config(noreplace) /etc/steelbx/profiles/*.conf

# Release is passed by rpm-build.sh as --define release N (commits since
# the last tag + 1); this entry and Release: expand from the same macro,
# so the changelog always matches the built Release.
%changelog
* Sun Sep 27 2026 Federico Simoncelli <federico.simoncelli@gmail.com> - 0.1.1-%{?release}%{!?release:1}
- Updated to upstream 0.1.1

* Sun Sep 20 2026 Federico Simoncelli <federico.simoncelli@gmail.com> - 0.1.0-%{?release}%{!?release:1}
- Initial RPM: podman-driven workload runner with profile-based config,
  dynamic bash completion, and self-contained offline build
