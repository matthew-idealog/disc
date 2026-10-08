# DISC 0.1.0-rc.21

**DISC Is a Subsonic Client**: a cross-platform terminal app for Subsonic-compatible music servers.

This release candidate packages the current DISC source tree as a flat Rust project:

```text
Cargo.toml
README.md
COMMANDS.md
CUSTOM_STYLES.md
BEGINNER_GUIDE.md
CHANGELOG.md
src/
```

## Build and run

```bash
cargo build --release
cargo run --release
```

The binary/package name is now `disc`:

```bash
disc
disc tui
disc server-list
disc ping
```

## First run

Start DISC with:

```bash
disc
```

If no servers are configured, the Home screen shows the setup hint. In the TUI, type:

```text
add-server
```

or use the shell command form:

```bash
disc server-add <name> <alias> <base_url> <username> <password> --primary
```

The TUI wizard accepts either a full server URL or a plain host name. If you enter an incomplete address such as `myserver.local`, DISC tries common Subsonic combinations such as `http://myserver.local:4040` and `https://myserver.local:4443` after your username and password are entered. If one responds to Subsonic `ping`, DISC saves that working full URL for you.

## Configuration location

DISC stores config in the platform config directory under `disc`:

- Windows: `%APPDATA%\\disc\\config.toml`
- macOS: `~/Library/Application Support/disc/config.toml`
- Linux: `~/.config/disc/config.toml`

For continuity with earlier test builds, if a legacy `subsonic_tui` config directory exists and the new `disc` config directory does not, DISC copies the existing config, saved queues, last session, and custom style file into the new location on startup.

## Main features in this RC

- Multiple Subsonic servers with one primary/home server.
- Bare commands target the primary server; prefixed commands target a named server alias. A server alias is a short nickname for a server that makes server-specific commands quicker to type, such as `sub rec` or `hel rnd 50`. `all <search-command>` searches across all configured servers and merges the results.
- DISC-branded Ratatui interface with top playback/status panel, paged main panel, optional bottom message panel, and numbered help topics.
- Search and browsing for recent albums, random albums/tracks, artists, albums, tracks, genres, playlists, general search, and all-server merged search.
- Result pages support `play <n...>` / `p <n...>` and `play *` / `p *` to append selected results to the queue and start at the first appended track.
- Wildcard support with `*` and `?`, including case-insensitive local matching where DISC filters results.
- Queue/playback support with add, remove, move, dedupe, sort, save/load queues, playback history, and session restore.
- Shuffle playback and stable shuffle-order modes, including `sh`, `so`, and `unsh`.
- Optional experimental gapless/preload handoff with `gap` / `gapless`.
- Audio endpoint recovery for Windows/RDP/device changes with `audio status`, `audio devices`, and `audio reset`; backend output-stream warnings are routed into the Messages panel.
- Playlist management and downloads.
- New installs default to Style 4 / `multi-soft`; editable custom style file support remains available with named colours, ANSI 256, hex RGB, `rgb(...)`, and `cmyk(...)` inputs.
- Optional media key / now-playing metadata support with `mk` / `media-keys`.

## All-server search

Use `all` before supported search/browse commands to query every configured server and display the merged results:

```text
all s deep
all al hazards
all ar rainbow
all tr love
all pl favourites
all rnd t 30 g folk
```

Multi-server results show the source alias at the end of each row, such as `[sub]`. In multi-colour styles, DISC also colours rows by source server to make merged lists easier to scan. Single-server searches do not add a redundant server suffix.

To queue and start all playable returned results immediately, add `--play`, `--p`, or `-p` to a search or browse command. By default this appends to the current play queue and starts at the first newly added track:

```text
all rnd t 30 g folk --play
all rnd t 30 g folk --p
all rnd t 30 g folk -p
```

To overwrite the current queue first, add `--replace --play`, `--rp`, or `-rp`:

```text
all rnd t 30 g folk --replace --play
all rnd t 30 g folk --rp
all rnd t 30 g folk -rp
```

On an existing result page, use `play` / `p` with numbers or `*` to append selected results to the current queue and start playback at the first appended track:

```text
play 1
p 1 3 5-7
p1
play *
p *
p*
```

`play *` appends rather than replaces. Use `-rp` on the original search when you want to replace the queue.

Use `kill` / `k` / `cancel` to cancel a currently running background search or browse request, then type the next command normally. This is intended for long-running server searches; it cannot interrupt a command that is already running synchronously inside the input handler.

### Queue skip grace

When playback is already active, `next`, `n`, PageDown, Right, `prev`, PageUp, and Left update the selected queue item immediately but wait about 120 ms before starting the newly selected track. Repeated skip keypresses reset that grace timer, so skipping from track 1 to track 11 should only buffer/play track 11 instead of briefly buffering tracks 2-10. Use `play` to start the selected queue item immediately.

## Help and command reference

Inside DISC:

```text
help
h
```

opens the numbered help index. Press `1`-`10` to open a topic, and use `[` / `]` or PageUp/PageDown to move between help topics.

For the beginner guide, see [`BEGINNER_GUIDE.md`](BEGINNER_GUIDE.md) or run `help beginner`. For the full reference, see [`COMMANDS.md`](COMMANDS.md) or run:

```text
help full
help commands
```

Custom style examples are in [`CUSTOM_STYLES.md`](CUSTOM_STYLES.md).

## Known RC caveats

- Gapless playback is experimental and switchable. Use `gap off` / `gapless off` to return to the safe single-track path.
- Media key support is optional and platform/session dependent. Use `mk off` if the OS integration causes problems.
- If Windows audio changes after RDP/local-login, Bluetooth, HDMI, USB DAC, or sleep/wake, use `audio status` to compare DISC's active output with the OS default output, `audio devices` to list outputs, and `audio reset` to reopen the current default output without restarting the app. Runtime audio backend warnings should appear in Messages rather than the command input panel.
- This package is the source release candidate. Platform binary archives should be built from this exact source once RC testing is complete.

## Release notes

See [`CHANGELOG.md`](CHANGELOG.md).


## Search progress and timeouts

DISC keeps the UI alive while supported remote search/browse commands are executing. The message/status panel shows elapsed time against the configured timeout, including all-server searches such as `all rnd t 30 g folk`.

Use `timeout` to inspect settings, `timeout 60` to set all servers to 60 seconds, or `timeout <server> <seconds>` for a per-server override.

The command line supports Left/Right editing while text is present, plus Home and End for start/end of the command.

### Queue edits during gapless playback

DISC invalidates and recomputes the prepared next-track plan whenever the play queue is edited. This keeps gapless/preloaded playback aligned with the visible queue after commands such as `r 5`, `move`, `sort queue`, `dedupe`, `clear-played`, `clear-upcoming`, `shuffle all`, `so refresh`, and `unsh`.

## Audio endpoint recovery

DISC opens an audio output stream when playback starts. On Windows, RDP can expose a temporary `Remote Audio` endpoint; when you later log in locally, the OS default output may change underneath the running process. If playback appears to advance but you hear no sound, try:

```text
audio status
audio devices
audio reset
```

`audio reset` drops the current sink, reopens the current OS default output device, clears stale audio errors, and if a track was active attempts to resume it at approximately the same position. Runtime audio backend warnings, including device-loss output-stream messages, are captured into the Messages panel instead of being allowed to write over the command input. Also check Windows **Settings -> System -> Sound -> Volume mixer** and make sure `disc.exe` is routed to the local speakers/headphones rather than an old RDP/remote endpoint.

## Cancelling long-running searches

Search and browse commands run as one background request at a time. Typing another remote search does **not** automatically tear down the previous request; DISC keeps the first one running and asks you to wait or cancel it. Use:

```text
kill
k
cancel
```

to abort the current background search/browse request and issue a fresh command. Local commands such as `status`, `queue`, playback controls, and `audio status` remain usable while a background search is running.
