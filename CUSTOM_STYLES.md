# DISC custom styles

DISC keeps built-in styles available, but can also load optional user-defined styles from your platform config directory.

Inside DISC:

```text
style path
style sample
style reload
style custom
style "Custom Amber"
```

`style sample` creates `custom-styles.toml` if it does not already exist. Edit that file, then use `style reload` without restarting DISC.

Example:

```toml
[[styles]]
name = "Custom Amber"
base = "multi-soft"
fg = "#ffb000"
accent = "#ffb000"
secondary = "#ff8800"
panel = "#090807"
danger = "light-red"
warning = "yellow"
queue_palette = ["#ffb000", "#ff8800", "#7db7ff", "#d7c75f"]
```

Supported colour formats:

```text
orange
green
ansi(208)
#ff8800
rgb(255,136,0)
cmyk(0,47,100,0)
```

CMYK values are converted internally to RGB for terminal display. RGB/hex values require a terminal with truecolour support for the best result; terminals with smaller palettes may approximate colours.
