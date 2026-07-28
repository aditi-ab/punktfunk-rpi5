#!/usr/bin/env bash
# Compat shim: this script was renamed to xcheck.sh when it learned to cross-check the LINUX
# backends too (pf-vdisplay's five compositor backends are as invisible to a Mac as the Windows
# half is). Forwards to the windows half, which is all this name ever meant.
exec "$(dirname "$(readlink -f "$0")")/xcheck.sh" windows "$@"
