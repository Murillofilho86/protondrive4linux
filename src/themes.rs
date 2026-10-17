//! GUI themes as data (M5-002).
//!
//! A theme is a small set of semantic color tokens (the same ones the GUI draws
//! with: surfaces, text, accent, status colors). The two built-in palettes live
//! here, and any number of others can be added as plain TOML files - three are
//! bundled (Dracula, Nord, Tokyo Night) and double as examples - without
//! touching the GUI code:
//!
//! ```toml
//! # ~/.config/protondrive4linux/themes/solarized.toml
//! name = "Solarized Dark"
//! base = "dark"            # dark | light: egui's stock widgets, and the
//!                          # colors this file doesn't spell out
//! [colors]
//! panel  = "#002B36"
//! accent = "#268BD2"
//! ```
//!
//! Every color in `[colors]` is optional - a theme can be three lines - and a
//! typo (`acent = ...`) is an error naming the key, not silently ignored. This
//! module is pure data and parsing; turning a [`ThemeColors`] into widgets is
//! the GUI's job.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// An sRGB color with alpha (straight, not premultiplied): `[r, g, b, a]`.
pub type Rgba = [u8; 4];

const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    [r, g, b, 255]
}

/// Light or dark: which stock egui widget look a theme builds on, and which
/// built-in colors a theme file doesn't have to spell out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Base {
    Dark,
    Light,
}

/// Names a custom theme can't take: they are the built-in choices.
const RESERVED: [&str; 3] = ["system", "light", "dark"];

/// A theme file is a few hundred bytes; anything much bigger isn't one.
const MAX_THEME_FILE: u64 = 64 * 1024;

macro_rules! colors {
    ($( $(#[$doc:meta])* $f:ident ),* $(,)?) => {
        /// Every color the GUI draws with, as semantic tokens.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct ThemeColors {
            $( $(#[$doc])* pub $f: Rgba, )*
        }

        /// The `[colors]` table of a theme file: any subset of the tokens.
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields)]
        struct RawColors {
            $( $f: Option<String>, )*
        }

        impl ThemeColors {
            /// `self` with every color `raw` sets laid over it.
            fn overlaid(mut self, raw: &RawColors) -> Result<Self> {
                $(
                    if let Some(s) = &raw.$f {
                        self.$f = parse_color(s)
                            .with_context(|| format!("colors.{}", stringify!($f)))?;
                    }
                )*
                Ok(self)
            }
        }
    };
}

colors! {
    /// Sidebar and the deepest surface.
    nav_bg,
    /// Page background.
    panel,
    /// Cards and raised surfaces.
    panel2,
    /// Selected / hovered rows and widgets.
    sel,
    /// Primary text.
    text,
    /// Secondary text, and tertiary text.
    dim,
    dim2,
    /// The accent, its lighter (hover) and darker (pressed) shades.
    accent,
    accent_hi,
    accent_lo,
    /// Text and icons drawn ON an accent-colored surface (primary buttons).
    on_accent,
    /// Status colors.
    ok,
    danger,
    warn,
    /// Hairline borders and hover washes. Translucent, so they work on any
    /// surface (white-ish on dark themes, black-ish on light ones).
    line,
    hover,
}

impl ThemeColors {
    /// The app's own dark palette, derived from `assets/logo.png`'s actual
    /// pixel colors (sampled, not eyeballed): a deep indigo-violet base, a
    /// vivid violet-purple accent, and the icon's bright green for success.
    pub fn dark() -> Self {
        ThemeColors {
            nav_bg: rgb(0x14, 0x10, 0x22),
            panel: rgb(0x1B, 0x16, 0x2A),
            panel2: rgb(0x22, 0x1C, 0x34),
            sel: rgb(0x2A, 0x20, 0x3E),
            text: rgb(0xFF, 0xFF, 0xFF),
            dim: rgb(0x8E, 0x8B, 0x9A),
            dim2: rgb(0x6C, 0x6A, 0x7A),
            accent: rgb(0x64, 0x28, 0xFA),
            accent_hi: rgb(0x7B, 0x48, 0xFB),
            accent_lo: rgb(0x50, 0x20, 0xC8),
            on_accent: rgb(0xFF, 0xFF, 0xFF),
            ok: rgb(0x52, 0xE6, 0x5C),
            danger: rgb(0xE0, 0x50, 0x64),
            warn: rgb(0xD8, 0xA0, 0x50),
            line: [255, 255, 255, 20],
            hover: [255, 255, 255, 14],
        }
    }

    /// The light counterpart: the same hues on a pale lavender base, with the
    /// status colors darkened until they read on it.
    pub fn light() -> Self {
        ThemeColors {
            nav_bg: rgb(0xEC, 0xE8, 0xF7),
            panel: rgb(0xF6, 0xF4, 0xFC),
            panel2: rgb(0xFF, 0xFF, 0xFF),
            sel: rgb(0xE1, 0xDA, 0xF5),
            text: rgb(0x1B, 0x16, 0x2A),
            dim: rgb(0x5A, 0x56, 0x6E),
            dim2: rgb(0x76, 0x72, 0x8A),
            accent: rgb(0x64, 0x28, 0xFA),
            accent_hi: rgb(0x7B, 0x48, 0xFB),
            accent_lo: rgb(0x50, 0x20, 0xC8),
            on_accent: rgb(0xFF, 0xFF, 0xFF),
            ok: rgb(0x1A, 0x8A, 0x35),
            danger: rgb(0xC4, 0x2B, 0x45),
            warn: rgb(0x9A, 0x62, 0x00),
            line: [0, 0, 0, 28],
            hover: [0, 0, 0, 16],
        }
    }

    pub fn for_base(base: Base) -> Self {
        match base {
            Base::Dark => Self::dark(),
            Base::Light => Self::light(),
        }
    }
}

/// A named theme: its colors, resolved against its base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub base: Base,
    pub colors: ThemeColors,
}

/// A theme file, as written by a person.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTheme {
    name: String,
    base: Base,
    #[serde(default)]
    colors: RawColors,
}

impl Theme {
    /// Parse a theme file. Colors it doesn't set come from its base palette.
    pub fn parse(text: &str) -> Result<Theme> {
        let raw: RawTheme = toml::from_str(text).map_err(|e| toml_error(text, &e))?;
        let name = raw.name.trim().to_string();
        if name.is_empty() {
            bail!("`name` must not be empty");
        }
        if RESERVED.contains(&name.to_lowercase().as_str()) {
            bail!("`name` {name:?} is reserved: system, light and dark are the built-in choices");
        }
        let colors = ThemeColors::for_base(raw.base).overlaid(&raw.colors)?;
        Ok(Theme {
            name,
            base: raw.base,
            colors,
        })
    }
}

/// A TOML error as one line - "line 4: unknown field `acent`, expected one of
/// ..." - instead of the parser's multi-line source excerpt, which doesn't
/// belong in a settings page.
fn toml_error(text: &str, e: &toml::de::Error) -> anyhow::Error {
    let line = e
        .span()
        .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
    match line {
        Some(l) => anyhow::anyhow!("not a valid theme file: line {l}: {}", e.message()),
        None => anyhow::anyhow!("not a valid theme file: {}", e.message()),
    }
}

/// `#RGB`, `#RRGGBB` or `#RRGGBBAA` (the leading `#` is optional).
fn parse_color(s: &str) -> Result<Rgba> {
    let hex = s.trim().trim_start_matches('#');
    let bad = || format!("invalid color {s:?}: expected #RRGGBB (or #RGB, or #RRGGBBAA)");
    if !hex.is_ascii() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(bad());
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
    let nibble = |i: usize| {
        let n = u8::from_str_radix(&hex[i..i + 1], 16).unwrap_or(0);
        n << 4 | n
    };
    match hex.len() {
        3 => Ok([nibble(0), nibble(1), nibble(2), 255]),
        6 => Ok([byte(0), byte(2), byte(4), 255]),
        8 => Ok([byte(0), byte(2), byte(4), byte(6)]),
        _ => bail!(bad()),
    }
}

/// WCAG relative luminance of an sRGB color (alpha ignored).
pub fn luminance(c: Rgba) -> f64 {
    let lin = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG contrast ratio between two opaque colors (1:1 .. 21:1).
pub fn contrast(a: Rgba, b: Rgba) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Legibility problems in `c`, as sentences for the person writing the theme
/// (empty when it reads fine). Deliberately more forgiving than the bar the
/// bundled themes are held to: this warns, it never refuses.
pub fn contrast_issues(c: &ThemeColors) -> Vec<String> {
    let mut out = Vec::new();
    for (sname, surface) in [
        ("nav_bg", c.nav_bg),
        ("panel", c.panel),
        ("panel2", c.panel2),
    ] {
        for (fname, fg, min) in [("text", c.text, 4.5), ("dim", c.dim, 3.0)] {
            let got = contrast(fg, surface);
            if got < min {
                out.push(format!(
                    "{fname} on {sname} is hard to read ({got:.1}:1; aim for {min}:1 or more)"
                ));
            }
        }
    }
    for (aname, accent) in [
        ("accent", c.accent),
        ("accent_hi", c.accent_hi),
        ("accent_lo", c.accent_lo),
    ] {
        let got = contrast(c.on_accent, accent);
        if got < 3.0 {
            out.push(format!(
                "on_accent over {aname} is hard to read ({got:.1}:1; aim for 3:1 or more)"
            ));
        }
    }
    out
}

/// The bundled themes: file name (for messages) and contents.
const BUNDLED: [(&str, &str); 3] = [
    (
        "dracula.toml",
        include_str!("../assets/themes/dracula.toml"),
    ),
    ("nord.toml", include_str!("../assets/themes/nord.toml")),
    (
        "tokyo-night.toml",
        include_str!("../assets/themes/tokyo-night.toml"),
    ),
];

/// Where a person puts their own theme files.
pub fn themes_dir() -> PathBuf {
    crate::config::config_home()
        .join("protondrive4linux")
        .join("themes")
}

/// Every theme available: the bundled ones plus the person's own, and a
/// sentence for each file that couldn't be used (one bad file never hides the
/// rest).
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub themes: Vec<Theme>,
    pub problems: Vec<String>,
}

impl Catalog {
    /// The bundled themes plus every `*.toml` in `user_dir`. A user theme with
    /// the same name as an earlier one (case-insensitively) replaces it, so a
    /// bundled preset can be tweaked by dropping in a file of the same name.
    pub fn load(user_dir: &Path) -> Catalog {
        let mut cat = Catalog::default();
        for (file, text) in BUNDLED {
            match Theme::parse(text) {
                Ok(t) => cat.add(t),
                Err(e) => cat.problems.push(format!("bundled {file}: {e:#}")),
            }
        }
        let Ok(entries) = std::fs::read_dir(user_dir) else {
            return cat;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "toml"))
            .collect();
        files.sort();
        for path in files {
            let shown = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match read_theme(&path) {
                Ok(t) => cat.add(t),
                Err(e) => cat.problems.push(format!("{shown}: {e:#}")),
            }
        }
        cat
    }

    fn add(&mut self, theme: Theme) {
        match self
            .themes
            .iter_mut()
            .find(|t| t.name.eq_ignore_ascii_case(&theme.name))
        {
            Some(existing) => *existing = theme,
            None => self.themes.push(theme),
        }
    }

    /// A theme by name, ignoring case.
    pub fn find(&self, name: &str) -> Option<&Theme> {
        self.themes
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name.trim()))
    }
}

fn read_theme(path: &Path) -> Result<Theme> {
    let len = std::fs::metadata(path)?.len();
    if len > MAX_THEME_FILE {
        bail!("too big to be a theme file ({len} bytes)");
    }
    Theme::parse(&std::fs::read_to_string(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "protondrive4linux-themes-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- contrast bar -------------------------------------------------------
    // The same bar for the two built-in palettes and the bundled themes, so a
    // theme shipped with the app is never harder to read than the app itself.

    fn surfaces(c: &ThemeColors) -> [(&'static str, Rgba); 4] {
        [
            ("nav_bg", c.nav_bg),
            ("panel", c.panel),
            ("panel2", c.panel2),
            ("sel", c.sel),
        ]
    }

    /// Every `(foreground, minimum ratio)` must hold on the first `n` surfaces
    /// (nav_bg, panel, panel2, sel - in that order).
    fn check(c: &ThemeColors, name: &str, n: usize, fgs: &[(&str, Rgba, f64)]) {
        for (sname, surface) in surfaces(c).into_iter().take(n) {
            for (fname, fg, min) in fgs {
                let got = contrast(*fg, surface);
                assert!(
                    got >= *min,
                    "{name}: {fname} on {sname} is {got:.2}:1, needs >= {min}:1"
                );
            }
        }
    }

    fn assert_meets_the_bar(name: &str, c: &ThemeColors) {
        // Everything that can sit on a selected/hovered row too.
        check(
            c,
            name,
            4,
            &[
                ("text", c.text, 7.0),
                ("dim", c.dim, 4.5),
                // status colors are drawn as small text and icons
                ("ok", c.ok, 3.0),
                ("danger", c.danger, 3.0),
                ("warn", c.warn, 3.0),
            ],
        );
        // Tertiary captions live on the sidebar, page and cards, never on a
        // selected row (the app's own dark palette sits near 3:1 there).
        check(c, name, 3, &[("dim2", c.dim2, 3.0)]);
        for (aname, accent) in [
            ("accent", c.accent),
            ("accent_hi", c.accent_hi),
            ("accent_lo", c.accent_lo),
        ] {
            let got = contrast(c.on_accent, accent);
            assert!(
                got >= 4.5,
                "{name}: on_accent over {aname} is {got:.2}:1, needs >= 4.5:1"
            );
        }
    }

    #[test]
    fn the_builtin_palettes_meet_the_bar() {
        assert_meets_the_bar("dark", &ThemeColors::dark());
        assert_meets_the_bar("light", &ThemeColors::light());
    }

    #[test]
    fn the_two_builtin_palettes_really_differ() {
        let (d, l) = (ThemeColors::dark(), ThemeColors::light());
        assert!(luminance(d.panel) < 0.05, "dark page background is dark");
        assert!(luminance(l.panel) > 0.8, "light page background is light");
        assert!(luminance(d.text) > luminance(d.panel));
        assert!(luminance(l.text) < luminance(l.panel));
        assert_eq!(ThemeColors::for_base(Base::Dark), d);
        assert_eq!(ThemeColors::for_base(Base::Light), l);
    }

    #[test]
    fn every_bundled_theme_loads_and_meets_the_bar() {
        let cat = Catalog::load(Path::new("/nonexistent/themes/dir"));
        assert!(cat.problems.is_empty(), "{:?}", cat.problems);
        let names: Vec<&str> = cat.themes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Dracula", "Nord", "Tokyo Night"]);
        for t in &cat.themes {
            assert_meets_the_bar(&t.name, &t.colors);
            assert!(
                contrast_issues(&t.colors).is_empty(),
                "{}: {:?}",
                t.name,
                contrast_issues(&t.colors)
            );
        }
    }

    #[test]
    fn bundled_themes_actually_set_their_own_colors() {
        // A preset that silently fell back to the base would look like the
        // built-in theme under another name.
        let cat = Catalog::load(Path::new("/nonexistent/themes/dir"));
        for t in &cat.themes {
            assert_ne!(t.colors, ThemeColors::for_base(t.base), "{}", t.name);
            assert_ne!(
                t.colors.panel,
                ThemeColors::for_base(t.base).panel,
                "{}",
                t.name
            );
            assert_ne!(
                t.colors.accent,
                ThemeColors::for_base(t.base).accent,
                "{}",
                t.name
            );
        }
    }

    // --- parsing ------------------------------------------------------------

    #[test]
    fn a_theme_can_be_two_lines_and_inherits_its_base() {
        let t = Theme::parse("name = \"Plain\"\nbase = \"light\"\n").unwrap();
        assert_eq!(t.name, "Plain");
        assert_eq!(t.base, Base::Light);
        assert_eq!(t.colors, ThemeColors::light());
    }

    #[test]
    fn colors_override_only_what_they_name() {
        let t = Theme::parse(
            "name = \"X\"\nbase = \"dark\"\n[colors]\naccent = \"#FF0000\"\nline = \"#ffffff40\"\n",
        )
        .unwrap();
        let base = ThemeColors::dark();
        assert_eq!(t.colors.accent, [255, 0, 0, 255]);
        assert_eq!(t.colors.line, [255, 255, 255, 0x40]);
        assert_eq!(
            t.colors.panel, base.panel,
            "untouched colors come from the base"
        );
        assert_eq!(t.colors.text, base.text);
    }

    #[test]
    fn color_syntax_is_forgiving_about_form_and_strict_about_content() {
        assert_eq!(parse_color("#f80").unwrap(), [0xff, 0x88, 0x00, 255]);
        assert_eq!(parse_color("FF8800").unwrap(), [0xff, 0x88, 0x00, 255]);
        assert_eq!(parse_color(" #12ab34 ").unwrap(), [0x12, 0xab, 0x34, 255]);
        assert_eq!(parse_color("#12ab3480").unwrap(), [0x12, 0xab, 0x34, 0x80]);
        for bad in [
            "",
            "#",
            "#12",
            "#12345",
            "#1234567",
            "#gg0000",
            "red",
            "#ff00zz",
            "#é1é2é3",
        ] {
            assert!(parse_color(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn mistakes_are_errors_that_name_the_problem() {
        let err = |t: &str| format!("{:#}", Theme::parse(t).expect_err("must not parse"));
        // a typo in a color name
        assert!(err("name=\"X\"\nbase=\"dark\"\n[colors]\nacent=\"#fff\"\n").contains("acent"));
        // a typo at the top level
        assert!(err("name=\"X\"\nbase=\"dark\"\nbse=\"x\"\n").contains("bse"));
        // a bad color names the key and the value
        let e = err("name=\"X\"\nbase=\"dark\"\n[colors]\nsel=\"nope\"\n");
        assert!(e.contains("colors.sel") && e.contains("nope"), "{e}");
        // missing required keys
        assert!(err("base=\"dark\"\n").contains("name"));
        assert!(err("name=\"X\"\n").contains("base"));
        // unknown base
        assert!(err("name=\"X\"\nbase=\"sepia\"\n").contains("sepia"));
        // an unusable name
        assert!(err("name=\"  \"\nbase=\"dark\"\n").contains("name"));
        for reserved in ["System", "light", " DARK "] {
            let e = err(&format!("name=\"{reserved}\"\nbase=\"dark\"\n"));
            assert!(e.contains("reserved"), "{reserved}: {e}");
        }
    }

    #[test]
    fn a_toml_mistake_is_one_line_with_its_line_number() {
        let msg = format!(
            "{:#}",
            Theme::parse("name = \"X\"\nbase = \"dark\"\n[colors]\nacent = \"#fff\"\n")
                .expect_err("typo must not parse")
        );
        assert!(!msg.contains('\n'), "must be a single line: {msg:?}");
        assert!(msg.contains("line 4") && msg.contains("acent"), "{msg}");
    }

    #[test]
    fn unreadable_themes_are_flagged_not_refused() {
        let mut c = ThemeColors::dark();
        assert!(contrast_issues(&c).is_empty());
        c.text = c.panel; // invisible text
        c.on_accent = c.accent;
        let issues = contrast_issues(&c);
        assert!(
            issues.iter().any(|i| i.starts_with("text on panel")),
            "{issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.starts_with("on_accent over accent")),
            "{issues:?}"
        );
    }

    // --- the catalog --------------------------------------------------------

    #[test]
    fn user_themes_are_added_and_can_override_a_bundled_one() {
        let dir = temp_dir("catalog");
        std::fs::write(
            dir.join("solarized.toml"),
            "name = \"Solarized\"\nbase = \"dark\"\n[colors]\npanel = \"#002B36\"\n",
        )
        .unwrap();
        // Same name as a bundled preset, different case: replaces it.
        std::fs::write(
            dir.join("my-nord.toml"),
            "name = \"NORD\"\nbase = \"dark\"\n[colors]\naccent = \"#FF00FF\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not a theme").unwrap();

        let cat = Catalog::load(&dir);
        assert!(cat.problems.is_empty(), "{:?}", cat.problems);
        let names: Vec<&str> = cat.themes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Dracula", "NORD", "Tokyo Night", "Solarized"]);
        assert_eq!(cat.find("nord").unwrap().colors.accent, [255, 0, 255, 255]);
        assert_eq!(
            cat.find(" solarized ").unwrap().colors.panel,
            [0x00, 0x2B, 0x36, 255]
        );
        assert!(cat.find("nope").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_bad_file_is_reported_and_the_rest_still_load() {
        let dir = temp_dir("problems");
        std::fs::write(
            dir.join("a-good.toml"),
            "name = \"Good\"\nbase = \"light\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b-broken.toml"),
            "name = \"Broken\"\nbase = \"dark\"\n[colors]\ntext = \"zzz\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("c-reserved.toml"),
            "name = \"Dark\"\nbase = \"dark\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("d-huge.toml"), "#".repeat(70 * 1024)).unwrap();

        let cat = Catalog::load(&dir);
        assert!(cat.find("Good").is_some(), "the good file still loads");
        assert!(cat.find("Broken").is_none());
        assert_eq!(cat.problems.len(), 3, "{:?}", cat.problems);
        assert!(cat.problems[0].starts_with("b-broken.toml") && cat.problems[0].contains("zzz"));
        assert!(
            cat.problems[1].starts_with("c-reserved.toml") && cat.problems[1].contains("reserved")
        );
        assert!(cat.problems[2].starts_with("d-huge.toml") && cat.problems[2].contains("too big"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_themes_dir_is_just_the_bundled_set() {
        let cat = Catalog::load(Path::new("/nonexistent/themes/dir"));
        assert_eq!(cat.themes.len(), BUNDLED.len());
    }
}
