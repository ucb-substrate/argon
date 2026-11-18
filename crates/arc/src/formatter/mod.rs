//! The Argon source formatter behind `arc fmt`.

mod groups;
mod layout;
mod tokens;

#[cfg(test)]
mod tests;

/// The widest line, in columns, that the formatter leaves unbroken.
pub const MAX_WIDTH: usize = 100;

/// Columns per indentation level.
const INDENT: usize = 4;

/// Formats Argon source: indents each line by the brackets open before it and
/// breaks lines wider than [`MAX_WIDTH`] at the brackets of lists, calls, and
/// blocks. Lines are only broken in source that parses.
pub fn format_source(source: &str) -> String {
    format_source_with_width(source, MAX_WIDTH)
}

/// A comment line that, among a file's leading comments, keeps its long lines
/// unbroken (for generated or transcribed geometry).
pub const NO_WRAP: &str = "// arc-fmt: no-wrap";

/// [`format_source`] with lines up to `width` columns wide.
pub fn format_source_with_width(source: &str, width: usize) -> String {
    let indented = reindent(source);
    if has_no_wrap(&indented) {
        return indented;
    }
    let tokens = tokens::tokenize(&indented);
    let Some(groups) = groups::Groups::new(&indented, &tokens) else {
        return indented;
    };
    let broken = layout::break_long_lines(&indented, &tokens, &groups, width);
    if broken == indented {
        indented
    } else {
        reindent(&broken)
    }
}

/// Whether the comment lines at the top of `source` include [`NO_WRAP`].
fn has_no_wrap(source: &str) -> bool {
    source
        .lines()
        .map(str::trim)
        .take_while(|line| line.is_empty() || line.starts_with("//"))
        .any(|line| line == NO_WRAP)
}

/// Re-indents every line one level deeper than the line that opened the
/// innermost bracket still open before it. A line that starts by closing
/// brackets takes the level of the line that opened them.
fn reindent(source: &str) -> String {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = Vec::new();
    // Each open bracket, with the level of the line that opened it.
    let mut delimiters: Vec<(char, usize)> = Vec::new();
    let mut in_string = false;

    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if output.last().is_some_and(|line: &String| !line.is_empty()) {
                output.push(String::new());
            }
            continue;
        }

        let closers = leading_closing_delimiters(trimmed, &delimiters, in_string);
        let level = if closers > 0 {
            delimiters[delimiters.len() - closers].1
        } else if trimmed.starts_with('}') && !in_string {
            // A brace that does not match the innermost bracket still lines
            // up with the brace it most likely closes.
            delimiters
                .iter()
                .rev()
                .find(|(open, _)| *open == '{')
                .map_or(0, |(_, level)| *level)
        } else {
            let level = delimiters.last().map_or(0, |(_, level)| level + 1);
            // A return type continued on its own line belongs to the signature above.
            level + usize::from(trimmed.starts_with("->") && !in_string)
        };
        output.push(format!("{}{trimmed}", " ".repeat(level * INDENT)));
        update_delimiters(trimmed, level, &mut delimiters, &mut in_string);
    }

    while output.last().is_some_and(String::is_empty) {
        output.pop();
    }
    if output.is_empty() {
        String::new()
    } else {
        format!("{}\n", output.join("\n"))
    }
}

fn leading_closing_delimiters(line: &str, delimiters: &[(char, usize)], in_string: bool) -> usize {
    if in_string {
        return 0;
    }
    let mut stack_index = delimiters.len();
    let mut closers = 0;
    for ch in line.chars() {
        if ch.is_whitespace() {
            continue;
        }
        let Some((open, _)) = stack_index
            .checked_sub(1)
            .and_then(|index| delimiters.get(index))
        else {
            break;
        };
        if closes(*open, ch) {
            stack_index -= 1;
            closers += 1;
        } else {
            break;
        }
    }
    closers
}

fn update_delimiters(
    line: &str,
    level: usize,
    delimiters: &mut Vec<(char, usize)>,
    in_string: &mut bool,
) {
    let mut chars = line.chars().peekable();
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if *in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                *in_string = false;
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            break;
        }
        match ch {
            '"' => *in_string = true,
            '{' | '(' | '[' => delimiters.push((ch, level)),
            '}' | ')' | ']' if delimiters.last().is_some_and(|(open, _)| closes(*open, ch)) => {
                delimiters.pop();
            }
            _ => {}
        }
    }
}

fn closes(open: char, close: char) -> bool {
    matches!((open, close), ('{', '}') | ('(', ')') | ('[', ']'))
}
