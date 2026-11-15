//! Query parameters (F-SQL-06): detection of `:name`, `?`, `?1`, `$1`, `@name`, `$name` per
//! dialect, the prompt model and the binding into each statement.
//!
//! Values are never logged nor written to history (I1): [`ParameterPrompt`] keeps them in
//! zeroized buffers, [`ParameterPrompt::submit`] consumes the prompt, and the bound values travel
//! as [`Param`]s inside the execution request only, whose `Debug` names types, never values.
//! Named parameters are rewritten to the dialect's positional placeholders: values are always
//! bound, never inlined.

use std::fmt;
use std::ops::Range;

use ninesql_core::driver_api::{EngineId, Param};
use zeroize::Zeroizing;

use crate::split::{Segment, SegmentKind};

/// Which placeholder syntaxes a dialect accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ParameterStyle {
    /// `:name`, rewritten client-side (DBeaver style).
    pub colon_named: bool,
    /// `?`.
    pub question_mark: bool,
    /// `?1` (SQLite).
    pub question_numbered: bool,
    /// `$1` (PostgreSQL family).
    pub dollar_numbered: bool,
    /// `@name` (SQLite; in MySQL it is a user variable, not a parameter).
    pub at_named: bool,
    /// `$name` (SQLite).
    pub dollar_named: bool,
    /// The placeholder the engine binds positionally: `$n` when true, `?` otherwise.
    pub binds_dollar: bool,
}

impl ParameterStyle {
    /// ANSI / JDBC: `?` and `:name`.
    pub const ANSI: Self = Self {
        colon_named: true,
        question_mark: true,
        question_numbered: false,
        dollar_numbered: false,
        at_named: false,
        dollar_named: false,
        binds_dollar: false,
    };
    /// PostgreSQL family: `$1` and `:name`; `?` is a JSON operator there, never a parameter.
    pub const POSTGRES: Self = Self {
        question_mark: false,
        dollar_numbered: true,
        binds_dollar: true,
        ..Self::ANSI
    };
    /// MySQL / MariaDB: `?` and `:name`.
    pub const MYSQL: Self = Self::ANSI;
    /// SQLite: `?`, `?1`, `:name`, `@name`, `$name`.
    pub const SQLITE: Self = Self {
        question_numbered: true,
        at_named: true,
        dollar_named: true,
        ..Self::ANSI
    };

    /// The style of an engine.
    #[must_use]
    pub fn for_engine(engine: &EngineId) -> Self {
        match engine.as_str() {
            "postgresql" | "timescaledb" | "yugabytedb" | "questdb" => Self::POSTGRES,
            "mysql" | "mariadb" | "tidb" => Self::MYSQL,
            "sqlite" => Self::SQLITE,
            _ => Self::ANSI,
        }
    }

    /// The bind placeholder for the 1-based `index`.
    #[must_use]
    pub fn placeholder(&self, index: usize) -> String {
        if self.binds_dollar {
            format!("${index}")
        } else {
            "?".to_owned()
        }
    }
}

/// What a placeholder refers to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ParamKey {
    /// A named parameter (`:id`, `@id`, `$id`), name without its sigil.
    Named(String),
    /// An anonymous `?`, the n-th of its statement (1-based).
    Positional(usize),
    /// A numbered parameter (`$1`, `?1`).
    Numbered(u32),
}

impl fmt::Display for ParamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(n) => write!(f, ":{n}"),
            Self::Positional(i) => write!(f, "? #{i}"),
            Self::Numbered(n) => write!(f, "${n}"),
        }
    }
}

/// One placeholder found in a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamRef {
    /// Byte range of the placeholder, sigil included, in the statement text.
    pub span: Range<usize>,
    /// What it refers to.
    pub key: ParamKey,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The placeholders of `text` (one statement), from its code segments only (never inside
/// strings, quoted identifiers or comments).
#[must_use]
pub fn find_parameters(text: &str, segments: &[Segment], style: ParameterStyle) -> Vec<ParamRef> {
    let mut out = Vec::new();
    let mut positional = 0usize;
    for seg in segments.iter().filter(|s| s.kind == SegmentKind::Code) {
        let code = &text[seg.range.clone()];
        let mut chars = code.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let start = seg.range.start + i;
            let prev = text[..start].chars().next_back();
            let ident_len = |from: usize| -> usize {
                let rest = &text[from..];
                match rest.chars().next() {
                    Some(c) if is_ident_start(c) => rest
                        .char_indices()
                        .find(|&(_, c)| !is_ident_char(c))
                        .map_or(rest.len(), |(j, _)| j),
                    _ => 0,
                }
            };
            let digits_len = |from: usize| -> usize {
                text[from..].bytes().take_while(u8::is_ascii_digit).count()
            };
            let after_word = prev.is_some_and(is_ident_char);
            let found = match c {
                ':' if style.colon_named
                    && !after_word
                    && !matches!(prev, Some(':' | '[' | '$'))
                    && !text[start + 1..].starts_with(':') =>
                {
                    let len = ident_len(start + 1);
                    (len > 0).then(|| {
                        let name = text[start + 1..start + 1 + len].to_owned();
                        (start + 1 + len, ParamKey::Named(name))
                    })
                }
                '@' if style.at_named && !after_word && prev != Some('@') => {
                    let len = ident_len(start + 1);
                    (len > 0).then(|| {
                        let name = text[start + 1..start + 1 + len].to_owned();
                        (start + 1 + len, ParamKey::Named(name))
                    })
                }
                '$' if !after_word => {
                    let digits = digits_len(start + 1);
                    if style.dollar_numbered && digits > 0 {
                        text[start + 1..start + 1 + digits]
                            .parse::<u32>()
                            .ok()
                            .map(|n| (start + 1 + digits, ParamKey::Numbered(n)))
                    } else if style.dollar_named {
                        let len = ident_len(start + 1);
                        (len > 0).then(|| {
                            let name = text[start + 1..start + 1 + len].to_owned();
                            (start + 1 + len, ParamKey::Named(name))
                        })
                    } else {
                        None
                    }
                }
                '?' if style.question_mark => {
                    let digits = digits_len(start + 1);
                    if style.question_numbered && digits > 0 {
                        text[start + 1..start + 1 + digits]
                            .parse::<u32>()
                            .ok()
                            .map(|n| (start + 1 + digits, ParamKey::Numbered(n)))
                    } else {
                        positional += 1;
                        Some((start + 1, ParamKey::Positional(positional)))
                    }
                }
                _ => None,
            };
            if let Some((end, key)) = found {
                out.push(ParamRef {
                    span: start..end,
                    key,
                });
                while chars
                    .peek()
                    .is_some_and(|&(j, _)| seg.range.start + j < end)
                {
                    chars.next();
                }
            }
        }
    }
    out
}

/// Why a statement's parameters cannot be bound.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParameterError {
    /// A statement mixes anonymous `?` with named or numbered parameters.
    #[error("a statement mixes anonymous `?` with named or numbered parameters")]
    MixedStyles,
    /// A parameter has no value yet.
    #[error("no value for parameter {0}")]
    Missing(String),
}

/// One prompt entry: a distinct parameter across the statements to run.
pub struct PromptEntry {
    /// The parameter, per statement for anonymous `?`.
    pub key: ParamKey,
    /// Index of the statement for anonymous `?` (they are not shared across statements).
    pub item: Option<usize>,
    value: Option<PromptValue>,
}

enum PromptValue {
    Text(Zeroizing<String>),
    Null,
}

impl PromptEntry {
    /// The label shown in the prompt (`:id`, `$1`, `? #2 (statement 3)`).
    #[must_use]
    pub fn label(&self) -> String {
        match self.item {
            Some(item) => format!("{} (statement {})", self.key, item + 1),
            None => self.key.to_string(),
        }
    }

    /// Whether a value (or NULL) was given.
    #[must_use]
    pub fn has_value(&self) -> bool {
        self.value.is_some()
    }

    /// Whether the value is NULL.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self.value, Some(PromptValue::Null))
    }
}

impl fmt::Debug for PromptEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the value (AGENTS.md: parameter values are never logged).
        f.debug_struct("PromptEntry")
            .field("key", &self.key)
            .field("item", &self.item)
            .field("has_value", &self.has_value())
            .finish()
    }
}

/// The parameters prompt for one execution. Built from the placeholders of each statement to
/// run; consumed by [`submit`](Self::submit).
#[derive(Debug)]
pub struct ParameterPrompt {
    entries: Vec<PromptEntry>,
    refs: Vec<Vec<ParamRef>>,
}

/// Values bound to one statement, in placeholder order, with the rewritten text.
pub struct BoundStatement {
    /// The statement text with named / numbered placeholders turned into the dialect's
    /// positional ones.
    pub text: String,
    /// The values, in placeholder order.
    pub params: Vec<Param>,
    /// For each placeholder: its range in [`text`](Self::text) and in the typed text.
    pub rewrites: Vec<(Range<usize>, Range<usize>)>,
}

impl fmt::Debug for BoundStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundStatement")
            .field("params", &self.params.len())
            .finish_non_exhaustive()
    }
}

impl ParameterPrompt {
    /// A prompt over the placeholders of each statement (`refs[i]` for statement `i`). `None`
    /// when no statement has a placeholder.
    ///
    /// # Errors
    /// [`ParameterError::MixedStyles`] when a statement mixes `?` with other placeholders.
    pub fn new(refs: Vec<Vec<ParamRef>>) -> Result<Option<Self>, ParameterError> {
        let mut entries: Vec<PromptEntry> = Vec::new();
        for (item, item_refs) in refs.iter().enumerate() {
            let anonymous = item_refs
                .iter()
                .any(|r| matches!(r.key, ParamKey::Positional(_)));
            let other = item_refs
                .iter()
                .any(|r| !matches!(r.key, ParamKey::Positional(_)));
            if anonymous && other {
                return Err(ParameterError::MixedStyles);
            }
            for r in item_refs {
                let item = matches!(r.key, ParamKey::Positional(_)).then_some(item);
                if !entries.iter().any(|e| e.key == r.key && e.item == item) {
                    entries.push(PromptEntry {
                        key: r.key.clone(),
                        item,
                        value: None,
                    });
                }
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self { entries, refs }))
    }

    /// The entries, in first-appearance order.
    #[must_use]
    pub fn entries(&self) -> &[PromptEntry] {
        &self.entries
    }

    /// Sets entry `index` to a text value (the engine converts it). Out of range: ignored.
    pub fn set_text(&mut self, index: usize, value: &str) {
        if let Some(e) = self.entries.get_mut(index) {
            e.value = Some(PromptValue::Text(Zeroizing::new(value.to_owned())));
        }
    }

    /// Sets entry `index` to NULL.
    pub fn set_null(&mut self, index: usize) {
        if let Some(e) = self.entries.get_mut(index) {
            e.value = Some(PromptValue::Null);
        }
    }

    /// Whether every entry has a value.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.entries.iter().all(PromptEntry::has_value)
    }

    /// Binds every statement and drops the prompt (its buffers are wiped). `texts[i]` is the
    /// text of statement `i`, the one its placeholders were found in.
    ///
    /// # Errors
    /// [`ParameterError::Missing`] when an entry has no value; the prompt is consumed anyway, so
    /// the caller asks again rather than keeping values around.
    pub fn submit(
        self,
        texts: &[&str],
        style: ParameterStyle,
    ) -> Result<Vec<BoundStatement>, ParameterError> {
        if let Some(missing) = self.entries.iter().find(|e| !e.has_value()) {
            return Err(ParameterError::Missing(missing.label()));
        }
        let mut out = Vec::with_capacity(texts.len());
        for (item, text) in texts.iter().enumerate() {
            let refs = self.refs.get(item).map_or(&[][..], Vec::as_slice);
            out.push(self.bind(item, text, refs, style));
        }
        Ok(out)
    }

    fn value_for(&self, item: usize, key: &ParamKey) -> Param {
        let item = matches!(key, ParamKey::Positional(_)).then_some(item);
        match self
            .entries
            .iter()
            .find(|e| &e.key == key && e.item == item)
            .and_then(|e| e.value.as_ref())
        {
            Some(PromptValue::Text(t)) => Param::Text(t.as_str().to_owned()),
            Some(PromptValue::Null) | None => Param::Null,
        }
    }

    fn bind(
        &self,
        item: usize,
        text: &str,
        refs: &[ParamRef],
        style: ParameterStyle,
    ) -> BoundStatement {
        let mut out = String::with_capacity(text.len());
        let mut params = Vec::with_capacity(refs.len());
        let mut rewrites = Vec::with_capacity(refs.len());
        let mut at = 0;
        for r in refs {
            out.push_str(&text[at..r.span.start]);
            params.push(self.value_for(item, &r.key));
            let start = out.len();
            out.push_str(&style.placeholder(params.len()));
            rewrites.push((start..out.len(), r.span.clone()));
            at = r.span.end;
        }
        out.push_str(&text[at..]);
        BoundStatement {
            text: out,
            params,
            rewrites,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::code_segments;
    use ninesql_core::driver_api::LexicalRules;

    fn keys(text: &str, rules: LexicalRules, style: ParameterStyle) -> Vec<String> {
        let segs = code_segments(text, rules);
        find_parameters(text, &segs, style)
            .into_iter()
            .map(|r| text[r.span].to_owned())
            .collect()
    }

    #[test]
    fn detects_per_dialect() {
        let pg = |t| keys(t, LexicalRules::POSTGRES, ParameterStyle::POSTGRES);
        assert_eq!(
            pg("select * from t where a = :id and b = $2 and c::int > 0 and j ? 'k'"),
            [":id", "$2"]
        );
        assert_eq!(
            pg("select arr[1:n], arr[:m], ':x', \":y\" -- :z"),
            Vec::<String>::new()
        );
        assert_eq!(pg("select $1, $q$ :no $q$"), ["$1"]);

        let my = |t| keys(t, LexicalRules::MYSQL, ParameterStyle::MYSQL);
        assert_eq!(my("select ?, @v, :n, ':s' # :c"), ["?", ":n"]);
        assert_eq!(my("set @a := 1"), Vec::<String>::new());

        let lite = |t| keys(t, LexicalRules::SQLITE, ParameterStyle::SQLITE);
        assert_eq!(
            lite("select ?, ?3, :a, @b, $c, e@x"),
            ["?", "?3", ":a", "@b", "$c"]
        );
    }

    #[test]
    fn engines_map_to_styles() {
        assert_eq!(
            ParameterStyle::for_engine(&EngineId::POSTGRESQL),
            ParameterStyle::POSTGRES
        );
        assert_eq!(
            ParameterStyle::for_engine(&EngineId::MARIADB),
            ParameterStyle::MYSQL
        );
        assert_eq!(
            ParameterStyle::for_engine(&EngineId::SQLITE),
            ParameterStyle::SQLITE
        );
        assert_eq!(
            ParameterStyle::for_engine(&EngineId::DUCKDB),
            ParameterStyle::ANSI
        );
    }

    fn refs_of(texts: &[&str], rules: LexicalRules, style: ParameterStyle) -> Vec<Vec<ParamRef>> {
        texts
            .iter()
            .map(|t| find_parameters(t, &code_segments(t, rules), style))
            .collect()
    }

    #[test]
    fn prompt_shares_named_values_and_binds() {
        let texts = ["select :id, :name, :id", "delete from t where id = :id"];
        let style = ParameterStyle::POSTGRES;
        let refs = refs_of(&texts, LexicalRules::POSTGRES, style);
        let Ok(Some(mut prompt)) = ParameterPrompt::new(refs) else {
            panic!("a prompt");
        };
        let labels: Vec<String> = prompt.entries().iter().map(PromptEntry::label).collect();
        assert_eq!(labels, [":id", ":name"]);
        assert!(!prompt.is_complete());
        prompt.set_text(0, "42");
        prompt.set_null(1);
        assert!(prompt.is_complete());
        let debug = format!("{prompt:?}");
        assert!(!debug.contains("42"), "{debug}");
        let Ok(bound) = prompt.submit(&texts, style) else {
            panic!("bound");
        };
        assert_eq!(bound[0].text, "select $1, $2, $3");
        assert_eq!(
            bound[0].params,
            [
                Param::Text("42".into()),
                Param::Null,
                Param::Text("42".into())
            ]
        );
        assert_eq!(bound[1].text, "delete from t where id = $1");
        assert!(!format!("{:?}", bound[0]).contains("42"));
    }

    #[test]
    fn anonymous_parameters_are_per_statement() {
        let texts = ["select ?, ?", "select ?"];
        let style = ParameterStyle::MYSQL;
        let refs = refs_of(&texts, LexicalRules::MYSQL, style);
        let Ok(Some(mut prompt)) = ParameterPrompt::new(refs) else {
            panic!("a prompt");
        };
        assert_eq!(prompt.entries().len(), 3);
        assert_eq!(prompt.entries()[2].label(), "? #1 (statement 2)");
        prompt.set_text(0, "a");
        prompt.set_text(1, "b");
        let missing = ParameterPrompt::submit(prompt, &texts, style);
        assert!(matches!(missing, Err(ParameterError::Missing(l)) if l.contains("statement 2")));
    }

    #[test]
    fn mixed_styles_are_refused() {
        let style = ParameterStyle::SQLITE;
        let refs = refs_of(&["select ?, :a"], LexicalRules::SQLITE, style);
        assert!(matches!(
            ParameterPrompt::new(refs),
            Err(ParameterError::MixedStyles)
        ));
        let none = refs_of(&["select 1"], LexicalRules::SQLITE, style);
        assert!(matches!(ParameterPrompt::new(none), Ok(None)));
    }
}
