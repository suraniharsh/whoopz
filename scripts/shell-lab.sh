#!/bin/sh
# The shell test lab: run the test suite inside containers that have many shells, in different
# versions. Needs Docker.
#
#   scripts/shell-lab.sh                    all images
#   scripts/shell-lab.sh arch               one image (a folder in tests/lab/)
#   scripts/shell-lab.sh arch fish          only tests whose name contains "fish"
#
# The tests are built as one static file, so the same binary runs in every image. The repo is
# mounted at the same path inside the container, so the paths baked into the tests still work.
set -eu
cd "$(dirname "$0")/.."

images=${1:-$(ls tests/lab)}
filter=${2:-}

echo "building the tests (static)…"
tests=$(cargo test --locked --target x86_64-unknown-linux-musl --no-run 2>&1 \
    | sed -n 's/.*Executable tests\/shell.rs (\(.*\))$/\1/p')
[ -n "$tests" ] || { echo "couldn't find the test binary" >&2; exit 1; }

failed=0
for image in $images; do
    tag="whoopz-lab-$image"
    echo
    echo "=== $image"
    # --network host: some machines can't give build containers their own network.
    if ! out=$(docker build --network host -q -t "$tag" "tests/lab/$image" 2>&1); then
        echo "$out" >&2
        exit 1
    fi
    docker run --rm --network none "$tag" sh -c '
        for s in bash zsh fish dash tcsh nu xonsh elvish; do
            command -v $s >/dev/null || continue
            v=$($s --version 2>/dev/null | head -1)
            echo "  $s: ${v:-installed}"
        done'
    if docker run --rm --network none -u "$(id -u):$(id -g)" \
        -v "$PWD:$PWD" -w /tmp -e HOME=/tmp "$tag" "$PWD/$tests" $filter; then
        echo "ok      $image"
    else
        echo "FAILED  $image"
        failed=1
    fi
done
exit $failed
