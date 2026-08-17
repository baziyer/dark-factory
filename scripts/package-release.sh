#!/bin/sh
# Package release binaries the way `.github/workflows/release.yml` publishes
# them and `factoryctl update --install` consumes them:
#
#   scripts/package-release.sh <tag> <target> <bin-dir> <out-dir> [<owner/repo>]
#
# Writes into <out-dir>:
#   dark-factory-<tag>-<target>.tar.gz   the four binaries, flat (no directory)
#   SHA256SUMS                           one line per asset
#   latest.json                          {version, tag, assets: {<target>: {url, sha256}}}
#
# `latest.json` is what `factoryctl update` reads via
# https://github.com/<owner/repo>/releases/latest/download/latest.json, so
# the asset URL inside it points at the same release's download path.
set -eu

tag="${1:?tag}"; target="${2:?target}"; bin_dir="${3:?bin-dir}"; out_dir="${4:?out-dir}"
repository="${5:-${GITHUB_REPOSITORY:-baziyer/dark-factory}}"
version="${tag#v}"
archive="dark-factory-$tag-$target.tar.gz"

for binary in factoryd factory-runner factoryctl factory-tui; do
    [ -x "$bin_dir/$binary" ] || { echo "missing executable: $bin_dir/$binary" >&2; exit 1; }
done

mkdir -p "$out_dir"
tar -czf "$out_dir/$archive" -C "$bin_dir" factoryd factory-runner factoryctl factory-tui
sha256=$(shasum -a 256 "$out_dir/$archive" | cut -d' ' -f1)
printf '%s  %s\n' "$sha256" "$archive" > "$out_dir/SHA256SUMS"
cat > "$out_dir/latest.json" <<JSON
{
  "version": "$version",
  "tag": "$tag",
  "assets": {
    "$target": {
      "url": "https://github.com/$repository/releases/download/$tag/$archive",
      "sha256": "$sha256"
    }
  }
}
JSON
echo "packaged $out_dir/$archive ($sha256)"
