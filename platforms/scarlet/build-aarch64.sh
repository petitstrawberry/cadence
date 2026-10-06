#!/bin/sh
set -eu
exec bash "$(dirname "$0")/install" aarch64-unknown-scarlet "$@"
