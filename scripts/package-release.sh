#!/bin/sh
# Package both supported macOS release targets in one all-or-nothing step:
#
#   scripts/package-release.sh <tag> <out-dir> <owner/repo> \
#     aarch64-apple-darwin <arm-bin-dir> \
#     x86_64-apple-darwin <intel-bin-dir>
#
# The output directory must not exist. Everything is built in a sibling
# staging directory and renamed into place only after both archives, their
# checksums, the shared update manifest, and the tap formula candidate exist.
# The result is two `dark-factory-<tag>-<target>.tar.gz` files plus
# `SHA256SUMS`, `latest.json`, and `dark-factory.rb`.
set -eu

tag="${1:-}"
out_dir="${2:-}"
repository="${3:-}"
if [ "$#" -ne 7 ] || [ -z "$tag" ] || [ -z "$out_dir" ] || [ -z "$repository" ]; then
    echo "usage: scripts/package-release.sh <tag> <out-dir> <owner/repo> <target> <bin-dir> <target> <bin-dir>" >&2
    exit 1
fi
shift 3

case "$tag" in
    v[0-9]*) ;;
    *) echo "release tag must start with v and a digit: $tag" >&2; exit 1 ;;
esac
case "$tag" in
    *[!A-Za-z0-9._-]*) echo "release tag has unsupported characters: $tag" >&2; exit 1 ;;
esac
case "$repository" in
    */*) ;;
    *) echo "repository must be owner/repo" >&2; exit 1 ;;
esac
repository_owner=${repository%%/*}
repository_name=${repository#*/}
case "$repository_owner" in
    "" | *[!A-Za-z0-9_.-]*)
        echo "repository must be owner/repo" >&2
        exit 1
        ;;
esac
case "$repository_name" in
    "" | */* | *[!A-Za-z0-9_.-]*)
        echo "repository must be owner/repo" >&2
        exit 1
        ;;
esac

arm_dir=
intel_dir=
while [ "$#" -gt 0 ]; do
    target=$1
    bin_dir=$2
    shift 2
    case "$target" in
        aarch64-apple-darwin)
            [ -z "$arm_dir" ] || { echo "duplicate release target: $target" >&2; exit 1; }
            arm_dir=$bin_dir
            ;;
        x86_64-apple-darwin)
            [ -z "$intel_dir" ] || { echo "duplicate release target: $target" >&2; exit 1; }
            intel_dir=$bin_dir
            ;;
        *) echo "unsupported release target: $target" >&2; exit 1 ;;
    esac
done
[ -n "$arm_dir" ] && [ -n "$intel_dir" ] || {
    echo "both aarch64-apple-darwin and x86_64-apple-darwin are required" >&2
    exit 1
}

for bin_dir in "$arm_dir" "$intel_dir"; do
    for binary in factoryd factory-runner factoryctl factory-tui; do
        [ -x "$bin_dir/$binary" ] || {
            echo "missing executable: $bin_dir/$binary" >&2
            exit 1
        }
    done
done

if [ -e "$out_dir" ] || [ -L "$out_dir" ]; then
    echo "release output already exists: $out_dir" >&2
    exit 1
fi
out_parent=$(dirname "$out_dir")
mkdir -p "$out_parent"
staging=$(mktemp -d "$out_parent/.dark-factory-package.XXXXXX")
cleanup=$staging
trap '[ -z "$cleanup" ] || rm -rf "$cleanup"' EXIT HUP INT TERM

package_target() {
    package_target_name=$1
    package_bin_dir=$2
    package_archive="dark-factory-$tag-$package_target_name.tar.gz"
    COPYFILE_DISABLE=1 tar --options gzip:!timestamp -czf "$staging/$package_archive" \
        -C "$package_bin_dir" factoryd factory-runner factoryctl factory-tui
    package_sha=$(shasum -a 256 "$staging/$package_archive" | cut -d' ' -f1)
    printf '%s  %s\n' "$package_sha" "$package_archive" >>"$staging/SHA256SUMS"
}

: >"$staging/SHA256SUMS"
package_target aarch64-apple-darwin "$arm_dir"
package_target x86_64-apple-darwin "$intel_dir"

arm_archive="dark-factory-$tag-aarch64-apple-darwin.tar.gz"
intel_archive="dark-factory-$tag-x86_64-apple-darwin.tar.gz"
arm_sha=$(awk -v name="$arm_archive" '$2 == name { print $1 }' "$staging/SHA256SUMS")
intel_sha=$(awk -v name="$intel_archive" '$2 == name { print $1 }' "$staging/SHA256SUMS")
version=${tag#v}
cat >"$staging/latest.json" <<JSON
{
  "version": "$version",
  "tag": "$tag",
  "assets": {
    "aarch64-apple-darwin": {
      "url": "https://github.com/$repository/releases/download/$tag/$arm_archive",
      "sha256": "$arm_sha"
    },
    "x86_64-apple-darwin": {
      "url": "https://github.com/$repository/releases/download/$tag/$intel_archive",
      "sha256": "$intel_sha"
    }
  }
}
JSON

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$script_dir/render-homebrew-formula.sh" "$tag" "$staging/SHA256SUMS" "$repository" \
    >"$staging/dark-factory.rb"

[ ! -e "$out_dir" ] && [ ! -L "$out_dir" ] || {
    echo "release output appeared while packaging: $out_dir" >&2
    exit 1
}
mv "$staging" "$out_dir"
cleanup=
echo "packaged $out_dir for aarch64-apple-darwin and x86_64-apple-darwin"
