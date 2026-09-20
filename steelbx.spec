%bcond_without check

Name:           steelbx
Version:        0.1.0
Release:        %{release}%{?dist}
Summary:        Run unsupervised workloads in isolated podman containers with security profiles
License:        Apache-2.0
URL:            https://github.com/simon3z/steelbx
# Source0: git archive of the project
# Source1: vendored dependencies (cargo vendor) + .cargo/config
Source0:        %{name}-%{version}.tar.xz
Source1:        %{name}-%{version}-vendor.tar.xz

ExclusiveArch:  %{rust_arches}

Requires:       podman
BuildRequires:  rust

%description
%{summary}.

%prep
%autosetup -n %{name}-%{version}
# Extract vendored deps + .cargo/config into the source tree
tar xJf %{_sourcedir}/%{name}-%{version}-vendor.tar.xz \
    -C %{_builddir}/%{name}-%{version}/

%build
# .cargo/config (from cargo vendor) redirects all deps to ./vendor
# --offline: no network access   --frozen: use Cargo.lock as-is
cargo build --release --offline --frozen

%install
install -D -m 0755 target/release/steelbx %{buildroot}%{_bindir}/steelbx
gzip -9 man/steelbx.1
install -D -m 0644 man/steelbx.1.gz %{buildroot}%{_mandir}/man1/steelbx.1.gz
mkdir -p %{buildroot}/etc/steelbx/profiles
for f in examples/*.conf; do
    install -m 0644 "$f" %{buildroot}/etc/steelbx/profiles/
done
mkdir -p %{buildroot}%{_datadir}/bash-completion/completions
COMPLETE=bash ./target/release/steelbx > %{buildroot}%{_datadir}/bash-completion/completions/steelbx

%if %{with check}
%check
cargo test --release --offline --frozen --lib --bins
%endif

%files
%license LICENSE
%doc README.md
%{_bindir}/steelbx
%{_mandir}/man1/steelbx.1.gz
%{_datadir}/bash-completion/completions/steelbx
%config(noreplace) /etc/steelbx/profiles/*.conf

%changelog
* Sun Sep 20 2026 Federico Simoncelli <federico.simoncelli@gmail.com> - 0.1.0-%{release}
- Initial RPM: podman-driven workload runner with profile-based config,
  dynamic bash completion, and self-contained offline build
