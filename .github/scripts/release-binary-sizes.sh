#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

asset_dir=${1:?需要发布包所在目录}
shopt -s nullglob
archives=("${asset_dir}"/sidediff-*.tar.xz "${asset_dir}"/sidediff-*.zip)
if (( ${#archives[@]} == 0 )); then
  echo '没有找到发布包。' >&2
  exit 1
fi

cat <<'EOF'
<details>
<summary>二进制体积</summary>

按安装到目标设备后的可执行文件大小统计。

| 平台 | 架构 | 体积 |
| --- | --- | ---: |
EOF

for archive in "${archives[@]}"; do
  filename=${archive##*/}
  case "${filename}" in
    *.tar.xz)
      package=${filename%.tar.xz}
      bytes=$(tar -xOJf "${archive}" "${package}/sidediff" | wc -c)
      ;;
    *.zip)
      package=${filename%.zip}
      bytes=$(unzip -p "${archive}" "${package}/sidediff.exe" | wc -c)
      ;;
  esac
  target=${package#sidediff-}
  case "${target}" in
    *-unknown-linux-gnu) platform='Linux（GNU）' ;;
    *-apple-darwin) platform='macOS' ;;
    *-pc-windows-msvc) platform='Windows' ;;
    *) echo "无法识别发布平台：${target}" >&2; exit 1 ;;
  esac
  arch=${target%%-*}
  if [[ "${arch}" == aarch64 ]]; then arch=ARM64; fi
  awk -v platform="${platform}" -v arch="${arch}" -v bytes="${bytes}" \
    'BEGIN { printf "| %s | %s | %.2f MiB |\n", platform, arch, bytes / 1048576 }'
done

printf '\n</details>\n'
