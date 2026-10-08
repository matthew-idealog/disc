# DISC changelog

## 0.1.0-rc.21

- Routed runtime stderr output produced by audio backends, including rodio/cpal output-stream device-loss warnings, into the bottom Messages panel instead of allowing it to corrupt the command input area.
- Kept the top playback/status panel concise during playback errors by showing `error (see Messages)` instead of wrapping the full device error across the UI.
- Improved `audio reset` recovery when playback was already in `Error` state: reset now clears stale audio errors and will try to restart the current track on the reopened default output when possible.
- Space now starts the selected track when the queue or queue selection changed while paused, instead of requiring `play`. Queue changes while paused still do not autoplay on their own.

## 0.1.0-rc.20

- Removed the album-art feature set and all related user commands/config from the RC line after the graphics experiments proved unreliable across target terminals.
- Restored the top `DISC` playback/status panel to the compact non-art layout so results and play-queue panels have normal space again.
- Kept the non-art RC10+ work: result-page `play` / `p` selectors, incomplete server URL discovery in the config wizard, and the beginner guide.
- `play <n...>`, `p <n...>`, `p<n...>`, `play *`, `p *`, and `p*` on result pages append selected results to the queue and start playback at the first appended track.
- The server setup wizard can test incomplete server addresses and save a discovered working URL when common Subsonic combinations respond to `ping`.

## 0.1.0-rc.9

- Clarified the config wizard server alias prompt: aliases are short server nicknames that make server-specific commands quicker and more convenient to type, such as `sub rec` or `hel rnd 50`.
- Added the same alias explanation to `help server`, README, and command documentation.
- Changed new-install/default theme to Style 4 / `multi-soft`. Existing configs that already specify a `theme` keep their current style.

## Earlier RCs

See prior handover documents for RC6-RC8 details, including audio endpoint recovery, cancel/kill for background searches, search suffix parsing, and queue-selection debounce.
