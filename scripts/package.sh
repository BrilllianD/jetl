#!/usr/bin/env bash
# Build release artifacts for jetl (linux x86_64 gnu) into dist/.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/package.sh [--arch] [-h|--help]

Builds a release binary with the bundled libgit2 and writes to dist/:
  jetl-<version>-x86_64-unknown-linux-gnu/            staging directory
  jetl-<version>-x86_64-unknown-linux-gnu.tar.gz      release tarball
  jetl-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256

Options:
  --arch      also build the Arch Linux package (jetl-bin) with makepkg
              from packaging/arch/PKGBUILD
  -h, --help  show this help
EOF
}

arch=0
for arg in "$@"; do
    case "$arg" in
        --arch) arch=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "package.sh: unknown argument: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

die() {
    echo "package.sh: error: $*" >&2
    exit 1
}

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

# Version from the [package] table of Cargo.toml.
version="$(sed -n '/^\[package\]/,/^\[/{s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p}' Cargo.toml | head -n1)"
[[ -n "$version" ]] || die "could not read the package version from Cargo.toml"

target="x86_64-unknown-linux-gnu"
name="jetl-$version-$target"
dist="$repo/dist"
stage="$dist/$name"
tarball="$dist/$name.tar.gz"

[[ "$(uname -m)" == "x86_64" ]] || die "only x86_64 hosts are supported (got $(uname -m))"

echo "==> Building jetl $version (release, bundled libgit2)"
LIBGIT2_NO_PKG_CONFIG=1 cargo build --release --locked

bin="$repo/target/release/jetl"
[[ -x "$bin" ]] || die "release binary not found at $bin"

echo "==> Checking dynamic libraries"
if linked="$(ldd "$bin" | grep -E 'libgit2|libssl|libcrypto|libssh2')"; then
    echo "$linked" >&2
    die "the binary links a system libgit2/openssl/libssh2; it must use the bundled libgit2 without network features"
fi

echo "==> Staging $stage"
rm -rf "$stage" "$tarball" "$tarball.sha256"
mkdir -p "$stage/docs"
install -m755 "$bin" "$stage/jetl"
install -m644 README.md LICENSE CHANGELOG.md "$stage/"
install -m644 docs/CONFIG.md docs/KEYMAP.md "$stage/docs/"

# Reproducible-ish tarball: fixed order, owner and mtime (last commit time).
if epoch="$(git log -1 --format=%ct 2>/dev/null)" && [[ -n "$epoch" ]]; then
    :
else
    epoch="$(date +%s)"
fi
export SOURCE_DATE_EPOCH="$epoch"

echo "==> Creating $tarball"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
    --mode='u+rwX,go+rX,go-w' \
    -C "$dist" -cf - "$name" | gzip -n -9 >"$tarball"
(cd "$dist" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")

artifacts=("$stage/jetl" "$tarball" "$tarball.sha256")

if ((arch)); then
    command -v makepkg >/dev/null 2>&1 || die "--arch needs makepkg, which is not installed"
    echo "==> Building Arch package"
    pkgdir_arch="$repo/packaging/arch"
    # JETL_VERSION feeds pkgver in the PKGBUILD; see the comment at its top.
    export JETL_VERSION="$version" SRCDEST="$dist" PKGDEST="$dist"
    (cd "$pkgdir_arch" && makepkg -f --cleanbuild --clean)
    while IFS= read -r pkg; do
        [[ -f "$pkg" ]] && artifacts+=("$pkg")
    done < <(cd "$pkgdir_arch" && makepkg --packagelist)
fi

echo "==> Artifacts"
for a in "${artifacts[@]}"; do
    echo "  ${a#"$repo"/}"
done
