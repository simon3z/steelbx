%bcond_without check

Name:           steelbx
Version:        0.1.0
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

# Bundled(crate(<name>) = <version> provides: the cargo_vendor fileattr
# hook (cargo-rpm-macros) emits them from the shipped cargo-vendor.txt
# (see "Bundled Dependencies" in the Fedora Rust Packaging Guidelines).

%description
%{summary}.

%prep
%autosetup -n %{name}-%{version}
# Extract vendored deps + .cargo/config into the source tree
tar xJf %{S:1} \
    -C %{_builddir}/%{name}-%{version}/

%build
# .cargo/config (from cargo vendor) redirects all deps to ./vendor
# --offline: no network access   --frozen: use Cargo.lock as-is
cargo build --release --offline --frozen
# Write cargo-vendor.txt (one "name vX.Y.Z" line per crate); shipped via
# %%license, the cargo_vendor fileattr hook turns it into
# bundled(crate(<name>) = <version> provides.
%cargo_vendor_manifest

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
install -Dm 0755 target/release/steelbx %{buildroot}%{_bindir}/steelbx
gzip -9 man/steelbx.1
install -Dm 0644 man/steelbx.1.gz %{buildroot}%{_mandir}/man1/steelbx.1.gz
mkdir -p %{buildroot}/etc/steelbx/profiles
for f in examples/*.conf; do
    install -m 0644 "$f" %{buildroot}/etc/steelbx/profiles/
done
# Dynamic bash completion (clap_complete engine): a wrapper that
# re-invokes the binary (COMPLETE=bash steelbx ...).
cat > steelbx-completion <<'EOF'
_steelbx_bash_autocomplete() {
    local cur opts
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    opts=$(COMPLETE=bash steelbx "${COMP_WORDS[@]:1:$COMP_CWORD}")
    COMPREPLY=( $(compgen -W "${opts}" -- "${cur}" ) )
    return 0
}
complete -F _steelbx_bash_autocomplete -o default steelbx
EOF
install -Dm 0644 steelbx-completion \
    %{buildroot}%{_datadir}/bash-completion/completions/steelbx

# Ship the bundled crates' license files to the package license directory
# (cargo-vendor.txt is copied there by the %%license scriptlet itself).
mkdir -p %{buildroot}%{_defaultlicensedir}/%{name}
for f in bundled-licenses/*; do
    install -m 0644 "$f" %{buildroot}%{_defaultlicensedir}/%{name}/
done

%check
%if %{with check}
cargo test --release --offline --frozen --lib --bins
%endif

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
* Sun Sep 20 2026 Federico Simoncelli <federico.simoncelli@gmail.com> - 0.1.0-%{?release}%{!?release:1}
- Initial RPM: podman-driven workload runner with profile-based config,
  dynamic bash completion, and self-contained offline build
