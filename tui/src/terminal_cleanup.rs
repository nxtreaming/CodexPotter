use std::io;
use std::io::Write;

use ratatui::backend::Backend;

/// Clears the current inline viewport so the shell prompt is clean after the TUI exits.
pub fn clear_inline_viewport_for_exit<B>(
    terminal: &mut crate::custom_terminal::Terminal<B>,
) -> io::Result<()>
where
    B: Backend + Write,
{
    terminal.clear()?;
    ratatui::backend::Backend::flush(terminal.backend_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insert_history::insert_history_lines;
    use crate::test_backend::VT100Backend;
    use insta::assert_snapshot;
    use ratatui::layout::Rect;
    use ratatui::text::Line;
    use ratatui::text::Text;
    use ratatui::widgets::Paragraph;
    use ratatui::widgets::Widget;
    use std::path::Path;

    #[test]
    fn clear_inline_viewport_for_exit_clears_pending_ui_vt100() {
        let width: u16 = 40;
        let height: u16 = 6;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");

        terminal.set_viewport_area(Rect::new(0, height - 2, width, 2));

        insert_history_lines(&mut terminal, vec![Line::from("history")]).expect("insert history");

        terminal
            .draw(|frame| {
                let area = frame.area();
                Paragraph::new(Text::from(vec![
                    Line::from("Working"),
                    Line::from("\u{203a} "),
                ]))
                .render(area, frame.buffer_mut());
                frame.set_cursor_position((2, area.y + 1));
            })
            .expect("draw");

        clear_inline_viewport_for_exit(&mut terminal).expect("clear viewport");

        assert_snapshot!(
            "clear_inline_viewport_for_exit_vt100",
            terminal.backend().vt100().screen().contents()
        );

        assert_eq!(
            terminal.backend().vt100().screen().cursor_position(),
            (height - 2, 0)
        );
    }

    #[test]
    fn clear_inline_viewport_for_exit_preserves_startup_history_vt100() {
        let width: u16 = 80;
        let height: u16 = 16;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");

        terminal.set_viewport_area(Rect::new(0, height - 2, width, 2));

        let banner = crate::startup_banner::build_startup_banner_lines(
            width,
            "0.0.0",
            "gpt-5.5 xhigh",
            Path::new("/workspace/CodexPotter"),
        );
        insert_history_lines(&mut terminal, banner).expect("insert banner");

        terminal
            .draw(|frame| {
                let area = frame.area();
                Paragraph::new(Text::from(vec![Line::from(""), Line::from("\u{203a} ")]))
                    .render(area, frame.buffer_mut());
                frame.set_cursor_position((2, area.y + 1));
            })
            .expect("draw");

        clear_inline_viewport_for_exit(&mut terminal).expect("clear viewport");

        assert_snapshot!(
            "clear_inline_viewport_for_exit_preserves_startup_history_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn clear_inline_viewport_for_exit_preserves_first_screen_banner_vt100() {
        let width: u16 = 80;
        let height: u16 = 16;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");

        terminal.set_viewport_area(Rect::new(0, height - 1, 0, 0));

        let viewport_height = 4;
        let mut area = terminal.viewport_area;
        area.height = viewport_height;
        area.width = width;
        if area.bottom() > height {
            terminal
                .backend_mut()
                .scroll_region_up(0..area.top(), area.bottom() - height)
                .expect("scroll viewport into screen");
            area.y = height - area.height;
        }
        if area != terminal.viewport_area {
            terminal.clear().expect("clear old viewport");
            terminal.set_viewport_area(area);
        }

        let banner = crate::startup_banner::build_startup_banner_lines(
            width,
            "0.0.0",
            "gpt-5.5 xhigh",
            Path::new("/workspace/CodexPotter"),
        );
        insert_history_lines(&mut terminal, banner).expect("insert banner");

        terminal
            .draw(|frame| {
                let area = frame.area();
                Paragraph::new(Text::from(vec![
                    Line::from(""),
                    Line::from("\u{203a} Assign new task to CodexPotter"),
                    Line::from(""),
                    Line::from("ctrl+g editor - main > /workspace/CodexPotter"),
                ]))
                .render(area, frame.buffer_mut());
                frame.set_cursor_position((2, area.y + 1));
            })
            .expect("draw prompt");

        clear_inline_viewport_for_exit(&mut terminal).expect("clear viewport");

        assert_snapshot!(
            "clear_inline_viewport_for_exit_preserves_first_screen_banner_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }

    #[test]
    fn clear_inline_viewport_for_exit_does_not_clear_below_viewport_vt100() {
        let width: u16 = 40;
        let height: u16 = 6;
        let backend = VT100Backend::new(width, height);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("create terminal");

        terminal.set_viewport_area(Rect::new(0, 0, width, height));
        terminal
            .draw(|frame| {
                let area = frame.area();
                Paragraph::new(Text::from(vec![
                    Line::from("above one"),
                    Line::from("above two"),
                    Line::from("Working"),
                    Line::from("\u{203a} "),
                    Line::from("below one"),
                    Line::from("below two"),
                ]))
                .render(area, frame.buffer_mut());
            })
            .expect("draw");

        terminal.set_viewport_area(Rect::new(0, 2, width, 2));

        clear_inline_viewport_for_exit(&mut terminal).expect("clear viewport");

        assert_snapshot!(
            "clear_inline_viewport_for_exit_does_not_clear_below_viewport_vt100",
            terminal.backend().vt100().screen().contents()
        );
    }
}
