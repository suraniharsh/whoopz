#!/bin/sh
# Check that a whoops binary works on other Linux distributions, using Docker.
#
#   scripts/portable-check.sh target/x86_64-unknown-linux-musl/release/whoops [image…]
#
# In each image, as a normal user: delete, move and overwrite files inside `whoops shell`,
# then undo, and check everything came back. None of the images has libseccomp, and Alpine
# has no glibc, so this proves the binary needs nothing from the system it runs on.
set -eu
binary=$(realpath "$1"); shift
images=${*:-ubuntu:20.04 debian:12 fedora:latest alpine:latest}

inner='
set -e
export HOME=/tmp/h XDG_RUNTIME_DIR=/tmp/r SHELL=/bin/sh
mkdir -p $HOME $XDG_RUNTIME_DIR /tmp/w && cd /tmp/w
echo keep > f; echo g > g; echo old > k
/w/whoops shell -- -c "rm f; mv g h; echo new > k; /w/whoops"
test "$(cat f)" = keep && test -e g && test ! -e h && test "$(cat k)" = old
'
failed=0
for image in $images; do
    if docker run --rm --network none -u 1000:1000 -v "$binary:/w/whoops:ro" "$image" \
        sh -c "$inner" >/dev/null 2>&1; then
        echo "ok      $image"
    else
        echo "FAILED  $image"; failed=1
    fi
done
exit $failed
