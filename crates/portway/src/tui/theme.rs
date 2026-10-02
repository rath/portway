//! What the dashboard is drawn in, by role. The view asks for `dim` or `wire`,
//! never for a color, so a theme is a palette and nothing else.
//!
//! Besides the terminal's own colors, the themes are the web console's, under
//! the same ids and with the same values (`webui/static/js/themes.js`; a test
//! holds the two together). Its palettes are tuned from the published ones
//! where those fall short of its contrast tests, which therefore hold here
//! too.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, PartialEq, Eq)]
pub struct Theme {
    /// What `portway.tui` calls it.
    pub id: &'static str,
    pub name: &'static str,
    /// Behind the panes, and behind a popup over them.
    pub surface: Color,
    pub raised: Color,
    pub border: Color,
    /// Text with no role of its own, and text that only labels another.
    pub text: Color,
    pub dim: Color,
    /// The chrome a reader acts on: keys, popup borders, the window in force.
    pub accent: Color,
    pub on_accent: Color,
    /// The body before compression, and the bytes that actually left.
    pub raw: Color,
    pub wire: Color,
    pub good: Color,
    pub time: Color,
    pub bad: Color,
    pub model: Color,
    /// Behind a highlighted line, on the surface and on a popup. `None` for a
    /// theme that paints no background, where the line is reversed instead.
    pub select: Option<Color>,
    pub select_raised: Option<Color>,
}

/// The 16 terminal colors only: the dashboard sits inside whatever theme the
/// terminal already has, paints no background of its own, and reverses a
/// highlighted line rather than choosing a color to put behind it.
pub static TERMINAL: Theme = Theme {
    id: "terminal",
    name: "Terminal",
    surface: Color::Reset,
    raised: Color::Reset,
    border: Color::DarkGray,
    text: Color::Reset,
    dim: Color::DarkGray,
    accent: Color::Cyan,
    on_accent: Color::Reset,
    raw: Color::Blue,
    wire: Color::Cyan,
    good: Color::Green,
    time: Color::Yellow,
    bad: Color::Red,
    model: Color::Magenta,
    select: None,
    select_raised: None,
};

/// Every theme, the terminal's own first.
pub static THEMES: [&Theme; 8] = [
    &TERMINAL,
    &PORTWAY_DARK,
    &CATPPUCCIN_MOCHA,
    &CATPPUCCIN_LATTE,
    &TOKYO_NIGHT,
    &NORD,
    &DRACULA,
    &GRUVBOX,
];

pub static PORTWAY_DARK: Theme = console(
    "portway-dark",
    "Portway Dark",
    [
        0x0e1014, 0x15181e, 0x1d2129, 0x2c323d, 0x232833, 0xe6e9ef, 0x9aa3b2, 0x78a9ff, 0x0b1020,
        0x8ab8ff, 0x6b98f0, 0x27bfa0, 0x4fc46e, 0xe2b342, 0xff6b6b, 0xd08cf5,
    ],
);
pub static CATPPUCCIN_MOCHA: Theme = console(
    "catppuccin-mocha",
    "Catppuccin Mocha",
    [
        0x181825, 0x1e1e2e, 0x313244, 0x45475a, 0x313244, 0xcdd6f4, 0xa6adc8, 0x89b4fa, 0x11111b,
        0xb4befe, 0x7f9af5, 0x4fd0b4, 0xa6e3a1, 0xf9e2af, 0xf38ba8, 0xcba6f7,
    ],
);
pub static CATPPUCCIN_LATTE: Theme = console(
    "catppuccin-latte",
    "Catppuccin Latte",
    [
        0xe6e9ef, 0xeff1f5, 0xe6e9ef, 0xccd0da, 0xdce0e8, 0x4c4f69, 0x50536b, 0x1a55cc, 0xffffff,
        0x5b6ee0, 0x2f5fc4, 0x006b5c, 0x276b1a, 0x7f4800, 0xb80d33, 0x7a2fd6,
    ],
);
pub static TOKYO_NIGHT: Theme = console(
    "tokyo-night",
    "Tokyo Night",
    [
        0x16161e, 0x1a1b26, 0x24283b, 0x2f3549, 0x232433, 0xc0caf5, 0x9aa5ce, 0x7aa2f7, 0x16161e,
        0x7dcfff, 0x7a95f5, 0x35c7ad, 0x9ece6a, 0xe0af68, 0xf7768e, 0xbb9af7,
    ],
);
pub static NORD: Theme = console(
    "nord",
    "Nord",
    [
        0x2e3440, 0x2e3440, 0x3b4252, 0x4c566a, 0x3b4252, 0xeceff4, 0xc0c8d6, 0x9fd3e0, 0x2e3440,
        0x88c0d0, 0x8fb0f0, 0x62d0a8, 0xb4cf9c, 0xebcb8b, 0xf0a0a6, 0xd8b6d3,
    ],
);
pub static DRACULA: Theme = console(
    "dracula",
    "Dracula",
    [
        0x282a36, 0x21222c, 0x343746, 0x44475a, 0x343746, 0xf8f8f2, 0xb4bbdc, 0xbd93f9, 0x21222c,
        0xff79c6, 0x9d9bff, 0x2cc5d6, 0x50fa7b, 0xf1fa8c, 0xff7a7a, 0xff79c6,
    ],
);
pub static GRUVBOX: Theme = console(
    "gruvbox",
    "Gruvbox",
    [
        0x282828, 0x282828, 0x32302f, 0x504945, 0x3c3836, 0xebdbb2, 0xbdae93, 0x8ec07c, 0x282828,
        0xfabd2f, 0x83a5f0, 0x4fbfa8, 0xb8bb26, 0xfabd2f, 0xff6f5c, 0xe29bb0,
    ],
);

/// A console palette, its sixteen tokens in the order `themes.js` lists them:
/// bg, surface, surface-2, border, grid, text, text-dim, accent, on-accent,
/// focus, raw, wire, good, warn, bad, model. The page behind the cards, the
/// chart grid and the focus ring have nothing to color here.
///
/// A fixed foreground is only legible on the background it was chosen for, so
/// every one of these paints its own.
const fn console(id: &'static str, name: &'static str, tokens: [u32; 16]) -> Theme {
    Theme {
        id,
        name,
        surface: rgb(tokens[1]),
        raised: rgb(tokens[2]),
        border: rgb(tokens[3]),
        text: rgb(tokens[5]),
        dim: rgb(tokens[6]),
        accent: rgb(tokens[7]),
        on_accent: rgb(tokens[8]),
        raw: rgb(tokens[10]),
        wire: rgb(tokens[11]),
        good: rgb(tokens[12]),
        time: rgb(tokens[13]),
        bad: rgb(tokens[14]),
        model: rgb(tokens[15]),
        select: Some(select(tokens[7], tokens[1])),
        select_raised: Some(select(tokens[7], tokens[2])),
    }
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// A quarter of the accent mixed into the ground a highlighted line sits on,
/// channel by channel as `color-mix(in srgb, …)` mixes. The console's own
/// selection takes an eighth, beside a pointer and a focus ring; here the
/// tint is the only mark of where the cursor is.
const fn select(accent: u32, ground: u32) -> Color {
    Color::Rgb(
        blend(accent >> 16, ground >> 16),
        blend(accent >> 8, ground >> 8),
        blend(accent, ground),
    )
}

const fn blend(accent: u32, ground: u32) -> u8 {
    (((accent & 0xff) + (ground & 0xff) * 3 + 2) / 4) as u8
}

fn highlight(select: Option<Color>) -> Style {
    match select {
        Some(select) => Style::new().bg(select),
        None => Style::new().add_modifier(Modifier::REVERSED),
    }
}

impl Theme {
    /// The highlighted line of a pane.
    pub fn cursor(&self) -> Style {
        highlight(self.select)
    }

    /// The highlighted line of a popup's list.
    pub fn raised_cursor(&self) -> Style {
        highlight(self.select_raised)
    }

    /// The one option in force among several, set on the accent.
    pub fn chosen(&self) -> Style {
        let style = Style::new().add_modifier(Modifier::BOLD);
        match self.select {
            Some(_) => style.fg(self.on_accent).bg(self.accent),
            None => style.fg(self.accent).add_modifier(Modifier::REVERSED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The console's rows out of `themes.js`: each `theme("id", "Name", …)`
    /// line, its id and name and the sixteen hex values after them.
    fn console_rows(source: &str) -> Vec<(String, String, [u32; 16])> {
        source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("theme(\""))
            .map(|line| {
                let quoted: Vec<&str> = line.split('"').collect();
                let hex: Vec<u32> = quoted
                    .iter()
                    .filter_map(|part| part.strip_prefix('#'))
                    .map(|hex| u32::from_str_radix(hex, 16).unwrap())
                    .collect();
                (
                    quoted[0].to_string(),
                    quoted[2].to_string(),
                    hex[..16].try_into().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn the_painted_themes_are_the_consoles() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/webui/static/js/themes.js"
        ))
        .unwrap();
        // The order `console` reads the tokens in.
        assert!(source.contains(
            r#""bg", "surface", "surface-2", "border", "grid", "text", "text-dim",
  "accent", "on-accent", "focus", "raw", "wire", "good", "warn", "bad", "model","#
        ));
        let rows = console_rows(&source);
        assert!(rows.len() >= 15, "{rows:?}");
        for theme in &THEMES[1..] {
            let (_, name, tokens) = rows
                .iter()
                .find(|(id, _, _)| id == theme.id)
                .unwrap_or_else(|| panic!("{} is not a console theme", theme.id));
            assert_eq!(theme.name, name);
            assert_eq!(**theme, console(theme.id, theme.name, *tokens));
        }
    }

    #[test]
    fn every_theme_has_its_own_id_and_the_terminals_comes_first() {
        assert_eq!(THEMES[0], &TERMINAL);
        for (at, theme) in THEMES.iter().enumerate() {
            assert!(
                THEMES[..at].iter().all(|other| other.id != theme.id),
                "{} twice",
                theme.id
            );
        }
    }

    #[test]
    fn a_highlighted_line_mixes_the_accent_into_its_ground() {
        // (0x89 + 3 * 0x1e) / 4 = 56.75 on the surface, and so on per channel;
        // (0x89 + 3 * 0x31) / 4 = 71 on a popup's surface-2.
        assert_eq!(CATPPUCCIN_MOCHA.select, Some(Color::Rgb(57, 68, 97)));
        assert_eq!(
            CATPPUCCIN_MOCHA.select_raised,
            Some(Color::Rgb(71, 83, 114))
        );
        assert_eq!(TERMINAL.cursor(), TERMINAL.raised_cursor());
    }
}
