#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
packager="$repository_root/scripts/package-release.sh"
renderer="$repository_root/scripts/render-homebrew-formula.sh"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-package-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "package-release test failed: $*" >&2
    exit 1
}

make_binaries() {
    target=$1
    directory=$2
    mkdir -p "$directory"
    for binary in factoryd factory-runner factoryctl factory-tui; do
        printf '#!/bin/sh\nprintf "%%s\\n" "%s %s 1.2.3"\n' "$binary" "$target" >"$directory/$binary"
        chmod +x "$directory/$binary"
    done
}

arm_dir="$temporary/arm"
intel_dir="$temporary/intel"
make_binaries arm "$arm_dir"
make_binaries intel "$intel_dir"
chmod 0700 "$arm_dir/factoryd"
chmod 0711 "$intel_dir/factory-tui"
output="$temporary/dist"
"$packager" v1.2.3 "$output" example/project \
    x86_64-apple-darwin "$intel_dir" \
    aarch64-apple-darwin "$arm_dir"

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    archive="$output/dark-factory-v1.2.3-$target.tar.gz"
    [ -f "$archive" ] || fail "missing $target archive"
    listing=$(tar -tzf "$archive" | LC_ALL=C sort)
    [ "$listing" = "factory-runner
factory-tui
factoryctl
factoryd" ] || fail "$target archive has unexpected contents: $listing"
    gzip_mtime=$(od -An -tu1 -j4 -N4 "$archive" | tr -d '[:space:]')
    [ "$gzip_mtime" = "0000" ] || fail "$target archive embeds its packaging time"
    LC_ALL=C tar -tvzf "$archive" | awk '
      $1 != "-rwxr-xr-x" || $2 != 0 || $3 != "root" || $4 != "wheel" ||
        $6 != "Jan" || $7 != 1 || $8 != 2000 { exit 1 }
      END { exit NR == 4 ? 0 : 1 }
    ' || fail "$target archive metadata is not normalized"
done
(cd "$output" && shasum -a 256 -c SHA256SUMS >/dev/null) || fail "archive checksums failed"

ruby -rjson -e '
  manifest = JSON.parse(File.read(ARGV.fetch(0)))
  abort "version" unless manifest["version"] == "1.2.3"
  abort "tag" unless manifest["tag"] == "v1.2.3"
  expected = %w[aarch64-apple-darwin x86_64-apple-darwin]
  abort "keys" unless manifest.fetch("assets").keys.sort == expected
  expected.each do |target|
    asset = manifest.fetch("assets").fetch(target)
    abort "url" unless asset.fetch("url").end_with?("dark-factory-v1.2.3-#{target}.tar.gz")
    abort "sha" unless asset.fetch("sha256").match?(/\A[0-9a-f]{64}\z/)
  end
' "$output/latest.json" || fail "manifest is not the exact two-target shape"

formula="$output/dark-factory.rb"
ruby -c "$formula" >/dev/null || fail "formula is not valid Ruby"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    grep -Fq "dark-factory-v1.2.3-$target.tar.gz" "$formula" \
        || fail "formula omitted $target"
done
for binary in factoryd factory-runner factoryctl factory-tui; do
    grep -Fq "$binary" "$formula" || fail "formula omitted $binary"
done
grep -Fq 'resource("binaries").stage' "$formula" || fail "formula does not install its selected resource"
grep -Fq 'on_arm do' "$formula" || fail "formula omitted the arm architecture block"
grep -Fq 'on_intel do' "$formula" || fail "formula omitted the Intel architecture block"
if grep -Fq 'Hardware::CPU' "$formula"; then
    fail "formula bypasses Homebrew architecture blocks"
fi
grep -Fq 'factoryctl update --install' "$formula" || fail "formula omitted runtime updater"
grep -Fq 'Do not use `brew services`' "$formula" || fail "formula permits competing service ownership"
grep -Fq '`brew uninstall dark-factory` removes only the bootstrap commands' "$formula" \
    || fail "formula hides bootstrap-only uninstall behavior"
grep -Fq 'launchd job, active runtime, and state under ~/.dark-factory remain' "$formula" \
    || fail "formula hides state retained by brew uninstall"
grep -Fq 'launchd/README.md#uninstall' "$formula" || fail "formula omits safe removal guidance"
if grep -Eq '^[[:space:]]*service do' "$formula"; then
    fail "formula defines a Homebrew service"
fi

second_formula="$temporary/second.rb"
"$renderer" v1.2.3 "$output/SHA256SUMS" example/project >"$second_formula"
cmp -s "$formula" "$second_formula" || fail "formula rendering is not deterministic"

# Reversing target order and changing only every source mtime cannot change
# any published byte. The packager owns one canonical archive representation.
TZ=UTC0 touch -t 203001010000.00 "$arm_dir"/*
TZ=UTC0 touch -t 204001010000.00 "$intel_dir"/*
second_output="$temporary/second-dist"
"$packager" v1.2.3 "$second_output" example/project \
    aarch64-apple-darwin "$arm_dir" \
    x86_64-apple-darwin "$intel_dir"
for artifact in \
    dark-factory-v1.2.3-aarch64-apple-darwin.tar.gz \
    dark-factory-v1.2.3-x86_64-apple-darwin.tar.gz \
    SHA256SUMS latest.json dark-factory.rb
do
    cmp -s "$output/$artifact" "$second_output/$artifact" \
        || fail "target order or source mtime changed $artifact"
done

# Validation finishes before the output transaction begins. A missing binary
# in either architecture leaves no partial output or staging directory.
rm "$intel_dir/factory-tui"
failed_output="$temporary/failed-dist"
if "$packager" v1.2.3 "$failed_output" example/project \
    aarch64-apple-darwin "$arm_dir" \
    x86_64-apple-darwin "$intel_dir" >"$temporary/failed.out" 2>"$temporary/failed.err"
then
    fail "incomplete Intel build was packaged"
fi
[ ! -e "$failed_output" ] || fail "failed package exposed a partial output"
if find "$temporary" -maxdepth 1 -name '.dark-factory-package.*' | grep -q .; then
    fail "failed package left a staging directory"
fi

# A complete output is immutable input to release publication; a rerun must
# not replace it with different binaries under the same tag.
if "$packager" v1.2.3 "$output" example/project \
    aarch64-apple-darwin "$arm_dir" \
    x86_64-apple-darwin "$arm_dir" >"$temporary/repack.out" 2>"$temporary/repack.err"
then
    fail "existing release output was overwritten"
fi
(cd "$output" && shasum -a 256 -c SHA256SUMS >/dev/null) \
    || fail "refused rerun changed the existing output"

echo "package-release tests passed"
