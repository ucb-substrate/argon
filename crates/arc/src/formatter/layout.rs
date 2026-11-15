//! Breaks lines that are too wide at the brackets of lists, calls, and blocks.
//!
//! Only whitespace changes: a broken group puts its opening bracket at the end
//! of a line, each element on a line of its own one level deeper, and its
//! closing bracket at the start of the line after them. The one other edit is
//! the comma added after the last element of a broken list.

use std::ops::Range;

use super::INDENT;
use super::groups::{Groups, Kind};
use super::tokens::{Token, TokenKind, closes};

/// The widest element of which a broken list packs several to a line.
const SHORT_ELEMENT_WIDTH: usize = 10;

/// A run of one line's code.
enum Piece {
    Text(Range<usize>),
    /// A separator directly inside a group.
    Sep(usize),
    Group(Group),
}

/// A bracketed group that opens and closes on the same line.
struct Group {
    kind: Option<Kind>,
    open: usize,
    close: usize,
    children: Vec<Piece>,
}

/// A run of output text, or a group that is written flat or broken.
#[derive(Clone, Copy)]
enum Item<'p> {
    Text(&'p str),
    Group(&'p Group),
}

/// One argument, list item, field, arm, or statement of a group.
struct Element<'p> {
    items: Vec<Item<'p>>,
    /// Whether a separator followed the element.
    separated: bool,
}

/// Breaks every line of `text` whose code, without a trailing comment, is
/// wider than `width` columns.
pub(super) fn break_long_lines(
    text: &str,
    tokens: &[Token],
    groups: &Groups,
    width: usize,
) -> String {
    let breaker = Breaker {
        text,
        groups,
        width,
    };
    let mut output = String::with_capacity(text.len());
    let mut start = 0;
    let mut first_token = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.trim_end_matches('\n').len();
        let last_token =
            first_token + tokens[first_token..].partition_point(|token| token.start < end);
        breaker.line(start..end, &tokens[first_token..last_token], &mut output);
        output.push_str(&text[end..start + line.len()]);
        start += line.len();
        first_token = last_token;
    }
    output
}

struct Breaker<'a> {
    text: &'a str,
    groups: &'a Groups,
    width: usize,
}

impl<'a> Breaker<'a> {
    fn line(&self, range: Range<usize>, tokens: &[Token], output: &mut String) {
        let line = &self.text[range.clone()];
        let indent = line.len() - line.trim_start().len();
        let (tokens, comment) = match tokens.split_last() {
            Some((last, rest)) if last.kind == TokenKind::Comment => (rest, Some(last.start)),
            _ => (tokens, None),
        };
        let code_start = range.start + indent;
        let code = self.text[code_start..comment.unwrap_or(range.end)].trim_end();
        let code_end = code_start + code.len();
        if code.is_empty() || indent + width(code) <= self.width {
            output.push_str(line);
            return;
        }
        let pieces = self.pieces(code_start..code_end, tokens);
        let items: Vec<Item> = pieces.iter().map(|piece| self.item(piece)).collect();
        let mut lines = Vec::new();
        self.layout(&items, indent, &mut lines);
        if comment.is_some() {
            let last = lines.last_mut().expect("layout writes at least one line");
            last.push_str(&self.text[code_end..range.end]);
        }
        output.push_str(&lines.join("\n"));
    }

    /// The pieces of the code in `range`. Brackets that do not close on the
    /// line are plain text.
    fn pieces(&self, range: Range<usize>, tokens: &[Token]) -> Vec<Piece> {
        struct Frame {
            open: usize,
            children: Vec<Piece>,
        }
        fn push(root: &mut Vec<Piece>, frames: &mut [Frame], piece: Piece) {
            match frames.last_mut() {
                Some(frame) => frame.children.push(piece),
                None => root.push(piece),
            }
        }

        let bytes = self.text.as_bytes();
        let mut root = Vec::new();
        let mut frames: Vec<Frame> = Vec::new();
        let mut cursor = range.start;
        for token in tokens {
            if token.start > cursor {
                push(&mut root, &mut frames, Piece::Text(cursor..token.start));
            }
            cursor = token.end;
            let piece = match token.kind {
                TokenKind::Open(_) => {
                    frames.push(Frame {
                        open: token.start,
                        children: Vec::new(),
                    });
                    continue;
                }
                TokenKind::Close(close)
                    if frames
                        .last()
                        .is_some_and(|frame| closes(bytes[frame.open], close)) =>
                {
                    let frame = frames.pop().expect("a frame was just checked");
                    Piece::Group(Group {
                        kind: self.groups.kind(frame.open),
                        open: frame.open,
                        close: token.start,
                        children: frame.children,
                    })
                }
                TokenKind::Comma if !self.groups.in_type(token.start) => Piece::Sep(token.start),
                TokenKind::Semi => Piece::Sep(token.start),
                _ => Piece::Text(token.start..token.end),
            };
            push(&mut root, &mut frames, piece);
        }
        if range.end > cursor {
            push(&mut root, &mut frames, Piece::Text(cursor..range.end));
        }
        while let Some(frame) = frames.pop() {
            push(
                &mut root,
                &mut frames,
                Piece::Text(frame.open..frame.open + 1),
            );
            for piece in frame.children {
                push(&mut root, &mut frames, piece);
            }
        }
        root
    }

    fn item<'p>(&self, piece: &'p Piece) -> Item<'p>
    where
        'a: 'p,
    {
        match piece {
            Piece::Text(range) => Item::Text(&self.text[range.clone()]),
            Piece::Sep(offset) => Item::Text(&self.text[*offset..*offset + 1]),
            Piece::Group(group) => Item::Group(group),
        }
    }

    fn flat(&self, items: &[Item]) -> String {
        let mut text = String::new();
        for item in items {
            match item {
                Item::Text(item) => text.push_str(item),
                Item::Group(group) => text.push_str(&self.text[group.open..=group.close]),
            }
        }
        text
    }

    fn brackets(&self, group: &Group) -> (&'a str, &'a str) {
        (
            &self.text[group.open..group.open + 1],
            &self.text[group.close..group.close + 1],
        )
    }

    /// Writes `items` as lines indented by `indent` columns, breaking groups
    /// until each line fits or has no group left to break.
    fn layout<'p>(&self, items: &[Item<'p>], indent: usize, lines: &mut Vec<String>)
    where
        'a: 'p,
    {
        let flat = self.flat(items);
        let flat = flat.trim();
        let is_block = |index: usize| match items[index] {
            Item::Group(group) => group.kind.is_some_and(Kind::is_block),
            Item::Text(_) => false,
        };
        // A list that is an operand of a binary operator breaks only if it is
        // most of the line; breaking a small one leaves the expression around
        // it as wide.
        let breakable: Vec<usize> = (0..items.len())
            .filter(|&index| match items[index] {
                Item::Group(group) => {
                    self.is_breakable(group)
                        && (is_block(index)
                            || 2 * width(&self.text[group.open..=group.close]) >= width(flat)
                            || !is_operand(items, index))
                }
                Item::Text(_) => false,
            })
            .collect();
        if indent + width(flat) <= self.width || breakable.is_empty() {
            lines.push(indented(indent, flat));
            return;
        }

        // The last body breaks first, with the bodies its `else`s chain it to.
        // Otherwise the widest list breaks, the last of equals.
        let blocks: Vec<usize> = breakable
            .iter()
            .copied()
            .filter(|&index| is_block(index))
            .collect();
        let chosen = if let Some((&last, earlier)) = blocks.split_last() {
            let mut chain = vec![last];
            for &index in earlier.iter().rev() {
                let between = self.flat(&items[index + 1..chain[chain.len() - 1]]);
                let follows_else = between
                    .trim_start()
                    .strip_prefix("else")
                    .is_some_and(|rest| {
                        !rest.starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
                    });
                if !follows_else {
                    break;
                }
                chain.push(index);
            }
            chain.reverse();
            chain
        } else {
            let widest = breakable
                .iter()
                .copied()
                .max_by_key(|&index| width(&self.flat(&items[index..=index])))
                .expect("some group is breakable");
            vec![widest]
        };

        let mut line = Vec::new();
        let mut next = 0;
        for index in chosen {
            line.extend_from_slice(&items[next..index]);
            let Item::Group(group) = items[index] else {
                unreachable!("only groups are chosen");
            };
            self.break_group(group, &mut line, indent, lines);
            next = index + 1;
        }
        line.extend_from_slice(&items[next..]);
        self.layout(&line, indent, lines);
    }

    /// Breaks `group`, which follows `line` on the current line: writes the
    /// lines up to the end of its contents and leaves in `line` the text that
    /// starts the next line.
    fn break_group<'p>(
        &self,
        group: &'p Group,
        line: &mut Vec<Item<'p>>,
        indent: usize,
        lines: &mut Vec<String>,
    ) where
        'a: 'p,
    {
        let kind = group.kind.expect("a breakable group has a kind");
        let (open, close) = self.brackets(group);
        if kind == Kind::Comprehension {
            let (head, body) = self
                .comprehension(group)
                .expect("a breakable comprehension has a body");
            line.push(Item::Text(open));
            line.extend(head);
            self.break_group(body, line, indent, lines);
            line.push(Item::Text(close));
            return;
        }

        let elements = self.elements(group);
        if let Some((prefix, inner)) = self.hug_target(kind, &elements) {
            let mut head = line.clone();
            head.push(Item::Text(open));
            head.extend_from_slice(&prefix);
            self.opening(inner, &mut head);
            if indent + width(self.flat(&head).trim()) <= self.width {
                line.push(Item::Text(open));
                line.extend(prefix);
                self.break_group(inner, line, indent, lines);
                line.push(Item::Text(close));
                return;
            }
        }

        line.push(Item::Text(open));
        self.layout(line, indent, lines);
        line.clear();
        let inner_indent = indent + INDENT;
        if let Some(texts) = self.packed(kind, &elements) {
            self.fill(texts, inner_indent, lines);
        } else {
            let count = elements.len();
            for (index, element) in elements.into_iter().enumerate() {
                let mut items = element.items;
                let separator = match kind {
                    Kind::Block => element.separated.then_some(";"),
                    Kind::Struct if index + 1 == count && self.flat(&items).starts_with("..") => {
                        None
                    }
                    _ => Some(","),
                };
                items.extend(separator.map(Item::Text));
                self.layout(&items, inner_indent, lines);
            }
        }
        line.push(Item::Text(close));
    }

    /// Appends the text that a broken `group` puts before its contents.
    fn opening<'p>(&self, group: &'p Group, head: &mut Vec<Item<'p>>)
    where
        'a: 'p,
    {
        head.push(Item::Text(self.brackets(group).0));
        if let Some((items, body)) = self.comprehension(group) {
            head.extend(items);
            self.opening(body, head);
        }
    }

    fn is_breakable(&self, group: &Group) -> bool {
        match group.kind {
            None => false,
            Some(Kind::Comprehension) => self.comprehension(group).is_some(),
            Some(_) => {
                let elements = self.elements(group);
                !elements.is_empty() && elements.iter().all(|element| !element.items.is_empty())
            }
        }
    }

    /// The elements of `group`, without the whitespace around each.
    fn elements<'p>(&self, group: &'p Group) -> Vec<Element<'p>>
    where
        'a: 'p,
    {
        let kind = group.kind.expect("only classified groups have elements");
        let separator = kind.separator();
        let mut elements = Vec::new();
        let mut items = Vec::new();
        for piece in &group.children {
            match piece {
                Piece::Sep(offset) if self.text.as_bytes()[*offset] == separator => {
                    elements.push(Element {
                        items: trim(std::mem::take(&mut items)),
                        separated: true,
                    });
                }
                Piece::Group(inner)
                    if kind == Kind::Block && self.groups.ends_statement(inner.close + 1) =>
                {
                    items.push(Item::Group(inner));
                    elements.push(Element {
                        items: trim(std::mem::take(&mut items)),
                        separated: false,
                    });
                }
                piece => items.push(self.item(piece)),
            }
        }
        let items = trim(items);
        if !items.is_empty() {
            elements.push(Element {
                items,
                separated: false,
            });
        }
        elements
    }

    /// The text before the body of a comprehension, and the body.
    fn comprehension<'p>(&self, group: &'p Group) -> Option<(Vec<Item<'p>>, &'p Group)>
    where
        'a: 'p,
    {
        if group.kind != Some(Kind::Comprehension) {
            return None;
        }
        let items = trim(
            group
                .children
                .iter()
                .map(|piece| self.item(piece))
                .collect(),
        );
        let Some((&Item::Group(body), head)) = items.split_last() else {
            return None;
        };
        (body.kind == Some(Kind::Block) && self.is_breakable(body)).then(|| (head.to_vec(), body))
    }

    /// The path and group of the single element of a `kind` group that the
    /// group hugs, as `f` and `[..]` in `g(f([..]))`. An element with a comma
    /// after it is not hugged, which would drop the comma.
    fn hug_target<'p>(
        &self,
        kind: Kind,
        elements: &[Element<'p>],
    ) -> Option<(Vec<Item<'p>>, &'p Group)>
    where
        'a: 'p,
    {
        let [element] = elements else {
            return None;
        };
        if element.separated {
            return None;
        }
        let Some((&Item::Group(inner), prefix)) = element.items.split_last() else {
            return None;
        };
        let inner_kind = inner.kind?;
        if !kind.hugs() || !inner_kind.is_huggable() || !self.is_breakable(inner) {
            return None;
        }
        let path = self.flat(prefix);
        let is_path = prefix.iter().all(|item| matches!(item, Item::Text(_)))
            && path
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ':');
        let named = matches!(inner_kind, Kind::Args | Kind::Struct);
        (is_path && path.is_empty() != named).then(|| (prefix.to_vec(), inner))
    }

    /// The elements of a `kind` group with their commas, if they are short
    /// enough to pack several to a line: always for `use` items, and for lists
    /// of short literals and names.
    fn packed(&self, kind: Kind, elements: &[Element]) -> Option<Vec<String>> {
        let texts = elements
            .iter()
            .map(|element| match element.items.as_slice() {
                [Item::Text(text)] => Some(*text),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let packs = match kind {
            Kind::Use => true,
            Kind::Args | Kind::Seq | Kind::Tuple => {
                texts.len() > 1
                    && texts
                        .iter()
                        .all(|text| width(text) <= SHORT_ELEMENT_WIDTH && is_simple(text))
            }
            _ => false,
        };
        packs.then(|| texts.iter().map(|text| format!("{text},")).collect())
    }

    fn fill(&self, texts: Vec<String>, indent: usize, lines: &mut Vec<String>) {
        let mut current = String::new();
        for text in texts {
            if current.is_empty() {
                current = text;
            } else if indent + width(&current) + 1 + width(&text) <= self.width {
                current.push(' ');
                current.push_str(&text);
            } else {
                lines.push(indented(indent, &current));
                current = text;
            }
        }
        if !current.is_empty() {
            lines.push(indented(indent, &current));
        }
    }
}

/// `items` without whitespace at either end.
fn trim(mut items: Vec<Item>) -> Vec<Item> {
    while let Some(&Item::Text(text)) = items.first() {
        let text = text.trim_start();
        if text.is_empty() {
            items.remove(0);
        } else {
            items[0] = Item::Text(text);
            break;
        }
    }
    while let Some(&Item::Text(text)) = items.last() {
        let text = text.trim_end();
        let last = items.len() - 1;
        if text.is_empty() {
            items.pop();
        } else {
            items[last] = Item::Text(text);
            break;
        }
    }
    items
}

/// Whether the group `items[index]`, with any path before it, is an operand
/// of a binary operator outside the brackets around it.
fn is_operand(items: &[Item], index: usize) -> bool {
    // Groups and string literals become `_`, so only operators and the
    // boundaries of the operand's expression remain.
    let mut text = Vec::new();
    let mut at = 0;
    for (item_index, item) in items.iter().enumerate() {
        match item {
            Item::Text(item) => {
                let mut in_string = false;
                for &byte in item.as_bytes() {
                    if byte == b'"' {
                        if !in_string {
                            text.push(b'_');
                        }
                        in_string = !in_string;
                    } else if !in_string {
                        text.push(byte);
                    }
                }
            }
            Item::Group(_) => {
                if item_index == index {
                    at = text.len();
                }
                text.push(b'_');
            }
        }
    }
    let start = (0..at)
        .rev()
        .find(|&position| is_boundary(&text, position))
        .map_or(0, |position| position + 1);
    let end = (at + 1..text.len())
        .find(|&position| is_boundary(&text, position))
        .unwrap_or(text.len());
    (start..end).any(|position| is_binary_operator(&text, position))
}

/// Whether `text[position]` ends the expression around it: a bracket, a
/// separator, `=`, `=>`, or `->`.
fn is_boundary(text: &[u8], position: usize) -> bool {
    let previous = position.checked_sub(1).map(|index| text[index]);
    let next = text.get(position + 1).copied();
    match text[position] {
        b',' | b';' | b'(' | b')' | b'[' | b']' | b'{' | b'}' => true,
        b'=' => !matches!(previous, Some(b'=' | b'!' | b'<' | b'>')) && next != Some(b'='),
        b'-' => next == Some(b'>'),
        b'>' => matches!(previous, Some(b'-' | b'=')),
        _ => false,
    }
}

fn is_binary_operator(text: &[u8], position: usize) -> bool {
    let next = text.get(position + 1).copied();
    match text[position] {
        b'*' | b'/' | b'%' | b'<' | b'>' => true,
        b'&' | b'|' => next == Some(text[position]),
        b'=' | b'!' => next == Some(b'='),
        b'+' | b'-' => {
            let operand = text[..position]
                .iter()
                .rev()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|&byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b')' | b']')
                });
            operand && !is_exponent_sign(text, position)
        }
        _ => false,
    }
}

/// Whether the sign at `position` is part of a number, as in `1e-3`.
fn is_exponent_sign(text: &[u8], position: usize) -> bool {
    let start = text[..position]
        .iter()
        .rposition(|&byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.')))
        .map_or(0, |index| index + 1);
    position > start && matches!(text[position - 1], b'e' | b'E') && text[start].is_ascii_digit()
}

/// Whether `text` is a literal, a name, or a field access, optionally negated.
fn is_simple(text: &str) -> bool {
    let text = text.strip_prefix('-').unwrap_or(text);
    if let Some(string) = text.strip_prefix('"') {
        return string
            .strip_suffix('"')
            .is_some_and(|body| !body.contains('"'));
    }
    let numeric = text.starts_with(|ch: char| ch.is_ascii_digit());
    let bytes = text.as_bytes();
    !text.is_empty()
        && bytes.iter().enumerate().all(|(index, &byte)| {
            byte.is_ascii_alphanumeric()
                || byte == b'_'
                || byte == b'.'
                || (numeric
                    && matches!(byte, b'+' | b'-')
                    && matches!(bytes[index - 1], b'e' | b'E'))
        })
}

fn width(text: &str) -> usize {
    text.chars().count()
}

fn indented(indent: usize, text: &str) -> String {
    format!("{:indent$}{text}", "")
}
