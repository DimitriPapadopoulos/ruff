use ruff_python_ast::{self as ast, Expr};

use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_text_size::Ranged;

use crate::Violation;
use crate::checkers::ast::Checker;
use crate::preview::is_invalid_async_mock_access_check_enabled;

#[derive(Debug, PartialEq, Eq)]
enum Reason {
    UncalledMethod(String),
    NonExistentMethod(String),
}

/// ## What it does
/// Checks for common mistakes when using mock objects.
///
/// ## Why is this bad?
/// The `mock` module exposes an assertion API that can be used to verify that
/// mock objects undergo expected interactions. This rule checks for common
/// mistakes when using this API.
///
/// For example, it checks for mock attribute accesses that should be replaced
/// with mock method calls.
///
/// ## Example
/// ```python
/// my_mock.assert_called
/// ```
///
/// Use instead:
/// ```python
/// my_mock.assert_called()
/// ```
#[derive(ViolationMetadata)]
pub(crate) struct InvalidMockAccess {
    reason: Reason,
}

impl Violation for InvalidMockAccess {
    #[derive_message_formats]
    fn message(&self) -> String {
        let InvalidMockAccess { reason } = self;
        match reason {
            Reason::UncalledMethod(name) => format!("Mock method should be called: `{name}`"),
            Reason::NonExistentMethod(name) => format!("Non-existent mock method: `{name}`"),
        }
    }
}

/// PGH005
pub(crate) fn uncalled_mock_method(checker: &Checker, expr: &Expr) {
    if let Expr::Attribute(ast::ExprAttribute { attr, .. }) = expr {
        let is_uncalled_mock_method = matches!(
            attr.as_str(),
            "assert_any_call"
                | "assert_called"
                | "assert_called_once"
                | "assert_called_once_with"
                | "assert_called_with"
                | "assert_has_calls"
                | "assert_not_called"
        );
        let is_uncalled_async_mock_method =
            is_invalid_async_mock_access_check_enabled(checker.settings())
                && matches!(
                    attr.as_str(),
                    "assert_awaited"
                        | "assert_awaited_once"
                        | "assert_awaited_with"
                        | "assert_awaited_once_with"
                        | "assert_any_await"
                        | "assert_has_awaits"
                        | "assert_not_awaited"
                );
        if is_uncalled_mock_method || is_uncalled_async_mock_method {
            checker.report_diagnostic(
                InvalidMockAccess {
                    reason: Reason::UncalledMethod(attr.to_string()),
                },
                expr.range(),
            );
        }
    }
}

/// PGH005
pub(crate) fn non_existent_mock_method(checker: &Checker, test: &Expr) {
    let attr = match test {
        Expr::Attribute(ast::ExprAttribute { attr, .. }) => attr,
        Expr::Call(ast::ExprCall { func, .. }) => match func.as_ref() {
            Expr::Attribute(ast::ExprAttribute { attr, .. }) => attr,
            _ => return,
        },
        _ => return,
    };
    let is_missing_mock_method = matches!(
        attr.as_str(),
        "any_call"
            | "called_once"
            | "called_once_with"
            | "called_with"
            | "has_calls"
            | "not_called"
    );
    let is_missing_async_mock_method =
        is_invalid_async_mock_access_check_enabled(checker.settings())
            && matches!(
                attr.as_str(),
                "awaited"
                    | "awaited_once"
                    | "awaited_with"
                    | "awaited_once_with"
                    | "any_await"
                    | "has_awaits"
                    | "not_awaited"
            );
    if is_missing_mock_method || is_missing_async_mock_method {
        checker.report_diagnostic(
            InvalidMockAccess {
                reason: Reason::NonExistentMethod(attr.to_string()),
            },
            test.range(),
        );
    }
}
