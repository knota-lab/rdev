use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{CellPos, TextSelection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UiLogLine {
    stream: LogStream,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LogStream {
    Stdout,
    Stderr,
    Rdev,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenderLogRow {
    pub(super) plain: String,
    segments: Vec<StyledTextSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StyledTextSegment {
    text: String,
    style: Style,
}

impl UiLogLine {
    pub(super) fn stdout(text: impl Into<String>) -> Self {
        Self {
            stream: LogStream::Stdout,
            text: text.into(),
        }
    }

    pub(super) fn stderr(text: impl Into<String>) -> Self {
        Self {
            stream: LogStream::Stderr,
            text: text.into(),
        }
    }

    pub(super) fn rdev(text: impl Into<String>) -> Self {
        Self {
            stream: LogStream::Rdev,
            text: text.into(),
        }
    }

    #[cfg(test)]
    fn copy_text(&self) -> String {
        let text = strip_ansi_escapes(&self.text);
        match self.stream {
            LogStream::Stdout => format!("[stdout] {text}"),
            LogStream::Stderr => format!("[stderr] {text}"),
            LogStream::Rdev => text,
        }
    }

    fn render_segments(&self) -> Vec<StyledTextSegment> {
        let mut segments = Vec::new();
        match self.stream {
            LogStream::Stdout => push_styled_text(
                &mut segments,
                "[stdout] ",
                Style::default().fg(Color::DarkGray),
            ),
            LogStream::Stderr => {
                push_styled_text(&mut segments, "[stderr] ", Style::default().fg(Color::Red))
            }
            LogStream::Rdev => {}
        }
        segments.extend(ansi_styled_segments(&self.text, Style::default()));
        segments
    }
}

pub(super) fn selected_line(
    log_row: &RenderLogRow,
    row: usize,
    selection: Option<TextSelection>,
) -> Line<'_> {
    let text = log_row.plain.as_str();
    let Some(selection) = selection else {
        return styled_line(log_row);
    };
    let (start, end) = ordered_selection(selection);
    if row < start.row || row > end.row {
        return styled_line(log_row);
    }

    let width = UnicodeWidthStr::width(text) as u16;
    let start_col = if row == start.row {
        start.col.min(width)
    } else {
        0
    };
    let end_col = if row == end.row {
        end.col.min(width)
    } else {
        width
    };
    if start_col == end_col {
        return Line::from(Span::raw(text.to_owned()));
    }
    let start_index = byte_index_for_display_col(text, start_col);
    let end_index = byte_index_for_display_col(text, end_col);
    Line::from(vec![
        Span::raw(text[..start_index].to_owned()),
        Span::styled(
            text[start_index..end_index].to_owned(),
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::raw(text[end_index..].to_owned()),
    ])
}

fn styled_line(log_row: &RenderLogRow) -> Line<'_> {
    Line::from(
        log_row
            .segments
            .iter()
            .map(|segment| Span::styled(segment.text.clone(), segment.style))
            .collect::<Vec<_>>(),
    )
}

pub(super) fn wrapped_log_rows(logs: &[UiLogLine], width: u16) -> Vec<RenderLogRow> {
    let width = width.max(1);
    logs.iter()
        .flat_map(|line| wrap_styled_line(&line.render_segments(), width))
        .collect()
}

#[cfg(test)]
fn wrap_display_line(text: &str, width: u16) -> Vec<String> {
    wrap_styled_line(
        &[StyledTextSegment {
            text: text.to_owned(),
            style: Style::default(),
        }],
        width,
    )
    .into_iter()
    .map(|row| row.plain)
    .collect()
}

fn wrap_styled_line(segments: &[StyledTextSegment], width: u16) -> Vec<RenderLogRow> {
    let width = width.max(1);
    if segments.iter().all(|segment| segment.text.is_empty()) {
        return vec![RenderLogRow {
            plain: String::new(),
            segments: Vec::new(),
        }];
    }

    let mut rows = Vec::new();
    let mut current = RenderLogRow {
        plain: String::new(),
        segments: Vec::new(),
    };
    let mut current_width = 0u16;
    for segment in segments {
        for ch in segment.text.chars() {
            let char_width = ch.width().unwrap_or(0) as u16;
            if current_width > 0 && current_width.saturating_add(char_width) > width {
                rows.push(current);
                current = RenderLogRow {
                    plain: String::new(),
                    segments: Vec::new(),
                };
                current_width = 0;
            }
            current.plain.push(ch);
            push_styled_char(&mut current.segments, ch, segment.style);
            current_width = current_width.saturating_add(char_width);
        }
    }
    rows.push(current);
    rows
}

pub(super) fn parse_log_line(line: &str) -> UiLogLine {
    if let Some(text) = line.strip_prefix("[stdout] ") {
        UiLogLine::stdout(text)
    } else if let Some(text) = line.strip_prefix("[stderr] ") {
        UiLogLine::stderr(text)
    } else {
        UiLogLine::rdev(line)
    }
}

fn ansi_styled_segments(text: &str, base_style: Style) -> Vec<StyledTextSegment> {
    let mut segments = Vec::new();
    let mut style = base_style;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            push_styled_char(&mut segments, ch, style);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        if next == 'm' {
                            style = apply_sgr(&params, base_style, style);
                        }
                        break;
                    }
                    params.push(next);
                }
            }
            Some(']') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if next == '\u{7}' || (previous == '\u{1b}' && next == '\\') {
                        break;
                    }
                    previous = next;
                }
            }
            _ => {}
        }
    }
    segments
}

fn apply_sgr(params: &str, base_style: Style, current_style: Style) -> Style {
    let mut style = current_style;
    let params = if params.is_empty() { "0" } else { params };
    for param in params.split(';') {
        let code = param.parse::<u16>().unwrap_or(0);
        match code {
            0 => style = base_style,
            1 => style = style.add_modifier(Modifier::BOLD),
            30 | 90 => style = style.fg(Color::DarkGray),
            31 | 91 => style = style.fg(Color::Red),
            32 | 92 => style = style.fg(Color::Green),
            33 | 93 => style = style.fg(Color::Yellow),
            34 | 94 => style = style.fg(Color::Blue),
            35 | 95 => style = style.fg(Color::Magenta),
            36 | 96 => style = style.fg(Color::Cyan),
            37 | 97 => style = style.fg(Color::White),
            _ => {}
        }
    }
    style
}

fn push_styled_text(segments: &mut Vec<StyledTextSegment>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = segments.last_mut() {
        if last.style == style {
            last.text.push_str(text);
            return;
        }
    }
    segments.push(StyledTextSegment {
        text: text.to_owned(),
        style,
    });
}

fn push_styled_char(segments: &mut Vec<StyledTextSegment>, ch: char, style: Style) {
    if let Some(last) = segments.last_mut() {
        if last.style == style {
            last.text.push(ch);
            return;
        }
    }
    segments.push(StyledTextSegment {
        text: ch.to_string(),
        style,
    });
}

#[cfg(test)]
fn strip_ansi_escapes(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            output.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if next == '\u{7}' || (previous == '\u{1b}' && next == '\\') {
                        break;
                    }
                    previous = next;
                }
            }
            _ => {}
        }
    }
    output
}

fn ordered_selection(selection: TextSelection) -> (CellPos, CellPos) {
    if selection.anchor <= selection.cursor {
        (selection.anchor, selection.cursor)
    } else {
        (selection.cursor, selection.anchor)
    }
}

fn byte_index_for_display_col(text: &str, col: u16) -> usize {
    let mut width = 0u16;
    for (index, ch) in text.char_indices() {
        let char_width = ch.width().unwrap_or(0) as u16;
        if width.saturating_add(char_width) > col {
            return index;
        }
        width = width.saturating_add(char_width);
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::{
        ansi_styled_segments, parse_log_line, strip_ansi_escapes, wrap_display_line,
        wrap_styled_line,
    };
    use ratatui::style::{Color, Style};

    #[test]
    fn log_wrapping_respects_display_width() {
        assert_eq!(
            wrap_display_line("abcdef", 3),
            vec!["abc".to_owned(), "def".to_owned()]
        );
        assert_eq!(
            wrap_display_line("中文ab", 4),
            vec!["中文".to_owned(), "ab".to_owned()]
        );
    }

    #[test]
    fn strips_ansi_escape_sequences() {
        assert_eq!(
            strip_ansi_escapes("\u{1b}[32mgreen\u{1b}[0m text"),
            "green text"
        );
    }

    #[test]
    fn parses_ansi_sgr_colors_for_rendering() {
        let segments = ansi_styled_segments("\u{1b}[32mgreen\u{1b}[0m text", Style::default());

        assert_eq!(segments[0].text, "green");
        assert_eq!(segments[0].style.fg, Some(Color::Green));
        assert_eq!(segments[1].text, " text");
        assert_eq!(segments[1].style, Style::default());
    }

    #[test]
    fn wrapped_styled_logs_keep_plain_text_without_escape_codes() {
        let segments = ansi_styled_segments("\u{1b}[31mabcdef\u{1b}[0m", Style::default());
        let rows = wrap_styled_line(&segments, 3);

        assert_eq!(rows[0].plain, "abc");
        assert_eq!(rows[0].segments[0].style.fg, Some(Color::Red));
        assert_eq!(rows[1].plain, "def");
        assert_eq!(rows[1].segments[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn parsed_log_copy_text_strips_ansi_but_render_keeps_style() {
        let line = parse_log_line("[stdout] \u{1b}[33mready\u{1b}[0m");

        assert_eq!(line.copy_text(), "[stdout] ready");
        let segments = line.render_segments();
        assert_eq!(segments[0].text, "[stdout] ");
        assert_eq!(segments[1].text, "ready");
        assert_eq!(segments[1].style.fg, Some(Color::Yellow));
    }
}
