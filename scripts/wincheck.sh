#!/usr/bin/env bash
# Compat shim: this script's former name. Forwards to xcheck.sh, which checks the Windows half.
exec "$(dirname "$(readlink -f "$0")")/xcheck.sh" "$@"
