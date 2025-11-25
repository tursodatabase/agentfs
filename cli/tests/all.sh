#!/bin/sh

DIR="$(dirname "$0")"

"$DIR/test-init.sh"
"$DIR/ls.sh"
"$DIR/test-syscalls.sh"
"$DIR/test-run-mount-flag.sh"
"$DIR/test-run-bash.sh"
"$DIR/test-mount.sh"
