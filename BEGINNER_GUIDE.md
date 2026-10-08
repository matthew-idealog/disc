# DISC beginner guide

DISC is a terminal music player for Subsonic-compatible servers. This guide covers the everyday flow: configure a server, choose a style, search for music, build a play queue, control playback, and save/restore what you were listening to.

## 1. Start DISC

From the source/build folder, run:

```bash
cargo run --release -- tui
```

Or, after installing/building the binary:

```bash
disc
disc tui
```

If no server is configured, DISC shows a first-run hint. Type:

```text
add-server
```

## 2. Configure a server

The setup wizard asks for:

1. **Server name**: a friendly display name, such as `Home Music`.
2. **Server alias**: a short nickname used when typing server-specific commands, such as `sub`, `home`, or `nas`.
3. **Server URL**: the Subsonic-compatible server address.
4. **Username** and **password**.
5. Whether this should be the **primary** server.

The alias is important because it lets you target a specific server quickly:

```text
sub rec
home rnd 50
nas tr miles
```

Bare commands use the primary server:

```text
rec
rnd 50
tr miles
```

You can also search every configured server with `all`:

```text
all tr miles
all rnd t 30 g folk
```

### Server URL tips

A full URL is safest:

```text
http://192.168.1.20:4040
https://music.example.com:4443
```

The wizard can also try to help with incomplete addresses. If you enter something like:

```text
192.168.1.20
music.local
```

DISC tries common Subsonic combinations such as:

```text
http://host:4040
https://host:4443
http://host
https://host
```

If one responds to Subsonic `ping`, DISC saves the discovered working URL.

## 3. Set a style

New installs default to Style 4 / `multi-soft`.

Show the current style:

```text
style
sty
```

Set a style:

```text
sty 1
sty 4
style multi-soft
```

Built-in styles:

```text
1 soft
2 mid
3 bright
4 multi-soft
5 multi-mid
6 multi-bright
```

Useful display commands:

```text
pmc              # play-queue multicolour setting
pmc on           # colour queue rows by album in multi-colour styles
rmc on           # optionally colour result rows too
msg              # show/hide message panel
```

## 4. Search and browse

Common searches:

```text
rec                  # recent albums, default 60
rec 20               # 20 recent albums
rnd                  # random albums, default 60
rnd 30               # 30 random albums
rnd t 30             # 30 random tracks
rnd t 30 g folk      # 30 random folk tracks
al blue              # album search
tr blue              # track search
ar bowie             # artist search
pl favourites        # playlist search
s blue               # general search
```

Target a specific server by putting its alias first:

```text
sub rec
sub rnd t 30 g jazz
```

Search all configured servers:

```text
all tr blue
all rnd t 30 g folk
```

Wildcard examples:

```text
al haz*
tr *river*
ar beat?
```

## 5. Add music to the play queue

After a result list appears, use the numbers shown on the left.

Append one result:

```text
add 3
a 3
```

Append several results:

```text
add 1 3 5
add 1-10
a 1 3 5-8
```

Append all current results:

```text
add *
a *
```

Loose multi-select shorthand also works on result pages:

```text
1 3 5-8
```

Single-number selection keeps its normal behaviour. For example, selecting a single track usually replaces the queue and plays it; selecting an album opens or plays that album depending on context.

## 6. Add and start playback immediately

Use search suffixes when you want the search result to go straight into the queue.

Append the returned tracks and start playing at the first new track:

```text
all rnd t 30 g folk -p
all rnd t 30 g folk --play
```

Replace the current queue with the returned tracks and start playing:

```text
all rnd t 30 g folk -rp
all rnd t 30 g folk --replace --play
```

On an existing results page, use `play` / `p` to append selected results and start at the first appended track:

```text
play 1
p 1 3 5-7
p1
play *
p *
p*
```

`play *` appends rather than replaces. Use `-rp` on the original search when you want to replace the queue.

## 7. Switch to and manage the play queue

Show the play queue:

```text
queue
q
```

Keyboard shortcuts:

```text
Ctrl+Q      show play queue
Ctrl+P      show play queue
PageDown    next track
PageUp      previous track
Right       next track when command input is empty
Left        previous track when command input is empty
Space       play/pause
```

Queue management:

```text
remove 3
r 3
r 3-5
move 8 2
dedupe
clear-played
clear-upcoming
clear
```

Sorting examples:

```text
sort queue album
sort queue artist
sort queue title desc
```

## 8. Playback controls

```text
play
pause
stop
next
prev
seek 1:30
seek +30
seek -15
vol 60
v 60
mute
```

If Windows/RDP or Bluetooth/HDMI/USB audio changes cause silent playback, try:

```text
audio status
audio devices
audio reset
```

## 9. Shuffle

Toggle shuffle:

```text
shuffle
sh
```

Stable visible shuffle-order mode:

```text
so on
so refresh
so off
unshuffle
unsh
```

`shuffle` controls playback randomness. `shuffle-order` / `so` controls whether the visible queue itself is rearranged into a stable shuffled order.

## 10. Save and restore sessions and local queues

DISC can save your current queue locally.

```text
save-queue roadtrip
sq roadtrip
queues
load-queue roadtrip
lq roadtrip
add 2              # while saved queues are visible, append saved queue 2
rename-queue 2 new-name
delete-queue 2
```

Session commands:

```text
session status
session save
session restore
session clear
```

DISC also saves a last-session queue on clean exit so you can restore it later.

## 11. Server playlists

Server-side playlist commands depend on what your Subsonic-compatible server supports.

```text
playlists
playlist-save favourites
pl save favourites
playlist-update favourites
pl update favourites
playlist-add favourites 1 3 5
pl add favourites 1-10
```

Local saved queues are stored by DISC. Server playlists are stored on the music server.

## 12. Getting more help

Inside DISC:

```text
help
help basics
help beginner
help commands
```

Use `[` and `]` or PageUp/PageDown to move through help pages.

## Extra beginner tips

A few additional commands are worth learning early:

```text
back          # return to the previous result/list view
find jazz     # filter/search inside the current list
info 3        # inspect result/queue item 3
kill          # cancel a long-running background search
ping all      # test configured servers
status        # current playback/server summary
doctor        # local diagnostics
```
