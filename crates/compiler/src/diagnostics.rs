use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    ast::Span,
    compile::{CompileOutput, ExecErrorCompileOutput, StaticErrorCompileOutput},
    parse::{CellInvocation, STD_PATH, STD_SOURCE},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warning,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<usize>,
    /// Source text for `path` when it does not name a file on disk, so that a
    /// renderer on the far side of the JSON boundary can still show the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            message: message.into(),
            path: None,
            start: None,
            end: None,
            source: None,
        }
    }

    pub fn at(level: Level, message: impl Into<String>, span: &Span) -> Self {
        Self {
            level,
            message: message.into(),
            path: Some(span.path.clone()),
            start: Some(span.span.start()),
            end: Some(span.span.end()),
            source: None,
        }
    }
}

/// Rewrites diagnostics that land inside a spliced cell invocation so they point
/// at the invocation the caller supplied, and carries its text along for
/// rendering.
pub fn remap_invocation(diagnostics: &mut Vec<Diagnostic>, invocation: &CellInvocation) {
    for diagnostic in diagnostics {
        let (Some(path), Some(start), Some(end)) =
            (&diagnostic.path, diagnostic.start, diagnostic.end)
        else {
            continue;
        };
        let span = Span {
            path: path.clone(),
            span: cfgrammar::Span::new(start, end),
        };
        if let Some(remapped) = invocation.remap(&span) {
            diagnostic.path = Some(remapped.path);
            diagnostic.start = Some(remapped.span.start());
            diagnostic.end = Some(remapped.span.end());
            diagnostic.source = Some(invocation.source.clone());
        }
    }
}

/// Most diagnostics rendered for one invocation.
///
/// A single unbalanced delimiter can put the parser into a state that reports
/// an error per remaining token; 36 KB of source produced 7,742 of them.
const MAX_DIAGNOSTICS: usize = 100;

/// Longest source line rendered under a diagnostic, in characters.
///
/// Each diagnostic re-renders its whole line, so the two limits multiply: the
/// same 36 KB input wrote 542 MB to stderr.
const MAX_SOURCE_LINE: usize = 200;

/// Longest diagnostic message, in characters.
///
/// A message quotes source text -- an identifier, a layer name, a parser's
/// token -- so it is as unbounded as the source is: a 5,000-character
/// identifier produced a 5 KB `is not declared in this scope`, which
/// [`MAX_SOURCE_LINE`] does not bound because it caps only the excerpt
/// rendered *under* the message. Applied in [`condense`] rather than in
/// [`render`] so the JSON output is bounded too.
const MAX_MESSAGE: usize = 400;

/// Drops duplicates, bounds each message, and caps the count, appending a
/// summary when anything was dropped.
///
/// Duplicates are common rather than exceptional: one `a && b` reports five
/// errors across two adjacent spans.
pub fn condense(diagnostics: &mut Vec<Diagnostic>) {
    // Borrowed keys: the pathological inputs this exists for have thousands of
    // diagnostics, and cloning a `PathBuf` and a `String` per entry to build a
    // set that is discarded immediately afterwards is the bulk of the work.
    let keep: Vec<bool> = {
        let mut seen = std::collections::HashSet::new();
        diagnostics
            .iter()
            .map(|diagnostic| {
                seen.insert((
                    diagnostic.path.as_deref(),
                    diagnostic.start,
                    diagnostic.end,
                    diagnostic.message.as_str(),
                ))
            })
            .collect()
    };
    let mut keep = keep.into_iter();
    diagnostics.retain(|_| keep.next().unwrap_or(true));
    for diagnostic in diagnostics.iter_mut() {
        truncate_message(&mut diagnostic.message);
    }
    if diagnostics.len() > MAX_DIAGNOSTICS {
        let dropped = diagnostics.len() - MAX_DIAGNOSTICS;
        diagnostics.truncate(MAX_DIAGNOSTICS);
        diagnostics.push(Diagnostic::error(format!(
            "... and {dropped} more error{}",
            if dropped == 1 { "" } else { "s" }
        )));
    }
}

/// The renderer-independent half of [`condense`], for a consumer that builds
/// its own diagnostic type from `(span, message)` pairs.
///
/// Returns how many entries the cap dropped, so the caller can say so in
/// whatever shape it publishes. The language server needs this: it emits over
/// LSP and so never reaches [`render`], but the same unbalanced delimiter puts
/// the same thousands of entries on the wire, once per keystroke.
pub fn condense_spanned(errors: &mut Vec<(Span, String)>) -> usize {
    let keep: Vec<bool> = {
        let mut seen = std::collections::HashSet::new();
        errors
            .iter()
            .map(|(span, message)| {
                seen.insert((
                    span.path.as_path(),
                    span.span.start(),
                    span.span.end(),
                    message.as_str(),
                ))
            })
            .collect()
    };
    let mut keep = keep.into_iter();
    errors.retain(|_| keep.next().unwrap_or(true));
    for (_, message) in errors.iter_mut() {
        truncate_message(message);
    }
    let dropped = errors.len().saturating_sub(MAX_DIAGNOSTICS);
    errors.truncate(MAX_DIAGNOSTICS);
    dropped
}

/// Truncates `message` to [`MAX_MESSAGE`] characters, on a character boundary.
fn truncate_message(message: &mut String) {
    const ELLIPSIS: &str = "...";
    if message.len() <= MAX_MESSAGE {
        // Byte length bounds character count, so the common case never scans.
        return;
    }
    let Some((cut, _)) = message.char_indices().nth(MAX_MESSAGE) else {
        return;
    };
    message.truncate(cut);
    message.push_str(ELLIPSIS);
}

/// Elides the middle of an over-long source line, keeping `column` visible.
///
/// Returns the text to render and the column it now sits at.
fn elide_line(line: &str, column: usize, underline: usize) -> (String, usize) {
    const ELLIPSIS: &str = "...";
    if line.len() <= MAX_SOURCE_LINE {
        // Byte length bounds character count, so a short line never pays for
        // the `Vec<char>` below -- which the pathological inputs this exists
        // for would otherwise build once per diagnostic.
        return (line.to_string(), column);
    }
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= MAX_SOURCE_LINE {
        return (line.to_string(), column);
    }
    // Centre the window on the underlined span so the caret stays on screen,
    // but never past the caret itself: `underline` may exceed `want`, and an
    // excerpt that starts after the span hides the very text it marks.
    let caret = column.saturating_sub(1);
    let want = MAX_SOURCE_LINE.saturating_sub(2 * ELLIPSIS.len());
    let start = caret
        .saturating_add(underline / 2)
        .saturating_sub(want / 2)
        .min(caret)
        .min(chars.len().saturating_sub(want));
    let end = (start + want).min(chars.len());
    let mut text = String::new();
    if start > 0 {
        text.push_str(ELLIPSIS);
    }
    text.extend(&chars[start..end]);
    if end < chars.len() {
        text.push_str(ELLIPSIS);
    }
    let prefix = if start > 0 { ELLIPSIS.len() } else { 0 };
    (text, caret.saturating_sub(start) + prefix + 1)
}

pub fn from_compile_output(output: &CompileOutput) -> Vec<Diagnostic> {
    match output {
        CompileOutput::FatalParseErrors => {
            vec![Diagnostic::error("fatal parse errors encountered")]
        }
        CompileOutput::StaticErrors(StaticErrorCompileOutput { errors }) => errors
            .iter()
            .map(|error| Diagnostic::at(Level::Error, error.kind.to_string(), &error.span))
            .collect(),
        CompileOutput::ExecErrors(ExecErrorCompileOutput { errors, .. }) => errors
            .iter()
            .map(|error| match &error.span {
                Some(span) => Diagnostic::at(Level::Error, error.kind.to_string(), span),
                None => Diagnostic::error(error.kind.to_string()),
            })
            .collect(),
        CompileOutput::Valid(_) => Vec::new(),
    }
}

pub fn emit(diagnostic: &Diagnostic, json: bool) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    if json {
        serde_json::to_writer(&mut stderr, diagnostic)?;
        writeln!(stderr)
    } else {
        render(&mut stderr, diagnostic, io::stderr().is_terminal())
    }
}

pub fn render(writer: &mut impl Write, diagnostic: &Diagnostic, color: bool) -> io::Result<()> {
    let (open, close) = if color {
        let open = match diagnostic.level {
            Level::Error => "\x1b[1;31m",
            Level::Warning => "\x1b[1;33m",
        };
        (open, "\x1b[0m")
    } else {
        ("", "")
    };
    writeln!(
        writer,
        "{open}{}{close}: {}",
        diagnostic.level.label(),
        diagnostic.message
    )?;

    let (Some(path), Some(start)) = (&diagnostic.path, diagnostic.start) else {
        return Ok(());
    };
    // Read the file only in the arm that uses it: a diagnostic carrying its
    // own source (every spliced `--cell` invocation) or one in `std` would
    // otherwise pay a full read per diagnostic and discard it.
    let file_source = match (&diagnostic.source, path == Path::new(STD_PATH)) {
        (Some(_), _) | (None, true) => None,
        (None, false) => fs::read_to_string(path).ok(),
    };
    let source = match (&diagnostic.source, path == Path::new(STD_PATH)) {
        (Some(source), _) => Some(source.as_str()),
        (None, true) => Some(STD_SOURCE),
        (None, false) => file_source.as_deref(),
    };
    let (line, column, line_text, underline) = source
        .map(|source| source_location(source, start, diagnostic.end.unwrap_or(start)))
        .unwrap_or((1, 1, None, 1));
    writeln!(writer, "  --> {}:{line}:{column}", path.display())?;
    if let Some(line_text) = line_text {
        let (line_text, column) =
            elide_line(line_text, column, underline.clamp(1, MAX_SOURCE_LINE));
        // Clamped against the *excerpt*, not `MAX_SOURCE_LINE`: eliding drops
        // characters the span covered, and a caret run sized for the original
        // span runs past the end of the line it is meant to mark.
        let underline = underline
            .clamp(1, MAX_SOURCE_LINE)
            .min(line_text.chars().count().saturating_sub(column - 1))
            .max(1);
        let gutter = line.to_string().len().max(1);
        writeln!(writer, "{:gutter$} |", "")?;
        writeln!(writer, "{line:>gutter$} | {line_text}")?;
        writeln!(
            writer,
            "{:gutter$} | {}{open}{}{close}",
            "",
            " ".repeat(column.saturating_sub(1)),
            "^".repeat(underline)
        )?;
    }
    Ok(())
}

fn source_location(source: &str, start: usize, end: usize) -> (usize, usize, Option<&str>, usize) {
    // Offsets arrive from spans that a JSON round-trip or a remapped
    // invocation can leave off a character boundary, and `&str` slicing panics
    // there rather than clamping. `end` in particular is `Option`, so a
    // location-only diagnostic deserializes with `end == start` and the
    // `.max(start + 1)` below would land mid-codepoint.
    let floor = |offset: usize| {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    };
    let start = floor(start);
    let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = source[start..]
        .find('\n')
        .map_or(source.len(), |i| start + i);
    let line = source[..line_start].bytes().filter(|b| *b == b'\n').count() + 1;
    let column = source[line_start..start].chars().count() + 1;
    let max_end = floor(end.max(start + 1).min(line_end)).max(start);
    let underline = source[start..max_end].chars().count().max(1);
    (line, column, Some(&source[line_start..line_end]), underline)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{
        ast::Span,
        parse::{STD_PATH, STD_SOURCE},
    };

    use super::{
        Diagnostic, Level, MAX_DIAGNOSTICS, MAX_MESSAGE, MAX_SOURCE_LINE, condense, render,
        source_location,
    };

    fn at(message: &str, start: usize) -> Diagnostic {
        Diagnostic::at(
            Level::Error,
            message,
            &Span {
                path: PathBuf::from("/virtual/lib.ar"),
                span: cfgrammar::Span::new(start, start + 1),
            },
        )
    }

    #[test]
    fn condense_drops_duplicates_and_caps_the_count() {
        // One unbalanced delimiter reports an error per remaining token: 36 KB
        // of source produced 7,742 diagnostics, and one `a && b` produces five
        // for a single construct.
        let mut diagnostics = vec![at("same", 0), at("same", 0), at("same", 4)];
        condense(&mut diagnostics);
        assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");

        let mut diagnostics = (0..MAX_DIAGNOSTICS + 50)
            .map(|i| at("distinct", i))
            .collect::<Vec<_>>();
        condense(&mut diagnostics);
        assert_eq!(diagnostics.len(), MAX_DIAGNOSTICS + 1);
        assert_eq!(
            diagnostics.last().expect("summary").message,
            "... and 50 more errors"
        );
    }

    #[test]
    fn condense_bounds_the_message() {
        // `MAX_SOURCE_LINE` caps only the excerpt rendered *under* a message.
        // A message quotes source text, so a 5,000-character identifier still
        // produced a 5 KB `is not declared in this scope` -- and, unlike the
        // excerpt, it also reached the JSON output and the language server.
        let mut diagnostics = vec![at(&"z".repeat(5_000), 0)];
        condense(&mut diagnostics);
        let message = &diagnostics[0].message;
        assert!(
            message.chars().count() <= MAX_MESSAGE + 3,
            "{}",
            message.len()
        );
        assert!(message.ends_with("..."), "{message}");

        // A short message is untouched, ellipsis included.
        let mut diagnostics = vec![at("short", 0)];
        condense(&mut diagnostics);
        assert_eq!(diagnostics[0].message, "short");
    }

    #[test]
    fn renders_a_bounded_excerpt_of_a_long_line() {
        // Each diagnostic re-renders its whole source line, so a long line and
        // a high error count multiplied: 36 KB of source wrote 542 MB.
        let line = "x".repeat(5_000);
        let source = format!("cell top() {{ {line} }}\n");
        let caret = source.find('x').expect("line content") + 4_000;
        let diagnostic = Diagnostic {
            source: Some(source.clone()),
            ..Diagnostic::at(
                Level::Error,
                "long line",
                &Span {
                    path: PathBuf::from("/virtual/lib.ar"),
                    span: cfgrammar::Span::new(caret, caret + 1),
                },
            )
        };
        let mut output = Vec::new();
        render(&mut output, &diagnostic, false).expect("diagnostic should render");
        let output = String::from_utf8(output).expect("diagnostic should be UTF-8");
        assert!(
            output.len() < 4 * MAX_SOURCE_LINE,
            "rendered {} bytes:\n{output}",
            output.len()
        );
        // The caret still lands under the excerpt rather than past its end.
        let excerpt = output
            .lines()
            .find(|line| line.starts_with("1 | "))
            .expect("source line");
        let caret_line = output
            .lines()
            .find(|line| line.contains('^'))
            .expect("caret");
        assert!(
            caret_line.find('^').expect("caret column") < excerpt.chars().count(),
            "{output}"
        );
    }

    #[test]
    fn a_wide_span_underlines_only_what_was_rendered() {
        // `underline` was clamped to `MAX_SOURCE_LINE` before elision, but the
        // excerpt keeps only `MAX_SOURCE_LINE - 2 * ELLIPSIS` source
        // characters, so the caret run ran past the end of the line it marks.
        let line = "x".repeat(5_000);
        let source = format!("{line}\n");
        let diagnostic = Diagnostic {
            source: Some(source.clone()),
            ..Diagnostic::at(
                Level::Error,
                "wide span",
                &Span {
                    path: PathBuf::from("/virtual/lib.ar"),
                    span: cfgrammar::Span::new(0, 5_000),
                },
            )
        };
        let mut output = Vec::new();
        render(&mut output, &diagnostic, false).expect("diagnostic should render");
        let output = String::from_utf8(output).expect("diagnostic should be UTF-8");
        let excerpt = output
            .lines()
            .find(|line| line.starts_with("1 | "))
            .expect("source line");
        let caret_line = output
            .lines()
            .find(|line| line.contains('^'))
            .expect("caret");
        assert!(
            caret_line.chars().count() <= excerpt.chars().count(),
            "carets overrun the excerpt:\n{output}"
        );
    }

    #[test]
    fn computes_source_location() {
        assert_eq!(source_location("one\ntwo\n", 5, 7), (2, 2, Some("two"), 2));
        // A span offset that a JSON round-trip left mid-codepoint clamps back
        // to a boundary instead of panicking in `&str` slicing.
        assert_eq!(source_location("é", 1, 1), (1, 1, Some("é"), 1));
        assert_eq!(source_location("aé", 2, 2), (1, 2, Some("aé"), 1));
    }

    #[test]
    fn renders_embedded_standard_library_source() {
        let needle = "let first_rect = rect(r.layer);";
        let start = STD_SOURCE
            .find(needle)
            .expect("standard-library source should contain array rectangle");
        let diagnostic = Diagnostic::at(
            Level::Error,
            "test standard-library diagnostic",
            &Span {
                path: PathBuf::from(STD_PATH),
                span: cfgrammar::Span::new(start, start + needle.len()),
            },
        );
        let mut output = Vec::new();
        render(&mut output, &diagnostic, false).expect("diagnostic should render");
        let output = String::from_utf8(output).expect("diagnostic should be UTF-8");
        assert!(output.contains(needle), "{output}");
        assert!(!output.contains("<argon-std>/lib.ar:1:1"), "{output}");
    }
}
