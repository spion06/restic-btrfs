#!/bin/sh
# Work out the next version from conventional commits and prepare the release files.
#
#   prepare-release.sh [auto|patch|minor|major]
#
# Needs git-cliff on PATH and a full git history. Rewrites Cargo.toml, Cargo.lock and
# CHANGELOG.md in the working tree, writes the new changelog section (without its
# heading) to release-notes.md, and prints `version=X.Y.Z` (also appended to
# $GITHUB_OUTPUT when set). Exits 1 if there is nothing to release.
set -eu

BUMP="${1:-auto}"
case "$BUMP" in auto | patch | minor | major) ;; *) echo "bump must be auto, patch, minor or major" >&2; exit 2 ;; esac

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
if [ "$BUMP" = auto ]; then
  next=$(git cliff --bumped-version)
  next="${next#v}"
else
  # forced: plain arithmetic on the current version, independent of the commits
  major=${current%%.*}; rest=${current#*.}; minor=${rest%%.*}; patch=${rest#*.}; patch=${patch%%[-+]*}
  case "$BUMP" in
    patch) next="$major.$minor.$((patch + 1))" ;;
    minor) next="$major.$((minor + 1)).0" ;;
    major) next="$((major + 1)).0.0" ;;
  esac
fi

if [ "$next" = "$current" ]; then
  echo "Nothing to release: no conventional commits (feat, fix, ...) since v$current." >&2
  echo "Use a forced bump (patch, minor or major) if you want a release anyway." >&2
  exit 1
fi

# New changelog section, e.g. "## 0.2.0 - 2026-10-04" followed by grouped entries.
git cliff --unreleased --tag "v$next" 2>/dev/null | sed '/./,$!d' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' > section.md || true
if ! grep -q '^### ' section.md; then
  printf '## %s - %s\n\nNo notable changes.\n' "$next" "$(date -u +%Y-%m-%d)" > section.md
fi
# release notes: the section without its "## version" heading line
awk 'f {print} /^## /{f=1}' section.md | sed '/./,$!d' > release-notes.md

# CHANGELOG.md = header, new section, then the previous sections.
{
  awk '/^## /{exit} {print}' CHANGELOG.md | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}'
  echo
  cat section.md
  echo
  awk '/^## /{f=1} f' CHANGELOG.md
} > CHANGELOG.new
mv CHANGELOG.new CHANGELOG.md
rm -f section.md

# Cargo.toml: only the [package] version. Cargo.lock follows.
awk -v v="$next" '/^\[package\]/{p=1} p && /^version = /{sub(/".*"/, "\"" v "\""); p=0} {print}' Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml
cargo update --workspace --quiet

echo "version=$next"
[ -z "${GITHUB_OUTPUT:-}" ] || echo "version=$next" >> "$GITHUB_OUTPUT"
