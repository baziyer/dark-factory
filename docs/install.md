# Manual installation

Homebrew is the shortest supported setup path; see the
[README](../README.md#install). Use this fallback when you need to install a
published release archive directly.

## Manual release archive

Install Git and sign in to at least one provider first. If you use a dedicated
`CODEX_HOME`, keep it set while running these commands. macOS supplies the
other required tools.

The script selects the archive for Apple silicon or Intel, reads its published
SHA-256 from `latest.json`, verifies the download, extracts all four commands,
and starts the normal guided install:

```sh
install_dir=$(mktemp -d /tmp/dark-factory-install.XXXXXX)
chmod 700 "$install_dir"
curl -fsSL https://github.com/baziyer/dark-factory/releases/latest/download/latest.json \
  -o "$install_dir/latest.json"

case "$(uname -m)" in
  arm64) platform_key=aarch64-apple-darwin ;;
  x86_64) platform_key=x86_64-apple-darwin ;;
  *) echo "unsupported macOS architecture: $(uname -m)" >&2; exit 1 ;;
esac

asset_url=$(plutil -extract "assets.$platform_key.url" raw -o - \
  "$install_dir/latest.json")
asset_sha=$(plutil -extract "assets.$platform_key.sha256" raw -o - \
  "$install_dir/latest.json")
curl -fL "$asset_url" -o "$install_dir/dark-factory.tar.gz"
printf '%s  %s\n' "$asset_sha" "$install_dir/dark-factory.tar.gz" | \
  shasum -a 256 -c -
tar -xzf "$install_dir/dark-factory.tar.gz" -C "$install_dir"
"$install_dir/factoryctl" init
```

Then add the active runtime to your shell and inspect the installation:

```sh
echo 'export PATH="$HOME/.dark-factory/bin/current:$PATH"' >> ~/.zprofile
source ~/.zprofile
factoryctl doctor
```

Downloads made with `curl` do not receive macOS browser quarantine. If you use
a browser instead, select the archive matching your Mac, verify its SHA-256
against `latest.json`, and extract it into one directory. If Gatekeeper blocks
the verified binaries, clear quarantine from that extracted directory before
running `factoryctl init`:

```sh
xattr -dr com.apple.quarantine /path/to/extracted/dark-factory
/path/to/extracted/dark-factory/factoryctl init
```
