#!/bin/sh
# Build and install libbtrfsutil from a btrfs-progs release.
#
# The `libbtrfsutil` crate needs a header newer than the one Debian 12 and Ubuntu
# 22.04/24.04 ship (btrfs_util_subvolume_get_default arrived after btrfs-progs 6.6).
# Release binaries are built on an old distribution for glibc compatibility, so the
# newer library is built from source here. At runtime any libbtrfsutil.so.1 works.
set -eu

VERSION="${BTRFS_PROGS_VERSION:-v6.14}"
PREFIX="${PREFIX:-/usr/local}"
SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
git clone --quiet --depth 1 --branch "$VERSION" https://github.com/kdave/btrfs-progs "$work/src"
cd "$work/src/libbtrfsutil"

major=$(sed -n 's/^#define BTRFS_UTIL_VERSION_MAJOR \([0-9]*\)$/\1/p' btrfsutil.h)
minor=$(sed -n 's/^#define BTRFS_UTIL_VERSION_MINOR \([0-9]*\)$/\1/p' btrfsutil.h)
patch=$(sed -n 's/^#define BTRFS_UTIL_VERSION_PATCH \([0-9]*\)$/\1/p' btrfsutil.h)
version="$major.$minor.$patch"

gcc -O2 -fPIC -D_GNU_SOURCE -I. -shared \
  -Wl,-soname,"libbtrfsutil.so.$major" -Wl,--version-script=libbtrfsutil.sym \
  errors.c filesystem.c qgroup.c stubs.c subvolume.c \
  -o "libbtrfsutil.so.$version"

cat > libbtrfsutil.pc <<PC
prefix=$PREFIX
exec_prefix=\${prefix}
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: libbtrfsutil
Description: libbtrfsutil library
Version: $version
URL: https://btrfs.readthedocs.io
Cflags: -I\${includedir}
Libs: -L\${libdir} -lbtrfsutil
PC

$SUDO install -d "$PREFIX/lib/pkgconfig" "$PREFIX/include"
$SUDO install -m755 "libbtrfsutil.so.$version" "$PREFIX/lib/"
$SUDO ln -sf "libbtrfsutil.so.$version" "$PREFIX/lib/libbtrfsutil.so.$major"
$SUDO ln -sf "libbtrfsutil.so.$version" "$PREFIX/lib/libbtrfsutil.so"
$SUDO install -m644 btrfsutil.h "$PREFIX/include/"
$SUDO install -m644 libbtrfsutil.pc "$PREFIX/lib/pkgconfig/"
$SUDO ldconfig
echo "installed libbtrfsutil $version ($VERSION) to $PREFIX"
