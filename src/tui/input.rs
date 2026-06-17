pub(super) fn previous_char_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .last()
        .map_or(0, |(index, _)| index)
}

pub(super) fn next_char_boundary(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    text[cursor..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(index, _)| cursor + index)
}

pub(super) fn previous_word_boundary(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut chars = text[..cursor].char_indices().collect::<Vec<_>>();
    while let Some((index, ch)) = chars.pop() {
        if !ch.is_whitespace() {
            chars.push((index, ch));
            break;
        }
    }
    let Some((_, last)) = chars.last().copied() else {
        return 0;
    };
    let target = word_class(last);
    while let Some((index, ch)) = chars.pop() {
        if word_class(ch) != target {
            return index + ch.len_utf8();
        }
    }
    0
}

pub(super) fn next_word_boundary(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let mut iter = text[cursor..].char_indices().peekable();
    while let Some((_, ch)) = iter.peek().copied() {
        if ch.is_whitespace() {
            iter.next();
        } else {
            break;
        }
    }
    let Some((_, first)) = iter.peek().copied() else {
        return text.len();
    };
    let target = word_class(first);
    for (index, ch) in iter {
        if word_class(ch) != target {
            return cursor + index;
        }
    }
    text.len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Word,
    Symbol,
    Whitespace,
}

fn word_class(ch: char) -> WordClass {
    if ch.is_whitespace() {
        WordClass::Whitespace
    } else if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '\\' | ':' | '~') {
        WordClass::Word
    } else {
        WordClass::Symbol
    }
}

#[cfg(test)]
mod tests {
    use super::{next_word_boundary, previous_word_boundary};

    #[test]
    fn word_navigation_skips_shell_words() {
        let input = "cd knota-studio && pnpm dev --host";

        assert_eq!(previous_word_boundary(input, input.len()), 28);
        assert_eq!(previous_word_boundary(input, 28), 24);
        assert_eq!(previous_word_boundary(input, 18), 16);
        assert_eq!(next_word_boundary(input, 0), 2);
        assert_eq!(next_word_boundary(input, 3), 15);
        assert_eq!(next_word_boundary(input, 16), 18);
    }

    #[test]
    fn word_navigation_keeps_utf8_boundaries() {
        let input = "echo 中文 路径/test";
        let end = input.len();
        let previous = previous_word_boundary(input, end);

        assert!(input.is_char_boundary(previous));
        assert_eq!(&input[previous..], "路径/test");
        assert!(input.is_char_boundary(next_word_boundary(input, 5)));
    }
}
