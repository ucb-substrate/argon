//! Completion for the arguments of the `:Argon` commands that take a cell
//! invocation.
//!
//! The editor's command line has no document to complete against, so the
//! typed text is resolved against the navigation index at an *anchor*: a
//! position in indexed source whose visible names are the ones the argument
//! will actually be evaluated among. `openCell` is anchored at the end of the
//! root module, where its entry cell is spliced, and `inst` at the scope
//! selected in the GUI, where its preview binding is inserted.
//!
//! Whether the cursor admits a name at all is a grammar question, so it is
//! answered the way the editor answers it: by classifying the argument in the
//! declaration the command will splice it into.

use std::path::{Path, PathBuf};

use argonc::{
    nav::{CompletionCandidate, CompletionKind, NavIndex},
    parse::{CompletionSite, completion_site},
};
use serde::{Deserialize, Serialize};
use tarpc::context;
use tokio::time::{Duration, timeout};
use tower_lsp_server::ls_types::CompletionItem;

use crate::{
    State,
    navigation::{
        CallContext, CompletionContext, call_context, completion_context, completion_items,
        filter_completions, is_ident_continue,
    },
};

/// How long to wait for the GUI to report its selected scope. Completion runs
/// while the user waits at the command line, so a slow GUI falls back to the
/// root module rather than delaying the response.
const SELECTED_SCOPE_TIMEOUT: Duration = Duration::from_millis(50);

/// Prefix of the names the compiler generates for entry cells and instance
/// previews, which are never worth offering.
const GENERATED_PREFIX: &str = "__argon";

/// The declaration `openCell` splices its argument into.
const OPEN_CELL_PREFIX: &str = "cell __argon_complete__() { let __argon_argument__ = ";

/// The statement `inst` inserts, whose argument sits one call deeper.
const INST_PREFIX: &str = "cell __argon_complete__() { let __argon_argument__ = inst(";

/// A `:Argon` subcommand whose argument is a cell invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CellExpressionCommand {
    OpenCell,
    Inst,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CommandCompletionParams {
    command: CellExpressionCommand,
    /// The argument text typed so far.
    text: String,
    /// Byte offset of the cursor in `text`.
    cursor: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandCompletion {
    /// Bytes immediately before the cursor that accepting a candidate
    /// replaces. The client keeps whatever precedes them.
    prefix_len: usize,
    items: Vec<CompletionItem>,
}

/// A position in indexed source that stands in for where an argument runs.
struct Anchor {
    path: PathBuf,
    offset: usize,
}

impl State {
    pub(crate) async fn command_completion(
        &self,
        params: CommandCompletionParams,
    ) -> CommandCompletion {
        let cursor = clamp_cursor(&params.text, params.cursor);
        let prefix_len = identifier_prefix_len(&params.text, cursor);
        let items = match self.nav_index().await {
            Some(index) => match self.anchor(params.command, &index).await {
                Some(anchor) => completion_items(candidates(
                    &index,
                    &anchor,
                    params.command,
                    &params.text,
                    cursor,
                )),
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        CommandCompletion { prefix_len, items }
    }

    async fn anchor(&self, command: CellExpressionCommand, index: &NavIndex) -> Option<Anchor> {
        if command == CellExpressionCommand::Inst
            && let Some(anchor) = self.selected_scope_anchor(index).await
        {
            return Some(anchor);
        }
        self.root_module_anchor(index).await
    }

    /// The end of the root module, where `openCell` splices its entry cell.
    async fn root_module_anchor(&self, index: &NavIndex) -> Option<Anchor> {
        let path = self
            .published_state
            .lock()
            .await
            .config
            .root_lib()
            .to_path_buf();
        let offset = index.source(&path)?.len();
        Some(Anchor { path, offset })
    }

    /// The scope selected in the GUI, where `inst` inserts its preview
    /// binding. `None` whenever that scope cannot be resolved against the
    /// current index, so completion falls back to the root module instead of
    /// offering names from a scope that has since moved.
    async fn selected_scope_anchor(&self, index: &NavIndex) -> Option<Anchor> {
        let connection = self.gui_connection().await?;
        let scope = timeout(
            SELECTED_SCOPE_TIMEOUT,
            connection.client.selected_scope(context::current()),
        )
        .await
        .ok()?
        .ok()??;
        // The scope's own end is inside every scope enclosing it and outside
        // its siblings, which is exactly what an inserted statement would see.
        let offset = scope.span.end();
        (offset <= index.source(&scope.path)?.len()).then_some(Anchor {
            path: scope.path,
            offset,
        })
    }
}

/// Classifies the cursor by parsing the argument inside the declaration the
/// command splices it into.
///
/// Only [`CompletionSite::Expression`] admits a name. A closed invocation
/// reports `Statement`, since the grammar position after a finished
/// expression is the next statement, and a command line has no next
/// statement; a cursor in a string or comment reports `Suppressed`.
fn argument_site(command: CellExpressionCommand, text: &str, cursor: usize) -> CompletionSite {
    let prefix = match command {
        CellExpressionCommand::OpenCell => OPEN_CELL_PREFIX,
        CellExpressionCommand::Inst => INST_PREFIX,
    };
    let source = format!("{prefix}{}", &text[..cursor]);
    completion_site(&source, source.len())
}

/// Whether a name may begin at `at`.
///
/// Nothing may directly follow a closing delimiter or a string literal, so a
/// finished expression is never extended into a name. This complements
/// [`argument_site`], which reports an expression for a closed but empty
/// argument list: the site recorded inside the parentheses wins at the
/// coinciding offset.
fn name_may_begin_at(text: &str, at: usize) -> bool {
    let Some(previous) = at
        .checked_sub(1)
        .and_then(|index| text.as_bytes().get(index))
    else {
        return true;
    };
    !matches!(previous, b')' | b']' | b'}' | b'"')
}

fn candidates(
    index: &NavIndex,
    anchor: &Anchor,
    command: CellExpressionCommand,
    text: &str,
    cursor: usize,
) -> Vec<CompletionCandidate> {
    let site = argument_site(command, text, cursor);
    if site != CompletionSite::Expression
        || !name_may_begin_at(text, cursor - identifier_prefix_len(text, cursor))
    {
        return Vec::new();
    }
    let candidates = match completion_context(text, cursor) {
        // Members need the base expression's inferred type, which text typed
        // at the command line does not have.
        CompletionContext::Member { .. } => Vec::new(),
        CompletionContext::Qualified { segments } => {
            filter_completions(index.qualified_completions(&anchor.path, &segments), site)
        }
        CompletionContext::Plain => match call_context(text, cursor) {
            None => callee_candidates(index, &anchor.path, anchor.offset),
            Some(call) => argument_candidates(index, &anchor.path, anchor.offset, &call, site),
        },
    };
    candidates
        .into_iter()
        .filter(|candidate| !candidate.label.starts_with(GENERATED_PREFIX))
        .collect()
}

/// Names that can open the argument. Only a cell invocation is valid there, so
/// functions, literals, and keywords are left out; cells complete with the
/// opening parenthesis so the next completion sees their arguments.
fn callee_candidates(index: &NavIndex, path: &Path, offset: usize) -> Vec<CompletionCandidate> {
    index
        .completions_at(path, offset)
        .into_iter()
        .filter_map(|mut candidate| match candidate.kind {
            CompletionKind::Cell => {
                candidate.insert_text = Some(format!("{}(", candidate.label));
                Some(candidate)
            }
            CompletionKind::Module => Some(candidate),
            _ => None,
        })
        .collect()
}

/// Names valid inside the invocation's parentheses, plus the callee's keyword
/// arguments.
fn argument_candidates(
    index: &NavIndex,
    path: &Path,
    offset: usize,
    call: &CallContext,
    site: CompletionSite,
) -> Vec<CompletionCandidate> {
    let mut candidates = filter_completions(index.completions_at(path, offset), site);
    // Keyword arguments matter more here than a same-named binding, and
    // deduplication keeps the last candidate for a label.
    if let Some(signature) = index.signature_named_at(path, offset, &call.path) {
        candidates.extend(index.keyword_completions_for_signature(&signature));
    }
    candidates
}

/// Length of the identifier immediately before `cursor`, which is the part of
/// the argument a candidate replaces. Zero after a delimiter such as `(`, `,`,
/// or `::`.
fn identifier_prefix_len(text: &str, cursor: usize) -> usize {
    let bytes = text.as_bytes();
    let mut start = cursor;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    cursor - start
}

/// Clamps a client-supplied byte offset to a character boundary.
fn clamp_cursor(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use argonc::{
        compile::static_compile,
        parse::{STD_PATH, STD_SOURCE, parse_source_text},
    };
    use indexmap::IndexMap;

    use super::*;

    const ROOT: &str = "/virtual/lib.ar";

    const SOURCE: &str = "\
fn double(x: Float) -> Float { 2. * x }

cell child(w: Float, h: Float = w, layer: String = \"met1\") {
    let body = rect(layer, x0=0., y0=0., x1=w, y1=h);
}

cell top(w: Float = 100.) {
    let square = inst(child(w));
}
";

    /// Builds a one-file workspace, plus the standard library, anchored at the
    /// end of its root module as `openCell` would be.
    fn fixture() -> (NavIndex, Anchor) {
        let root = parse_source_text(SOURCE.to_owned(), PathBuf::from(ROOT)).unwrap();
        let std = parse_source_text(STD_SOURCE, PathBuf::from(STD_PATH)).unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (typed, errors) = static_compile(&ast).unwrap();
        assert!(errors.errors.is_empty(), "{:?}", errors.errors);
        let index = NavIndex::build(&typed);
        let offset = index.source(Path::new(ROOT)).unwrap().len();
        (
            index,
            Anchor {
                path: PathBuf::from(ROOT),
                offset,
            },
        )
    }

    /// Items offered for `text` with the cursor at its end.
    fn items(command: CellExpressionCommand, text: &str) -> Vec<CompletionItem> {
        let (index, anchor) = fixture();
        completion_items(candidates(&index, &anchor, command, text, text.len()))
    }

    /// Labels offered for an `openCell` argument.
    fn labels(text: &str) -> Vec<String> {
        items(CellExpressionCommand::OpenCell, text)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    /// What accepting the candidate named `label` inserts.
    fn insert_text(text: &str, label: &str) -> Option<String> {
        items(CellExpressionCommand::OpenCell, text)
            .into_iter()
            .find(|item| item.label == label)
            .and_then(|item| item.insert_text)
    }

    #[test]
    fn the_argument_opens_with_a_cell() {
        let labels = labels("");
        assert!(labels.iter().any(|label| label == "child"));
        assert!(labels.iter().any(|label| label == "top"));
        // A function, a builtin, and a keyword cannot open a cell invocation.
        assert!(!labels.iter().any(|label| label == "double"));
        assert!(!labels.iter().any(|label| label == "rect"));
        assert!(!labels.iter().any(|label| label == "true"));
    }

    #[test]
    fn a_completed_cell_opens_its_argument_list() {
        assert_eq!(insert_text("ch", "child").as_deref(), Some("child("));
    }

    #[test]
    fn arguments_offer_the_callees_keyword_parameters() {
        let labels = labels("child(1., ");
        assert!(labels.iter().any(|label| label == "h"));
        assert!(labels.iter().any(|label| label == "layer"));
        // `w` has no default, so it is positional only.
        assert!(!labels.iter().any(|label| label == "w"));
        assert_eq!(insert_text("child(1., ", "h").as_deref(), Some("h="));
    }

    #[test]
    fn arguments_offer_expressions_rather_than_only_cells() {
        let labels = labels("child(");
        assert!(labels.iter().any(|label| label == "double"));
        assert!(labels.iter().any(|label| label == "rect"));
        // Declaration keywords and type names are not expressions.
        assert!(!labels.iter().any(|label| label == "cell"));
        assert!(!labels.iter().any(|label| label == "Float"));
    }

    #[test]
    fn a_qualified_path_offers_the_modules_items() {
        assert!(!labels("child(std::").is_empty());
        assert!(labels("child(nonexistent::").is_empty());
    }

    #[test]
    fn members_are_not_offered() {
        assert!(labels("child(something.").is_empty());
    }

    /// The reported failure: a finished invocation was read as the callee
    /// position, because its parentheses balance.
    #[test]
    fn a_finished_invocation_offers_nothing() {
        for text in [
            "child(1., 2.)",
            "child(1., 2.) ",
            "child()",
            "top(100.)",
            "child(1., 2.).",
        ] {
            assert!(labels(text).is_empty(), "{text} should offer nothing");
        }
        // `inst` splices its argument one call deeper, so it is classified
        // there rather than at the same place as `openCell`.
        assert!(items(CellExpressionCommand::Inst, "child(1., 2.)").is_empty());
    }

    #[test]
    fn a_literal_is_not_extended_into_a_name() {
        for text in ["child(2000.", "child(1., 4)", "child(\"met1\")"] {
            assert!(labels(text).is_empty(), "{text} should offer nothing");
        }
    }

    #[test]
    fn a_string_or_comment_offers_nothing() {
        for text in [
            "child(\"me",
            "child(\"met1 la",
            "child(1., // ",
            "child(1., /* la",
        ] {
            assert!(labels(text).is_empty(), "{text} should offer nothing");
        }
    }

    #[test]
    fn generated_names_are_never_offered() {
        assert!(
            !labels("")
                .iter()
                .any(|label| label.starts_with(GENERATED_PREFIX))
        );
    }

    #[test]
    fn a_candidate_replaces_only_the_identifier_before_the_cursor() {
        assert_eq!(identifier_prefix_len("chi", 3), 3);
        assert_eq!(identifier_prefix_len("child(", 6), 0);
        assert_eq!(identifier_prefix_len("child(1., ", 10), 0);
        assert_eq!(identifier_prefix_len("child(1., la", 12), 2);
        assert_eq!(identifier_prefix_len("child(std::", 11), 0);
        assert_eq!(identifier_prefix_len("child(std::fo", 13), 2);
    }

    #[test]
    fn a_cursor_inside_a_character_is_clamped_to_its_boundary() {
        let text = "child(\"µ";
        assert_eq!(clamp_cursor(text, text.len()), text.len());
        assert_eq!(clamp_cursor(text, text.len() - 1), text.len() - 2);
        assert_eq!(clamp_cursor(text, text.len() + 10), text.len());
    }
}
