#!/bin/sh
# Prepare a release: one commit and a tag, the same way every time.
#
#   scripts/release.sh 0.1.0
#
# It sets the version in Cargo.toml, moves the changelog's [Unreleased] entries under the new
# version with today's date, adds the compare links, commits "chore: release 0.1.0" and tags
# v0.1.0. Nothing is pushed: look it over, then push the branch and the tag. Pushing the tag
# starts .github/workflows/release.yml, which builds and publishes the binaries.
set -eu
cd "$(dirname "$0")/.."

die() { echo "release: $*" >&2; exit 1; }

version=${1:-}
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' \
    || die "give a version like 0.1.0 or 0.2.0-beta.1"
tag="v$version"

[ "$(git branch --show-current)" = main ] || die "releases are made from main"
git diff --quiet && git diff --cached --quiet || die "commit or stash your changes first"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$tag already exists"
[ -n "$(scripts/changelog-section.sh Unreleased)" ] \
    || die "CHANGELOG.md has nothing under [Unreleased]"

repo=$(sed -n 's/^repository = "\(.*\)"$/\1/p' Cargo.toml)
previous=$(git describe --tags --abbrev=0 2>/dev/null || true)
today=$(date +%Y-%m-%d)

# 1. The version in Cargo.toml (the first `version =` is the package's), and in Cargo.lock.
sed -i "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
cargo check --quiet

# 2. The changelog: a new heading under [Unreleased], and the links at the bottom kept in
#    order: [Unreleased] first, then newest to oldest.
awk -v version="$version" -v today="$today" -v repo="$repo" -v tag="$tag" -v previous="$previous" '
    function new_links() {
        print "[Unreleased]: " repo "/compare/" tag "...HEAD"
        if (previous != "") print "[" version "]: " repo "/compare/" previous "..." tag
        else print "[" version "]: " repo "/releases/tag/" tag
        done = 1
    }
    /^## \[Unreleased\]/ { print; print ""; print "## [" version "] - " today; next }
    /^\[Unreleased\]: / { next }
    /^\[[0-9][^]]*\]: / && !done { new_links() }
    { print }
    END { if (!done) { print ""; new_links() } }
' CHANGELOG.md > CHANGELOG.md.new
mv CHANGELOG.md.new CHANGELOG.md

# 3. Commit and tag.
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "chore: release $version"
git tag -a "$tag" -m "Whoopz $version"

echo "Made the release commit and tag $tag:"
git show --stat --format='  %h %s' HEAD | sed 1d
echo
echo "Check it, then publish with:"
echo "  git push && git push origin $tag"
