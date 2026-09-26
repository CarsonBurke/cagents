(
  set -eu
  repository='github.com/CarsonBurke/cagents'
  gh auth status --hostname github.com
  tag="$(gh release view --repo "$repository" --json tagName --jq .tagName)"
  case "$tag" in v[0-9]*) ;; *) echo 'Expected a versioned release tag' >&2; exit 1 ;; esac
  version="${tag#v}"
  download_dir="$(mktemp -d)"
  trap 'rm -rf "$download_dir"' 0
  trap 'exit 1' HUP INT TERM
  archive="cagents-${version}-x86_64-unknown-linux-gnu.tar.gz"

  gh release download "$tag" --repo "$repository" \
    --pattern install.sh --pattern SHA256SUMS --pattern "$archive" \
    --dir "$download_dir"
  (
    cd "$download_dir"
    awk -v archive="$archive" \
      '$2 == "install.sh" { print; installer++ } $2 == archive { print; payload++ }
       END { exit(installer != 1 || payload != 1) }' \
      SHA256SUMS > selected-checksums
    sha256sum --strict --check selected-checksums
  )

  CAGENTS_REPOSITORY='CarsonBurke/cagents' \
  CAGENTS_ARCHIVE="$download_dir/$archive" \
  CAGENTS_CHECKSUMS="$download_dir/SHA256SUMS" \
    sh "$download_dir/install.sh" --version "$version"
)
