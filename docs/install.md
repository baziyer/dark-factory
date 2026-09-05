# Installation

The same three sibling binaries can come from a published macOS release or a
source build. The [development workflow](development/WORKFLOW.md) documents a
temporary `DARK_FACTORY_HOME` and explicit socket for isolated checks.

## Install a release

The release provides one archive for each supported macOS target:

- Apple silicon: `dark-factory-vX.Y.Z-aarch64-apple-darwin.tar.gz`
- Intel: `dark-factory-vX.Y.Z-x86_64-apple-darwin.tar.gz`

Download the matching archive from the
[GitHub releases](https://github.com/dark-factory-build/dark-factory/releases),
verify its entry in that release's `SHA256SUMS`, and put `factoryd`,
`factory-runner`, and `factoryctl` from the archive together on `PATH`. The
release's Homebrew formula installs the same commands if it has been added to a
tap.

Create and install one managed home. Those two commands are the whole terminal
side of setup: an install that starts a fresh service loads the launchd job,
waits for the daemon to listen, and opens <http://127.0.0.1:43123/pair> in this
machine's default browser, where confirming pairs that browser. A repeated
install returns the service it found and opens nothing. `service start` is the
explicit command to use after a later stop:

```sh
factoryctl init --home "$HOME/.dark-factory"
factoryctl service install --home "$HOME/.dark-factory"
```

To upgrade a running installation to a new build, run `factoryctl service
uninstall --home "$HOME/.dark-factory"` with the new `factoryctl`, then
`factoryctl service install --home "$HOME/.dark-factory"`. Only the service's
binaries, plist, and receipt are replaced; the data home is untouched. An
installation that used `--relay-origin` must repeat that flag on the install
after the uninstall, or the new job comes back loopback-only.

To reach the factory from the hosted PWA rather than only from this machine's
loopback, install with `factoryctl service install --home "$HOME/.dark-factory"
--relay-origin wss://relay.darkfactory.build`. The flag is optional and has no
default: omitting it installs a loopback-only job exactly as before. The
installed launchd job then starts `factoryd` with that `--relay-origin`, and the
service receipt records the exact argument list it was rendered from, so
`service status` and `service uninstall` reproduce that exact plist from the
receipt and keep proving ownership. Changing or removing the relay origin is
`factoryctl service uninstall` followed by a fresh install carrying the origin
you want: repeating an install with a different origin refuses, printing the
origin already installed, rather than silently keeping or dropping it. Pair a
phone from the console's PAIR A PHONE button.

Pairing never needs the terminal. Another browser on this machine pairs from the
console's PAIR THIS BROWSER link, which goes to
<http://127.0.0.1:43123/pair>; the daemon serves that one first-party page and
nothing else. Pairing again there is also how a browser replaces a saved
credential this daemon no longer accepts.

Inspection and revocation still run through the operator client, so those
commands need the socket and token exported:

```sh
export DARK_FACTORY_SOCKET="$HOME/.dark-factory/runtimes/factory.sock"
export DARK_FACTORY_OPERATOR_TOKEN_FILE="$HOME/.dark-factory/operator.token"
factoryctl web status
factoryctl web list-clients
factoryctl web revoke CLIENT_ID --revision REVISION
factoryctl remote status
```

The CLI cannot delete origin-scoped browser storage; pairing afresh from the
pair page makes that manual browser action unnecessary.

Create each agent with an explicit provider. `shell` needs no external tool;
`claude_code` and `codex` require the corresponding `claude` or `codex` CLI to
be installed and already signed in through its normal account workflow:

```sh
factoryctl agent create --project PROJECT_ID --name worker --provider shell --tool-budget 100
factoryctl agent create --project PROJECT_ID --name worker --provider claude_code --reasoning-effort medium --tool-budget 100
```

The managed daemon finds native tools on its fixed path and reuses the
operator's existing signed-in Claude or Codex account without copying a
credential into the Dark Factory home. See the [provider
contract](providers.md) for discovery, model, effort, and task-delivery details.

`factoryctl service stop` stops the managed daemon without removing the
installation; restart it with `factoryctl service start --home
"$HOME/.dark-factory"`. `factoryctl service uninstall` is the evidence-first
removal path for that exact home and label. Homebrew does not own the running
service; do not use `brew services` for Dark Factory.
