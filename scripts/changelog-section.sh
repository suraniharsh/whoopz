#!/bin/sh
# Print one version's section of CHANGELOG.md, without its heading.
#
#   scripts/changelog-section.sh 0.1.0
#   scripts/changelog-section.sh Unreleased
#
# Used for the GitHub release notes, so they always say the same as the changelog.
set -eu
version=$1
awk -v want="$version" '
    /^## \[/ {
        if (inside) exit
        if (index($0, "## [" want "]") == 1) { inside = 1; next }
    }
    /^\[[^]]+\]: / { if (inside) exit }
    inside { lines[++n] = $0 }
    END {
        first = 1; while (first <= n && lines[first] == "") first++
        last = n;  while (last >= first && lines[last] == "") last--
        for (i = first; i <= last; i++) print lines[i]
    }
' "$(dirname "$0")/../CHANGELOG.md"
