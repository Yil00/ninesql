//! Execution requests (F-SQL-03, F-SQL-06, F-SQL-12): what to run (statement under the cursor,
//! selection, script), split by the dialect, bound to the prompted parameters, handed to the
//! core's execution service through [`ExecutionSink`]; error positions mapped back into the
//! editor.
//!
//! The editor only describes the run. The core classifies every item, asks for production
//! confirmations, refuses writes in read-only sessions and runs the statements on the driver
//! runtime (ADR-0004, ADR-0007); the editor never awaits it (I6).

use std::fmt;
use std::ops::Range;

use ninesql_core::ConnectionId;
use ninesql_core::driver_api::Param;

use crate::params::{BoundStatement, ParamRef, ParameterStyle, find_parameters};
use crate::split::{ItemKind, ScriptItem, StatementSplitter, item_at_cursor};
use crate::text::{LineIndex, TextPosition, char_to_byte};

/// What the user asked to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionKind {
    /// The statement under the cursor (the selection, when there is one).
    Statement,
    /// The selected text, split into its statements.
    Selection,
    /// The whole script (the selection, when there is one).
    Script,
}

/// Why nothing can run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// No statement under the cursor, an empty selection or an empty script.
    #[error("nothing to run here")]
    NothingToRun,
    /// "Execute selection" without a selection.
    #[error("select the statements to run first")]
    NoSelection,
}

/// One item to run, located in the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedItem {
    /// Statement, client directive or refused (the core refuses it again).
    pub kind: ItemKind,
    /// Byte range in the document.
    pub span: Range<usize>,
    /// Where it starts (line / column for messages).
    pub start: TextPosition,
    /// Its text, as typed.
    pub text: String,
    /// Its placeholders, spans relative to `text`.
    pub params: Vec<ParamRef>,
}

/// The items of a run, before parameter binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPlan {
    /// What was asked.
    pub kind: ExecutionKind,
    /// Items in order.
    pub items: Vec<PlannedItem>,
    /// The document text from the first item's start to the last item's end when the plan was
    /// built: a script runs this range, so the run is refused if any byte of it changed since
    /// (between the items included).
    pub range_text: String,
}

impl ExecutionPlan {
    /// Builds the plan for `kind` over `document`, with `selection` (byte range, empty for a
    /// plain cursor) and `cursor`.
    ///
    /// # Errors
    /// [`PlanError`] when there is nothing to run.
    pub fn build(
        document: &str,
        selection: Range<usize>,
        cursor: usize,
        kind: ExecutionKind,
        splitter: &dyn StatementSplitter,
        style: ParameterStyle,
    ) -> Result<Self, PlanError> {
        let has_selection = !selection.is_empty()
            && document
                .get(selection.clone())
                .is_some_and(|s| !s.trim().is_empty());
        let (base, items): (usize, Vec<ScriptItem>) = match kind {
            ExecutionKind::Selection if !has_selection => return Err(PlanError::NoSelection),
            ExecutionKind::Selection | ExecutionKind::Statement | ExecutionKind::Script
                if has_selection =>
            {
                let text = document.get(selection.clone()).unwrap_or("");
                (selection.start, splitter.split(text))
            }
            ExecutionKind::Statement => {
                let all = splitter.split(document);
                let item = item_at_cursor(document, &all, cursor).cloned();
                (0, item.into_iter().collect())
            }
            ExecutionKind::Script | ExecutionKind::Selection => (0, splitter.split(document)),
        };
        let index = LineIndex::new(document);
        let items: Vec<PlannedItem> = items
            .into_iter()
            .filter(|it| !it.span.is_empty())
            .map(|it| {
                let span = it.span.start + base..it.span.end + base;
                let text = document.get(span.clone()).unwrap_or("").to_owned();
                let params = if it.kind == ItemKind::Statement {
                    find_parameters(&text, &splitter.segments(&text), style)
                } else {
                    Vec::new()
                };
                PlannedItem {
                    kind: it.kind,
                    start: index.position(span.start),
                    span,
                    text,
                    params,
                }
            })
            .collect();
        let (Some(first), Some(last)) = (items.first(), items.last()) else {
            return Err(PlanError::NothingToRun);
        };
        let range_text = document
            .get(first.span.start..last.span.end)
            .unwrap_or("")
            .to_owned();
        Ok(Self {
            kind,
            items,
            range_text,
        })
    }

    /// Whether any item has a placeholder (the parameters prompt is needed).
    #[must_use]
    pub fn needs_parameters(&self) -> bool {
        self.items.iter().any(|i| !i.params.is_empty())
    }

    /// Placeholders per item, for [`ParameterPrompt::new`](crate::params::ParameterPrompt::new).
    #[must_use]
    pub fn parameter_refs(&self) -> Vec<Vec<ParamRef>> {
        self.items.iter().map(|i| i.params.clone()).collect()
    }

    /// Item texts, for [`ParameterPrompt::submit`](crate::params::ParameterPrompt::submit).
    #[must_use]
    pub fn texts(&self) -> Vec<&str> {
        self.items.iter().map(|i| i.text.as_str()).collect()
    }

    /// The request, without parameters (`bound` is `None`) or with the bound statements, one
    /// per item.
    #[must_use]
    pub fn into_request(
        self,
        id: RequestId,
        connection: Option<ConnectionId>,
        bound: Option<Vec<BoundStatement>>,
        bidi_controls: usize,
    ) -> ExecutionRequest {
        let mut bound = bound.map(Vec::into_iter);
        let items = self
            .items
            .into_iter()
            .map(|item| {
                let (text, params, rewrites) = match bound.as_mut().and_then(Iterator::next) {
                    Some(b) => (b.text, b.params, b.rewrites),
                    None => (item.text.clone(), Vec::new(), Vec::new()),
                };
                ExecutionItem {
                    kind: item.kind,
                    span: item.span,
                    start: item.start,
                    original: item.text,
                    text,
                    params,
                    rewrites,
                }
            })
            .collect();
        ExecutionRequest {
            id,
            kind: self.kind,
            connection,
            items,
            range_text: self.range_text,
            bidi_controls,
        }
    }
}

/// Identifies one request of an editor, so a late answer to an older run is recognised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// One item of a request.
#[derive(Clone, PartialEq)]
pub struct ExecutionItem {
    /// Statement, client directive or refused.
    pub kind: ItemKind,
    /// Byte range in the document when the request was made.
    pub span: Range<usize>,
    /// Its start position.
    pub start: TextPosition,
    /// The text as typed (placeholders unchanged).
    pub original: String,
    /// The text to send: placeholders rewritten to the dialect's positional ones.
    pub text: String,
    /// The bound values, in placeholder order. Never logged, never in history (I1).
    pub params: Vec<Param>,
    rewrites: Vec<(Range<usize>, Range<usize>)>,
}

impl ExecutionItem {
    /// Maps a byte offset in [`text`](Self::text) back to one in
    /// [`original`](Self::original): inside a rewritten placeholder, its start.
    #[must_use]
    pub fn sent_to_original(&self, sent: usize) -> usize {
        let mut delta: isize = 0;
        for (sent_range, typed) in &self.rewrites {
            if sent < sent_range.start {
                break;
            }
            if sent < sent_range.end {
                return typed.start;
            }
            delta = typed.end as isize - sent_range.end as isize;
        }
        sent.saturating_add_signed(delta).min(self.original.len())
    }
}

impl fmt::Debug for ExecutionItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No text (query text is logged only at trace level and never on production) and no
        // values.
        f.debug_struct("ExecutionItem")
            .field("kind", &self.kind)
            .field("span", &self.span)
            .field("params", &self.params.len())
            .finish_non_exhaustive()
    }
}

/// What the editor hands to the core's execution service.
#[derive(Clone, PartialEq)]
pub struct ExecutionRequest {
    /// Request id, increasing per editor.
    pub id: RequestId,
    /// What was asked.
    pub kind: ExecutionKind,
    /// The editor's connection (`None`: the core asks for one or refuses).
    pub connection: Option<ConnectionId>,
    /// Items in order.
    pub items: Vec<ExecutionItem>,
    /// The document text from the first item's start to the last item's end when the request
    /// was made (see [`ExecutionPlan::range_text`]).
    pub range_text: String,
    /// Bidirectional control characters in the document when the request was made; the
    /// confirmation dialogs mention them.
    pub bidi_controls: usize,
}

impl fmt::Debug for ExecutionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionRequest")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("connection", &self.connection)
            .field("items", &self.items)
            .field("range_text_len", &self.range_text.len())
            .field("bidi_controls", &self.bidi_controls)
            .finish()
    }
}

/// The core's execution service, as the editor sees it. `submit` returns at once: the run
/// happens on the driver runtime and the outcome comes back through
/// `SqlEditor::report_error` / `SqlEditor::finish_run` (adapter wiring in P3-C).
pub trait ExecutionSink {
    /// Queues `request`.
    fn submit(&self, request: ExecutionRequest);

    /// Cancels the running request `id`, if any.
    fn cancel(&self, id: RequestId);
}

/// Where an engine located an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorLocation {
    /// 1-based character offset in the statement sent (PostgreSQL `position`,
    /// `DriverError::Query::position`).
    CharOffset(u32),
    /// 1-based line and optional 1-based column in the statement sent (MySQL "at line n").
    LineColumn {
        /// 1-based line.
        line: u32,
        /// 1-based column.
        column: Option<u32>,
    },
}

/// An error reported for one item of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementError {
    /// The request.
    pub request: RequestId,
    /// Index of the item in the request.
    pub item: usize,
    /// Display-only engine message.
    pub message: String,
    /// Engine code (SQLSTATE…).
    pub code: Option<String>,
    /// Where, when the engine said.
    pub location: Option<ErrorLocation>,
}

/// An error placed in the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedError {
    /// Byte range to mark in the document (the item, or one character at the position).
    pub range: Range<usize>,
    /// Position of `range.start`.
    pub position: TextPosition,
    /// Whether the engine gave a position (otherwise the whole item is marked).
    pub precise: bool,
}

/// Places `error` in `document`, given the request it answers. `None` when the item was edited
/// since the request (its text no longer matches): the message is shown without a position
/// rather than at a wrong one.
#[must_use]
pub fn place_error(
    document: &str,
    request: &ExecutionRequest,
    error: &StatementError,
) -> Option<PlacedError> {
    let item = request.items.get(error.item)?;
    if document.get(item.span.clone()) != Some(item.original.as_str()) {
        return None;
    }
    let local_sent = match error.location {
        Some(ErrorLocation::CharOffset(n)) => {
            Some(char_to_byte(&item.text, (n.max(1) - 1) as usize))
        }
        Some(ErrorLocation::LineColumn { line, column }) => {
            let index = LineIndex::new(&item.text);
            Some(index.offset(
                (line.max(1) - 1) as usize,
                column.map_or(0, |c| (c.max(1) - 1) as usize),
            ))
        }
        None => None,
    };
    let index = LineIndex::new(document);
    match local_sent {
        Some(sent) => {
            let local = item.sent_to_original(sent);
            let start = item.span.start + local;
            let len = document[start..]
                .chars()
                .next()
                .filter(|c| *c != '\n')
                .map_or(0, char::len_utf8);
            Some(PlacedError {
                range: start..start + len,
                position: index.position(start),
                precise: true,
            })
        }
        None => Some(PlacedError {
            range: item.span.clone(),
            position: index.position(item.span.start),
            precise: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::ParameterPrompt;
    use crate::split::DialectSplitter;
    use ninesql_core::driver_api::{AnsiDialect, LexicalRules};
    use std::sync::Arc;

    struct Pg;
    impl StatementSplitter for Pg {
        fn split(&self, text: &str) -> Vec<ScriptItem> {
            DialectSplitter::new(Arc::new(AnsiDialect)).split(text)
        }
        fn segments(&self, text: &str) -> Vec<crate::split::Segment> {
            crate::split::code_segments(text, LexicalRules::POSTGRES)
        }
    }

    fn plan(
        doc: &str,
        sel: Range<usize>,
        cursor: usize,
        kind: ExecutionKind,
    ) -> Result<ExecutionPlan, PlanError> {
        ExecutionPlan::build(doc, sel, cursor, kind, &Pg, ParameterStyle::POSTGRES)
    }

    fn texts(p: &ExecutionPlan) -> Vec<&str> {
        p.texts()
    }

    const DOC: &str = "select 1;\nselect 2;\n\nselect 3";

    #[test]
    fn statement_selection_script() {
        let p = plan(DOC, 12..12, 12, ExecutionKind::Statement);
        assert_eq!(p.as_ref().map(texts).ok(), Some(vec!["select 2"]));
        let p = plan(DOC, 0..0, 20, ExecutionKind::Statement);
        assert_eq!(p, Err(PlanError::NothingToRun), "blank line");
        let p = plan(DOC, 0..0, 0, ExecutionKind::Script);
        assert_eq!(
            p.as_ref().map(texts).ok(),
            Some(vec!["select 1", "select 2", "select 3"])
        );
        let p = plan(DOC, 0..0, 0, ExecutionKind::Selection);
        assert_eq!(p, Err(PlanError::NoSelection));
        // A selection over two statements, offsets in the document.
        let p = plan(DOC, 7..18, 18, ExecutionKind::Selection);
        let Ok(p) = p else { panic!("plan") };
        assert_eq!(texts(&p), ["1", "select 2"]);
        assert_eq!(p.items[1].span, 10..18);
        assert_eq!((p.items[1].start.line, p.items[1].start.column), (1, 0));
        // With a selection, "statement" runs the selection (DBeaver behaviour).
        let p = plan(DOC, 10..18, 18, ExecutionKind::Statement);
        assert_eq!(p.as_ref().map(texts).ok(), Some(vec!["select 2"]));
    }

    #[test]
    fn the_range_from_first_to_last_item_is_kept() {
        let Ok(p) = plan(DOC, 0..0, 0, ExecutionKind::Script) else {
            panic!("plan")
        };
        assert_eq!(p.range_text, "select 1;\nselect 2;\n\nselect 3");
        let Ok(p) = plan(DOC, 7..18, 18, ExecutionKind::Selection) else {
            panic!("plan")
        };
        assert_eq!(p.range_text, "1;\nselect 2");
        let req = p.into_request(RequestId(1), None, None, 0);
        assert_eq!(req.range_text, "1;\nselect 2");
    }

    #[test]
    fn errors_map_back_to_the_document() {
        let doc = "select 1;\nselect * frm t;";
        let Ok(p) = plan(doc, 0..0, 12, ExecutionKind::Statement) else {
            panic!("plan")
        };
        let req = p.into_request(RequestId(1), None, None, 0);
        let err = StatementError {
            request: RequestId(1),
            item: 0,
            message: "syntax error at or near \"frm\"".into(),
            code: Some("42601".into()),
            location: Some(ErrorLocation::CharOffset(10)),
        };
        let Some(placed) = place_error(doc, &req, &err) else {
            panic!("placed")
        };
        assert_eq!(&doc[placed.range.clone()], "f");
        assert_eq!((placed.position.line, placed.position.column), (1, 9));
        assert!(placed.precise);
        // Line / column form.
        let err2 = StatementError {
            location: Some(ErrorLocation::LineColumn {
                line: 1,
                column: Some(10),
            }),
            ..err.clone()
        };
        assert_eq!(
            place_error(doc, &req, &err2).map(|p| p.range.start),
            Some(placed.range.start)
        );
        // No location: the whole statement.
        let err3 = StatementError {
            location: None,
            ..err.clone()
        };
        let whole = place_error(doc, &req, &err3).map(|p| p.range);
        assert_eq!(whole.map(|r| &doc[r]), Some("select * frm t"));
        // Edited since: no position rather than a wrong one.
        assert_eq!(place_error("select 1;\nselect * from t;", &req, &err), None);
    }

    #[test]
    fn error_positions_survive_placeholder_rewrites() {
        let doc = "select :long_name, x frm t where a = :b";
        let Ok(p) = plan(doc, 0..0, 0, ExecutionKind::Statement) else {
            panic!("plan")
        };
        assert!(p.needs_parameters());
        let Ok(Some(mut prompt)) = ParameterPrompt::new(p.parameter_refs()) else {
            panic!("prompt")
        };
        prompt.set_text(0, "v");
        prompt.set_text(1, "w");
        let texts: Vec<String> = p.texts().into_iter().map(str::to_owned).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let Ok(bound) = prompt.submit(&refs, ParameterStyle::POSTGRES) else {
            panic!("bound")
        };
        let req = p.into_request(RequestId(2), None, Some(bound), 0);
        assert_eq!(req.items[0].text, "select $1, x frm t where a = $2");
        assert_eq!(req.items[0].params.len(), 2);
        let frm_sent = req.items[0].text.find("frm").unwrap_or(0) as u32 + 1;
        let err = StatementError {
            request: RequestId(2),
            item: 0,
            message: "syntax error".into(),
            code: None,
            location: Some(ErrorLocation::CharOffset(frm_sent)),
        };
        let Some(placed) = place_error(doc, &req, &err) else {
            panic!("placed")
        };
        assert_eq!(placed.range.start, doc.find("frm").unwrap_or(0));
        // Inside a rewritten placeholder: its start in the typed text.
        let err = StatementError {
            location: Some(ErrorLocation::CharOffset(9)),
            ..err
        };
        assert_eq!(place_error(doc, &req, &err).map(|p| p.range.start), Some(7));
        // The request's Debug shows neither text nor values.
        let debug = format!("{req:?}");
        assert!(
            !debug.contains("frm") && !debug.contains("\"v\""),
            "{debug}"
        );
    }
}
