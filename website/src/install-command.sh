(
  set -eu
  releases='https://github.com/CarsonBurke/cagents/releases'
  latest="$(curl -fsSL --proto '=https' -o /dev/null -w '%{url_effective}' "$releases/latest")"
  tag="${latest##*/}"
  case "$tag" in v[0-9]*) ;; *) echo 'Expected a versioned release tag' >&2; exit 1 ;; esac
  version="${tag#v}"
  download_dir="$(mktemp -d)"
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  archive="cagents-${version}-x86_64-unknown-linux-gnu.tar.gz"

  (
    cd "$download_dir"
    curl -fsSL --proto '=https' --remote-name-all \
      "$releases/download/$tag/install.sh" "$releases/download/$tag/SHA256SUMS" \
      "$releases/download/$tag/$archive"
    awk -v archive="$archive" \
      '$2 == "install.sh" { print; installer++ } $2 == archive { print; payload++ }
       END { exit(installer != 1 || payload != 1) }' \
      SHA256SUMS > selected-checksums
    sha256sum --strict --check selected-checksums
  )

  CAGENTS_ARCHIVE="$download_dir/$archive" \
  CAGENTS_CHECKSUMS="$download_dir/SHA256SUMS" \
    sh "$download_dir/install.sh" --version "$version"
)
