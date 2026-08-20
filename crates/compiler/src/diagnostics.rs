use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{
    ast::Span,
    compile::{CompileOutput, ExecErrorCompileOutput, StaticErrorCompileOutput},
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
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            message: message.into(),
            path: None,
            start: None,
            end: None,
        }
    }

    pub fn at(level: Level, message: impl Into<String>, span: &Span) -> Self {
        Self {
            level,
            message: message.into(),
            path: Some(span.path.clone()),
            start: Some(span.span.start()),
            end: Some(span.span.end()),
        }
    }
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
    let source = fs::read_to_string(path).ok();
    let (line, column, line_text, underline) = source
        .as_deref()
        .map(|source| source_location(source, start, diagnostic.end.unwrap_or(start)))
        .unwrap_or((1, 1, None, 1));
    writeln!(writer, "  --> {}:{line}:{column}", path.display())?;
    if let Some(line_text) = line_text {
        let gutter = line.to_string().len().max(1);
        writeln!(writer, "{:gutter$} |", "")?;
        writeln!(writer, "{line:>gutter$} | {line_text}")?;
        writeln!(
            writer,
            "{:gutter$} | {}{open}{}{close}",
            "",
            " ".repeat(column.saturating_sub(1)),
            "^".repeat(underline.max(1))
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
    use super::source_location;

    #[test]
    fn computes_source_location() {
        assert_eq!(source_location("one\ntwo\n", 5, 7), (2, 2, Some("two"), 2));
    }
}
