//! [`Checker`] for AST-based lint rules.
//!
//! The [`Checker`] is responsible for traversing over the AST, building up the [`SemanticModel`],
//! and running any enabled [`Rule`]s at the appropriate place and time.
//!
//! The [`Checker`] is structured as a single pass over the AST that proceeds in "evaluation" order.
//! That is: the [`Checker`] typically iterates over nodes in the order in which they're evaluated
//! by the Python interpreter. This includes, e.g., deferring function body traversal until after
//! parent scopes have been fully traversed. Individual rules may also perform internal traversals
//! of the AST.
//!
//! The individual [`Visitor`] implementations within the [`Checker`] typically proceed in four
//! steps:
//!
//! 1. Binding: Bind any names introduced by the current node.
//! 2. Traversal: Recurse into the children of the current node.
//! 3. Clean-up: Perform any necessary clean-up after the current node has been fully traversed.
//! 4. Analysis: Run any relevant lint rules on the current node.
//!
//! The first three steps together compose the semantic analysis phase, while the last step
//! represents the lint-rule analysis phase. In the future, these steps may be separated into
//! distinct passes over the AST.

use std::cell::RefCell;
use std::path::Path;

use itertools::Itertools;
use log::debug;
use rustc_hash::{FxHashMap, FxHashSet};

use ruff_db::diagnostic::Diagnostic;
use ruff_diagnostics::{Applicability, Fix, IsolationLevel};
use ruff_notebook::{CellOffsets, NotebookIndex};
use ruff_python_ast::helpers::{collect_import_from_member, is_docstring_stmt, to_module_path};
use ruff_python_ast::identifier::Identifier;
use ruff_python_ast::name::QualifiedName;
use ruff_python_ast::str::Quote;
use ruff_python_ast::visitor::{Visitor, walk_except_handler, walk_pattern};
use ruff_python_ast::{
    self as ast, AnyParameterRef, ArgOrKeyword, Comprehension, ElifElseClause, ExceptHandler, Expr,
    ExprContext, ExprFString, ExprTString, InterpolatedStringElement, Keyword, MatchCase,
    ModModule, Parameter, Parameters, Pattern, PythonVersion, Stmt, Suite, UnaryOp,
};
use ruff_python_ast::{PySourceType, helpers, str, visitor};
use ruff_python_codegen::{Generator, Stylist};
use ruff_python_index::Indexer;
use ruff_python_parser::semantic_errors::{
    SemanticSyntaxChecker, SemanticSyntaxContext, SemanticSyntaxError, SemanticSyntaxErrorKind,
};
use ruff_python_parser::typing::{AnnotationKind, ParsedAnnotation, parse_type_annotation};
use ruff_python_parser::{ParseError, Parsed, Tokens};
use ruff_python_semantic::all::{DunderAllDefinition, DunderAllFlags};
use ruff_python_semantic::analyze::{imports, typing};
use ruff_python_semantic::{
    BindingFlags, BindingId, BindingKind, Exceptions, Export, FromImport, GeneratorKind, Globals,
    Import, Module, ModuleKind, ModuleSource, NodeId, ScopeId, ScopeKind, SemanticModel,
    SemanticModelFlags, StarImport, SubmoduleImport,
};
use ruff_python_stdlib::builtins::{MAGIC_GLOBALS, python_builtins};
use ruff_python_trivia::CommentRanges;
use ruff_source_file::{OneIndexed, SourceFile, SourceFileBuilder, SourceRow};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::checkers::ast::annotation::AnnotationContext;
use crate::docstrings::extraction::ExtractionTarget;
use crate::importer::{ImportRequest, Importer, ResolutionError};
use crate::noqa::NoqaMapping;
use crate::package::PackageRoot;
use crate::preview::is_undefined_export_in_dunder_init_enabled;
use crate::registry::Rule;
use crate::rules::pyflakes::rules::{
    LateFutureImport, ReturnOutsideFunction, YieldOutsideFunction,
};
use crate::rules::pylint::rules::{AwaitOutsideAsync, LoadBeforeGlobalDeclaration};
use crate::rules::{flake8_pyi, flake8_type_checking, pyflakes, pyupgrade};
use crate::settings::rule_table::RuleTable;
use crate::settings::{LinterSettings, TargetVersion, flags};
use crate::{Edit, Violation};
use crate::{Locator, docstrings, noqa};

mod analyze;
mod annotation;
mod deferred;

/// State representing whether a docstring is expected or not for the next statement.
#[derive(Debug, Copy, Clone, PartialEq)]
enum DocstringState {
    /// The next statement is expected to be a docstring, but not necessarily so.
    ///
    /// For example, in the following code:
    ///
    /// ```python
    /// class Foo:
    ///     pass
    ///
    ///
    /// def bar(x, y):
    ///     """Docstring."""
    ///     return x +  y
    /// ```
    ///
    /// For `Foo`, the state is expected when the checker is visiting the class
    /// body but isn't going to be present. While, for `bar` function, the docstring
    /// is expected and present.
    Expected(ExpectedDocstringKind),
    Other,
}

impl Default for DocstringState {
    /// Returns the default docstring state which is to expect a module-level docstring.
    fn default() -> Self {
        Self::Expected(ExpectedDocstringKind::Module)
    }
}

impl DocstringState {
    /// Returns the docstring kind if the state is expecting a docstring.
    const fn expected_kind(self) -> Option<ExpectedDocstringKind> {
        match self {
            DocstringState::Expected(kind) => Some(kind),
            DocstringState::Other => None,
        }
    }
}

/// The kind of an expected docstring.
#[derive(Debug, Copy, Clone, PartialEq)]
enum ExpectedDocstringKind {
    /// A module-level docstring.
    ///
    /// For example,
    /// ```python
    /// """This is a module-level docstring."""
    ///
    /// a = 1
    /// ```
    Module,

    /// A class-level docstring.
    ///
    /// For example,
    /// ```python
    /// class Foo:
    ///     """This is the docstring for `Foo` class."""
    ///
    ///     def __init__(self) -> None:
    ///         ...
    /// ```
    Class,

    /// A function-level docstring.
    ///
    /// For example,
    /// ```python
    /// def foo():
    ///     """This is the docstring for `foo` function."""
    ///     pass
    /// ```
    Function,

    /// An attribute-level docstring.
    ///
    /// For example,
    /// ```python
    /// a = 1
    /// """This is the docstring for `a` variable."""
    ///
    ///
    /// class Foo:
    ///     b = 1
    ///     """This is the docstring for `Foo.b` class variable."""
    /// ```
    Attribute,
}

impl ExpectedDocstringKind {
    /// Returns the semantic model flag that represents the current docstring state.
    const fn as_flag(self) -> SemanticModelFlags {
        match self {
            ExpectedDocstringKind::Attribute => SemanticModelFlags::ATTRIBUTE_DOCSTRING,
            _ => SemanticModelFlags::PEP_257_DOCSTRING,
        }
    }
}

pub(crate) struct Checker<'a> {
    /// The [`Parsed`] output for the source code.
    parsed: &'a Parsed<ModModule>,
    /// An internal cache for parsed string annotations
    parsed_annotations_cache: ParsedAnnotationsCache<'a>,
    /// The [`Parsed`] output for the type annotation the checker is currently in.
    parsed_type_annotation: Option<&'a ParsedAnnotation>,
    /// The [`Path`] to the file under analysis.
    path: &'a Path,
    /// The [`Path`] to the package containing the current file.
    package: Option<PackageRoot<'a>>,
    /// The module representation of the current file (e.g., `foo.bar`).
    pub(crate) module: Module<'a>,
    /// The [`PySourceType`] of the current file.
    pub(crate) source_type: PySourceType,
    /// The [`CellOffsets`] for the current file, if it's a Jupyter notebook.
    cell_offsets: Option<&'a CellOffsets>,
    /// The [`NotebookIndex`] for the current file, if it's a Jupyter notebook.
    notebook_index: Option<&'a NotebookIndex>,
    /// The [`flags::Noqa`] for the current analysis (i.e., whether to respect suppression
    /// comments).
    noqa: flags::Noqa,
    /// The [`NoqaMapping`] for the current analysis (i.e., the mapping from line number to
    /// suppression commented line number).
    noqa_line_for: &'a NoqaMapping,
    /// The [`Locator`] for the current file, which enables extraction of source code from byte
    /// offsets.
    locator: &'a Locator<'a>,
    /// The [`Stylist`] for the current file, which detects the current line ending, quote, and
    /// indentation style.
    stylist: &'a Stylist<'a>,
    /// The [`Indexer`] for the current file, which contains the offsets of all comments and more.
    indexer: &'a Indexer,
    /// The [`Importer`] for the current file, which enables importing of other modules.
    importer: Importer<'a>,
    /// The [`SemanticModel`], built up over the course of the AST traversal.
    semantic: SemanticModel<'a>,
    /// A set of deferred nodes to be visited after the current traversal (e.g., function bodies).
    visit: deferred::Visit<'a>,
    /// A set of deferred nodes to be analyzed after the AST traversal (e.g., `for` loops).
    analyze: deferred::Analyze,
    /// The list of names already seen by flake8-bugbear diagnostics, to avoid duplicate violations.
    flake8_bugbear_seen: RefCell<FxHashSet<TextRange>>,
    /// The end offset of the last visited statement.
    last_stmt_end: TextSize,
    /// A state describing if a docstring is expected or not.
    docstring_state: DocstringState,
    /// The target [`PythonVersion`] for version-dependent checks.
    target_version: TargetVersion,
    /// Helper visitor for detecting semantic syntax errors.
    #[expect(clippy::struct_field_names)]
    semantic_checker: SemanticSyntaxChecker,
    /// Errors collected by the `semantic_checker`.
    semantic_errors: RefCell<Vec<SemanticSyntaxError>>,
    context: &'a LintContext<'a>,
}

impl<'a> Checker<'a> {
    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        parsed: &'a Parsed<ModModule>,
        parsed_annotations_arena: &'a typed_arena::Arena<Result<ParsedAnnotation, ParseError>>,
        settings: &'a LinterSettings,
        noqa_line_for: &'a NoqaMapping,
        noqa: flags::Noqa,
        path: &'a Path,
        package: Option<PackageRoot<'a>>,
        module: Module<'a>,
        locator: &'a Locator,
        stylist: &'a Stylist,
        indexer: &'a Indexer,
        source_type: PySourceType,
        cell_offsets: Option<&'a CellOffsets>,
        notebook_index: Option<&'a NotebookIndex>,
        target_version: TargetVersion,
        context: &'a LintContext<'a>,
    ) -> Self {
        let semantic = SemanticModel::new(&settings.typing_modules, path, module);
        Self {
            parsed,
            parsed_type_annotation: None,
            parsed_annotations_cache: ParsedAnnotationsCache::new(parsed_annotations_arena),
            noqa_line_for,
            noqa,
            path,
            package,
            module,
            source_type,
            locator,
            stylist,
            indexer,
            importer: Importer::new(parsed, locator, stylist),
            semantic,
            visit: deferred::Visit::default(),
            analyze: deferred::Analyze::default(),
            flake8_bugbear_seen: RefCell::default(),
            cell_offsets,
            notebook_index,
            last_stmt_end: TextSize::default(),
            docstring_state: DocstringState::default(),
            target_version,
            semantic_checker: SemanticSyntaxChecker::new(),
            semantic_errors: RefCell::default(),
            context,
        }
    }
}

impl<'a> Checker<'a> {
    /// Return `true` if a [`Rule`] is disabled by a `noqa` directive.
    pub(crate) fn rule_is_ignored(&self, code: Rule, offset: TextSize) -> bool {
        // TODO(charlie): `noqa` directives are mostly enforced in `check_lines.rs`.
        // However, in rare cases, we need to check them here. For example, when
        // removing unused imports, we create a single fix that's applied to all
        // unused members on a single import. We need to preemptively omit any
        // members from the fix that will eventually be excluded by a `noqa`.
        // Unfortunately, we _do_ want to register a `Diagnostic` for each
        // eventually-ignored import, so that our `noqa` counts are accurate.
        if !self.noqa.is_enabled() {
            return false;
        }

        noqa::rule_is_ignored(
            code,
            offset,
            self.noqa_line_for,
            self.comment_ranges(),
            self.locator,
        )
    }

    /// Create a [`Generator`] to generate source code based on the current AST state.
    pub(crate) fn generator(&self) -> Generator {
        Generator::new(self.stylist.indentation(), self.stylist.line_ending())
    }

    /// Return the preferred quote for a generated `StringLiteral` node, given where we are in the
    /// AST.
    fn preferred_quote(&self) -> Quote {
        self.interpolated_string_quote_style()
            .unwrap_or(self.stylist.quote())
    }

    /// Return the default string flags a generated `StringLiteral` node should use, given where we
    /// are in the AST.
    pub(crate) fn default_string_flags(&self) -> ast::StringLiteralFlags {
        ast::StringLiteralFlags::empty().with_quote_style(self.preferred_quote())
    }

    /// Return the default bytestring flags a generated `ByteStringLiteral` node should use, given
    /// where we are in the AST.
    pub(crate) fn default_bytes_flags(&self) -> ast::BytesLiteralFlags {
        ast::BytesLiteralFlags::empty().with_quote_style(self.preferred_quote())
    }

    // TODO(dylan) add similar method for t-strings
    /// Return the default f-string flags a generated `FString` node should use, given where we are
    /// in the AST.
    pub(crate) fn default_fstring_flags(&self) -> ast::FStringFlags {
        ast::FStringFlags::empty().with_quote_style(self.preferred_quote())
    }

    /// Returns the appropriate quoting for interpolated strings by reversing the one used outside of
    /// the interpolated string.
    ///
    /// If the current expression in the context is not an interpolated string, returns ``None``.
    pub(crate) fn interpolated_string_quote_style(&self) -> Option<Quote> {
        if !self.semantic.in_interpolated_string() {
            return None;
        }

        // Find the quote character used to start the containing interpolated string.
        self.semantic
            .current_expressions()
            .find_map(|expr| match expr {
                Expr::FString(ExprFString { value, .. }) => {
                    Some(value.iter().next()?.quote_style().opposite())
                }
                Expr::TString(ExprTString { value, .. }) => {
                    Some(value.iter().next()?.quote_style().opposite())
                }
                _ => None,
            })
    }

    /// Returns the [`SourceRow`] for the given offset.
    pub(crate) fn compute_source_row(&self, offset: TextSize) -> SourceRow {
        #[expect(deprecated)]
        let line = self.locator.compute_line_index(offset);

        if let Some(notebook_index) = self.notebook_index {
            let cell = notebook_index.cell(line).unwrap_or(OneIndexed::MIN);
            let line = notebook_index.cell_row(line).unwrap_or(OneIndexed::MIN);
            SourceRow::Notebook { cell, line }
        } else {
            SourceRow::SourceFile { line }
        }
    }

    /// Returns the [`CommentRanges`] for the parsed source code.
    pub(crate) fn comment_ranges(&self) -> &'a CommentRanges {
        self.indexer.comment_ranges()
    }

    /// Return a [`DiagnosticGuard`] for reporting a diagnostic.
    ///
    /// The guard derefs to a [`Diagnostic`], so it can be used to further modify the diagnostic
    /// before it is added to the collection in the checker on `Drop`.
    pub(crate) fn report_diagnostic<'chk, T: Violation>(
        &'chk self,
        kind: T,
        range: TextRange,
    ) -> DiagnosticGuard<'chk, 'a> {
        self.context.report_diagnostic(kind, range)
    }

    /// Return a [`DiagnosticGuard`] for reporting a diagnostic if the corresponding rule is
    /// enabled.
    ///
    /// The guard derefs to a [`Diagnostic`], so it can be used to further modify the diagnostic
    /// before it is added to the collection in the checker on `Drop`.
    pub(crate) fn report_diagnostic_if_enabled<'chk, T: Violation>(
        &'chk self,
        kind: T,
        range: TextRange,
    ) -> Option<DiagnosticGuard<'chk, 'a>> {
        self.context.report_diagnostic_if_enabled(kind, range)
    }

    /// Adds a [`TextRange`] to the set of ranges of variable names
    /// flagged in `flake8-bugbear` violations so far.
    ///
    /// Returns whether the value was newly inserted.
    pub(crate) fn insert_flake8_bugbear_range(&self, range: TextRange) -> bool {
        let mut ranges = self.flake8_bugbear_seen.borrow_mut();
        ranges.insert(range)
    }

    /// Returns the [`Tokens`] for the parsed type annotation if the checker is in a typing context
    /// or the parsed source code.
    pub(crate) fn tokens(&self) -> &'a Tokens {
        if let Some(type_annotation) = self.parsed_type_annotation {
            type_annotation.parsed().tokens()
        } else {
            self.parsed.tokens()
        }
    }

    /// The [`Locator`] for the current file, which enables extraction of source code from byte
    /// offsets.
    pub(crate) const fn locator(&self) -> &'a Locator<'a> {
        self.locator
    }

    pub(crate) const fn source(&self) -> &'a str {
        self.locator.contents()
    }

    /// The [`Stylist`] for the current file, which detects the current line ending, quote, and
    /// indentation style.
    pub(crate) const fn stylist(&self) -> &'a Stylist<'a> {
        self.stylist
    }

    /// The [`Indexer`] for the current file, which contains the offsets of all comments and more.
    pub(crate) const fn indexer(&self) -> &'a Indexer {
        self.indexer
    }

    /// The [`Importer`] for the current file, which enables importing of other modules.
    pub(crate) const fn importer(&self) -> &Importer<'a> {
        &self.importer
    }

    /// The [`SemanticModel`], built up over the course of the AST traversal.
    pub(crate) const fn semantic(&self) -> &SemanticModel<'a> {
        &self.semantic
    }

    /// The [`LinterSettings`] for the current analysis, including the enabled rules.
    pub(crate) const fn settings(&self) -> &'a LinterSettings {
        self.context.settings
    }

    /// The [`Path`] to the file under analysis.
    pub(crate) const fn path(&self) -> &'a Path {
        self.path
    }

    /// The [`Path`] to the package containing the current file.
    pub(crate) const fn package(&self) -> Option<PackageRoot<'_>> {
        self.package
    }

    /// The [`CellOffsets`] for the current file, if it's a Jupyter notebook.
    pub(crate) const fn cell_offsets(&self) -> Option<&'a CellOffsets> {
        self.cell_offsets
    }

    /// Returns whether the given rule should be checked.
    #[inline]
    pub(crate) const fn is_rule_enabled(&self, rule: Rule) -> bool {
        self.context.is_rule_enabled(rule)
    }

    /// Returns whether any of the given rules should be checked.
    #[inline]
    pub(crate) const fn any_rule_enabled(&self, rules: &[Rule]) -> bool {
        self.context.any_rule_enabled(rules)
    }

    /// Returns the [`IsolationLevel`] to isolate fixes for a given node.
    ///
    /// The primary use-case for fix isolation is to ensure that we don't delete all statements
    /// in a given indented block, which would cause a syntax error. We therefore need to ensure
    /// that we delete at most one statement per indented block per fixer pass. Fix isolation should
    /// thus be applied whenever we delete a statement, but can otherwise be omitted.
    pub(crate) fn isolation(node_id: Option<NodeId>) -> IsolationLevel {
        node_id
            .map(|node_id| IsolationLevel::Group(node_id.into()))
            .unwrap_or_default()
    }

    /// Parse a stringified type annotation as an AST expression,
    /// e.g. `"List[str]"` in `x: "List[str]"`
    ///
    /// This method is a wrapper around [`ruff_python_parser::typing::parse_type_annotation`]
    /// that adds caching.
    pub(crate) fn parse_type_annotation(
        &self,
        annotation: &ast::ExprStringLiteral,
    ) -> Result<&'a ParsedAnnotation, &'a ParseError> {
        self.parsed_annotations_cache
            .lookup_or_parse(annotation, self.locator.contents())
    }

    /// Apply a test to an annotation expression,
    /// abstracting over the fact that the annotation expression might be "stringized".
    ///
    /// A stringized annotation is one enclosed in string quotes:
    /// `foo: "typing.Any"` means the same thing to a type checker as `foo: typing.Any`.
    pub(crate) fn match_maybe_stringized_annotation(
        &self,
        expr: &ast::Expr,
        match_fn: impl FnOnce(&ast::Expr) -> bool,
    ) -> bool {
        if let ast::Expr::StringLiteral(string_annotation) = expr {
            let Some(parsed_annotation) = self.parse_type_annotation(string_annotation).ok() else {
                return false;
            };
            match_fn(parsed_annotation.expression())
        } else {
            match_fn(expr)
        }
    }

    /// Push `diagnostic` if the checker is not in a `@no_type_check` context.
    pub(crate) fn report_type_diagnostic<T: Violation>(&self, kind: T, range: TextRange) {
        if !self.semantic.in_no_type_check() {
            self.report_diagnostic(kind, range);
        }
    }

    /// Return the [`PythonVersion`] to use for version-related lint rules.
    ///
    /// If the user did not provide a target version, this defaults to the lowest supported Python
    /// version ([`PythonVersion::default`]).
    ///
    /// Note that this method should not be used for version-related syntax errors emitted by the
    /// parser or the [`SemanticSyntaxChecker`], which should instead default to the _latest_
    /// supported Python version.
    pub(crate) fn target_version(&self) -> PythonVersion {
        self.target_version.linter_version()
    }

    fn with_semantic_checker(&mut self, f: impl FnOnce(&mut SemanticSyntaxChecker, &Checker)) {
        let mut checker = std::mem::take(&mut self.semantic_checker);
        f(&mut checker, self);
        self.semantic_checker = checker;
    }

    /// Create a [`TypingImporter`] that will import `member` from either `typing` or
    /// `typing_extensions`.
    ///
    /// On Python <`version_added_to_typing`, `member` is imported from `typing_extensions`, while
    /// on Python >=`version_added_to_typing`, it is imported from `typing`.
    ///
    /// If the Python version is less than `version_added_to_typing` but
    /// `LinterSettings::typing_extensions` is `false`, this method returns `None`.
    pub(crate) fn typing_importer<'b>(
        &'b self,
        member: &'b str,
        version_added_to_typing: PythonVersion,
    ) -> Option<TypingImporter<'b, 'a>> {
        let source_module = if self.target_version() >= version_added_to_typing {
            "typing"
        } else if !self.settings().typing_extensions {
            return None;
        } else {
            "typing_extensions"
        };
        Some(TypingImporter {
            checker: self,
            source_module,
            member,
        })
    }

    /// Return the [`LintContext`] for the current analysis.
    ///
    /// Note that you should always prefer calling methods like `settings`, `report_diagnostic`, or
    /// `is_rule_enabled` directly on [`Checker`] when possible. This method exists only for the
    /// rare cases where rules or helper functions need to be accessed by both a `Checker` and a
    /// `LintContext` in different analysis phases.
    pub(crate) const fn context(&self) -> &'a LintContext<'a> {
        self.context
    }
}

pub(crate) struct TypingImporter<'a, 'b> {
    checker: &'a Checker<'b>,
    source_module: &'static str,
    member: &'a str,
}

impl TypingImporter<'_, '_> {
    /// Create an [`Edit`] that makes the requested symbol available at `position`.
    ///
    /// See [`Importer::get_or_import_symbol`] for more details on the returned values and
    /// [`Checker::typing_importer`] for a way to construct a [`TypingImporter`].
    pub(crate) fn import(&self, position: TextSize) -> Result<(Edit, String), ResolutionError> {
        let request = ImportRequest::import_from(self.source_module, self.member);
        self.checker
            .importer
            .get_or_import_symbol(&request, position, self.checker.semantic())
    }
}

impl SemanticSyntaxContext for Checker<'_> {
    fn python_version(&self) -> PythonVersion {
        // Reuse `parser_version` here, which should default to `PythonVersion::latest` instead of
        // `PythonVersion::default` to minimize version-related semantic syntax errors when
        // `target_version` is unset.
        self.target_version.parser_version()
    }

    fn global(&self, name: &str) -> Option<TextRange> {
        self.semantic.global(name)
    }

    fn report_semantic_error(&self, error: SemanticSyntaxError) {
        match error.kind {
            SemanticSyntaxErrorKind::LateFutureImport => {
                // F404
                if self.is_rule_enabled(Rule::LateFutureImport) {
                    self.report_diagnostic(LateFutureImport, error.range);
                }
            }
            SemanticSyntaxErrorKind::LoadBeforeGlobalDeclaration { name, start } => {
                if self.is_rule_enabled(Rule::LoadBeforeGlobalDeclaration) {
                    self.report_diagnostic(
                        LoadBeforeGlobalDeclaration {
                            name,
                            row: self.compute_source_row(start),
                        },
                        error.range,
                    );
                }
            }
            SemanticSyntaxErrorKind::YieldOutsideFunction(kind) => {
                if self.is_rule_enabled(Rule::YieldOutsideFunction) {
                    self.report_diagnostic(YieldOutsideFunction::new(kind), error.range);
                }
            }
            SemanticSyntaxErrorKind::ReturnOutsideFunction => {
                // F706
                if self.is_rule_enabled(Rule::ReturnOutsideFunction) {
                    self.report_diagnostic(ReturnOutsideFunction, error.range);
                }
            }
            SemanticSyntaxErrorKind::AwaitOutsideAsyncFunction(_) => {
                if self.is_rule_enabled(Rule::AwaitOutsideAsync) {
                    self.report_diagnostic(AwaitOutsideAsync, error.range);
                }
            }
            SemanticSyntaxErrorKind::ReboundComprehensionVariable
            | SemanticSyntaxErrorKind::DuplicateTypeParameter
            | SemanticSyntaxErrorKind::MultipleCaseAssignment(_)
            | SemanticSyntaxErrorKind::IrrefutableCasePattern(_)
            | SemanticSyntaxErrorKind::SingleStarredAssignment
            | SemanticSyntaxErrorKind::WriteToDebug(_)
            | SemanticSyntaxErrorKind::InvalidExpression(..)
            | SemanticSyntaxErrorKind::DuplicateMatchKey(_)
            | SemanticSyntaxErrorKind::DuplicateMatchClassAttribute(_)
            | SemanticSyntaxErrorKind::InvalidStarExpression
            | SemanticSyntaxErrorKind::AsyncComprehensionInSyncComprehension(_)
            | SemanticSyntaxErrorKind::DuplicateParameter(_)
            | SemanticSyntaxErrorKind::NonlocalDeclarationAtModuleLevel
            | SemanticSyntaxErrorKind::LoadBeforeNonlocalDeclaration { .. }
            | SemanticSyntaxErrorKind::NonlocalAndGlobal(_)
            | SemanticSyntaxErrorKind::AnnotatedGlobal(_)
            | SemanticSyntaxErrorKind::AnnotatedNonlocal(_) => {
                self.semantic_errors.borrow_mut().push(error);
            }
        }
    }

    fn source(&self) -> &str {
        self.source()
    }

    fn future_annotations_or_stub(&self) -> bool {
        self.semantic.future_annotations_or_stub()
    }

    fn in_async_context(&self) -> bool {
        for scope in self.semantic.current_scopes() {
            match scope.kind {
                ScopeKind::Class(_) | ScopeKind::Lambda(_) => return false,
                ScopeKind::Function(ast::StmtFunctionDef { is_async, .. }) => return *is_async,
                ScopeKind::Generator { .. } | ScopeKind::Module | ScopeKind::Type => {}
            }
        }
        false
    }

    fn in_await_allowed_context(&self) -> bool {
        for scope in self.semantic.current_scopes() {
            match scope.kind {
                ScopeKind::Class(_) => return false,
                ScopeKind::Function(_) | ScopeKind::Lambda(_) => return true,
                ScopeKind::Generator { .. } | ScopeKind::Module | ScopeKind::Type => {}
            }
        }
        false
    }

    fn in_yield_allowed_context(&self) -> bool {
        for scope in self.semantic.current_scopes() {
            match scope.kind {
                ScopeKind::Class(_) | ScopeKind::Generator { .. } => return false,
                ScopeKind::Function(_) | ScopeKind::Lambda(_) => return true,
                ScopeKind::Module | ScopeKind::Type => {}
            }
        }
        false
    }

    fn in_sync_comprehension(&self) -> bool {
        for scope in self.semantic.current_scopes() {
            if let ScopeKind::Generator {
                kind:
                    GeneratorKind::ListComprehension
                    | GeneratorKind::DictComprehension
                    | GeneratorKind::SetComprehension,
                is_async: false,
            } = scope.kind
            {
                return true;
            }
        }
        false
    }

    fn in_module_scope(&self) -> bool {
        self.semantic.current_scope().kind.is_module()
    }

    fn in_function_scope(&self) -> bool {
        let kind = &self.semantic.current_scope().kind;
        matches!(kind, ScopeKind::Function(_) | ScopeKind::Lambda(_))
    }

    fn in_notebook(&self) -> bool {
        self.source_type.is_ipynb()
    }

    fn in_generator_scope(&self) -> bool {
        matches!(
            &self.semantic.current_scope().kind,
            ScopeKind::Generator {
                kind: GeneratorKind::Generator,
                ..
            }
        )
    }
}

impl<'a> Visitor<'a> for Checker<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        // For functions, defer semantic syntax error checks until the body of the function is
        // visited
        if !stmt.is_function_def_stmt() {
            self.with_semantic_checker(|semantic, context| semantic.visit_stmt(stmt, context));
        }

        // Step 0: Pre-processing
        self.semantic.push_node(stmt);

        // For Jupyter Notebooks, we'll reset the `IMPORT_BOUNDARY` flag when
        // we encounter a cell boundary.
        if self.source_type.is_ipynb()
            && self.semantic.at_top_level()
            && self.semantic.seen_import_boundary()
            && self.cell_offsets.is_some_and(|cell_offsets| {
                cell_offsets.has_cell_boundary(TextRange::new(self.last_stmt_end, stmt.start()))
            })
        {
            self.semantic.flags -= SemanticModelFlags::IMPORT_BOUNDARY;
        }

        // Track whether we've seen module docstrings, non-imports, etc.
        match stmt {
            Stmt::Expr(ast::StmtExpr { value, .. })
                if !self.semantic.seen_module_docstring_boundary()
                    && value.is_string_literal_expr() =>
            {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;
            }
            Stmt::ImportFrom(ast::StmtImportFrom { module, names, .. }) => {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;

                // Allow __future__ imports until we see a non-__future__ import.
                if let Some("__future__") = module.as_deref() {
                    if names
                        .iter()
                        .any(|alias| alias.name.as_str() == "annotations")
                    {
                        self.semantic.flags |= SemanticModelFlags::FUTURE_ANNOTATIONS;
                    }
                }
            }
            Stmt::Import(_) => {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;
            }
            _ => {
                self.semantic.flags |= SemanticModelFlags::MODULE_DOCSTRING_BOUNDARY;
                if !(self.semantic.seen_import_boundary()
                    || stmt.is_ipy_escape_command_stmt()
                    || helpers::is_assignment_to_a_dunder(stmt)
                    || helpers::in_nested_block(self.semantic.current_statements())
                    || imports::is_matplotlib_activation(stmt, self.semantic())
                    || imports::is_sys_path_modification(stmt, self.semantic())
                    || imports::is_os_environ_modification(stmt, self.semantic())
                    || imports::is_pytest_importorskip(stmt, self.semantic())
                    || imports::is_site_sys_path_modification(stmt, self.semantic()))
                {
                    self.semantic.flags |= SemanticModelFlags::IMPORT_BOUNDARY;
                }
            }
        }

        // Store the flags prior to any further descent, so that we can restore them after visiting
        // the node.
        let flags_snapshot = self.semantic.flags;

        // Update the semantic model if it is in a docstring. This should be done after the
        // flags snapshot to ensure that it gets reset once the statement is analyzed.
        if let Some(kind) = self.docstring_state.expected_kind() {
            if is_docstring_stmt(stmt) {
                self.semantic.flags |= kind.as_flag();
            }
            // Reset the state irrespective of whether the statement is a docstring or not.
            self.docstring_state = DocstringState::Other;
        }

        // Step 1: Binding
        match stmt {
            Stmt::AugAssign(ast::StmtAugAssign {
                target,
                op: _,
                value: _,
                range: _,
                node_index: _,
            }) => {
                self.handle_node_load(target);
            }
            Stmt::Import(ast::StmtImport {
                names,
                range: _,
                node_index: _,
            }) => {
                if self.semantic.at_top_level() {
                    self.importer.visit_import(stmt);
                }

                for alias in names {
                    // Given `import foo.bar`, `module` would be "foo", and `call_path` would be
                    // `["foo", "bar"]`.
                    let module = alias.name.split('.').next().unwrap();

                    // Mark the top-level module as "seen" by the semantic model.
                    self.semantic.add_module(module);

                    if alias.asname.is_none() && alias.name.contains('.') {
                        let qualified_name = QualifiedName::user_defined(&alias.name);
                        self.add_binding(
                            module,
                            alias.identifier(),
                            BindingKind::SubmoduleImport(SubmoduleImport {
                                qualified_name: Box::new(qualified_name),
                            }),
                            BindingFlags::EXTERNAL,
                        );
                    } else {
                        let mut flags = BindingFlags::EXTERNAL;
                        if alias.asname.is_some() {
                            flags |= BindingFlags::ALIAS;
                        }
                        if alias
                            .asname
                            .as_ref()
                            .is_some_and(|asname| asname.as_str() == alias.name.as_str())
                        {
                            flags |= BindingFlags::EXPLICIT_EXPORT;
                        }

                        let name = alias.asname.as_ref().unwrap_or(&alias.name);
                        let qualified_name = QualifiedName::user_defined(&alias.name);
                        self.add_binding(
                            name,
                            alias.identifier(),
                            BindingKind::Import(Import {
                                qualified_name: Box::new(qualified_name),
                            }),
                            flags,
                        );
                    }
                }
            }
            Stmt::ImportFrom(ast::StmtImportFrom {
                names,
                module,
                level,
                range: _,
                node_index: _,
            }) => {
                if self.semantic.at_top_level() {
                    self.importer.visit_import(stmt);
                }

                let module = module.as_deref();
                let level = *level;

                // Mark the top-level module as "seen" by the semantic model.
                if level == 0 {
                    if let Some(module) = module.and_then(|module| module.split('.').next()) {
                        self.semantic.add_module(module);
                    }
                }

                for alias in names {
                    if let Some("__future__") = module {
                        let name = alias.asname.as_ref().unwrap_or(&alias.name);
                        self.add_binding(
                            name,
                            alias.identifier(),
                            BindingKind::FutureImport,
                            BindingFlags::empty(),
                        );
                    } else if &alias.name == "*" {
                        self.semantic
                            .current_scope_mut()
                            .add_star_import(StarImport { level, module });
                    } else {
                        let mut flags = BindingFlags::EXTERNAL;
                        if alias.asname.is_some() {
                            flags |= BindingFlags::ALIAS;
                        }
                        if alias
                            .asname
                            .as_ref()
                            .is_some_and(|asname| asname.as_str() == alias.name.as_str())
                        {
                            flags |= BindingFlags::EXPLICIT_EXPORT;
                        }

                        // Given `from foo import bar`, `name` would be "bar" and `qualified_name` would
                        // be "foo.bar". Given `from foo import bar as baz`, `name` would be "baz"
                        // and `qualified_name` would be "foo.bar".
                        let name = alias.asname.as_ref().unwrap_or(&alias.name);

                        // Attempt to resolve any relative imports; but if we don't know the current
                        // module path, or the relative import extends beyond the package root,
                        // fallback to a literal representation (e.g., `[".", "foo"]`).
                        let qualified_name = collect_import_from_member(level, module, &alias.name);
                        self.add_binding(
                            name,
                            alias.identifier(),
                            BindingKind::FromImport(FromImport {
                                qualified_name: Box::new(qualified_name),
                            }),
                            flags,
                        );
                    }
                }
            }
            Stmt::Global(ast::StmtGlobal {
                names,
                range: _,
                node_index: _,
            }) => {
                if !self.semantic.scope_id.is_global() {
                    for name in names {
                        let binding_id = self.semantic.global_scope().get(name);

                        // Mark the binding in the global scope as "rebound" in the current scope.
                        if let Some(binding_id) = binding_id {
                            self.semantic
                                .add_rebinding_scope(binding_id, self.semantic.scope_id);
                        }

                        // Add a binding to the current scope.
                        let binding_id = self.semantic.push_binding(
                            name.range(),
                            BindingKind::Global(binding_id),
                            BindingFlags::GLOBAL,
                        );
                        let scope = self.semantic.current_scope_mut();
                        scope.add(name, binding_id);
                    }
                }
            }
            Stmt::Nonlocal(ast::StmtNonlocal {
                names,
                range: _,
                node_index: _,
            }) => {
                if !self.semantic.scope_id.is_global() {
                    for name in names {
                        if let Some((scope_id, binding_id)) = self.semantic.nonlocal(name) {
                            // Mark the binding as "used", since the `nonlocal` requires an existing
                            // binding.
                            self.semantic.add_local_reference(
                                binding_id,
                                ExprContext::Load,
                                name.range(),
                            );

                            // Mark the binding in the enclosing scope as "rebound" in the current
                            // scope.
                            self.semantic
                                .add_rebinding_scope(binding_id, self.semantic.scope_id);

                            // Add a binding to the current scope.
                            let binding_id = self.semantic.push_binding(
                                name.range(),
                                BindingKind::Nonlocal(binding_id, scope_id),
                                BindingFlags::NONLOCAL,
                            );
                            let scope = self.semantic.current_scope_mut();
                            scope.add(name, binding_id);
                        }
                    }
                }
            }
            _ => {}
        }

        // Step 2: Traversal
        match stmt {
            Stmt::FunctionDef(
                function_def @ ast::StmtFunctionDef {
                    name,
                    body,
                    parameters,
                    decorator_list,
                    returns,
                    type_params,
                    ..
                },
            ) => {
                // Visit the decorators and arguments, but avoid the body, which will be
                // deferred.
                for decorator in decorator_list {
                    self.visit_decorator(decorator);

                    if self
                        .semantic
                        .match_typing_expr(&decorator.expression, "no_type_check")
                    {
                        self.semantic.flags |= SemanticModelFlags::NO_TYPE_CHECK;
                    }
                }

                // Function annotations are always evaluated at runtime, unless future annotations
                // are enabled or the Python version is at least 3.14.
                let annotation = AnnotationContext::from_function(
                    function_def,
                    &self.semantic,
                    self.settings(),
                    self.target_version(),
                );

                // The first parameter may be a single dispatch.
                let singledispatch =
                    flake8_type_checking::helpers::is_singledispatch_implementation(
                        function_def,
                        self.semantic(),
                    );

                // The default values of the parameters needs to be evaluated in the enclosing
                // scope.
                for parameter in parameters {
                    if let Some(expr) = parameter.default() {
                        self.visit_expr(expr);
                    }
                }

                self.semantic.push_scope(ScopeKind::Type);

                if let Some(type_params) = type_params {
                    self.visit_type_params(type_params);
                }

                for parameter in parameters {
                    if let Some(expr) = parameter.annotation() {
                        if singledispatch && !parameter.is_variadic() {
                            self.visit_runtime_required_annotation(expr);
                        } else {
                            match annotation {
                                AnnotationContext::RuntimeRequired => {
                                    self.visit_runtime_required_annotation(expr);
                                }
                                AnnotationContext::RuntimeEvaluated => {
                                    self.visit_runtime_evaluated_annotation(expr);
                                }
                                AnnotationContext::TypingOnly => {
                                    self.visit_annotation(expr);
                                }
                            }
                        }
                    }
                }
                if let Some(expr) = returns {
                    if singledispatch {
                        self.visit_runtime_required_annotation(expr);
                    } else {
                        match annotation {
                            AnnotationContext::RuntimeRequired => {
                                self.visit_runtime_required_annotation(expr);
                            }
                            AnnotationContext::RuntimeEvaluated => {
                                self.visit_runtime_evaluated_annotation(expr);
                            }
                            AnnotationContext::TypingOnly => {
                                self.visit_annotation(expr);
                            }
                        }
                    }
                }

                let definition = docstrings::extraction::extract_definition(
                    ExtractionTarget::Function(function_def),
                    self.semantic.definition_id,
                    &self.semantic.definitions,
                );
                self.semantic.push_definition(definition);
                self.semantic.push_scope(ScopeKind::Function(function_def));
                self.semantic.flags -= SemanticModelFlags::EXCEPTION_HANDLER;

                self.visit.functions.push(self.semantic.snapshot());

                // Extract any global bindings from the function body.
                if let Some(globals) = Globals::from_body(body) {
                    self.semantic.set_globals(globals);
                }
                let scope_id = self.semantic.scope_id;
                self.analyze.scopes.push(scope_id);
                self.semantic.pop_scope(); // Function scope
                self.semantic.pop_definition();
                self.semantic.pop_scope(); // Type parameter scope
                self.add_binding(
                    name,
                    stmt.identifier(),
                    BindingKind::FunctionDefinition(scope_id),
                    BindingFlags::empty(),
                );
            }
            Stmt::ClassDef(
                class_def @ ast::StmtClassDef {
                    name,
                    body,
                    arguments,
                    decorator_list,
                    type_params,
                    ..
                },
            ) => {
                for decorator in decorator_list {
                    self.visit_decorator(decorator);
                }

                self.semantic.push_scope(ScopeKind::Type);

                if let Some(type_params) = type_params {
                    self.visit_type_params(type_params);
                }

                if let Some(arguments) = arguments {
                    self.semantic.flags |= SemanticModelFlags::CLASS_BASE;
                    self.visit_arguments(arguments);
                    self.semantic.flags -= SemanticModelFlags::CLASS_BASE;
                }

                let definition = docstrings::extraction::extract_definition(
                    ExtractionTarget::Class(class_def),
                    self.semantic.definition_id,
                    &self.semantic.definitions,
                );
                self.semantic.push_definition(definition);
                self.semantic.push_scope(ScopeKind::Class(class_def));
                self.semantic.flags -= SemanticModelFlags::EXCEPTION_HANDLER;

                // Extract any global bindings from the class body.
                if let Some(globals) = Globals::from_body(body) {
                    self.semantic.set_globals(globals);
                }

                // Set the docstring state before visiting the class body.
                self.docstring_state = DocstringState::Expected(ExpectedDocstringKind::Class);
                self.visit_body(body);

                let scope_id = self.semantic.scope_id;
                self.analyze.scopes.push(scope_id);
                self.semantic.pop_scope(); // Class scope
                self.semantic.pop_definition();
                self.semantic.pop_scope(); // Type parameter scope
                self.add_binding(
                    name,
                    stmt.identifier(),
                    BindingKind::ClassDefinition(scope_id),
                    BindingFlags::empty(),
                );
            }
            Stmt::TypeAlias(ast::StmtTypeAlias {
                range: _,
                node_index: _,
                name,
                type_params,
                value,
            }) => {
                self.semantic.push_scope(ScopeKind::Type);
                if let Some(type_params) = type_params {
                    self.visit_type_params(type_params);
                }
                self.visit_deferred_type_alias_value(value);
                self.semantic.pop_scope();
                self.visit_expr(name);
            }
            Stmt::Try(
                try_node @ ast::StmtTry {
                    body,
                    handlers,
                    orelse,
                    finalbody,
                    ..
                },
            ) => {
                // Iterate over the `body`, then the `handlers`, then the `orelse`, then the
                // `finalbody`, but treat the body and the `orelse` as a single branch for
                // flow analysis purposes.
                let branch = self.semantic.push_branch();
                self.semantic
                    .handled_exceptions
                    .push(Exceptions::from_try_stmt(try_node, &self.semantic));
                self.visit_body(body);
                self.semantic.handled_exceptions.pop();
                self.semantic.pop_branch();

                for except_handler in handlers {
                    self.semantic.push_branch();
                    self.visit_except_handler(except_handler);
                    self.semantic.pop_branch();
                }

                self.semantic.set_branch(branch);
                self.visit_body(orelse);
                self.semantic.pop_branch();

                self.semantic.push_branch();
                self.visit_body(finalbody);
                self.semantic.pop_branch();
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value,
                ..
            }) => {
                match AnnotationContext::from_model(
                    &self.semantic,
                    self.settings(),
                    self.target_version(),
                ) {
                    AnnotationContext::RuntimeRequired => {
                        self.visit_runtime_required_annotation(annotation);
                    }
                    AnnotationContext::RuntimeEvaluated => {
                        self.visit_runtime_evaluated_annotation(annotation);
                    }
                    AnnotationContext::TypingOnly
                        if flake8_type_checking::helpers::is_dataclass_meta_annotation(
                            annotation,
                            self.semantic(),
                        ) =>
                    {
                        if let Expr::Subscript(subscript) = &**annotation {
                            // Ex) `InitVar[str]`
                            self.visit_runtime_required_annotation(&subscript.value);
                            self.visit_annotation(&subscript.slice);
                        } else {
                            // Ex) `InitVar`
                            self.visit_runtime_required_annotation(annotation);
                        }
                    }
                    AnnotationContext::TypingOnly => self.visit_annotation(annotation),
                }

                if let Some(expr) = value {
                    if self.semantic.match_typing_expr(annotation, "TypeAlias") {
                        self.visit_annotated_type_alias_value(expr);
                    } else {
                        self.visit_expr(expr);
                    }
                }
                self.visit_expr(target);
            }
            Stmt::Assert(ast::StmtAssert {
                test,
                msg,
                range: _,
                node_index: _,
            }) => {
                let snapshot = self.semantic.flags;
                self.semantic.flags |= SemanticModelFlags::ASSERT_STATEMENT;
                self.visit_boolean_test(test);
                if let Some(expr) = msg {
                    self.visit_expr(expr);
                }
                self.semantic.flags = snapshot;
            }
            Stmt::With(ast::StmtWith {
                items,
                body,
                is_async: _,
                range: _,
                node_index: _,
            }) => {
                for item in items {
                    self.visit_with_item(item);
                }
                self.semantic.push_branch();
                self.visit_body(body);
                self.semantic.pop_branch();
            }
            Stmt::While(ast::StmtWhile {
                test,
                body,
                orelse,
                range: _,
                node_index: _,
            }) => {
                self.visit_boolean_test(test);
                self.visit_body(body);
                self.visit_body(orelse);
            }
            Stmt::If(
                stmt_if @ ast::StmtIf {
                    test,
                    body,
                    elif_else_clauses,
                    range: _,
                    node_index: _,
                },
            ) => {
                self.visit_boolean_test(test);

                self.semantic.push_branch();
                if typing::is_type_checking_block(stmt_if, &self.semantic) {
                    if self.semantic.at_top_level() {
                        self.importer.visit_type_checking_block(stmt);
                    }
                    self.visit_type_checking_block(body);
                } else {
                    self.visit_body(body);
                }
                self.semantic.pop_branch();

                for clause in elif_else_clauses {
                    self.semantic.push_branch();
                    self.visit_elif_else_clause(clause);
                    self.semantic.pop_branch();
                }
            }
            _ => visitor::walk_stmt(self, stmt),
        }

        if self.semantic().at_top_level() || self.semantic().current_scope().kind.is_class() {
            match stmt {
                Stmt::Assign(ast::StmtAssign { targets, .. }) => {
                    if let [Expr::Name(_)] = targets.as_slice() {
                        self.docstring_state =
                            DocstringState::Expected(ExpectedDocstringKind::Attribute);
                    }
                }
                Stmt::AnnAssign(ast::StmtAnnAssign { target, .. }) => {
                    if target.is_name_expr() {
                        self.docstring_state =
                            DocstringState::Expected(ExpectedDocstringKind::Attribute);
                    }
                }
                _ => {}
            }
        }

        // Step 3: Clean-up

        // Step 4: Analysis
        analyze::statement(stmt, self);

        self.semantic.flags = flags_snapshot;
        self.semantic.pop_node();
        self.last_stmt_end = stmt.end();
    }

    fn visit_annotation(&mut self, expr: &'a Expr) {
        let flags_snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::TYPING_ONLY_ANNOTATION;
        self.visit_type_definition(expr);
        self.semantic.flags = flags_snapshot;
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        self.with_semantic_checker(|semantic, context| semantic.visit_expr(expr, context));

        // Step 0: Pre-processing
        if self.source_type.is_stub()
            && self.semantic.in_class_base()
            && !self.semantic.in_deferred_class_base()
        {
            self.visit
                .class_bases
                .push((expr, self.semantic.snapshot()));
            return;
        }

        if !self.semantic.in_typing_literal()
            // `in_deferred_type_definition()` will only be `true` if we're now visiting the deferred nodes
            // after having already traversed the source tree once. If we're now visiting the deferred nodes,
            // we can't defer again, or we'll infinitely recurse!
            && !self.semantic.in_deferred_type_definition()
            && self.semantic.in_type_definition()
            && (self.semantic.future_annotations_or_stub()||self.target_version().defers_annotations())
            && (self.semantic.in_annotation() || self.source_type.is_stub())
        {
            if let Expr::StringLiteral(string_literal) = expr {
                self.visit
                    .string_type_definitions
                    .push((string_literal, self.semantic.snapshot()));
            } else {
                self.visit
                    .future_type_definitions
                    .push((expr, self.semantic.snapshot()));
            }
            return;
        }

        self.semantic.push_node(expr);

        // Store the flags prior to any further descent, so that we can restore them after visiting
        // the node.
        let flags_snapshot = self.semantic.flags;

        // If we're in a boolean test (e.g., the `test` of a `Stmt::If`), but now within a
        // subexpression (e.g., `a` in `f(a)`), then we're no longer in a boolean test.
        if !matches!(
            expr,
            Expr::BoolOp(_)
                | Expr::UnaryOp(ast::ExprUnaryOp {
                    op: UnaryOp::Not,
                    ..
                })
        ) {
            self.semantic.flags -= SemanticModelFlags::BOOLEAN_TEST;
        }

        // Step 1: Binding
        match expr {
            Expr::Call(ast::ExprCall {
                func,
                arguments: _,
                range: _,
                node_index: _,
            }) => {
                if let Expr::Name(ast::ExprName {
                    id,
                    ctx,
                    range: _,
                    node_index: _,
                }) = func.as_ref()
                {
                    if id == "locals" && ctx.is_load() {
                        let scope = self.semantic.current_scope_mut();
                        scope.set_uses_locals();
                    }
                }
            }
            Expr::Name(ast::ExprName {
                id,
                ctx,
                range: _,
                node_index: _,
            }) => match ctx {
                ExprContext::Load => self.handle_node_load(expr),
                ExprContext::Store => self.handle_node_store(id, expr),
                ExprContext::Del => self.handle_node_delete(expr),
                ExprContext::Invalid => {}
            },
            _ => {}
        }

        // Step 2: Traversal
        match expr {
            Expr::ListComp(ast::ExprListComp {
                elt,
                generators,
                range: _,
                node_index: _,
            }) => {
                self.visit_generators(GeneratorKind::ListComprehension, generators);
                self.visit_expr(elt);
            }
            Expr::SetComp(ast::ExprSetComp {
                elt,
                generators,
                range: _,
                node_index: _,
            }) => {
                self.visit_generators(GeneratorKind::SetComprehension, generators);
                self.visit_expr(elt);
            }
            Expr::Generator(ast::ExprGenerator {
                elt,
                generators,
                range: _,
                node_index: _,
                parenthesized: _,
            }) => {
                self.visit_generators(GeneratorKind::Generator, generators);
                self.visit_expr(elt);
            }
            Expr::DictComp(ast::ExprDictComp {
                key,
                value,
                generators,
                range: _,
                node_index: _,
            }) => {
                self.visit_generators(GeneratorKind::DictComprehension, generators);
                self.visit_expr(key);
                self.visit_expr(value);
            }
            Expr::Lambda(
                lambda @ ast::ExprLambda {
                    parameters,
                    body: _,
                    range: _,
                    node_index: _,
                },
            ) => {
                // Visit the default arguments, but avoid the body, which will be deferred.
                if let Some(parameters) = parameters {
                    for default in parameters
                        .iter_non_variadic_params()
                        .filter_map(|param| param.default.as_deref())
                    {
                        self.visit_expr(default);
                    }
                }

                self.semantic.push_scope(ScopeKind::Lambda(lambda));
                self.visit.lambdas.push(self.semantic.snapshot());
                self.analyze.lambdas.push(self.semantic.snapshot());
            }
            Expr::If(ast::ExprIf {
                test,
                body,
                orelse,
                range: _,
                node_index: _,
            }) => {
                self.visit_boolean_test(test);
                self.visit_expr(body);
                self.visit_expr(orelse);
            }
            Expr::UnaryOp(ast::ExprUnaryOp {
                op: UnaryOp::Not,
                operand,
                range: _,
                node_index: _,
            }) => {
                self.visit_boolean_test(operand);
            }
            Expr::Call(ast::ExprCall {
                func,
                arguments,
                range: _,
                node_index: _,
            }) => {
                self.visit_expr(func);

                let callable =
                    self.semantic
                        .resolve_qualified_name(func)
                        .and_then(|qualified_name| {
                            if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "cast")
                            {
                                Some(typing::Callable::Cast)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "NewType")
                            {
                                Some(typing::Callable::NewType)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "TypeVar")
                            {
                                Some(typing::Callable::TypeVar)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "TypeAliasType")
                            {
                                Some(typing::Callable::TypeAliasType)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "NamedTuple")
                            {
                                Some(typing::Callable::NamedTuple)
                            } else if self
                                .semantic
                                .match_typing_qualified_name(&qualified_name, "TypedDict")
                            {
                                Some(typing::Callable::TypedDict)
                            } else if matches!(
                                qualified_name.segments(),
                                [
                                    "mypy_extensions",
                                    "Arg"
                                        | "DefaultArg"
                                        | "NamedArg"
                                        | "DefaultNamedArg"
                                        | "VarArg"
                                        | "KwArg"
                                ]
                            ) {
                                Some(typing::Callable::MypyExtension)
                            } else if matches!(qualified_name.segments(), ["" | "builtins", "bool"])
                            {
                                Some(typing::Callable::Bool)
                            } else {
                                None
                            }
                        });
                match callable {
                    Some(typing::Callable::Bool) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_boolean_test(arg);
                        }
                        for arg in args {
                            self.visit_expr(arg);
                        }
                    }
                    Some(typing::Callable::Cast) => {
                        for (i, arg) in arguments.arguments_source_order().enumerate() {
                            match (i, arg) {
                                (0, ArgOrKeyword::Arg(arg)) => self.visit_cast_type_argument(arg),
                                (_, ArgOrKeyword::Arg(arg)) => self.visit_non_type_definition(arg),
                                (_, ArgOrKeyword::Keyword(Keyword { arg, value, .. })) => {
                                    if let Some(id) = arg {
                                        if id == "typ" {
                                            self.visit_cast_type_argument(value);
                                        } else {
                                            self.visit_non_type_definition(value);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(typing::Callable::NewType) => {
                        for (i, arg) in arguments.arguments_source_order().enumerate() {
                            match (i, arg) {
                                (1, ArgOrKeyword::Arg(arg)) => self.visit_type_definition(arg),
                                (_, ArgOrKeyword::Arg(arg)) => self.visit_non_type_definition(arg),
                                (_, ArgOrKeyword::Keyword(Keyword { arg, value, .. })) => {
                                    if let Some(id) = arg {
                                        if id == "tp" {
                                            self.visit_type_definition(value);
                                        } else {
                                            self.visit_non_type_definition(value);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(typing::Callable::TypeVar) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_non_type_definition(arg);
                        }
                        for arg in args {
                            self.visit_type_definition(arg);
                        }
                        for keyword in &*arguments.keywords {
                            let Keyword {
                                arg,
                                value,
                                range: _,
                                node_index: _,
                            } = keyword;
                            if let Some(id) = arg {
                                if matches!(&**id, "bound" | "default") {
                                    self.visit_type_definition(value);
                                } else {
                                    self.visit_non_type_definition(value);
                                }
                            }
                        }
                    }
                    Some(typing::Callable::TypeAliasType) => {
                        // Ex) TypeAliasType("Json", "Union[dict[str, Json]]", type_params=())
                        for (i, arg) in arguments.arguments_source_order().enumerate() {
                            match (i, arg) {
                                (1, ArgOrKeyword::Arg(arg)) => self.visit_type_definition(arg),
                                (_, ArgOrKeyword::Arg(arg)) => self.visit_non_type_definition(arg),
                                (_, ArgOrKeyword::Keyword(Keyword { arg, value, .. })) => {
                                    if let Some(id) = arg {
                                        if matches!(&**id, "value" | "type_params") {
                                            self.visit_type_definition(value);
                                        } else {
                                            self.visit_non_type_definition(value);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(typing::Callable::NamedTuple) => {
                        // Ex) NamedTuple("a", [("a", int)])
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_non_type_definition(arg);
                        }

                        for arg in args {
                            match arg {
                                // Ex) NamedTuple("a", [("a", int)])
                                Expr::List(ast::ExprList { elts, .. })
                                | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                                    for elt in elts {
                                        match elt {
                                            Expr::List(ast::ExprList { elts, .. })
                                            | Expr::Tuple(ast::ExprTuple { elts, .. })
                                                if elts.len() == 2 =>
                                            {
                                                self.visit_non_type_definition(&elts[0]);
                                                self.visit_type_definition(&elts[1]);
                                            }
                                            _ => {
                                                self.visit_non_type_definition(elt);
                                            }
                                        }
                                    }
                                }
                                _ => self.visit_non_type_definition(arg),
                            }
                        }

                        for keyword in &*arguments.keywords {
                            let Keyword { arg, value, .. } = keyword;
                            match (arg.as_ref(), value) {
                                // Ex) NamedTuple("a", **{"a": int})
                                (None, Expr::Dict(dict)) => {
                                    for ast::DictItem { key, value } in dict {
                                        if let Some(key) = key.as_ref() {
                                            self.visit_non_type_definition(key);
                                            self.visit_type_definition(value);
                                        } else {
                                            self.visit_non_type_definition(value);
                                        }
                                    }
                                }
                                // Ex) NamedTuple("a", **obj)
                                (None, _) => {
                                    self.visit_non_type_definition(value);
                                }
                                // Ex) NamedTuple("a", a=int)
                                _ => {
                                    self.visit_type_definition(value);
                                }
                            }
                        }
                    }
                    Some(typing::Callable::TypedDict) => {
                        // Ex) TypedDict("a", {"a": int})
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            self.visit_non_type_definition(arg);
                        }
                        for arg in args {
                            if let Expr::Dict(ast::ExprDict {
                                items,
                                range: _,
                                node_index: _,
                            }) = arg
                            {
                                for ast::DictItem { key, value } in items {
                                    if let Some(key) = key {
                                        self.visit_non_type_definition(key);
                                    }
                                    self.visit_type_definition(value);
                                }
                            } else {
                                self.visit_non_type_definition(arg);
                            }
                        }

                        // Ex) TypedDict("a", a=int)
                        for keyword in &*arguments.keywords {
                            let Keyword { value, .. } = keyword;
                            self.visit_type_definition(value);
                        }
                    }
                    Some(typing::Callable::MypyExtension) => {
                        let mut args = arguments.args.iter();
                        if let Some(arg) = args.next() {
                            // Ex) DefaultNamedArg(bool | None, name="some_prop_name")
                            self.visit_type_definition(arg);

                            for arg in args {
                                self.visit_non_type_definition(arg);
                            }
                            for keyword in &*arguments.keywords {
                                let Keyword { value, .. } = keyword;
                                self.visit_non_type_definition(value);
                            }
                        } else {
                            // Ex) DefaultNamedArg(type="bool", name="some_prop_name")
                            for keyword in &*arguments.keywords {
                                let Keyword {
                                    value,
                                    arg,
                                    range: _,
                                    node_index: _,
                                } = keyword;
                                if arg.as_ref().is_some_and(|arg| arg == "type") {
                                    self.visit_type_definition(value);
                                } else {
                                    self.visit_non_type_definition(value);
                                }
                            }
                        }
                    }
                    None => {
                        // If we're in a type definition, we need to treat the arguments to any
                        // other callables as non-type definitions (i.e., we don't want to treat
                        // any strings as deferred type definitions).
                        for arg in &*arguments.args {
                            self.visit_non_type_definition(arg);
                        }
                        for keyword in &*arguments.keywords {
                            let Keyword { value, .. } = keyword;
                            self.visit_non_type_definition(value);
                        }
                    }
                }
            }
            Expr::Subscript(ast::ExprSubscript {
                value,
                slice,
                ctx,
                range: _,
                node_index: _,
            }) => {
                // Only allow annotations in `ExprContext::Load`. If we have, e.g.,
                // `obj["foo"]["bar"]`, we need to avoid treating the `obj["foo"]`
                // portion as an annotation, despite having `ExprContext::Load`. Thus, we track
                // the `ExprContext` at the top-level.
                if self.semantic.in_subscript() {
                    visitor::walk_expr(self, expr);
                } else if matches!(ctx, ExprContext::Store | ExprContext::Del) {
                    self.semantic.flags |= SemanticModelFlags::SUBSCRIPT;
                    visitor::walk_expr(self, expr);
                } else {
                    self.visit_expr(value);

                    match typing::match_annotated_subscript(
                        value,
                        &self.semantic,
                        self.settings().typing_modules.iter().map(String::as_str),
                        &self.settings().pyflakes.extend_generics,
                    ) {
                        // Ex) Literal["Class"]
                        Some(typing::SubscriptKind::Literal) => {
                            self.semantic.flags |= SemanticModelFlags::TYPING_LITERAL;

                            self.visit_expr(slice);
                            self.visit_expr_context(ctx);
                        }
                        // Ex) Optional[int]
                        Some(typing::SubscriptKind::Generic) => {
                            self.visit_type_definition(slice);
                            self.visit_expr_context(ctx);
                        }
                        // Ex) Annotated[int, "Hello, world!"]
                        Some(typing::SubscriptKind::PEP593Annotation) => {
                            // First argument is a type (including forward references); the
                            // rest are arbitrary Python objects.
                            if let Expr::Tuple(ast::ExprTuple {
                                elts,
                                ctx,
                                range: _,
                                node_index: _,
                                parenthesized: _,
                            }) = slice.as_ref()
                            {
                                let mut iter = elts.iter();
                                if let Some(expr) = iter.next() {
                                    self.visit_type_definition(expr);
                                }
                                for expr in iter {
                                    self.visit_non_type_definition(expr);
                                }
                                self.visit_expr_context(ctx);
                            } else {
                                if self.semantic.in_type_definition() {
                                    // this should potentially trigger some kind of violation in the
                                    // future, since it would indicate an invalid type expression
                                    debug!("Found non-Expr::Tuple argument to PEP 593 Annotation.");
                                }
                                // even if the expression is invalid as a type expression, we should
                                // still visit it so we don't accidentally treat variables as unused
                                self.visit_expr(slice);
                                self.visit_expr_context(ctx);
                            }
                        }
                        Some(typing::SubscriptKind::TypedDict) => {
                            if let Expr::Dict(ast::ExprDict {
                                items,
                                range: _,
                                node_index: _,
                            }) = slice.as_ref()
                            {
                                for item in items {
                                    if let Some(key) = &item.key {
                                        self.visit_non_type_definition(key);
                                        self.visit_type_definition(&item.value);
                                    } else {
                                        self.visit_non_type_definition(&item.value);
                                    }
                                }
                            } else {
                                self.visit_non_type_definition(slice);
                            }
                        }
                        None => {
                            self.visit_expr(slice);
                            self.visit_expr_context(ctx);
                        }
                    }
                }
            }
            Expr::StringLiteral(string_literal) => {
                if self.semantic.in_type_definition() && !self.semantic.in_typing_literal() {
                    self.visit
                        .string_type_definitions
                        .push((string_literal, self.semantic.snapshot()));
                }
            }
            Expr::FString(_) => {
                self.semantic.flags |= SemanticModelFlags::F_STRING;
                visitor::walk_expr(self, expr);
            }
            Expr::TString(_) => {
                self.semantic.flags |= SemanticModelFlags::T_STRING;
                visitor::walk_expr(self, expr);
            }
            Expr::Named(ast::ExprNamed {
                target,
                value,
                range: _,
                node_index: _,
            }) => {
                self.visit_expr(value);

                self.semantic.flags |= SemanticModelFlags::NAMED_EXPRESSION_ASSIGNMENT;
                self.visit_expr(target);
            }
            _ => visitor::walk_expr(self, expr),
        }

        // Step 3: Clean-up
        match expr {
            Expr::Lambda(_)
            | Expr::Generator(_)
            | Expr::ListComp(_)
            | Expr::DictComp(_)
            | Expr::SetComp(_) => {
                self.analyze.scopes.push(self.semantic.scope_id);
                self.semantic.pop_scope();
            }
            _ => {}
        }

        // Step 4: Analysis
        match expr {
            Expr::StringLiteral(string_literal) => {
                analyze::string_like(string_literal.into(), self);
            }
            Expr::BytesLiteral(bytes_literal) => analyze::string_like(bytes_literal.into(), self),
            Expr::FString(f_string) => analyze::string_like(f_string.into(), self),
            Expr::TString(t_string) => analyze::string_like(t_string.into(), self),
            _ => {}
        }

        self.semantic.flags = flags_snapshot;
        analyze::expression(expr, self);
        self.semantic.pop_node();
    }

    fn visit_except_handler(&mut self, except_handler: &'a ExceptHandler) {
        let flags_snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::EXCEPTION_HANDLER;

        // Step 1: Binding
        let binding = match except_handler {
            ExceptHandler::ExceptHandler(ast::ExceptHandlerExceptHandler {
                type_: _,
                name,
                body: _,
                range: _,
                node_index: _,
            }) => {
                if let Some(name) = name {
                    // Store the existing binding, if any.
                    let binding_id = self.semantic.lookup_symbol(name.as_str());

                    // Add the bound exception name to the scope.
                    self.add_binding(
                        name.as_str(),
                        name.range(),
                        BindingKind::BoundException,
                        BindingFlags::empty(),
                    );

                    Some((name, binding_id))
                } else {
                    None
                }
            }
        };

        // Step 2: Traversal
        walk_except_handler(self, except_handler);

        // Step 3: Clean-up
        if let Some((name, binding_id)) = binding {
            self.add_binding(
                name.as_str(),
                name.range(),
                BindingKind::UnboundException(binding_id),
                BindingFlags::empty(),
            );
        }

        // Step 4: Analysis
        analyze::except_handler(except_handler, self);

        self.semantic.flags = flags_snapshot;
    }

    fn visit_parameters(&mut self, parameters: &'a Parameters) {
        // Step 1: Binding.
        // Bind, but intentionally avoid walking default expressions, as we handle them
        // upstream.
        for parameter in parameters.iter().map(AnyParameterRef::as_parameter) {
            self.visit_parameter(parameter);
        }

        // Step 4: Analysis
        analyze::parameters(parameters, self);
    }

    fn visit_parameter(&mut self, parameter: &'a Parameter) {
        // Step 1: Binding.
        // Bind, but intentionally avoid walking the annotation, as we handle it
        // upstream.
        self.add_binding(
            &parameter.name,
            parameter.identifier(),
            BindingKind::Argument,
            BindingFlags::empty(),
        );

        // Step 4: Analysis
        analyze::parameter(parameter, self);
    }

    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        // Step 1: Binding
        if let Pattern::MatchAs(ast::PatternMatchAs {
            name: Some(name), ..
        })
        | Pattern::MatchStar(ast::PatternMatchStar {
            name: Some(name),
            range: _,
            node_index: _,
        })
        | Pattern::MatchMapping(ast::PatternMatchMapping {
            rest: Some(name), ..
        }) = pattern
        {
            self.add_binding(
                name,
                name.range(),
                BindingKind::Assignment,
                BindingFlags::empty(),
            );
        }

        // Step 2: Traversal
        walk_pattern(self, pattern);
    }

    fn visit_body(&mut self, body: &'a [Stmt]) {
        // Step 4: Analysis
        analyze::suite(body, self);

        // Step 2: Traversal
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }

    fn visit_match_case(&mut self, match_case: &'a MatchCase) {
        self.visit_pattern(&match_case.pattern);
        if let Some(expr) = &match_case.guard {
            self.visit_boolean_test(expr);
        }

        self.semantic.push_branch();
        self.visit_body(&match_case.body);
        self.semantic.pop_branch();
    }

    fn visit_type_param(&mut self, type_param: &'a ast::TypeParam) {
        // Step 1: Binding
        match type_param {
            ast::TypeParam::TypeVar(ast::TypeParamTypeVar { name, .. })
            | ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple { name, .. })
            | ast::TypeParam::ParamSpec(ast::TypeParamParamSpec { name, .. }) => {
                self.add_binding(
                    name.as_str(),
                    name.range(),
                    BindingKind::TypeParam,
                    BindingFlags::empty(),
                );
            }
        }
        // Step 2: Traversal
        match type_param {
            ast::TypeParam::TypeVar(ast::TypeParamTypeVar {
                bound,
                default,
                name: _,
                range: _,
                node_index: _,
            }) => {
                if let Some(expr) = bound {
                    self.visit
                        .type_param_definitions
                        .push((expr, self.semantic.snapshot()));
                }
                if let Some(expr) = default {
                    self.visit
                        .type_param_definitions
                        .push((expr, self.semantic.snapshot()));
                }
            }
            ast::TypeParam::TypeVarTuple(ast::TypeParamTypeVarTuple {
                default,
                name: _,
                range: _,
                node_index: _,
            }) => {
                if let Some(expr) = default {
                    self.visit
                        .type_param_definitions
                        .push((expr, self.semantic.snapshot()));
                }
            }
            ast::TypeParam::ParamSpec(ast::TypeParamParamSpec {
                default,
                name: _,
                range: _,
                node_index: _,
            }) => {
                if let Some(expr) = default {
                    self.visit
                        .type_param_definitions
                        .push((expr, self.semantic.snapshot()));
                }
            }
        }
    }

    fn visit_interpolated_string_element(
        &mut self,
        interpolated_string_element: &'a InterpolatedStringElement,
    ) {
        let snapshot = self.semantic.flags;
        if interpolated_string_element.is_interpolation() {
            self.semantic.flags |= SemanticModelFlags::INTERPOLATED_STRING_REPLACEMENT_FIELD;
        }
        visitor::walk_interpolated_string_element(self, interpolated_string_element);
        self.semantic.flags = snapshot;
    }
}

impl<'a> Checker<'a> {
    /// Visit a [`Module`].
    fn visit_module(&mut self, python_ast: &'a Suite) {
        // Extract any global bindings from the module body.
        if let Some(globals) = Globals::from_body(python_ast) {
            self.semantic.set_globals(globals);
        }
        analyze::module(python_ast, self);
    }

    /// Visit a list of [`Comprehension`] nodes, assumed to be the comprehensions that compose a
    /// generator expression, like a list or set comprehension.
    fn visit_generators(&mut self, kind: GeneratorKind, generators: &'a [Comprehension]) {
        let mut iterator = generators.iter();

        let Some(generator) = iterator.next() else {
            unreachable!("Generator expression must contain at least one generator");
        };

        let flags = self.semantic.flags;

        // Generators are compiled as nested functions. (This may change with PEP 709.)
        // As such, the `iter` of the first generator is evaluated in the outer scope, while all
        // subsequent nodes are evaluated in the inner scope.
        //
        // For example, given:
        // ```python
        // class A:
        //     T = range(10)
        //
        //     L = [x for x in T for y in T]
        // ```
        //
        // Conceptually, this is compiled as:
        // ```python
        // class A:
        //     T = range(10)
        //
        //     def foo(x=T):
        //         def bar(y=T):
        //             pass
        //         return bar()
        //     foo()
        // ```
        //
        // Following Python's scoping rules, the `T` in `x=T` is thus evaluated in the outer scope,
        // while all subsequent reads and writes are evaluated in the inner scope. In particular,
        // `x` is local to `foo`, and the `T` in `y=T` skips the class scope when resolving.
        self.visit_expr(&generator.iter);
        self.semantic.push_scope(ScopeKind::Generator {
            kind,
            is_async: generators
                .iter()
                .any(|comprehension| comprehension.is_async),
        });

        self.visit_expr(&generator.target);
        self.semantic.flags = flags;

        for expr in &generator.ifs {
            self.visit_boolean_test(expr);
        }

        for generator in iterator {
            self.visit_expr(&generator.iter);

            self.visit_expr(&generator.target);
            self.semantic.flags = flags;

            for expr in &generator.ifs {
                self.visit_boolean_test(expr);
            }
        }

        // Step 4: Analysis
        for generator in generators {
            analyze::comprehension(generator, self);
        }
    }

    /// Visit an body of [`Stmt`] nodes within a type-checking block.
    fn visit_type_checking_block(&mut self, body: &'a [Stmt]) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::TYPE_CHECKING_BLOCK;
        self.visit_body(body);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as a runtime-evaluated type annotation.
    fn visit_runtime_evaluated_annotation(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::RUNTIME_EVALUATED_ANNOTATION;
        self.visit_type_definition(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as a runtime-required type annotation.
    fn visit_runtime_required_annotation(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::RUNTIME_REQUIRED_ANNOTATION;
        self.visit_type_definition(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as the value expression
    /// of a [PEP 613] type alias.
    ///
    /// For example:
    /// ```python
    /// from typing import TypeAlias
    ///
    /// OptStr: TypeAlias = str | None  # We're visiting the RHS
    /// ```
    ///
    /// [PEP 613]: https://peps.python.org/pep-0613/
    fn visit_annotated_type_alias_value(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::ANNOTATED_TYPE_ALIAS;
        self.visit_type_definition(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as the value expression
    /// of a [PEP 695] type alias.
    ///
    /// For example:
    /// ```python
    /// type OptStr = str | None  # We're visiting the RHS
    /// ```
    ///
    /// [PEP 695]: https://peps.python.org/pep-0695/#generic-type-alias
    fn visit_deferred_type_alias_value(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        // even though we don't visit these nodes immediately we need to
        // modify the semantic flags before we push the expression and its
        // corresponding semantic snapshot
        self.semantic.flags |= SemanticModelFlags::DEFERRED_TYPE_ALIAS;
        self.visit
            .type_param_definitions
            .push((expr, self.semantic.snapshot()));
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as a type definition.
    fn visit_type_definition(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::TYPE_DEFINITION;
        self.visit_expr(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as _not_ a type definition.
    fn visit_non_type_definition(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags -= SemanticModelFlags::TYPE_DEFINITION;
        self.visit_expr(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`Expr`], and treat it as the `typ` argument to `typing.cast`.
    fn visit_cast_type_argument(&mut self, arg: &'a Expr) {
        self.visit_type_definition(arg);

        if !self.source_type.is_stub() && self.is_rule_enabled(Rule::RuntimeCastValue) {
            flake8_type_checking::rules::runtime_cast_value(self, arg);
        }
    }

    /// Visit an [`Expr`], and treat it as a boolean test. This is useful for detecting whether an
    /// expressions return value is significant, or whether the calling context only relies on
    /// its truthiness.
    fn visit_boolean_test(&mut self, expr: &'a Expr) {
        let snapshot = self.semantic.flags;
        self.semantic.flags |= SemanticModelFlags::BOOLEAN_TEST;
        self.visit_expr(expr);
        self.semantic.flags = snapshot;
    }

    /// Visit an [`ElifElseClause`]
    fn visit_elif_else_clause(&mut self, clause: &'a ElifElseClause) {
        if let Some(test) = &clause.test {
            self.visit_boolean_test(test);
        }
        self.visit_body(&clause.body);
    }

    /// Add a [`Binding`] to the current scope, bound to the given name.
    fn add_binding(
        &mut self,
        name: &'a str,
        range: TextRange,
        kind: BindingKind<'a>,
        mut flags: BindingFlags,
    ) -> BindingId {
        // Determine the scope to which the binding belongs.
        // Per [PEP 572](https://peps.python.org/pep-0572/#scope-of-the-target), named
        // expressions in generators and comprehensions bind to the scope that contains the
        // outermost comprehension.
        let scope_id = if kind.is_named_expr_assignment() {
            self.semantic
                .scopes
                .ancestor_ids(self.semantic.scope_id)
                .find_or_last(|scope_id| !self.semantic.scopes[*scope_id].kind.is_generator())
                .unwrap_or(self.semantic.scope_id)
        } else {
            self.semantic.scope_id
        };

        if self.semantic.in_exception_handler() {
            flags |= BindingFlags::IN_EXCEPT_HANDLER;
        }
        if self.semantic.in_assert_statement() {
            flags |= BindingFlags::IN_ASSERT_STATEMENT;
        }

        // Create the `Binding`.
        let binding_id = self.semantic.push_binding(range, kind, flags);

        // If the name is private, mark is as such.
        if name.starts_with('_') {
            self.semantic.bindings[binding_id].flags |= BindingFlags::PRIVATE_DECLARATION;
        }

        // If there's an existing binding in this scope, copy its references.
        if let Some(shadowed_id) = self.semantic.scopes[scope_id].get(name) {
            // If this is an annotation, and we already have an existing value in the same scope,
            // don't treat it as an assignment, but track it as a delayed annotation.
            if self.semantic.binding(binding_id).kind.is_annotation() {
                self.semantic
                    .add_delayed_annotation(shadowed_id, binding_id);
                return binding_id;
            }

            // Avoid shadowing builtins.
            let shadowed = &self.semantic.bindings[shadowed_id];
            if !matches!(
                shadowed.kind,
                BindingKind::Builtin | BindingKind::Deletion | BindingKind::UnboundException(_)
            ) {
                let references = shadowed.references.clone();
                let is_global = shadowed.is_global();
                let is_nonlocal = shadowed.is_nonlocal();

                // If the shadowed binding was global, then this one is too.
                if is_global {
                    self.semantic.bindings[binding_id].flags |= BindingFlags::GLOBAL;
                }

                // If the shadowed binding was non-local, then this one is too.
                if is_nonlocal {
                    self.semantic.bindings[binding_id].flags |= BindingFlags::NONLOCAL;
                }

                self.semantic.bindings[binding_id].references = references;
            }
        } else if let Some(shadowed_id) = self
            .semantic
            .scopes
            .ancestors(scope_id)
            .skip(1)
            .filter(|scope| scope.kind.is_function() || scope.kind.is_module())
            .find_map(|scope| scope.get(name))
        {
            // Otherwise, if there's an existing binding in a parent scope, mark it as shadowed.
            self.semantic
                .shadowed_bindings
                .insert(binding_id, shadowed_id);
        }

        // Add the binding to the scope.
        let scope = &mut self.semantic.scopes[scope_id];
        scope.add(name, binding_id);

        binding_id
    }

    fn bind_builtins(&mut self) {
        let target_version = self.target_version();
        let settings = self.settings();
        let mut bind_builtin = |builtin| {
            // Add the builtin to the scope.
            let binding_id = self.semantic.push_builtin();
            let scope = self.semantic.global_scope_mut();
            scope.add(builtin, binding_id);
        };
        let standard_builtins = python_builtins(target_version.minor, self.source_type.is_ipynb());
        for builtin in standard_builtins {
            bind_builtin(builtin);
        }
        for builtin in MAGIC_GLOBALS {
            bind_builtin(builtin);
        }
        for builtin in &settings.builtins {
            bind_builtin(builtin);
        }
    }

    fn handle_node_load(&mut self, expr: &Expr) {
        let Expr::Name(expr) = expr else {
            return;
        };
        self.semantic.resolve_load(expr);
    }

    fn handle_node_store(&mut self, id: &'a str, expr: &Expr) {
        let parent = self.semantic.current_statement();

        let mut flags = BindingFlags::empty();
        if helpers::is_unpacking_assignment(parent, expr) {
            flags.insert(BindingFlags::UNPACKED_ASSIGNMENT);
        }

        match parent {
            Stmt::TypeAlias(_) => flags.insert(BindingFlags::DEFERRED_TYPE_ALIAS),
            Stmt::AnnAssign(ast::StmtAnnAssign { annotation, .. }) => {
                // TODO: It is a bit unfortunate that we do this check twice
                //       maybe we should change how we visit this statement
                //       so the semantic flag for the type alias sticks around
                //       until after we've handled this store, so we can check
                //       the flag instead of duplicating this check
                if self.semantic.match_typing_expr(annotation, "TypeAlias") {
                    flags.insert(BindingFlags::ANNOTATED_TYPE_ALIAS);
                }
            }
            _ => {}
        }

        let scope = self.semantic.current_scope();

        if scope.kind.is_module()
            && match parent {
                Stmt::Assign(ast::StmtAssign { targets, .. }) => {
                    if let Some(Expr::Name(ast::ExprName { id, .. })) = targets.first() {
                        id == "__all__"
                    } else {
                        false
                    }
                }
                Stmt::AugAssign(ast::StmtAugAssign { target, .. }) => {
                    if let Expr::Name(ast::ExprName { id, .. }) = target.as_ref() {
                        id == "__all__"
                    } else {
                        false
                    }
                }
                Stmt::AnnAssign(ast::StmtAnnAssign { target, .. }) => {
                    if let Expr::Name(ast::ExprName { id, .. }) = target.as_ref() {
                        id == "__all__"
                    } else {
                        false
                    }
                }
                _ => false,
            }
        {
            let (all_names, all_flags) = self.semantic.extract_dunder_all_names(parent);

            if all_flags.intersects(DunderAllFlags::INVALID_OBJECT) {
                flags |= BindingFlags::INVALID_ALL_OBJECT;
            }
            if all_flags.intersects(DunderAllFlags::INVALID_FORMAT) {
                flags |= BindingFlags::INVALID_ALL_FORMAT;
            }

            self.add_binding(
                id,
                expr.range(),
                BindingKind::Export(Export {
                    names: all_names.into_boxed_slice(),
                }),
                flags,
            );
            return;
        }

        // If the expression is the left-hand side of a walrus operator, then it's a named
        // expression assignment, as in:
        // ```python
        // if (x := 10) > 5:
        //     ...
        // ```
        if self.semantic.in_named_expression_assignment() {
            self.add_binding(id, expr.range(), BindingKind::NamedExprAssignment, flags);
            return;
        }

        // Match the left-hand side of an annotated assignment without a value,
        // like `x` in `x: int`. N.B. In stub files, these should be viewed
        // as assignments on par with statements such as `x: int = 5`.
        if matches!(
            parent,
            Stmt::AnnAssign(ast::StmtAnnAssign { value: None, .. })
        ) && !self.semantic.in_annotation()
        {
            self.add_binding(id, expr.range(), BindingKind::Annotation, flags);
            return;
        }

        // A binding within a `for` must be a loop variable, as in:
        // ```python
        // for x in range(10):
        //     ...
        // ```
        if parent.is_for_stmt() {
            self.add_binding(id, expr.range(), BindingKind::LoopVar, flags);
            return;
        }

        // A binding within a `with` must be an item, as in:
        // ```python
        // with open("file.txt") as fp:
        //     ...
        // ```
        if parent.is_with_stmt() {
            self.add_binding(id, expr.range(), BindingKind::WithItemVar, flags);
            return;
        }

        self.add_binding(id, expr.range(), BindingKind::Assignment, flags);
    }

    fn handle_node_delete(&mut self, expr: &'a Expr) {
        let Expr::Name(ast::ExprName { id, .. }) = expr else {
            return;
        };

        self.semantic.resolve_del(id, expr.range());

        if helpers::on_conditional_branch(&mut self.semantic.current_statements()) {
            return;
        }

        // Create a binding to model the deletion.
        let binding_id =
            self.semantic
                .push_binding(expr.range(), BindingKind::Deletion, BindingFlags::empty());
        let scope = self.semantic.current_scope_mut();
        scope.add(id, binding_id);
    }

    /// After initial traversal of the AST, visit all class bases that were deferred.
    ///
    /// This method should only be relevant in stub files, where forward references are
    /// legal in class bases. For other kinds of Python files, using a forward reference
    /// in a class base is never legal, so `self.visit.class_bases` should always be empty.
    ///
    /// For example, in a stub file:
    /// ```python
    /// class Foo(list[Bar]): ...  # <-- `Bar` is a forward reference in a class base
    /// class Bar: ...
    /// ```
    fn visit_deferred_class_bases(&mut self) {
        let snapshot = self.semantic.snapshot();
        let deferred_bases = std::mem::take(&mut self.visit.class_bases);
        debug_assert!(
            self.source_type.is_stub() || deferred_bases.is_empty(),
            "Class bases should never be deferred outside of stub files"
        );
        for (expr, snapshot) in deferred_bases {
            self.semantic.restore(snapshot);
            // Set this flag to avoid infinite recursion, or we'll just defer it again:
            self.semantic.flags |= SemanticModelFlags::DEFERRED_CLASS_BASE;
            self.visit_expr(expr);
        }
        self.semantic.restore(snapshot);
    }

    /// After initial traversal of the AST, visit all "future type definitions".
    ///
    /// A "future type definition" is a type definition where [PEP 563] semantics
    /// apply (i.e., an annotation in a module that has `from __future__ import annotations`
    /// at the top of the file, or an annotation in a stub file). These type definitions
    /// support forward references, so they are deferred on initial traversal
    /// of the source tree.
    ///
    /// For example:
    /// ```python
    /// from __future__ import annotations
    ///
    /// def foo() -> Bar:  # <-- return annotation is a "future type definition"
    ///     return Bar()
    ///
    /// class Bar: pass
    /// ```
    ///
    /// [PEP 563]: https://peps.python.org/pep-0563/
    fn visit_deferred_future_type_definitions(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.future_type_definitions.is_empty() {
            let type_definitions = std::mem::take(&mut self.visit.future_type_definitions);
            for (expr, snapshot) in type_definitions {
                self.semantic.restore(snapshot);

                // Type definitions should only be considered "`__future__` type definitions"
                // if they are annotations in a module where `from __future__ import
                // annotations` is active, or they are type definitions in a stub file.
                debug_assert!(
                    (self.semantic.future_annotations_or_stub()
                        || self.target_version().defers_annotations())
                        && (self.source_type.is_stub() || self.semantic.in_annotation())
                );

                self.semantic.flags |= SemanticModelFlags::TYPE_DEFINITION
                    | SemanticModelFlags::FUTURE_TYPE_DEFINITION;
                self.visit_expr(expr);
            }
        }
        self.semantic.restore(snapshot);
    }

    /// After initial traversal of the AST, visit all [type parameter definitions].
    ///
    /// Type parameters natively support forward references,
    /// so are always deferred during initial traversal of the source tree.
    ///
    /// For example:
    /// ```python
    /// class Foo[T: Bar]: pass  # <-- Forward reference used in definition of type parameter `T`
    /// type X[T: Bar] = Foo[T]  # <-- Ditto
    /// class Bar: pass
    /// ```
    ///
    /// [type parameter definitions]: https://docs.python.org/3/reference/executionmodel.html#annotation-scopes
    fn visit_deferred_type_param_definitions(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.type_param_definitions.is_empty() {
            let type_params = std::mem::take(&mut self.visit.type_param_definitions);
            for (type_param, snapshot) in type_params {
                self.semantic.restore(snapshot);

                self.semantic.flags |=
                    SemanticModelFlags::TYPE_PARAM_DEFINITION | SemanticModelFlags::TYPE_DEFINITION;
                self.visit_expr(type_param);
            }
        }
        self.semantic.restore(snapshot);
    }

    /// After initial traversal of the AST, visit all "string type definitions",
    /// i.e., type definitions that are enclosed within quotes so as to allow
    /// the type definition to use forward references.
    ///
    /// For example:
    /// ```python
    /// def foo() -> "Bar":  # <-- return annotation is a "string type definition"
    ///     return Bar()
    ///
    /// class Bar: pass
    /// ```
    fn visit_deferred_string_type_definitions(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.string_type_definitions.is_empty() {
            let type_definitions = std::mem::take(&mut self.visit.string_type_definitions);
            for (string_expr, snapshot) in type_definitions {
                match self.parse_type_annotation(string_expr) {
                    Ok(parsed_annotation) => {
                        self.parsed_type_annotation = Some(parsed_annotation);

                        let annotation = string_expr.value.to_str();
                        let range = string_expr.range();

                        self.semantic.restore(snapshot);

                        if self.is_rule_enabled(Rule::QuotedAnnotation) {
                            pyupgrade::rules::quoted_annotation(self, annotation, range);
                        }

                        if self.source_type.is_stub() {
                            if self.is_rule_enabled(Rule::QuotedAnnotationInStub) {
                                flake8_pyi::rules::quoted_annotation_in_stub(
                                    self, annotation, range,
                                );
                            }
                        }

                        let type_definition_flag = match parsed_annotation.kind() {
                            AnnotationKind::Simple => {
                                SemanticModelFlags::SIMPLE_STRING_TYPE_DEFINITION
                            }
                            AnnotationKind::Complex => {
                                SemanticModelFlags::COMPLEX_STRING_TYPE_DEFINITION
                            }
                        };

                        self.semantic.flags |=
                            SemanticModelFlags::TYPE_DEFINITION | type_definition_flag;
                        let parsed_expr = parsed_annotation.expression();
                        self.visit_expr(parsed_expr);
                        if self.semantic.in_type_alias_value() {
                            // stub files are covered by PYI020
                            if !self.source_type.is_stub()
                                && self.is_rule_enabled(Rule::QuotedTypeAlias)
                            {
                                flake8_type_checking::rules::quoted_type_alias(
                                    self,
                                    parsed_expr,
                                    string_expr,
                                );
                            }
                        }
                        self.parsed_type_annotation = None;
                    }
                    Err(parse_error) => {
                        self.semantic.restore(snapshot);

                        // F722
                        if self.is_rule_enabled(Rule::ForwardAnnotationSyntaxError) {
                            self.report_type_diagnostic(
                                pyflakes::rules::ForwardAnnotationSyntaxError {
                                    parse_error: parse_error.error.to_string(),
                                },
                                string_expr.range(),
                            );
                        }
                    }
                }
            }

            // If we're parsing string annotations inside string annotations
            // (which is the only reason we might enter a second iteration of this loop),
            // the cache is no longer valid. We must invalidate it to avoid an infinite loop.
            //
            // For example, consider the following annotation:
            // ```python
            // x: "list['str']"
            // ```
            //
            // The first time we visit the AST, we see `"list['str']"`
            // and identify it as a stringified annotation.
            // We store it in `self.visit.string_type_definitions` to be analyzed later.
            //
            // After the entire tree has been visited, we look through
            // `self.visit.string_type_definitions` and find `"list['str']"`.
            // We parse it, and it becomes `list['str']`.
            // After parsing it, we call `self.visit_expr()` on the `list['str']` node,
            // and that `visit_expr` call is going to find `'str'` inside that node and
            // identify it as a string type definition, appending it to
            // `self.visit.string_type_definitions`, ensuring that there will be one
            // more iteration of this loop.
            //
            // Unfortunately, the `TextRange` of `'str'`
            // here will be *relative to the parsed `list['str']` node* rather than
            // *relative to the original module*, meaning the cache
            // (which uses `TextSize` as the key) becomes invalid on the second
            // iteration of this loop.
            self.parsed_annotations_cache.clear();
        }
        self.semantic.restore(snapshot);
    }

    /// After initial traversal of the AST, visit all function bodies.
    ///
    /// Function bodies are always deferred on initial traversal of the source tree,
    /// as the body of a function may validly contain references to global-scope symbols
    /// that were not yet defined at the point when the function was defined.
    fn visit_deferred_functions(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.functions.is_empty() {
            let deferred_functions = std::mem::take(&mut self.visit.functions);
            for snapshot in deferred_functions {
                self.semantic.restore(snapshot);

                let stmt = self.semantic.current_statement();

                let Stmt::FunctionDef(ast::StmtFunctionDef {
                    body, parameters, ..
                }) = stmt
                else {
                    unreachable!("Expected Stmt::FunctionDef")
                };

                self.with_semantic_checker(|semantic, context| semantic.visit_stmt(stmt, context));

                self.visit_parameters(parameters);
                // Set the docstring state before visiting the function body.
                self.docstring_state = DocstringState::Expected(ExpectedDocstringKind::Function);
                self.visit_body(body);
            }
        }
        self.semantic.restore(snapshot);
    }

    /// After initial traversal of the source tree has been completed,
    /// visit all lambdas. Lambdas are deferred during the initial traversal
    /// for the same reason as function bodies.
    fn visit_deferred_lambdas(&mut self) {
        let snapshot = self.semantic.snapshot();
        while !self.visit.lambdas.is_empty() {
            let lambdas = std::mem::take(&mut self.visit.lambdas);
            for snapshot in lambdas {
                self.semantic.restore(snapshot);

                let Some(Expr::Lambda(ast::ExprLambda {
                    parameters,
                    body,
                    range: _,
                    node_index: _,
                })) = self.semantic.current_expression()
                else {
                    unreachable!("Expected Expr::Lambda");
                };

                if let Some(parameters) = parameters {
                    self.visit_parameters(parameters);
                }
                self.visit_expr(body);
            }
        }
        self.semantic.restore(snapshot);
    }

    /// After initial traversal of the source tree has been completed,
    /// recursively visit all AST nodes that were deferred on the first pass.
    /// This includes lambdas, functions, type parameters, and type annotations.
    fn visit_deferred(&mut self) {
        while !self.visit.is_empty() {
            self.visit_deferred_class_bases();
            self.visit_deferred_functions();
            self.visit_deferred_type_param_definitions();
            self.visit_deferred_lambdas();
            self.visit_deferred_future_type_definitions();
            self.visit_deferred_string_type_definitions();
        }
    }

    /// Run any lint rules that operate over the module exports (i.e., members of `__all__`).
    fn visit_exports(&mut self) {
        let snapshot = self.semantic.snapshot();

        let definitions: Vec<DunderAllDefinition> = self
            .semantic
            .global_scope()
            .get_all("__all__")
            .map(|binding_id| &self.semantic.bindings[binding_id])
            .filter_map(|binding| match &binding.kind {
                BindingKind::Export(Export { names }) => {
                    Some(DunderAllDefinition::new(binding.range(), names.to_vec()))
                }
                _ => None,
            })
            .collect();

        for definition in definitions {
            for export in definition.names() {
                let (name, range) = (export.name(), export.range());
                if let Some(binding_id) = self.semantic.global_scope().get(name) {
                    self.semantic.flags |= SemanticModelFlags::DUNDER_ALL_DEFINITION;
                    // Mark anything referenced in `__all__` as used.
                    self.semantic
                        .add_global_reference(binding_id, ExprContext::Load, range);
                    self.semantic.flags -= SemanticModelFlags::DUNDER_ALL_DEFINITION;
                } else {
                    if self.semantic.global_scope().uses_star_imports() {
                        // F405
                        if self.is_rule_enabled(Rule::UndefinedLocalWithImportStarUsage) {
                            self.report_diagnostic(
                                pyflakes::rules::UndefinedLocalWithImportStarUsage {
                                    name: name.to_string(),
                                },
                                range,
                            )
                            .set_parent(definition.start());
                        }
                    } else {
                        // F822
                        if self.is_rule_enabled(Rule::UndefinedExport) {
                            if is_undefined_export_in_dunder_init_enabled(self.settings())
                                || !self.path.ends_with("__init__.py")
                            {
                                self.report_diagnostic(
                                    pyflakes::rules::UndefinedExport {
                                        name: name.to_string(),
                                    },
                                    range,
                                )
                                .set_parent(definition.start());
                            }
                        }
                    }
                }
            }
        }

        self.semantic.restore(snapshot);
    }
}

struct ParsedAnnotationsCache<'a> {
    arena: &'a typed_arena::Arena<Result<ParsedAnnotation, ParseError>>,
    by_offset: RefCell<FxHashMap<TextSize, Result<&'a ParsedAnnotation, &'a ParseError>>>,
}

impl<'a> ParsedAnnotationsCache<'a> {
    fn new(arena: &'a typed_arena::Arena<Result<ParsedAnnotation, ParseError>>) -> Self {
        Self {
            arena,
            by_offset: RefCell::default(),
        }
    }

    fn lookup_or_parse(
        &self,
        annotation: &ast::ExprStringLiteral,
        source: &str,
    ) -> Result<&'a ParsedAnnotation, &'a ParseError> {
        *self
            .by_offset
            .borrow_mut()
            .entry(annotation.start())
            .or_insert_with(|| {
                self.arena
                    .alloc(parse_type_annotation(annotation, source))
                    .as_ref()
            })
    }

    fn clear(&self) {
        self.by_offset.borrow_mut().clear();
    }
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn check_ast(
    parsed: &Parsed<ModModule>,
    locator: &Locator,
    stylist: &Stylist,
    indexer: &Indexer,
    noqa_line_for: &NoqaMapping,
    settings: &LinterSettings,
    noqa: flags::Noqa,
    path: &Path,
    package: Option<PackageRoot<'_>>,
    source_type: PySourceType,
    cell_offsets: Option<&CellOffsets>,
    notebook_index: Option<&NotebookIndex>,
    target_version: TargetVersion,
    context: &LintContext,
) -> Vec<SemanticSyntaxError> {
    let module_path = package
        .map(PackageRoot::path)
        .and_then(|package| to_module_path(package, path));
    let module = Module {
        kind: if path.ends_with("__init__.py") {
            ModuleKind::Package
        } else {
            ModuleKind::Module
        },
        name: if let Some(module_path) = &module_path {
            module_path.last().map(String::as_str)
        } else {
            path.file_stem().and_then(std::ffi::OsStr::to_str)
        },
        source: if let Some(module_path) = module_path.as_ref() {
            ModuleSource::Path(module_path)
        } else {
            ModuleSource::File(path)
        },
        python_ast: parsed.suite(),
    };

    let allocator = typed_arena::Arena::new();
    let mut checker = Checker::new(
        parsed,
        &allocator,
        settings,
        noqa_line_for,
        noqa,
        path,
        package,
        module,
        locator,
        stylist,
        indexer,
        source_type,
        cell_offsets,
        notebook_index,
        target_version,
        context,
    );
    checker.bind_builtins();

    // Iterate over the AST.
    checker.visit_module(parsed.suite());
    checker.visit_body(parsed.suite());

    // Visit any deferred syntax nodes. Take care to visit in order, such that we avoid adding
    // new deferred nodes after visiting nodes of that kind. For example, visiting a deferred
    // function can add a deferred lambda, but the opposite is not true.
    checker.visit_deferred();
    checker.visit_exports();

    // Check docstrings, bindings, and unresolved references.
    analyze::deferred_lambdas(&mut checker);
    analyze::deferred_for_loops(&mut checker);
    analyze::definitions(&mut checker);
    analyze::bindings(&checker);
    analyze::unresolved_references(&checker);

    // Reset the scope to module-level, and check all consumed scopes.
    checker.semantic.scope_id = ScopeId::global();
    checker.analyze.scopes.push(ScopeId::global());
    analyze::deferred_scopes(&checker);

    let Checker {
        semantic_errors, ..
    } = checker;

    semantic_errors.into_inner()
}

/// A type for collecting diagnostics in a given file.
///
/// [`LintContext::report_diagnostic`] can be used to obtain a [`DiagnosticGuard`], which will push
/// a [`Violation`] to the contained [`Diagnostic`] collection on `Drop`.
pub(crate) struct LintContext<'a> {
    diagnostics: RefCell<Vec<Diagnostic>>,
    source_file: SourceFile,
    rules: RuleTable,
    settings: &'a LinterSettings,
}

impl<'a> LintContext<'a> {
    /// Create a new collector with the given `source_file` and an empty collection of
    /// `Diagnostic`s.
    pub(crate) fn new(path: &Path, contents: &str, settings: &'a LinterSettings) -> Self {
        let source_file =
            SourceFileBuilder::new(path.to_string_lossy().as_ref(), contents).finish();

        // Ignore diagnostics based on per-file-ignores.
        let mut rules = settings.rules.clone();
        for ignore in crate::fs::ignores_from_path(path, &settings.per_file_ignores) {
            rules.disable(ignore);
        }

        Self {
            diagnostics: RefCell::default(),
            source_file,
            rules,
            settings,
        }
    }

    /// Return a [`DiagnosticGuard`] for reporting a diagnostic.
    ///
    /// The guard derefs to a [`Diagnostic`], so it can be used to further modify the diagnostic
    /// before it is added to the collection in the context on `Drop`.
    pub(crate) fn report_diagnostic<'chk, T: Violation>(
        &'chk self,
        kind: T,
        range: TextRange,
    ) -> DiagnosticGuard<'chk, 'a> {
        DiagnosticGuard {
            context: self,
            diagnostic: Some(kind.into_diagnostic(range, &self.source_file)),
            rule: T::rule(),
        }
    }

    /// Return a [`DiagnosticGuard`] for reporting a diagnostic if the corresponding rule is
    /// enabled.
    ///
    /// The guard derefs to a [`Diagnostic`], so it can be used to further modify the diagnostic
    /// before it is added to the collection in the context on `Drop`.
    pub(crate) fn report_diagnostic_if_enabled<'chk, T: Violation>(
        &'chk self,
        kind: T,
        range: TextRange,
    ) -> Option<DiagnosticGuard<'chk, 'a>> {
        let rule = T::rule();
        if self.is_rule_enabled(rule) {
            Some(DiagnosticGuard {
                context: self,
                diagnostic: Some(kind.into_diagnostic(range, &self.source_file)),
                rule,
            })
        } else {
            None
        }
    }

    #[inline]
    pub(crate) const fn is_rule_enabled(&self, rule: Rule) -> bool {
        self.rules.enabled(rule)
    }

    #[inline]
    pub(crate) const fn any_rule_enabled(&self, rules: &[Rule]) -> bool {
        self.rules.any_enabled(rules)
    }

    #[inline]
    pub(crate) fn iter_enabled_rules(&self) -> impl Iterator<Item = Rule> + '_ {
        self.rules.iter_enabled()
    }

    #[inline]
    pub(crate) fn into_parts(self) -> (Vec<Diagnostic>, SourceFile) {
        (self.diagnostics.into_inner(), self.source_file)
    }

    #[inline]
    pub(crate) fn as_mut_vec(&mut self) -> &mut Vec<Diagnostic> {
        self.diagnostics.get_mut()
    }

    #[inline]
    pub(crate) fn iter(&mut self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.get_mut().iter()
    }

    /// The [`LinterSettings`] for the current analysis, including the enabled rules.
    pub(crate) const fn settings(&self) -> &LinterSettings {
        self.settings
    }
}

/// An abstraction for mutating a diagnostic.
///
/// Callers can build this guard by starting with `Checker::report_diagnostic`.
///
/// The primary function of this guard is to add the underlying diagnostic to the `Checker`'s list
/// of diagnostics on `Drop`, while dereferencing to the underlying diagnostic for mutations like
/// adding fixes or parent ranges.
pub(crate) struct DiagnosticGuard<'a, 'b> {
    /// The parent checker that will receive the diagnostic on `Drop`.
    context: &'a LintContext<'b>,
    /// The diagnostic that we want to report.
    ///
    /// This is always `Some` until the `Drop` (or `defuse`) call.
    diagnostic: Option<Diagnostic>,
    rule: Rule,
}

impl DiagnosticGuard<'_, '_> {
    /// Consume the underlying `Diagnostic` without emitting it.
    ///
    /// In general you should avoid constructing diagnostics that may not be emitted, but this
    /// method can be used where this is unavoidable.
    pub(crate) fn defuse(mut self) {
        self.diagnostic = None;
    }
}

impl DiagnosticGuard<'_, '_> {
    fn resolve_applicability(&self, fix: &Fix) -> Applicability {
        self.context
            .settings
            .fix_safety
            .resolve_applicability(self.rule, fix.applicability())
    }

    /// Set the [`Fix`] used to fix the diagnostic.
    #[inline]
    pub(crate) fn set_fix(&mut self, fix: Fix) {
        if !self.context.rules.should_fix(self.rule) {
            self.diagnostic.as_mut().unwrap().remove_fix();
            return;
        }
        let applicability = self.resolve_applicability(&fix);
        self.diagnostic
            .as_mut()
            .unwrap()
            .set_fix(fix.with_applicability(applicability));
    }

    /// Set the [`Fix`] used to fix the diagnostic, if the provided function returns `Ok`.
    /// Otherwise, log the error.
    #[inline]
    pub(crate) fn try_set_fix(&mut self, func: impl FnOnce() -> anyhow::Result<Fix>) {
        match func() {
            Ok(fix) => self.set_fix(fix),
            Err(err) => log::debug!("Failed to create fix for {}: {}", self.name(), err),
        }
    }

    /// Set the [`Fix`] used to fix the diagnostic, if the provided function returns `Ok`.
    /// Otherwise, log the error.
    #[inline]
    pub(crate) fn try_set_optional_fix(
        &mut self,
        func: impl FnOnce() -> anyhow::Result<Option<Fix>>,
    ) {
        match func() {
            Ok(None) => {}
            Ok(Some(fix)) => self.set_fix(fix),
            Err(err) => log::debug!("Failed to create fix for {}: {}", self.name(), err),
        }
    }
}

impl std::ops::Deref for DiagnosticGuard<'_, '_> {
    type Target = Diagnostic;

    fn deref(&self) -> &Diagnostic {
        // OK because `self.diagnostic` is only `None` within `Drop`.
        self.diagnostic.as_ref().unwrap()
    }
}

/// Return a mutable borrow of the diagnostic in this guard.
impl std::ops::DerefMut for DiagnosticGuard<'_, '_> {
    fn deref_mut(&mut self) -> &mut Diagnostic {
        // OK because `self.diagnostic` is only `None` within `Drop`.
        self.diagnostic.as_mut().unwrap()
    }
}

impl Drop for DiagnosticGuard<'_, '_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Don't submit diagnostics when panicking because they might be incomplete.
            return;
        }

        if let Some(diagnostic) = self.diagnostic.take() {
            self.context.diagnostics.borrow_mut().push(diagnostic);
        }
    }
}
