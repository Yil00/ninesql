//! gpui-component's `CompletionProvider` over [`CompletionEngine`]: the editor's completion menu.

use std::rc::Rc;

use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::{App, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    Range as LspRange, TextEdit,
};

use super::{CandidateKind, CompletionEngine, MetadataProvider};
use crate::split::StatementSplitter;

/// Completion for one editor: the engine, the dialect's splitter and the metadata provider of
/// the editor's connection.
pub struct SqlCompletionProvider {
    engine: CompletionEngine,
    splitter: Rc<dyn StatementSplitter>,
    metadata: Rc<dyn MetadataProvider>,
}

impl SqlCompletionProvider {
    /// A provider over `engine`, `splitter` and `metadata`.
    #[must_use]
    pub fn new(
        engine: CompletionEngine,
        splitter: Rc<dyn StatementSplitter>,
        metadata: Rc<dyn MetadataProvider>,
    ) -> Self {
        Self {
            engine,
            splitter,
            metadata,
        }
    }

    /// The menu items for `offset` in `text`.
    #[must_use]
    pub fn items(&self, text: &Rope, offset: usize) -> Vec<CompletionItem> {
        let source = text.to_string();
        let result = self.engine.complete(
            &source,
            offset,
            self.splitter.as_ref(),
            self.metadata.as_ref(),
        );
        let range = LspRange::new(
            text.offset_to_position(result.range.start),
            text.offset_to_position(result.range.end),
        );
        result
            .candidates
            .into_iter()
            .map(|c| CompletionItem {
                label: c.label.clone(),
                kind: Some(match c.kind {
                    CandidateKind::Keyword => CompletionItemKind::KEYWORD,
                    CandidateKind::Schema => CompletionItemKind::MODULE,
                    CandidateKind::Table | CandidateKind::Cte => CompletionItemKind::STRUCT,
                    CandidateKind::View => CompletionItemKind::INTERFACE,
                    CandidateKind::Function => CompletionItemKind::FUNCTION,
                    CandidateKind::Column => CompletionItemKind::FIELD,
                    CandidateKind::Alias => CompletionItemKind::VARIABLE,
                }),
                detail: c.detail,
                filter_text: Some(c.label),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: c.insert,
                })),
                ..CompletionItem::default()
            })
            .collect()
    }
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        Task::ready(Ok(CompletionResponse::Array(self.items(text, offset))))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{ColumnEntry, KeywordSet, ObjectEntry};
    use crate::split::DialectSplitter;
    use ninesql_core::driver_api::AnsiDialect;
    use std::sync::Arc;

    struct OneTable;

    impl MetadataProvider for OneTable {
        fn schemas(&self) -> Vec<String> {
            Vec::new()
        }
        fn default_schema(&self) -> Option<String> {
            None
        }
        fn objects(&self, _: Option<&str>) -> Vec<ObjectEntry> {
            vec![ObjectEntry {
                name: "Order Lines".into(),
                kind: crate::completion::ObjectKind::Table,
            }]
        }
        fn columns(&self, _: Option<&str>, _: &str) -> Option<Vec<ColumnEntry>> {
            None
        }
    }

    #[test]
    fn items_replace_the_prefix_with_the_quoted_name() {
        let provider = SqlCompletionProvider::new(
            CompletionEngine::new(KeywordSet::Ansi),
            Rc::new(DialectSplitter::new(Arc::new(AnsiDialect))),
            Rc::new(OneTable),
        );
        let text = Rope::from("select 1;\nselect * from ord");
        let items = provider.items(&text, text.len());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, Some(CompletionItemKind::STRUCT));
        let Some(CompletionTextEdit::Edit(edit)) = &items[0].text_edit else {
            panic!("a text edit");
        };
        assert_eq!(edit.new_text, "\"Order Lines\"");
        assert_eq!((edit.range.start.line, edit.range.start.character), (1, 14));
        assert_eq!((edit.range.end.line, edit.range.end.character), (1, 17));
    }
}
