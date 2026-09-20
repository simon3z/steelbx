#!/usr/bin/env bash
# Build a self-contained RPM from git source + vendored deps.
# Usage: ./rpm-build.sh
#
# Requires: cargo, rpmbuild, xz
set -euo pipefail
cd "$(dirname "$0")"

version=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["packages"][0]["version"])')

echo "==> Building RPM for steelbx v${version}"

# --- 1. Source tarball (git archive) -----------------------------------
echo "==> Creating source tarball (git archive)"
git archive --prefix="steelbx-${version}/" HEAD | xz -T0 > "steelbx-${version}.tar.xz"

# --- 2. Vendored deps tarball -------------------------------------------
echo "==> Vendoring dependencies"
rm -rf vendor .cargo
cargo vendor vendor/

# cargo vendor does NOT create .cargo/config — create it manually
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor/"
EOF

# Bundle vendor/ + .cargo/ into a single tar.xz
tar cJf "steelbx-${version}-vendor.tar.xz" vendor .cargo
rm -rf vendor .cargo

# --- 3. rpmbuild --------------------------------------------------------
echo "==> Running rpmbuild"
RPM_TOP=~/rpmbuild
mkdir -p "${RPM_TOP}"/{SOURCES,SPECS,BUILD,BUILDROOT,RPMS,TMP}

cp "steelbx-${version}.tar.xz"          "${RPM_TOP}/SOURCES/"
cp "steelbx-${version}-vendor.tar.xz"   "${RPM_TOP}/SOURCES/"
cp steelbx.spec                           "${RPM_TOP}/SPECS/"
# Compute release number: commits since last tag + 1
if last_tag=$(git describe --tags --abbrev=0 2>/dev/null); then
    rel=$(( $(git rev-list --count "${last_tag}..HEAD") + 1 ))
else
    rel=1
fi
echo "==> Release number: ${rel} (since ${last_tag:-no tag})"

rpmbuild --define "release ${rel}" -ba "${RPM_TOP}/SPECS/steelbx.spec"

# --- 4. Result ----------------------------------------------------------
echo "==> Done"
ls -lh "${RPM_TOP}/RPMS/x86_64/"*.rpm

# Clean up tarballs (keep the RPM)
rm -f "steelbx-${version}.tar.xz" "steelbx-${version}-vendor.tar.xz"
