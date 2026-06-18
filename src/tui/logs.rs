use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{CellPos, TextSelection};

pub(super) const LOG_PREFIX_WIDTH: u16 = 8;

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
    pub(super) starts_log_line: bool,
    prefix: Option<StyledTextSegment>,
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

    fn prefix_segment(&self) -> Option<StyledTextSegment> {
        match self.stream {
            LogStream::Stdout => Some(StyledTextSegment {
                text: "stdout".to_owned(),
                style: Style::default().fg(Color::DarkGray),
            }),
            LogStream::Stderr => Some(StyledTextSegment {
                text: "stderr".to_owned(),
                style: Style::default().fg(Color::Red),
            }),
            LogStream::Rdev => Some(StyledTextSegment {
                text: "rdev".to_owned(),
                style: Style::default().fg(Color::DarkGray),
            }),
        }
    }

    fn render_segments(&self) -> Vec<StyledTextSegment> {
        ansi_styled_segments(&self.text, Style::default())
    }

    #[cfg(test)]
    fn plain_text(&self) -> String {
        strip_ansi_escapes(&self.text)
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
        return Line::from(prefixed_spans(log_row, vec![Span::raw(text.to_owned())]));
    }
    let start_index = byte_index_for_display_col(text, start_col);
    let end_index = byte_index_for_display_col(text, end_col);
    Line::from(prefixed_spans(
        log_row,
        vec![
            Span::raw(text[..start_index].to_owned()),
            Span::styled(
                text[start_index..end_index].to_owned(),
                Style::default().fg(Color::White).bg(Color::DarkGray),
            ),
            Span::raw(text[end_index..].to_owned()),
        ],
    ))
}

fn styled_line(log_row: &RenderLogRow) -> Line<'_> {
    let message = log_row
        .segments
        .iter()
        .map(|segment| Span::styled(segment.text.clone(), segment.style))
        .collect::<Vec<_>>();
    Line::from(prefixed_spans(log_row, message))
}

fn prefixed_spans(log_row: &RenderLogRow, mut message: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(prefix) = &log_row.prefix {
        spans.push(Span::styled(
            format!(
                "{:>width$} ",
                prefix.text,
                width = LOG_PREFIX_WIDTH as usize - 1
            ),
            prefix.style,
        ));
    } else {
        spans.push(Span::raw(" ".repeat(LOG_PREFIX_WIDTH as usize)));
    }
    spans.append(&mut message);
    spans
}

pub(super) fn wrapped_log_rows(logs: &[UiLogLine], width: u16) -> Vec<RenderLogRow> {
    let width = width.max(1);
    logs.iter()
        .flat_map(|line| {
            wrap_styled_line(
                line.prefix_segment(),
                &line.render_segments(),
                message_width(width),
            )
        })
        .collect()
}

#[cfg(test)]
fn wrap_display_line(text: &str, width: u16) -> Vec<String> {
    wrap_styled_line(
        None,
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

fn wrap_styled_line(
    prefix: Option<StyledTextSegment>,
    segments: &[StyledTextSegment],
    width: u16,
) -> Vec<RenderLogRow> {
    let width = width.max(1);
    if segments.iter().all(|segment| segment.text.is_empty()) {
        return vec![RenderLogRow {
            plain: String::new(),
            starts_log_line: true,
            prefix,
            segments: Vec::new(),
        }];
    }

    let mut rows = Vec::new();
    let mut current = RenderLogRow {
        plain: String::new(),
        starts_log_line: true,
        prefix,
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
                    starts_log_line: false,
                    prefix: None,
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

fn message_width(width: u16) -> u16 {
    if width > LOG_PREFIX_WIDTH.saturating_add(4) {
        width.saturating_sub(LOG_PREFIX_WIDTH)
    } else {
        width
    }
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
        wrap_styled_line, wrapped_log_rows,
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
        let rows = wrap_styled_line(None, &segments, 3);

        assert_eq!(rows[0].plain, "abc");
        assert_eq!(rows[0].segments[0].style.fg, Some(Color::Red));
        assert_eq!(rows[1].plain, "def");
        assert_eq!(rows[1].segments[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn parsed_log_copy_text_strips_ansi_but_render_keeps_style() {
        let line = parse_log_line("[stdout] \u{1b}[33mready\u{1b}[0m");

        assert_eq!(line.plain_text(), "ready");
        let rows = wrapped_log_rows(&[line], 80);
        assert_eq!(rows[0].plain, "ready");
        assert_eq!(
            rows[0].prefix.as_ref().map(|prefix| prefix.text.as_str()),
            Some("stdout")
        );
        assert_eq!(rows[0].segments[0].text, "ready");
        assert_eq!(rows[0].segments[0].style.fg, Some(Color::Yellow));
    }
}
