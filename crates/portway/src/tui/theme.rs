//! What the dashboard is drawn in, by role. The view asks for `dim` or `wire`,
//! never for a color, so a theme is a palette and nothing else.

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
    /// Behind a highlighted line. `None` for a theme that paints no
    /// background, where the line is reversed instead.
    pub select: Option<Color>,
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
};

impl Theme {
    /// The highlighted line of a list.
    pub fn cursor(&self) -> Style {
        match self.select {
            Some(select) => Style::new().bg(select),
            None => Style::new().add_modifier(Modifier::REVERSED),
        }
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
