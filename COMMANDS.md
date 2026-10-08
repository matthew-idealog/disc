# DISC command reference

This reference covers the current DISC TUI and CLI command set. Commands are case-insensitive unless a value such as a queue name, playlist name, search term, or file path needs its original spelling.

## How command routing works

| Command form | Purpose |
|---|---|
| `<command>` | Runs against the primary/home Subsonic server where a server is needed. |
| `<alias> <command>` | Runs against a specific configured server. An alias is a short nickname for the server, for example `sub rec` or `hel rnd 50`. |
| `all <search-command>` | Runs supported search/browse commands against every configured server and merges the results, for example `all s deep`, `all al hazards`, or `all rnd t 30 g folk`. |
| numbers such as `1` | Act on the currently active list: results, queue, saved queues, or playback history. Single-number behaviour is unchanged. |
| ranges such as `1-5` or `8..4` | Select multiple items for commands such as `add`, `remove`, `dl`, `star`, and `unstar`. In results-like contexts, multiple bare selectors such as `1 3 5-7` also append to the play queue. |
| `<search-command> --play` / `<search-command> --p` / `<search-command> -p` | After displaying the results, append every playable returned item to the queue and start playback at the first newly added track. Works with single-server and `all` searches. |
| `<search-command> --replace --play` / `<search-command> --rp` / `<search-command> -rp` | After displaying the results, replace the queue with every playable returned item and start playback. |
| `play <n...>` / `p <n...>` / `play *` / `p *` on a result page | Append selected current results to the play queue and start playback at the first appended track. |
| `kill`, `k`, `cancel` | Cancel the currently running background search/browse request. Use this before issuing a new remote search if the previous one is taking too long. |
| `[` / `]` | Previous / next page for the active list. |
| `page <n>` | Jump to a page in the active list. |

Reserved aliases that should not be used as server aliases: `h`, `k`, `kill`, `cancel`, `all`, `s`, `al`, `ar`, `art`, `tr`, `i`, `sh`, `so`, `unsh`, `v`, `qf`, `ss`, `rs`, `msg`, `pl`, `diag`, `doc`, `gap`, `gapless`, `mk`, `media`, `media-keys`, and `audio`.

## CLI commands

These are run from the shell before opening the TUI.

| Command | Purpose |
|---|---|
| `disc` or `disc tui` | Start the TUI. |
| `disc server-list` | List configured servers and show the primary server. |
| `disc server-add <name> <alias> <base_url> <username> <password> [--primary]` | Add or update a server. |
| `disc server-edit <target> <name> <alias> <base_url> <username> <password> [--primary]` | Edit a server by alias or name. |
| `disc server-remove <target>` | Remove a server by alias or name. |
| `disc server-primary <target>` | Set the primary/home server. |
| `disc ping [target]` | Ping the primary server or a named server. |

## General TUI commands

| Command | Purpose |
|---|---|
| `help`, `h` | Show the numbered DISC help index in the main panel. Type a number to open a topic. Use `[` / `]` or PageUp/PageDown to move between help topics. |
| `help <topic>`, `h <topic>` | Show a numbered help topic, such as `help search`, `help playback`, or `help keys`. Use `help full` or `help commands` for the full command reference. |
| `version`, `about` | Show build/version information. |
| `kill`, `k`, `cancel` | Cancel the currently running background search/browse request so a new command can be issued. |
| `home` | Return the main panel to the home page. |
| `view` | Show available view targets. |
| `view home` | Show the home page. |
| `view results` | Show the current results page. |
| `view queue` | Show the play queue. |
| `view saved` | Show saved local queues. |
| `view history` | Show playback history. |
| `view messages`, `message-log`, `log`, `msg log` | Show the message log in the main panel. |
| `view help` | Show the numbered DISC help index. |
| `view commands`, `view command-reference` | Show the full command reference in the main panel. |
| `messages`, `message`, `msg` | Toggle the bottom message panel on/off. |
| `messages on/off`, `message on/off`, `msg on/off` | Explicitly show or hide the bottom message panel. |
| `msg status` | Show whether the bottom message panel is visible. |
| `verbose`, `verbose toggle`, `vt` | Toggle verbose bottom-panel status and action hints. |
| `verbose on`, `verbose off`, `verbose status` | Explicitly enable, disable, or inspect verbose status. |
| `status colour`, `status color` | Show the current status-title colour setting and supported colours. |
| `status colour <colour>` | Set the top-panel static label colour. Supports `auto/default`, named colours, `ansi(208)`, `#ff8800`, `rgb(255,136,0)`, and `cmyk(0,47,100,0)`. |
| `status colour auto`, `status colour default` | Reset status-title labels to style-aware defaults: green for monochrome styles, orange for multi-colour styles. |
| `queue-follow`, `qf` | Toggle queue-follow. Queue-follow defaults to off. |
| `media-keys`, `media`, `mk` | Toggle optional OS media key support and now-playing metadata publishing. |
| `media-keys on/off/status`, `mk on/off/status` | Explicitly enable, disable, or inspect OS media controls. |
| `audio status`, `audio` | Show DISC audio engine availability, active output, current OS default output, and detected output devices. |
| `audio devices`, `audio outputs`, `audio list` | List output devices reported by the OS audio host. |
| `audio reset`, `audio reopen`, `audio restart` | Reopen the current OS default output device and try to resume the active track near the same position. Use after RDP/local-login, Bluetooth, HDMI, USB DAC, or sleep/wake audio changes. |
| `queue-follow on/off/toggle/status`, `qf on/off/toggle/status` | Control or inspect whether queue-affecting result commands automatically show the play queue. Bare `qf` toggles. |
| `cls`, `clear-screen`, `clear-console` | Clear console messages only. Does not clear the play queue. |
| `clear`, `c` | Clear the play queue and stop playback. |
| `quit`, `exit` | Save the last session and exit. |
| `Esc` | Save the last session and exit the TUI. |
| `Ctrl+C` | Clear the play queue and stop playback. |
| `Ctrl+B` | Back through result history. |
| `Ctrl+Q` | Show the play queue. |
| `Ctrl+P` | Also show the play queue. |
| `Ctrl+M` | Toggle mute/unmute. |
| `Ctrl++`, `Ctrl+=` | Raise volume by 5. |
| `Ctrl+-` | Lower volume by 5. |
| `Ctrl+Shift+Q` | Save the last session and quit. |

## Server/config commands inside the TUI

| Command | Purpose |
|---|---|
| `servers`, `server-list` | List configured servers. |
| `primary` | Show the primary/home server. |
| `use <alias-or-name>`, `primary <alias-or-name>` | Set the primary/home server. |
| `add-server`, `server-add` | Start the interactive server setup wizard. This is the command shown on the first-run Home screen when no servers are configured. |
| `edit-server <alias-or-name>` | Start the interactive server edit wizard. |
| `remove-server <alias-or-name>` | Remove a configured server. |
| `ping [alias-or-name]` | Ping the primary server or a named server. |
| `ping all`, `doctor ping`, `doc ping`, `diag ping`, `doctor servers` | Ping every configured server. |

## Browse/search commands

| Command | Purpose |
|---|---|
| `rec`, `recent`, `recent albums` | Show 60 recently added albums by default. |
| `rec <n>`, `recent <n>`, `recent albums <n>` | Show the requested number of recent albums. |
| `rnd`, `random` | Show 60 random albums by default. |
| `rnd <n>`, `random <n>` | Show the requested number of random albums. |
| `rnd t [n]`, `rnd track [n]`, `rnd tracks [n]` | Show random tracks. Defaults to 60 when `n` is omitted. |
| `random tracks [n]` | Long form for random tracks. |
| `rnd [n] g <genre>` | Show random albums filtered by matching genre. Examples: `rnd 5 g ?rock`, `rnd g *jazz*`. |
| `rnd t [n] g <genre>` | Show random tracks filtered by matching genre. Example: `rnd t 50 g folk`. |
| `genres` | List all genres. |
| `g <query>`, `genre <query>`, `genres <query>` | Search genres. Supports `*` and `?` wildcards. |
| `playlists`, `pls`, `pl` | List server playlists. |
| `playlist <query>`, `playlists <query>`, `pl <query>` | Search playlists. |
| `artists` | List indexed artists. |
| `artist <query>`, `artists <query>`, `ar <query>`, `art <query>` | Search artists. |
| `album <query>`, `albums <query>`, `al <query>` | Search albums. |
| `track <query>`, `tracks <query>`, `tr <query>`, `song <query>`, `songs <query>` | Search tracks. |
| `search <query>`, `s <query>` | General search across supported result types. |
| `starred`, `favorites`, `favourites`, `favs` | Show starred/favourite tracks, albums, and artists. |
| `<server-alias> <browse-command>` | Run browse/search against a specific server, for example `hel rec`, `sub rnd 100`, or `hel rnd t 25 g folk`. |
| `all <browse/search-command>` | Run supported browse/search commands against every configured server. Multi-server results show a source suffix such as `[sub]`; multi-colour styles colour rows by server. |
| `<browse/search-command> --play` / `<browse/search-command> --p` / `<browse/search-command> -p` | Display the results, append every playable returned item to the queue, and start playback at the first newly added track. Example: `all rnd t 30 g folk --p`. |
| `<browse/search-command> --replace --play` / `<browse/search-command> --rp` / `<browse/search-command> -rp` | Display the results, replace the queue with every playable returned item, and start playback. Example: `all rnd t 30 g folk --rp`. |

### Long-running search/browse cancellation

Remote search and browse commands run in the background so the TUI remains responsive. DISC does not automatically tear down an existing remote request when you type a new remote search; it protects the in-flight request and asks you to wait or cancel. Use `kill`, `k`, or `cancel` to abort the current background search/browse request, discard any eventual result, and free the prompt for a new remote command. Local commands such as `status`, `queue`, `audio status`, and playback controls can still be used while a background search is running.

Wildcard searches use `*` for any number of characters and `?` for one character. Matching is case-insensitive where DISC performs local filtering. Album wildcard searches use a bounded local album-list fallback when a server seed search misses partial terms, so patterns such as `album haz*`, `al hazard*`, and `s haz*` are more forgiving.

## Acting on result lists

| Command | Purpose |
|---|---|
| `<n>` | Select result number `n`. Tracks replace the queue and play; albums/playlists replace the queue with their tracks; artists/genres navigate deeper. |
| `<n> q`, `<n> queue` | Select result number `n`, then show the play queue once when the selection sends tracks to the queue. |
| `<n...>` such as `1 3 5 6-8 9` | Loose shorthand for `add <n...>` in results-like contexts. Appends those results to the play queue. |
| `<n...> q`, `<n...> queue` | Loose multi-add plus one-shot queue display, for example `1 3 5-7 q`. |
| `x <n>`, `x<n>`, `explore <n>` | Explore an album or playlist as tracks without replacing the queue. |
| `add <n...>`, `a <n...>`, `a<n...>` | Append tracks/albums/playlists from results to the queue. |
| `add <n...> q`, `a <n...> queue`, `add * q` | Append results, then show the play queue once without changing queue-follow. |
| `add *`, `a *` | Append all current results where supported. |
| `play <n...>`, `p <n...>`, `p<n...>` | Append selected results to the play queue and start playback at the first appended track. Examples: `play 1`, `p 1 3 5-7`. |
| `play *`, `p *`, `p*` | Append all playable current results to the play queue and start playback at the first appended track. This appends; use a search suffix such as `-rp` when you want replacement. |
| `back`, `b` | Return from queue/history/saved view to the previous list when possible, otherwise return to the previous result list. |
| `find <text>`, `filter <text>`, `/ <text>` | Search the active list without changing it. Supports wildcards. |
| `sort <key> [asc|desc]` | Sort the active results. |
| `sort results <key> [asc|desc]` | Explicitly sort the current results. |
| `info <n>`, `details <n>` | Show metadata and actions for result number `n`. |
| `star <n...>`, `unstar <n...>` | Star or unstar supported result items. |
| `dl <n...>`, `download <n...>` | Download selected result items. |

## Play queue commands

Queue-follow is intentionally off by default, so selecting a result or using `add` keeps you on the current results page. Turn on `qf` to always jump to the queue, or add a one-shot suffix such as `a 1 3 5-7 q`.


| Command | Purpose |
|---|---|
| `queue`, `q` | Show the play queue. |
| `queue-follow`, `qf` | Toggle whether queue-follow is enabled. |
| `qf on`, `qf off`, `qf toggle`, `qf status` | Persistently control or inspect whether result selections/additions show the play queue automatically. Bare `qf` toggles. |
| `<n>` while queue is active | Select queue item `n`. |
| `add <n...>`, `a <n...>`, `a<n...>` | Append tracks from the active result/history/saved-queue context. In results/history/saved contexts, multiple bare selectors such as `1 3 5-7` also append. |
| `remove <n...>`, `rm <n...>`, `r<n...>` | Remove queue items. Supports ranges. |
| `move <from> <to>` | Move a queue item without restarting playback. |
| `dedupe`, `dedupe queue`, `queue-dedupe`, `queue dedupe` | Remove duplicate tracks while preserving the current selection where possible. |
| `clear-played`, `remove-played`, `trim-before`, `queue trim-before` | Remove items before the current queue item. |
| `clear-upcoming`, `trim-after`, `queue trim-after` | Remove items after the current queue item. |
| `sort queue <key> [asc|desc]` | Sort the queue while preserving the current selection where possible. |
| `info`, `i`, `info now`, `i now` | Show the current selected/playing queue item. |
| `info <n>`, `i <n>`, `details <n>` | Show metadata for queue item `n` when the queue is active. |
| `save-queue <name>`, `sq <name>` | Save the current play queue locally. |
| `save-queue`, `sq` | Save the queue using an auto-generated name. |
| `save-queue <name> --replace`, `sq <name> -f` | Overwrite an existing local saved queue. |

## Playback

| Command | Purpose |
|---|---|
| `play` | Start or resume playback of the selected/current queue item. |
| `pause` | Pause playback. |
| `p` | Toggle play/pause. |
| `Space` | Toggle play/pause when the command input is empty. If the queue or selection changed while paused, starts the selected track. |
| `next`, `n`, `PageDown`, `Right` | Move to the next queue item. When playback is active, playback starts after a short 120 ms grace period so repeated skips do not buffer every intermediate track. `Right` only skips tracks when the command input is empty; otherwise it moves the text cursor. |
| `prev`, `previous`, `PageUp`, `Left` | Move to the previous queue item. When playback is active, playback starts after a short 120 ms grace period so repeated skips do not buffer every intermediate track. `Left` only skips tracks when the command input is empty; otherwise it moves the text cursor. |
| `stop` | Stop playback. |
| `now`, `np`, `status` | Show current playback, queue status, and active audio output. |
| `vol <0-100>`, `v <0-100>`, `vol70`, `v70` | Set volume. Compact forms without a space are accepted. |
| `vol +<n>`, `vol -<n>`, `v +<n>`, `v -<n>`, `vol+5`, `v-5` | Raise or lower volume. Compact forms without a space are accepted. |
| `vol`, `v` | Show the current volume. |
| `gapless`, `gap` | Toggle experimental gapless/preload playback on/off. |
| `gapless on`, `gap on`, `gapless off`, `gap off`, `gapless status`, `gap status` | Enable, disable, or inspect experimental gapless/preload playback. |
| `audio status`, `audio` | Show the playback engine status, DISC active output, current OS default output, and detected output devices. |
| `audio devices`, `audio outputs`, `audio list` | List all output devices reported by the OS audio host. |
| `audio reset`, `audio reopen`, `audio restart` | Drop the current sink/stream, reopen the current OS default output, and if a track was active try to resume it at approximately the same position. Useful after RDP Remote Audio, local-login, Bluetooth, HDMI, USB DAC, or sleep/wake endpoint changes. |
| `mute`, `unmute` | Mute or restore volume. |
| `seek <time>`, `sk <time>` | Seek to an absolute time such as `90`, `1:30`, or `1:02:03`. |
| `seek +<time>`, `seek -<time>` | Seek relative to the current position. |
| `ff [seconds]` | Fast-forward. Defaults to the app's built-in step when seconds are omitted. |
| `rew [seconds]` | Rewind. |
| `skip <seconds>`, `skp <seconds>` | Skip forward or backward; use negative values to move backward. |
| `repeat`, `rp` | Show repeat mode or cycle/toggle depending on command form. |
| `repeat off`, `repeat one`, `repeat all` | Set repeat mode. |

## Shuffle commands

| Command | Purpose |
|---|---|
| `shuffle`, `sh` | Toggle shuffle playback. With shuffle-order in standby/on, this applies a stable shuffled queue order. With shuffle-order off, the visible queue stays in original order and the current cursor moves through a pseudo-random play path. |
| `shuffle on`, `sh on` | Turn shuffle on. Behaviour depends on shuffle-order: stable visible order when `so` is standby/on; fixed visible queue with random cursor movement when `so off`. |
| `shuffle off`, `sh off` | Turn shuffle off. If shuffle-order had visibly rearranged the queue, the original queue order is restored. |
| `shuffle status`, `sh status` | Show shuffle and shuffle-order status. |
| `so`, `shuffle-order`, `shuffle order` | Toggle stable visible shuffle-order mode. Default state is `standby`: enabled, but waiting for shuffle to be on. |
| `so on`, `shuffle-order on` | Enable stable visible shuffle-order mode. When shuffle is already on, this now reapplies a full stable shuffled queue order while preserving the current track identity. |
| `so off`, `shuffle-order off` | Disable stable visible shuffle-order mode and restore original queue order if one is stored. Shuffle can still be on; in that case the visible queue remains fixed while the cursor jumps through a random play path. |
| `so status` | Show shuffle-order status: `standby`, `on`, or `off`. |
| `so refresh`, `so reshuffle`, `so now` | Re-apply stable shuffle order to the queue. |
| `so restore`, `shuffle-order restore`, `unshuffle`, `unsh` | Restore the stored original queue order and turn shuffle playback off. If shuffle-order was enabled, it remains enabled and shows `standby`; if it was off, it remains off. |
| `shuffle upcoming`, `shuffle queue`, `shuffle now` | Manually reorder upcoming queue items. This is an explicit queue-order command, separate from random cursor shuffle playback. |
| `shuffle all` | Manually reorder the full queue while preserving the current selection identity. |

## Saved local queues

| Command | Purpose |
|---|---|
| `queues`, `saved-queues`, `list-queues` | List local saved queues. |
| `load-queue <name-or-number>`, `lq <name-or-number>` | Load a saved queue and replace the current play queue. |
| `add <n>`, `a<n>` while saved queues are active | Append saved queue number `n` to the current play queue. Multiple bare selectors such as `1 3` also append those saved queues. |
| `delete-queue <name-or-number>`, `dq <name-or-number>` | Delete a local saved queue. |
| `rename-queue <name-or-number> <new-name>`, `rq <name-or-number> <new-name>` | Rename a local saved queue. |
| `rename-queue <old> <new> --replace`, `rq <old> <new> -f` | Rename and replace an existing target queue name. |
| `sort saved <key> [asc|desc]` | Sort the saved queue list. |
| `info <n>`, `details <n>` | Show saved queue details when saved queues are active. |

## Session commands

| Command | Purpose |
|---|---|
| `session status` | Show last-session availability. |
| `session save`, `save-session`, `ss` | Save the current queue as the last session. |
| `session restore`, `restore-session`, `rs`, `resume-session`, `last-queue` | Restore the last-session queue. |
| `session clear`, `clear-session` | Remove the last-session file. |

## Server playlist commands

These modify playlists on the Subsonic server, not local saved queues.

| Command | Purpose |
|---|---|
| `playlist-save <name>`, `pl save <name>`, `ps <name>` | Create a server-side playlist from the current play queue. |
| `playlist-update <target>`, `pl update <target>`, `pu <target>` | Replace a server playlist with the current play queue. |
| `playlist-add <target> [items]`, `pl add <target> [items]`, `pa <target> [items]` | Append the current queue or selected items to a server playlist. |
| `playlist-delete <target>`, `pl delete <target>`, `pd <target>` | Delete a server playlist. |
| `playlist-rename <target> to <new-name>`, `pl rename <target> to <new-name>`, `pr <target> to <new-name>` | Rename a server playlist. |

`<target>` can be a playlist number from `playlists`, an unambiguous playlist name, or a raw playlist id. Server playlists can only contain tracks from the same server.

## Download commands

| Command | Purpose |
|---|---|
| `download`, `dl` | Show download usage. |
| `dl now`, `download now` | Download the current selected/playing queue track. |
| `dl <n...>`, `download <n...>` | Download items from the active results, queue, saved queue, or playback history. |
| `dl *`, `download *` | Download all items in the active context where supported. |
| `download-path`, `dl-path` | Show current download folder. |
| `download-path <folder>`, `dl-path <folder>` | Set a custom download folder. |
| `download-path default`, `download-path reset`, `download-path clear` | Return to the platform default download folder. |
| `download-overwrite`, `dl-overwrite` | Show overwrite setting. |
| `download-overwrite on/off/toggle`, `dl-overwrite on/off/toggle` | Control the persisted overwrite setting. |
| `dl <items> --replace`, `dl <items> --overwrite`, `dl <items> -f` | Force overwrite for this download command. |
| `dl <items> --skip`, `dl <items> --no-overwrite` | Force skip existing files for this command. |

## Star/favourite commands

| Command | Purpose |
|---|---|
| `starred`, `favorites`, `favourites`, `favs` | Show starred/favourite items. |
| `star <n...>`, `unstar <n...>` | Star or unstar supported current result or queue items. |
| `star now`, `unstar now` | Star or unstar the current queue item. |
| `star *`, `unstar *` | Star or unstar all supported items in the active result or queue context. |

## Playback history commands

| Command | Purpose |
|---|---|
| `history`, `played`, `play-history`, `playback history`, `recent tracks` | Show tracks that have started playback in the current TUI session. |
| `history clear`, `playback history clear`, `recent tracks clear` | Clear in-session playback history. |
| `<n>` while history is active | Replace the queue with history track `n` and play it unless playback is paused. |
| `<n...>` while history is active | With multiple selectors, append those history tracks to the queue, equivalent to `a<n...>`. |
| `a<n...>` while history is active | Append history tracks to the queue. |
| `sort history <key> [asc|desc]` | Sort playback history. |

## Diagnostics

| Command | Purpose |
|---|---|
| `doctor`, `doc`, `diagnostics`, `diag` | Show local app/config/playback diagnostics. |
| `doctor downloads`, `doc downloads`, `diag downloads` | Check configured download folder and perform a write/delete test. |
| `doctor ping`, `doc ping`, `diag ping`, `doctor servers`, `ping all` | Ping all configured servers. |
| `doctor all`, `doc all`, `diag all` | Run local diagnostics plus server pings. |

## Styling and display

| Command | Purpose |
|---|---|
| `style`, `sty`, `theme` | Show current style. |
| `style <id-or-name>`, `sty <id-or-name>`, `theme <id-or-name>` | Set style. Available styles: `1 soft`, `2 mid`, `3 bright`, `4 multi-soft`, `5 multi-mid`, `6 multi-bright`. |
| `config sty <id-or-name>`, `conf sty <id-or-name>` | Web-parity aliases for setting style. |
| `style path` | Show the editable custom styles file path. |
| `style sample` | Create a starter `custom-styles.toml` if one does not already exist. |
| `style reload` | Reload custom style definitions after editing the file. |
| `style custom` | List loaded custom styles. |
| `pmc`, `playlist-multicolour` | Show play-queue multicolour setting. |
| `pmc on/off/toggle`, `playlist-multicolour on/off/toggle` | Control play-queue multicolour album grouping. |
| `rmc`, `result-multicolour`, `result-multicolor` | Show result-list multicolour setting. |
| `rmc on/off/toggle`, `result-multicolour on/off/toggle` | Control optional result-list multicolour rendering. |
| `status colour`, `status color` | Show the current status-title colour setting and supported colours. |
| `status colour <colour>`, `status color <colour>` | Set the top-panel static label colour. Use `auto/default` to follow the current style: green for monochrome styles and orange for multi-colour styles. |
| `msg`, `message`, `messages` | Toggle the bottom message panel. Use `msg on/off/status` to force or inspect the setting, and `msg log` / `log` to show the full message log in the main panel. |
| `verbose`, `vt` | Toggle verbose hints/status. Use `verbose on/off/status` for explicit control. |

## Notes

- Main-panel list headers show `Showing x-y of z | page a/b`; the panel title already indicates the active view.
- Page size adapts to the visible panel height, so use `[` and `]` to reach items that do not fit in a small terminal window.
- Bottom-panel persistent action hints and debug/status lines appear only when verbose mode is on. Use `verbose`, `verbose on`, `verbose off`, `verbose toggle`, or `vt`; `v` is now reserved for volume. One-off feedback, errors, seek/skip confirmations, and command results still appear when verbose is off.
- The top DISC status panel shows a `Playlist:` line when the active queue has a known server playlist, saved queue, or named queue context; it is omitted when no relevant playlist/queue name is known.
- The `Track:` line shows only the current track title; artist and album are displayed on their own line. Only the current track title is bold.
- Gapless playback is experimental and switchable. With `gapless on` / `gap on`, DISC preloads the likely next track and attempts a near-end same-sink handoff; `gapless off` / `gap off` returns to the safer single-track playback path.
- Audio endpoint recovery commands are available for Windows/RDP and device-change cases: `audio status`, `audio devices`, and `audio reset`. Also check Windows Sound > Volume mixer if playback moves but no sound is heard.
- Custom styles can be defined in `custom-styles.toml`; see `CUSTOM_STYLES.md` for an example. Colour values may use names, ANSI 256 values such as `ansi(208)`, RGB/hex values such as `#ff8800` or `rgb(255,136,0)`, and CMYK values such as `cmyk(0,47,100,0)` converted to RGB.
- Optional media controls/now-playing metadata can be toggled with `mk` / `media-keys`. On Windows, media controls use a hidden native window handle for the OS integration; if that backend is unavailable, DISC continues normal playback and reports a warning.




## Search progress and timeouts

Supported remote search/browse commands run asynchronously, so DISC keeps redrawing while a slow server responds. The message/status area shows elapsed time against the configured timeout.

- `timeout` / `search-timeout` / `server-timeout` shows timeout settings.
- `timeout 60` sets the default and all configured server search timeouts to 60 seconds.
- `timeout <server> <seconds>` sets a per-server timeout, for example `timeout sub 90`.
- Timeout values must be between 5 and 600 seconds.

Command-line editing: Left/Right move within typed text, Home moves to the start, and End moves to the end. Left/Right only control previous/next playback when the command input is empty.

## Queue edits and gapless playback

Queue edit commands such as `r 5`, `move`, `sort queue`, `dedupe`, `clear-played`, `clear-upcoming`, `shuffle all`, `so refresh`, and `unsh` invalidate any prepared next-track handoff and recompute it from the visible queue. This prevents a removed or moved upcoming track from continuing to play from stale gapless state.


## Flag style notes

Canonical long flags remain double-dash forms such as `--play`, `--replace`, `--overwrite`, and `--skip`, matching common CLI convention. DISC also accepts friendly aliases for the TUI command parser where practical: conventional short forms such as `-p` and `-rp`, double-dash compact aliases such as `--p` and `--rp`, and single-dash long aliases such as `-play`, `-replace`, and `-overwrite` for the search/play, saved-queue overwrite, and download overwrite parsers.
