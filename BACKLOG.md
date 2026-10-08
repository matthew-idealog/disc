# DISC — Backlog

## Tech Debt

### TD-1 — `src/ui/terminal.rs` is an 11,887-line monolith
**Violates:** Structural Hygiene #2 (one file, one job), #3 (extract at the seam while it's small), #8 (a folder is a module, not a bucket).

`src/ui/terminal.rs` holds ~81% of the codebase's source lines. It cannot be described in one
sentence: it currently owns the ratatui render loop, key/command dispatch, app state, the queue
model, search/browse views, the status panel, media-control glue, and config-editing screens.
`src/ui/mod.rs`, `src/app/mod.rs` and `src/playback/mod.rs` are 1 line each, so there is no
internal structure absorbing new code — every feature lands inline in `terminal.rs` and makes
the eventual extraction larger.

**Cost of inaction:** the extraction cost rises with every feature added. Any change to this file
carries broad blast radius, and the file is effectively untestable in units.

**Suggested direction (incremental, not a big-bang rewrite):**
- Extract at seams as features are touched, one cohesive unit at a time, into feature folders
  (`src/ui/queue/`, `src/ui/search/`, `src/ui/status/`, `src/ui/config_editor/`), rather than a
  flat `src/ui/` pile.
- Move app/queue state out of the render module so state has a single owner (Hygiene #4) and
  function is separable from form (#5).
- Rule going forward: new UI features arrive as new modules; nothing new is added inline to
  `terminal.rs`.

**Status:** open. Logged 2026-10-05.

### TD-2 — Playback downloads 100% of a track before emitting any audio
**Violates:** nothing structural; this is a design weakness, logged so it isn't rediscovered.

`download_track_bytes` (`src/playback/engine.rs:1021`) calls `.bytes()`, which blocks until the
final byte arrives. `load_and_start` (`src/playback/engine.rs:354`) only then builds the decoder
and starts the sink, so time-to-first-sound equals the full transfer time of the file. There is
no progressive playback and no partial buffering.

**Impact:** low on a LAN (a 10 MB track transfers in ~1 s), severe on any slow link — measured
~19 s to start a 10 MB track over a ~4 Mbps path. Gapless prefetch hides this for tracks 2+ in a
queue, but the first track of every playback always pays the full cost.

**Found via:** 2026-10-05 investigation of slow starts on macOS, which turned out to be a config
problem (see note below), not this. Fixing the config made the symptom largely disappear, so this
was deliberately NOT fixed at the time — building ~150 lines of buffering machinery would have
masked the real cause.

**Suggested direction if it resurfaces (e.g. for remote/off-LAN use):** download on a background
thread into a shared growing buffer, hand rodio a blocking `Read + Seek` reader over it, and start
the sink once a prime threshold (~256 KB, or header plus a couple of seconds of audio) has landed.
The fiddly part is seek semantics past the downloaded watermark. Put the download concern in its
own module rather than adding more inline code to `engine.rs`.

**Status:** open, low priority. Logged 2026-10-05.

---

*Note (2026-10-05):* slow playback starts on macOS were caused by `base_url` being a dynamic-DNS
hostname resolving to the public IP, routing LAN traffic out over the WAN uplink (~0.5 MB/s)
instead of the local network (~6-22 MB/s). Not a code defect.
