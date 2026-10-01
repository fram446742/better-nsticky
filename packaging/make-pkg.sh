#!/usr/bin/env bash
# Build or install the tagged better-nsticky release with makepkg.
#
#     packaging/make-pkg.sh       # build only
#     packaging/make-pkg.sh -si    # build and install (asks for sudo)

# Extra arguments are passed to makepkg, so `-f` (rebuild), `-c` (clean) and
# `-si` (install) all work.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PKG_DIR="$REPO_ROOT/packaging"

echo "==> cleaning previous build output"
rm -rf "${PKG_DIR:?}/src" "${PKG_DIR:?}/pkg"

echo "==> running makepkg $*"
cd "$PKG_DIR"
makepkg "$@"

echo
echo "built:"
ls -1 "$PKG_DIR"/*.pkg.tar.* 2>/dev/null | sed 's/^/    /' || echo "    (no package? check the makepkg output above)"
