#!/usr/bin/env bash
# SPEC-MAP coverage report.
#
# Scans crates/*/tests/behavior/SPEC-MAP.md and counts ported/unported
# behavioral cases. Query-only; not a hard gate.
#
# Usage: scripts/spec-coverage.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
total_all=0
ported_all=0

while IFS= read -r map; do
  rel="${map#"$root"/}"
  total="$(awk -F'|' '/^\| *[0-9]+ *\|/ { n++ } END { print n + 0 }' "$map")"
  ported="$(awk -F'|' '
    /^\| *[0-9]+ *\|/ {
      status = $5
      gsub(/[[:space:]]/, "", status)
      if (status == "已移植") n++
    }
    END { print n + 0 }
  ' "$map")"
  unported="$((total - ported))"
  total_all="$((total_all + total))"
  ported_all="$((ported_all + ported))"
  printf '%-60s total=%3d ported=%3d unported=%3d\n' "$rel" "$total" "$ported" "$unported"
done < <(find "$root/crates" -path '*/tests/behavior/SPEC-MAP.md' -print | sort)

printf '\nTOTAL: %d cases, %d ported, %d unported\n' \
  "$total_all" "$ported_all" "$((total_all - ported_all))"
