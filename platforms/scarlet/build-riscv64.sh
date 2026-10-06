#!/bin/sh
set -eu
exec bash "$(dirname "$0")/install" riscv64gc-unknown-scarlet "$@"
