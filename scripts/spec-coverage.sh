#!/usr/bin/env bash
# SPEC-MAP 覆盖率报告（story card A2）。
#
# 扫描 crates/*/tests/behavior/SPEC-MAP.md，统计已移植/未移植的行为用例数。
# 仅用于查询，不做硬性门槛（A2 验收标准 T2）。
#
# 用法: scripts/spec-coverage.sh
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
