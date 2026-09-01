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

/// Drops duplicates and caps the count, appending a summary when anything was
/// dropped.
///
/// Duplicates are common rather than exceptional: one `a && b` reports five
/// errors across two adjacent spans.
pub fn condense(diagnostics: &mut Vec<Diagnostic>) {
    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|diagnostic| {
        seen.insert((
            diagnostic.path.clone(),
            diagnostic.start,
            diagnostic.end,
            diagnostic.message.clone(),
        ))
    });
    if diagnostics.len() > MAX_DIAGNOSTICS {
        let dropped = diagnostics.len() - MAX_DIAGNOSTICS;
        diagnostics.truncate(MAX_DIAGNOSTICS);
        diagnostics.push(Diagnostic::error(format!(
            "... and {dropped} more error{}",
            if dropped == 1 { "" } else { "s" }
        )));
    }
}

/// Elides the middle of an over-long source line, keeping `column` visible.
///
/// Returns the text to render and the column it now sits at.
fn elide_line(line: &str, column: usize, underline: usize) -> (String, usize) {
    const ELLIPSIS: &str = "...";
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= MAX_SOURCE_LINE {
        return (line.to_string(), column);
    }
    // Centre the window on the underlined span so the caret stays on screen.
    let caret = column.saturating_sub(1);
    let want = MAX_SOURCE_LINE.saturating_sub(2 * ELLIPSIS.len());
    let start = caret
        .saturating_add(underline / 2)
        .saturating_sub(want / 2)
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
    let file_source = fs::read_to_string(path).ok();
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
        let underline = underline.clamp(1, MAX_SOURCE_LINE);
        let (line_text, column) = elide_line(line_text, column, underline);
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
    let start = start.min(source.len());
    let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = source[start..]
        .find('\n')
        .map_or(source.len(), |i| start + i);
    let line = source[..line_start].bytes().filter(|b| *b == b'\n').count() + 1;
    let column = source[line_start..start].chars().count() + 1;
    let max_end = end.max(start + 1).min(line_end);
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
        Diagnostic, Level, MAX_DIAGNOSTICS, MAX_SOURCE_LINE, condense, render, source_location,
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
    fn computes_source_location() {
        assert_eq!(source_location("one\ntwo\n", 5, 7), (2, 2, Some("two"), 2));
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
