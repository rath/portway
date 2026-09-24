//! The compression chart: one column per request, the raw body behind and the
//! bytes that actually went out in front.
//!
//! An agent's traffic is bursty — one big upload per turn, then nothing — so a
//! time axis mostly draws gaps. Per request, the same columns show the context
//! growing turn by turn and how much of it the compressor kept off the wire.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// Eighth blocks, index 0 = empty cell.
const LADDER: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub struct TwoToneBars<'a> {
    samples: &'a [(u64, u64)],
    raw: Color,
    wire: Color,
}

impl<'a> TwoToneBars<'a> {
    /// `samples` are `(raw, wire)`, oldest first; the newest end is what gets
    /// drawn when there are more of them than columns.
    pub fn new(samples: &'a [(u64, u64)], raw: Color, wire: Color) -> Self {
        TwoToneBars { samples, raw, wire }
    }
}

/// Value to eighths of a cell, with anything non-zero getting at least one so
/// a small request is visible next to a huge one.
fn eighths(value: u64, max: u64, full: u64) -> u64 {
    if value == 0 || max == 0 {
        return 0;
    }
    ((value as u128 * full as u128) / max as u128).max(1) as u64
}

impl Widget for TwoToneBars<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;
        let shown = self.samples.len().min(width);
        if shown == 0 {
            return;
        }
        let visible = &self.samples[self.samples.len() - shown..];
        let Some(max) = visible.iter().map(|(raw, _)| *raw).max().filter(|m| *m > 0) else {
            return;
        };
        let full = area.height as u64 * 8;
        // Right-aligned: the newest request is always the rightmost column.
        let left = area.x + (width - shown) as u16;

        for (column, (raw, wire)) in visible.iter().enumerate() {
            let x = left + column as u16;
            let raw_top = eighths(*raw, max, full);
            let wire_top = eighths((*wire).min(*raw), max, full);
            for row in 0..area.height {
                let y = area.y + area.height - 1 - row;
                let floor = row as u64 * 8;
                let raw_cell = raw_top.saturating_sub(floor).min(8) as usize;
                let wire_cell = wire_top.saturating_sub(floor).min(8) as usize;
                let (symbol, style) = if wire_cell >= 8 {
                    ('█', Style::default().fg(self.wire))
                } else if wire_cell > 0 {
                    // The wire bar ends inside this cell. Filling the rest of
                    // it with the raw color as background continues the raw
                    // bar behind the partial block — exact whenever raw still
                    // covers the whole cell, which is the normal case at these
                    // ratios, and off by at most one cell's top when it does
                    // not.
                    let style = Style::default().fg(self.wire);
                    let style = if raw_cell >= 8 {
                        style.bg(self.raw)
                    } else {
                        style
                    };
                    (LADDER[wire_cell], style)
                } else if raw_cell > 0 {
                    (LADDER[raw_cell], Style::default().fg(self.raw))
                } else {
                    continue;
                };
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(symbol).set_style(style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(samples: &[(u64, u64)], width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        TwoToneBars::new(samples, Color::Blue, Color::Cyan).render(area, &mut buf);
        buf
    }

    fn column(buf: &Buffer, x: u16) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn the_tallest_request_fills_its_column_and_the_wire_part_is_marked() {
        let buf = draw(&[(1000, 250)], 1, 4);
        // Full height for raw; the bottom quarter is the wire bar, so the
        // boundary lands on a cell edge and no blending is needed.
        assert_eq!(column(&buf, 0), vec!["█", "█", "█", "█"]);
        assert_eq!(buf.cell((0, 3)).unwrap().style().fg, Some(Color::Cyan));
        assert_eq!(buf.cell((0, 0)).unwrap().style().fg, Some(Color::Blue));
    }

    #[test]
    fn a_partial_wire_boundary_keeps_the_raw_bar_behind_it() {
        // wire is 1/8 of the bottom cell, raw fills the column.
        let buf = draw(&[(800, 100)], 1, 1);
        let cell = buf.cell((0, 0)).unwrap();
        assert_eq!(cell.symbol(), "▁");
        assert_eq!(cell.style().fg, Some(Color::Cyan));
        assert_eq!(cell.style().bg, Some(Color::Blue), "raw continues behind");
    }

    #[test]
    fn bars_are_right_aligned_and_scaled_against_the_tallest() {
        let buf = draw(&[(100, 50), (400, 100)], 4, 2);
        // Two samples, four columns: the left two stay empty.
        assert_eq!(column(&buf, 0), vec![" ", " "]);
        assert_eq!(column(&buf, 1), vec![" ", " "]);
        // The newest (and tallest) is rightmost: raw fills both cells and its
        // quarter of wire takes the bottom half of the lower one.
        assert_eq!(column(&buf, 3), vec!["█", "▄"]);
        assert_eq!(buf.cell((3, 1)).unwrap().style().bg, Some(Color::Blue));
        // A quarter as tall, half of it wire.
        assert_eq!(column(&buf, 2), vec![" ", "▂"]);
    }

    #[test]
    fn a_tiny_request_still_gets_a_visible_sliver() {
        let buf = draw(&[(1_000_000, 900_000), (1, 1)], 2, 3);
        assert_eq!(buf.cell((1, 2)).unwrap().symbol(), "▁");
    }

    #[test]
    fn degenerate_input_draws_nothing_instead_of_panicking() {
        assert_eq!(column(&draw(&[], 3, 2), 0), vec![" ", " "]);
        assert_eq!(column(&draw(&[(0, 0)], 3, 2), 2), vec![" ", " "]);
        // More samples than columns: the single column is the newest sample,
        // whose raw fills the cell behind a one-eighth sliver of wire.
        let buf = draw(&[(10, 1), (20, 2), (30, 3)], 1, 1);
        let cell = buf.cell((0, 0)).unwrap();
        assert_eq!(cell.symbol(), "▁");
        assert_eq!(cell.style().bg, Some(Color::Blue));
        // A zero-sized area is a no-op.
        let mut empty = Buffer::empty(Rect::new(0, 0, 0, 0));
        TwoToneBars::new(&[(1, 1)], Color::Blue, Color::Cyan)
            .render(Rect::new(0, 0, 0, 0), &mut empty);
    }
}
