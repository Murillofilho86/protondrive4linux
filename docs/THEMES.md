# Custom themes

protondrive4linux ships with a light and a dark palette, and three more bundled as examples
(Dracula, Nord, Tokyo Night). You can add your own by dropping a small TOML file into the themes
folder - no code, no rebuild.

- **Folder:** `~/.config/protondrive4linux/themes/` (or `$XDG_CONFIG_HOME/protondrive4linux/themes/`).
  Settings -> Appearance has a **Folder** button that creates and opens it.
- **Pick it:** Settings -> Appearance -> Theme lists every theme found, or set
  `theme = "My Theme"` under `[options]` in the config. Files are re-read while Settings is open,
  so a new one appears without restarting.

## The file

```toml
# ~/.config/protondrive4linux/themes/solarized-light.toml
name = "Solarized Light"
base = "light"          # light | dark

[colors]
panel     = "#FDF6E3"
panel2    = "#FFFFFF"
text      = "#073642"
accent    = "#268BD2"
```

`name` and `base` are required. **Every color is optional**: whatever you leave out comes from the
`base` palette, so a theme can be three lines. `base` also picks which stock widget look
(light or dark) the rest of the interface builds on, so choose the one your page background is
closest to.

Colors are `#RRGGBB`, `#RGB`, or `#RRGGBBAA` (the alpha form is mainly for `line` and `hover`).

## The tokens

| Token | What it colors |
| --- | --- |
| `nav_bg` | the sidebar and the deepest surface |
| `panel` | the page background |
| `panel2` | cards and raised surfaces |
| `sel` | selected and hovered rows, switch tracks |
| `text` | primary text |
| `dim`, `dim2` | secondary and tertiary text (captions, hints) |
| `accent` | the accent: primary buttons, focus, the selected nav marker |
| `accent_hi`, `accent_lo` | its lighter (hover) and darker (pressed) shades |
| `on_accent` | text and icons **on** an accent-colored surface (the label of a primary button) |
| `ok`, `danger`, `warn` | status colors: synced / errors / needs attention |
| `line` | hairline borders (translucent, so it works on any surface) |
| `hover` | the wash drawn over a hovered row (translucent) |

A typo is an error, not silently ignored: `acent = "#fff"` is reported as
`unknown field 'acent', expected one of ...`, on the Settings page. A file that can't be used never
hides the others, and a theme named like a built-in (`system`, `light`, `dark`) is refused because
those names are the built-in choices.

## Legibility

Settings warns when the theme you picked is hard to read (WCAG contrast): text on the page,
sidebar or cards under 4.5:1, secondary text under 3:1, or `on_accent` under 3:1 on an accent
shade. It only warns - your theme is yours - but the bundled ones are held to a stricter bar in
the test suite (text 7:1, secondary 4.5:1, status colors 3:1 on every surface, `on_accent` 4.5:1
on every accent shade), and it's a good target.

A common pitfall: a **light accent** (Dracula's purple, Nord's frost blue) needs a **dark**
`on_accent`, or the primary button's label disappears.

## Overriding a bundled theme

A file whose `name` matches a bundled theme (case-insensitively) replaces it, so you can tweak
Nord's accent by dropping in a `nord.toml` of your own. The three bundled files live in
`assets/themes/` in the source tree - copy one as a starting point.
