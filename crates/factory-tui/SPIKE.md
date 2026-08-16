# factory-tui: ratatui operator UI — terminal-pane fidelity spike

**Verdict: GO, with caveats.** `tui-term` (a ratatui widget over the `vt100` crate) faithfully
renders both `claude`'s and `codex`'s real interactive TUIs — colors (both 256-indexed and
24-bit truecolor), box drawing, cursor position, alt-screen, resize/reflow, and app-level
navigation all worked correctly in testing. Idle CPU is ~0%, memory is a few MB, and no hangs or
screen garbage were observed, including a code path (Kitty graphics protocol) that neither
`vt100` nor we support at all. The caveats are real but bounded: no mouse forwarding, no Kitty
keyboard protocol passthrough to children, no synchronized-output support, and scrollback isn't
exposed by the widget. None of these blocked normal interactive use of either app. See "GO
recommendation" at the end for the full list and effort estimates.

## MSRV (do not bump the toolchain — reporting per the brief)

The workspace is pinned to `rust-version = "1.85"`. The newest crates.io releases of two of the
requested crates need a newer compiler:

| Crate | Latest on crates.io | Needs | Used instead |
|---|---|---|---|
| `ratatui` | 0.30.2 | rustc 1.88.0 | **0.29.0** |
| `tui-term` | 0.3.4 | rustc 1.86.0 | **0.2.0** |

`vt100` 0.16.2, `portable-pty` 0.9.0, and `crossterm` 0.28-0.29 all build fine on 1.85.
`cargo add --dry-run` against `+1.85.0` is what surfaced these (it prints the exact "requires
rustc X" reason when it silently downgrades). Nothing in this crate requires a newer compiler
than what `ratatui 0.29` / `tui-term 0.2` already require.

**A second, related decision:** `factory-tui`'s `Cargo.toml` does **not** depend on `crossterm`
or `vt100` directly, even though the brief's crate landscape lists them. Both `ratatui` and
`tui-term` re-export the exact versions they were built against (`ratatui::crossterm`,
`tui_term::vt100`) specifically so downstream crates don't end up with a second,
version-mismatched copy in the dependency tree. Adding `vt100 = "0.16.2"` directly, for example,
produces a hard resolver conflict: it wants `unicode-width ^0.2.1` while `ratatui 0.29` pins
`unicode-width =0.2.0` exactly. Using `tui_term::vt100` (which resolves to `0.15.2`, whatever
`tui-term 0.2.0` actually depends on) sidesteps this entirely. Everywhere in this crate that
would `use vt100::...` or `use crossterm::...` instead does `use tui_term::vt100;` /
`use ratatui::crossterm;`.

## What's here

- `src/main.rs` — CLI args, terminal setup/teardown (raw mode, alt screen, bracketed paste),
  panic hook, the ~30fps-ceiling/dirty-flag-driven event loop.
- `src/app.rs` — two-pane layout, focus/zoom, the prefix-key mode toggle, rendering, cursor
  placement.
- `src/pane.rs` — one PTY-backed child per pane: spawn, reader thread, resize, query responder
  wiring.
- `src/keys.rs` — crossterm `KeyEvent` → raw terminal bytes (the encoder the brief asked for
  explicit unit tests on; 15 tests).
- `src/query.rs` — answers the handful of terminal queries `claude`/`codex` actually send (12
  tests).

Run it: `cargo +1.85.0 run -p factory-tui` (real `claude`/`codex`) or `--shell` (bash/vim, for
cheap iteration) or `--left <cmd>` / `--right <cmd>` to override either side. `--debug-log <dir>`
logs each pane's raw PTY bytes for ~6s after spawn, which is how the "Terminal-query handling"
section below was produced.

## Architecture: why tui-term, not a custom widget

`tui-term::widget::PseudoTerminal` takes a `&vt100::Screen` and renders it into a ratatui `Rect`
in one call, including an overlay cursor glyph. It proved adequate throughout testing — no
rendering bug traced back to `tui-term` itself; every fidelity gap traced back to either
`vt100` not tracking something (see caveats) or to us not wiring a capability up (mouse, Kitty
protocol). Given that, writing a custom `vt100`-backed widget would only reproduce what
`tui-term` already does correctly, for no fidelity gain — **not recommended.** The one place we
went around `tui-term`'s API rather than through it: real hardware-cursor placement (see
"Cursor" below) — `PseudoTerminal` only draws a synthetic overlay glyph, so for the *focused*
pane we additionally compute the true cursor cell ourselves from `vt100::Screen::cursor_position`
and call `frame.set_cursor_position`, so the terminal's real cursor (blink, IME, shape) tracks
the focused pane natively.

## Input routing

### Prefix key: `Ctrl-]`

Chosen for the reasons stated in the brief's suggestion: it's the classic telnet/rlogin escape
character (existing operator muscle memory for "control the wrapper, not the thing inside it"),
it's one byte with no Alt/multi-key ambiguity, and neither target app nor common shells/editors
bind it to anything reachable in normal use. The mode model is a **toggle**, not a
press-then-one-command tmux-style prefix: `Ctrl-]` flips between `Pane` mode (default — keys
forwarded to the focused child) and `Control` mode (keys interpreted as app commands: `Tab`/`h`/
`l` change focus, `z` zooms, `q` quits, `Esc` returns to `Pane` mode). This was chosen over a
single-shot prefix because the brief's spec ("toggles between keys-go-to-pane and
keys-go-to-app") reads as a persistent mode, and it tested well: the status line always shows
which mode you're in, so there's no ambiguity about where the next keystroke goes.

**A real bug this surfaced, worth flagging loudly:** without Kitty keyboard protocol
negotiation, crossterm's legacy Unix decoder maps the raw control bytes 0x1C-0x1F to
`KeyCode::Char('4'..'7') + CONTROL`, **not** `Char('\\'/']'/'^'/'_') + CONTROL` (confirmed by
reading `crossterm::event::sys::unix::parse::parse_event` in the vendored source, then
empirically: `PREFIX_KEY` built as `Char(']') + CONTROL` silently never matched a real `Ctrl-]`
keypress in tmux — the keystroke fell through to `forward_key`, and since `']'` wasn't in
`ctrl_byte()`'s table, it forwarded the literal character `'5'` to the child). Physical `Ctrl-]`
decodes to `Char('5') + CONTROL` under this crossterm version's legacy path. `app::PREFIX_KEY`
and `keys::ctrl_byte` are both written against this now (with tests: `keys::tests::
ctrl_4_5_6_7_map_to_the_0x1c_0x1f_control_bytes`), and it was re-verified live afterward (see
"Verification log" below). This is exactly the "Escape vs Alt-key ambiguity" class of problem
the brief flagged — it just showed up one byte range over. It's also a good illustration of why
round-trip correctness (byte in == byte out) matters more than what name crossterm gives a key:
our encoder doesn't need to "know" this byte range means `]` to a human, it just needs to send
back what it received.

We deliberately do **not** negotiate the Kitty keyboard protocol on our own (outer) terminal.
Doing so would let crossterm report unambiguous press/release/repeat events with exact
modifiers — a real fidelity improvement — but roughly doubles the encoder's surface area for a
spike, and neither target app depends on it being on. Flagged as a follow-up in "GO
recommendation".

### Byte-for-byte re-encoding (`keys.rs`)

Enter → `\r` (not `\n`); Backspace → `0x7f` (matches `TERM=xterm-256color`'s terminfo, not
`0x08`); Escape → single `0x1b` (crossterm already resolved the Escape-vs-Alt-prefix ambiguity
using its own read-timing heuristic before we ever see the event, so we don't need to
re-implement that); Tab/Shift-Tab, arrows/Home/End (honoring the child's application-cursor mode
via `vt100::Screen::application_cursor`, tracked live per pane), PageUp/PageDown/Insert/Delete,
F1-F12 (SS3 for unmodified F1-F4 matching xterm's default terminfo, `CSI ~` for F5-F12 and any
modified F-key), Ctrl-letter and the recognized Ctrl-punctuation forms, Alt-anything (`ESC`
prefix). 27 unit tests in `keys.rs`, all passing (see "Verification log").

### Bracketed paste

`crossterm::event::Event::Paste(text)` is forwarded wrapped in `ESC[200~...ESC[201~` **iff**
`vt100::Screen::bracketed_paste()` says the focused child currently wants it (both `claude` and
`codex` request it on startup). Getting a clean empirical test of this took two tries and
produced the incident described below — the short version: it works correctly end-to-end,
confirmed against `bash` with `tmux paste-buffer -p`, which showed all three pasted lines landing
in one unsubmitted readline buffer rather than three separately-executed commands.

## Resize

`App::sync_pty_sizes` runs at the top of every `terminal.draw()` closure, using the frame's
*actual* area (post any resize the backend already picked up) — computes both pane rects
(halved, or full-screen if zoomed), subtracts the 1-cell border via `Block::inner`, and calls
`Pane::resize` (a no-op if the size didn't change), which resizes the PTY master (delivering
SIGWINCH) and calls `vt100::Parser::set_size`.

**Verified with real content, not just a blank pane:** resizing a live `factory-tui` session
(claude+codex) from 200x50 down to 140x40 caused `claude` to genuinely reflow its layout — not
just clip: the "Tips for getting started" side panel it shows next to the welcome box at 200
columns was *removed entirely* at 140 columns (the app decided there wasn't room for two
columns), and the welcome box border/ASCII-art logo re-centered to the new width. That's an
app-level relayout decision reacting to a real SIGWINCH, not a rendering artifact.

## Terminal-query handling

`vt100::Parser` only *models* terminal state — confirmed by reading its source
(`vte::Perform` impl in `screen.rs`): there is no code path that writes a reply back to the
child for `hook`/`osc_dispatch`/etc. Without something answering, both apps are talking to a
terminal that never responds to capability queries.

**What they actually send** (captured via a standalone `portable-pty` probe first, then
cross-checked against `factory-tui`'s own `--debug-log` output — both agree):

| Query | `claude` | `codex` |
|---|:---:|:---:|
| DA1 (`CSI c`) | yes | yes |
| XTVERSION (`CSI > 0 q`) | yes | no |
| CPR (`CSI 6 n`) | no | yes |
| Kitty keyboard query (`CSI ? u`) | no | yes |
| OSC 10/11 (fg/bg color `?`) | no | yes (both) |
| Kitty keyboard *push* (`CSI > 1 u` / `CSI > 5 u`), no reply expected | yes | yes |
| `modifyOtherKeys` set (`CSI > 4 ; N m`), no reply expected | yes | yes |

Neither app blocked its initial render waiting for a reply to any of these — both apps drew their
full startup UI regardless of whether the query was ever answered (confirmed by capturing a
window *before* `query.rs` existed too, during the standalone-probe phase). That's an important,
reassuring finding for the GO call: even a fully mute terminal doesn't hang either app. But
fidelity is visibly better with real answers (confirmed cursor position for `codex`'s CPR use;
codex's OSC 10/11 query implies it wants to theme itself off the real terminal background, which
an unanswered query can't give it).

`query.rs` answers all seven query *types* above (DA1, XTVERSION, CPR, Kitty-flags, OSC 10, OSC
11 — the two Kitty/modifyOtherKeys *sets* need no reply, so there's nothing to answer). It's a
small hand-rolled incremental byte scanner, not a full parser — deliberately, since `vt100`
already owns full escape-sequence parsing and duplicating that would be wasteful. It handles a
query split across two PTY `read()`s (tested) and stays cheap against a large non-matching blob
(also tested — see next paragraph) because only bytes that could still be a prefix of a
recognized pattern are ever retained.

**The Kitty graphics protocol finding:** `codex` sends a ~600KB APC-framed (`ESC _ G ... ESC \`)
base64 image blob a couple seconds after startup — its splash logo, transmitted as a real raster
image on the assumption the terminal might support Kitty graphics. We don't, and don't try to.
The concerning question was whether `vt100` would garbage that onto the screen as literal text.
It does not: `vte`'s state machine (which `vt100` is built on) treats APC as a
"consume-until-terminator, no callbacks" state (confirmed in `vte-0.11.1/src/table.rs`/
`definitions.rs` — `SosPmApcString` → `Action::Unhook` is a no-op), so the entire blob is
silently and correctly dropped. No garbage was observed on screen in testing (visually confirmed:
`codex`'s pane rendered its normal, clean composer UI throughout). The image itself never
appears, but nothing breaks either.

Replies sent: DA1 → `\x1b[?1;2c` (conservative VT100-with-AVO — both apps appear to use the reply
only as a completion sentinel, not for detailed capability parsing, given neither blocked without
one); XTVERSION → a DCS identifying `factory-tui`; CPR → the pane's real cursor position from
`vt100::Screen::cursor_position` (1-indexed per the CPR spec); Kitty query → `\x1b[?0u` ("no
enhancements supported", so the app falls back to legacy encoding deterministically instead of
guessing from a timeout); OSC 10/11 → a fixed white-on-black pair (we don't track the *real*
outer terminal's palette in this spike, so this is a reasonable default, not an accurate readback
— flagged as a follow-up).

## Fidelity report

Tested against real `claude` (v2.1.233) and `codex` (v0.147.0) in a 200x50 host terminal
(`tmux`, for scriptable, reproducible key injection — see "Verification log"), plus `bash`/`vim`
for cheap, zero-cost iteration.

**Colors.** Both 256-indexed (`CSI 38;5;N`) and 24-bit truecolor (`CSI 38;2;r;g;b`) round-trip
correctly. Verified by capturing `factory-tui`'s own raw stdout (via `tmux pipe-pane`, which
captures what our process writes *before* tmux's own redisplay layer — avoids a `tmux
capture-pane -e` quirk that downsamples to 256-color) and finding exact RGB triples from
`codex`'s real output (e.g. `38;2;228;49;113`, its pink "gpt-5.6-luna" label) verbatim in what
we emit. Both color spaces appear because the apps themselves mix them (`claude`'s chrome uses
256-indexed colors for some border elements, truecolor for others) — we don't discriminate,
whatever the child sent comes through as sent.

**Box drawing / Unicode.** Correct throughout: `claude`'s ASCII-art logo (`▐▛███▜▌`), rounded
box borders (`╭─╮`/`╰─╯`), `codex`'s straight borders, braille-adjacent glyphs in status
indicators. No mis-rendered or missing glyphs observed.

**Cursor.** Position/visibility verified precisely, not just "looked right": with a bash pane
focused and the prompt read `bash-5.3$ ` (10 chars) inside a 1-cell border, the real terminal
hardware cursor (queried via `tmux display-message -p '#{cursor_x},#{cursor_y}'`) sat at exactly
`(11,1)` — pane origin + border + prompt length. Switching focus to the right pane moved it to
`(101,1)`, matching that pane's origin. Entering `Control` mode hides the real cursor entirely
(`cursor_flag=0`), a deliberate signal that keystrokes are no longer going to either pane. Shape
(blink/block/bar) isn't set explicitly by us — it inherits whatever the child's own cursor-shape
escape sequences (`DECSCUSR`) last set via `vt100`, or the terminal default; not separately
verified.

**Alt-screen entry/exit.** Confirmed via the raw byte capture: `claude` sends `CSI ?1049h` at
the point it switches from its startup/trust-prompt flow into the persistent chat UI, and (from
the earlier shell-mode `vim` test, which is simpler to fully exit) `CSI ?1049l` cleanly on quit,
after which the pane correctly reverts to showing blank primary-screen content. No leftover
alt-screen artifacts observed.

**Spinners / animation.** Not exhaustively exercised (no real prompt was sent — see hard rules —
so no extended "thinking" spinner ran). What *was* observed: `codex` continuously updates its
window title (OSC 0) with braille spinner frames (⠦⠧⠴⠼) during its startup burst, and `claude`'s
"auto mode on" indicator (`⏵⏵`) is present at idle. The render loop redraws whenever either
pane's dirty flag is set (i.e. whenever the child writes anything), capped at ~30fps, so any
animation the child drives should track at up to that rate; not fully verified with a
long-running spinner given the no-real-prompt constraint.

**Wrap/reflow after resize.** Confirmed with real content — see "Resize" above.

**Scrolling.** `vt100::Parser` keeps a scrollback buffer (constructed with 10,000 lines in
`Pane::spawn`), but `tui_term::widget::PseudoTerminal` only ever renders the *visible* screen —
there's no widget-level way to scroll the view into that scrollback. Neither app's own internal
scrolling (e.g. `claude`'s transcript, which likely manages its own virtual scroll and just
redraws the visible region) is affected by this, but an operator can't use terminal-level
scrollback (e.g. a mouse wheel or `Shift-PageUp`) to look back further than what's currently on
screen. Flagged as a follow-up.

**Mouse.** `claude` requests SGR mouse tracking on entering its main UI (`CSI ?1000h ?1002h
?1003h ?1006h` — click, button-drag, and full motion tracking, extended coordinate encoding).
`codex` was not observed requesting mouse modes in this testing. We receive
`crossterm::event::Event::Mouse` in the event loop but currently discard it — no forwarding
implemented. Given `claude` explicitly asks for motion tracking, this is a real, deliberate gap:
click-to-position-cursor in a multi-line composer, or any future mouse-driven UI, won't work
through us yet. Flagged as a follow-up with an effort estimate.

**Copy/paste (bracketed paste).** Works correctly — see "Input routing" above, and the incident
note below for how this was verified.

**Escape/arrow/ctrl handling.** Extensively exercised live: `vim` insert-mode entry/exit (`i`,
`Escape`), normal-mode motions (`0`, `dw`), `codex`'s `/model` picker (`Up`/`Up` correctly moved
the selection, `Escape` correctly canceled without applying a change), `claude`'s `/help` overlay
open/dismiss, `Ctrl-C` delivered to the focused child (not to us — confirmed `codex` exits
immediately on a single `Ctrl-C` from its idle prompt; `claude`/`bash` behave normally). All
correct.

**Hangs or garbage.** None observed anywhere in testing, including the Kitty-graphics-blob case
above (the single scenario most likely to produce garbage, given ~600KB of non-terminal binary
data arriving unannounced).

### CPU / memory / latency

Measured with `top -pid <pid> -s 1` against the real `claude`+`codex` session:

- **Idle** (both apps sitting at their composer, `--shell` mode's bash+vim too): 0.0-0.1% CPU,
  ~3.5-4.6MB RSS.
- **During `codex`'s startup burst** (the ~600KB Kitty-image transfer plus full-screen redraws,
  the heaviest sustained write either app produced in testing): still 0.0-0.1% CPU sampled at
  1s resolution. The dirty-flag/tick-based redraw loop (draw only when a pane actually wrote
  something, capped at ~30fps, plus a 250ms forced-redraw fallback) appears to coalesce bursts
  effectively rather than redrawing per-`read()`.
- **During a resize** (200x50 → 140x40 → 100x30, full reflow both panes): still 0.0-0.1% CPU
  sampled immediately after.
- **Input-to-echo latency:** not instrumented numerically. Typing into `bash`/`vim`/`claude`'s
  composer felt immediate in interactive use (tmux `send-keys` + `capture-pane` round trips
  consistently resolved within the ~200-300ms sleep windows used between test steps, which is a
  loose upper bound, not a real measurement). No perceptible input lag was observed.

One caveat on the CPU numbers: `vt100::Parser` doesn't understand mode 2026 (synchronized
output — both apps wrap their frame updates in `CSI ?2026h...CSI ?2026l`, confirmed in the raw
capture; `vt100` silently ignores the unrecognized private mode, verified against its explicit
mode-handling table in `screen.rs`, which has an unconditional fallthrough for unknown `CSI ?
Ps h/l`). Since our render loop batches all currently-available PTY bytes into the parser between
redraws rather than redrawing per `read()`, most of the visual benefit of synchronized output
comes through anyway (no partial-frame tearing was observed), but this isn't a guarantee for
pathological timing. Flagged as a follow-up.

## Incident: an accidental real prompt was sent to `claude` during testing

While testing bracketed paste against the real `claude` pane (not `--shell` mode), I used
`tmux paste-buffer` **without** its `-p` flag. Per `tmux`'s own docs, `-p` is required to
actually insert bracket control codes; without it, tmux converts embedded linefeeds in the
pasted buffer to literal carriage returns (Enter) before sending. The result: a 3-line paste
("line one" / "line two" / "line three") arrived at `claude` as three separately-submitted
lines. `claude` treated the first two as real, if fragmented, user messages, ran one read-only
shell command to inspect the worktree state, and replied acknowledging the fragmented input
without taking any action ("No task in the message so far, so I haven't changed anything").

This was a **test-harness mistake** (my `tmux paste-buffer` invocation, not a `factory-tui` bug)
— confirmed by reproducing the identical symptom against `bash` (zero cost: each line ran as a
separate, harmless, visibly-failed command) and then confirming the fix (`tmux paste-buffer -p`)
produces the correct behavior against both `bash` (all three lines land in one unsubmitted
readline buffer) and is architecturally the same code path `claude`'s pane uses. I stopped the
session immediately (`Escape`, cleared the unsubmitted third line with `Ctrl-U`, killed the
session) and confirmed via `git status --porcelain` that the shell command `claude` ran made no
working-tree changes — only my own known crate additions were present. Reported per the brief's
instruction to disclose anything I'm unsure about or that touched a hard rule; this did consume
one real, small `claude` turn that I did not intend to trigger.

## GO recommendation

**GO on `tui-term` + `vt100`** as the rendering layer for the real board. Concretely:

| Gap | Effort to close |
|---|---|
| Mouse forwarding (crossterm `Event::Mouse` → SGR mouse bytes to the focused child) | Small — same shape as `keys.rs`; `claude` already asks for it. |
| Kitty keyboard protocol passthrough (negotiate on our outer terminal, proxy flags/events to children that ask) | Medium — doubles `keys.rs`'s cases (press/release/repeat, exact modifiers) and needs per-child flag tracking. |
| Scrollback view (let the operator scroll a pane's `vt100` history, not just the live screen) | Small-medium — `vt100::Screen` exposes scrollback via `Parser`; needs a scroll-offset in `Pane` and a custom render path since `PseudoTerminal` doesn't support one. |
| Synchronized-output (mode 2026) awareness | Small — could gate redraws on `?2026l` if empirically needed; not needed so far. |
| Real OSC 10/11 answers (reflect the *actual* outer terminal's palette instead of a fixed guess) | Small — query the real terminal once at startup (same query-response mechanism, aimed outward instead of inward) and cache the answer. |

None of these blocked normal interactive use in testing. If the real board needs mouse-driven
interaction with `claude`'s composer specifically, prioritize that gap first; everything else is
cosmetic/completeness.

## For the next agent: intended layout for the real board

The owner's steer: this is a Dwarf Fortress-style operator view, not a dashboard. Left side is an
ASCII "floor" of agents as glyphs at their workstations (color/glyph = state: idle, working,
waiting-for-input, stopped, failed) plus a unit list (agents) and jobs list (task queue) with
single-key DF-style navigation; a scrolling announcements log is the daemon event feed; braille
sparklines show per-agent activity (tokens/turns/tool calls) over time. The terminal panes this
crate builds are what you get when you "look at" (zoom into) an agent — full-screen or split,
exactly the `z`/focus mechanics already implemented here.

```
┌ floor ─────────────────────────────┬ announcements ──────────────────┐
│  @alice (working)   @bob (idle)    │ 20:41 bob   task#42 done         │
│      [bench]           [desk]      │ 20:42 carol waiting for input    │
│                                     │ 20:44 alice tool: apply_patch    │
│  @carol (waiting) *  #queue: 3     │                                  │
│      [desk]                        │ ⣀⣠⣤⣴⣶⣾⣿ tokens/min (alice)      │
├ units ──────────┬ jobs ────────────┴──────────────────────────────────┤
│ >alice  working  │ #41 review PR    → alice                           │
│  bob    idle      │ #42 fix flaky    ✓ done                           │
│  carol  waiting   │ #43 spike TUI    → carol                          │
├─────────────────────────────────────────────────────────────────────┤
│ [zoomed: carol's terminal — codex, full-screen]                     │
│ (Ctrl-] control mode: Enter/z = zoom out, Tab = next agent)          │
└─────────────────────────────────────────────────────────────────────┘
```

Zooming (`z`) replaces the floor/log/lists with the full pane, same as this spike; splitting
shows two agents' panes side by side, same layout code as `app.rs::pane_rects`. Once the
daemon's PTY/attach protocol lands, `pane.rs`'s `Pane` should grow a second constructor that
attaches to a daemon-proxied stream by `run_id` over the existing Unix-socket client
(`crates/factoryctl/src/lib.rs::Client::subscribe`/`request`) instead of spawning a local
`portable-pty` child — the reader-thread/`vt100::Parser`/`QueryResponder` plumbing downstream of
"bytes arrived" doesn't need to change, only where the bytes come from and where keystrokes get
written back to.

## Verification log

- `CARGO_BUILD_JOBS=3 cargo +1.85.0 test -p factory-tui --all-targets -- --test-threads=1`:
  27 tests, all passing (15 key-encoding cases including the crossterm-quirk regression test,
  12 query-responder cases including split-read and large-non-matching-blob handling).
- `CARGO_BUILD_JOBS=3 cargo +1.85.0 clippy -p factory-tui --all-targets --all-features -- -D
  warnings`: clean.
- `cargo +1.85.0 fmt -p factory-tui -- --check`: clean.
- Interactive testing used `tmux` (sized 200x50) purely as a scriptable host terminal — it lets
  `tmux send-keys`/`capture-pane`/`paste-buffer`/`resize-window`/`pipe-pane`/`display-message`
  drive and inspect `factory-tui` deterministically without a human at a keyboard. All of
  `--shell` mode (bash/vim) and the real `claude`+`codex` mode were exercised this way, per the
  brief's cost rule: UI navigation only (`/help`, `/model`, arrows, `Escape`, `Ctrl-C`), no real
  prompt submitted intentionally (see the incident note for the one unintentional exception).
