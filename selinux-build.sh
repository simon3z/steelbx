#!/usr/bin/env bash
# Build the steelbx_selinux SELinux module from source.
#
# Replicates the distro header build pipeline (m4 -> checkmodule ->
# semodule_package) because /usr/share/selinux/devel/Makefile resolves
# HEADERDIR incorrectly in minimal/container environments.
#
# Two m4 stages, mirroring the distro's support/Makefile.devel:
#
#   stage 1  Generate all_interfaces.conf: m4-preprocess ALL installed .if
#            interface files (support .spt + include/**/*.if). The result
#            contains literal `define(...)' calls for every interface
#            function, suppressed via divert(-1).
#   stage 2  m4 over support .spt + all_interfaces.conf + the module .te.
#            Processing all_interfaces.conf re-executes those defines, so
#            the module can call the same interface functions the distro
#            sources call (virt_sandbox_domain_template, kernel_read_messages,
#            ...), exactly like container.te does.
#
# Usage: selinux-build.sh [module.te] [output.pp]
# Run from the project root (default module: selinux/steelbx_selinux.te).
set -euo pipefail

for tool in m4 checkmodule semodule_package; do
    command -v "$tool" >/dev/null || {
        echo "error: $tool not found (install selinux-policy-devel and m4)" >&2
        exit 1
    }
done

cd "$(dirname "$0")"

MOD_TE=${1:-selinux/steelbx_selinux.te}
MOD_PP=${2:-selinux/steelbx_selinux.pp}
BASE=$(basename "$MOD_TE" .te)

# m4 parameters: targeted policy is an MCS build (16 sensitivities, 1024 cats).
M4PARAM="-D enable_mcs -D mcs_num_cats=1024 -D mls_num_sens=16 -D mls_num_cats=1024 -D hide_broken_symptoms"
SUPPORT_DIR=$(dirname "$(find /usr/share/selinux -path '*/include/support/loadable_module.spt' 2>/dev/null | head -1)")
if [ ! -f "$SUPPORT_DIR/loadable_module.spt" ]; then
    echo "error: SELinux policy headers not found (install selinux-policy-devel)" >&2
    exit 1
fi
INCLUDE_DIR=$(dirname "$SUPPORT_DIR")
mapfile -t IF_FILES < <(find "$INCLUDE_DIR" -name '*.if' | sort)
if [ ${#IF_FILES[@]} -eq 0 ]; then
    echo "error: no .if interface files found under $INCLUDE_DIR" >&2
    exit 1
fi
# Vendored interface definitions (container-selinux does not install its
# .if files as headers; see selinux/vendor/).
VENDOR_DIR="selinux/vendor"
mapfile -t VENDOR_IFS < <(find "$VENDOR_DIR" -name '*.if' | sort)

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# --- stage 1: all_interfaces.conf (recipe from support/Makefile.devel)
echo "ifdef(\`__if_error',\`m4exit(1)')" > "$TMP/iferror.m4"
echo 'divert(-1)' > "$TMP/all_interfaces.conf"
m4 -s "$SUPPORT_DIR"/*.spt ${IF_FILES[@]} ${VENDOR_IFS[@]} "$TMP/iferror.m4" \
    | sed -e 's/dollarsstar/\$\*/g' >> "$TMP/all_interfaces.conf"
echo 'divert' >> "$TMP/all_interfaces.conf"

# --- stage 2: module (.spt + all_interfaces.conf + .te, as in Makefile.devel)
m4 $M4PARAM -s "$SUPPORT_DIR"/*.spt "$TMP/all_interfaces.conf" "$MOD_TE" > "$TMP/$BASE.tmp"
checkmodule -m -M "$TMP/$BASE.tmp" -o "$TMP/$BASE.mod"
semodule_package -o "$MOD_PP" -m "$TMP/$BASE.mod"

echo "built $MOD_PP"
