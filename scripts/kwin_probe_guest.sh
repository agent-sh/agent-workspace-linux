#!/usr/bin/env bash
set -euo pipefail

# This process is the container entry point. Do not invoke an acceptance test
# from an `if` condition: Bash would disable errexit in all nested functions.
python3 /opt/kwin-probe/kwin_smoke.py "$@"
exec python3 /opt/kwin-probe/kwin_click_smoke.py
