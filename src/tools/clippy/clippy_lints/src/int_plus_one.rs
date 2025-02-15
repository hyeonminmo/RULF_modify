//! lint on blocks unnecessarily using >= with a + 1 or - 1

use rustc_ast::ast::{BinOpKind, Expr, ExprKind, Lit, LitKind};
use rustc_errors::Applicability;
use rustc_lint::{EarlyContext, EarlyLintPass};
use rustc_session::{declare_lint_pass, declare_tool_lint};

use crate::utils::{snippet_opt, span_lint_and_sugg};

declare_clippy_lint! {
    /// **What it does:** Checks for usage of `x >= y + 1` or `x - 1 >= y` (and `<=`) in a block
    ///
    /// **Why is this bad?** Readability -- better to use `> y` instead of `>= y + 1`.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```rust
    /// # let x = 1;
    /// # let y = 1;
    /// if x >= y + 1 {}
    /// ```
    ///
    /// Could be written as:
    ///
    /// ```rust
    /// # let x = 1;
    /// # let y = 1;
    /// if x > y {}
    /// ```
    pub INT_PLUS_ONE,
    complexity,
    "instead of using `x >= y + 1`, use `x > y`"
}

declare_lint_pass!(IntPlusOne => [INT_PLUS_ONE]);

// cases:
// BinOpKind::Ge
// x >= y + 1
// x - 1 >= y
//
// BinOpKind::Le
// x + 1 <= y
// x <= y - 1

#[derive(Copy, Clone)]
enum Side {
    LHS,
    RHS,
}

impl IntPlusOne {
    #[allow(clippy::cast_sign_loss)]
    fn check_lit(lit: &Lit, target_value: i128) -> bool {
        if let LitKind::Int(value, ..) = lit.kind {
            return value == (target_value as u128);
        }
        false
    }

    fn check_binop(cx: &EarlyContext<'_>, binop: BinOpKind, lhs: &Expr, rhs: &Expr) -> Option<String> {
        match (binop, &lhs.kind, &rhs.kind) {
            // case where `x - 1 >= ...` or `-1 + x >= ...`
            (BinOpKind::Ge, &ExprKind::Binary(ref lhskind, ref lhslhs, ref lhsrhs), _) => {
                match (lhskind.node, &lhslhs.kind, &lhsrhs.kind) {
                    // `-1 + x`
                    (BinOpKind::Add, &ExprKind::Lit(ref lit), _) if Self::check_lit(lit, -1) => {
                        Self::generate_recommendation(cx, binop, lhsrhs, rhs, Side::LHS)
                    },
                    // `x - 1`
                    (BinOpKind::Sub, _, &ExprKind::Lit(ref lit)) if Self::check_lit(lit, 1) => {
                        Self::generate_recommendation(cx, binop, lhslhs, rhs, Side::LHS)
                    },
                    _ => None,
                }
            },
            // case where `... >= y + 1` or `... >= 1 + y`
            (BinOpKind::Ge, _, &ExprKind::Binary(ref rhskind, ref rhslhs, ref rhsrhs))
                if rhskind.node == BinOpKind::Add =>
            {
                match (&rhslhs.kind, &rhsrhs.kind) {
                    // `y + 1` and `1 + y`
                    (&ExprKind::Lit(ref lit), _) if Self::check_lit(lit, 1) => {
                        Self::generate_recommendation(cx, binop, rhsrhs, lhs, Side::RHS)
                    },
                    (_, &ExprKind::Lit(ref lit)) if Self::check_lit(lit, 1) => {
                        Self::generate_recommendation(cx, binop, rhslhs, lhs, Side::RHS)
                    },
                    _ => None,
                }
            },
            // case where `x + 1 <= ...` or `1 + x <= ...`
            (BinOpKind::Le, &ExprKind::Binary(ref lhskind, ref lhslhs, ref lhsrhs), _)
                if lhskind.node == BinOpKind::Add =>
            {
                match (&lhslhs.kind, &lhsrhs.kind) {
                    // `1 + x` and `x + 1`
                    (&ExprKind::Lit(ref lit), _) if Self::check_lit(lit, 1) => {
                        Self::generate_recommendation(cx, binop, lhsrhs, rhs, Side::LHS)
                    },
                    (_, &ExprKind::Lit(ref lit)) if Self::check_lit(lit, 1) => {
                        Self::generate_recommendation(cx, binop, lhslhs, rhs, Side::LHS)
                    },
                    _ => None,
                }
            },
            // case where `... >= y - 1` or `... >= -1 + y`
            (BinOpKind::Le, _, &ExprKind::Binary(ref rhskind, ref rhslhs, ref rhsrhs)) => {
                match (rhskind.node, &rhslhs.kind, &rhsrhs.kind) {
                    // `-1 + y`
                    (BinOpKind::Add, &ExprKind::Lit(ref lit), _) if Self::check_lit(lit, -1) => {
                        Self::generate_recommendation(cx, binop, rhsrhs, lhs, Side::RHS)
                    },
                    // `y - 1`
                    (BinOpKind::Sub, _, &ExprKind::Lit(ref lit)) if Self::check_lit(lit, 1) => {
                        Self::generate_recommendation(cx, binop, rhslhs, lhs, Side::RHS)
                    },
                    _ => None,
                }
            },
            _ => None,
        }
    }

    fn generate_recommendation(
        cx: &EarlyContext<'_>,
        binop: BinOpKind,
        node: &Expr,
        other_side: &Expr,
        side: Side,
    ) -> Option<String> {
        let binop_string = match binop {
            BinOpKind::Ge => ">",
            BinOpKind::Le => "<",
            _ => return None,
        };
        if let Some(snippet) = snippet_opt(cx, node.span) {
            if let Some(other_side_snippet) = snippet_opt(cx, other_side.span) {
                let rec = match side {
                    Side::LHS => Some(format!("{} {} {}", snippet, binop_string, other_side_snippet)),
                    Side::RHS => Some(format!("{} {} {}", other_side_snippet, binop_string, snippet)),
                };
                return rec;
            }
        }
        None
    }

    fn emit_warning(cx: &EarlyContext<'_>, block: &Expr, recommendation: String) {
        span_lint_and_sugg(
            cx,
            INT_PLUS_ONE,
            block.span,
            "Unnecessary `>= y + 1` or `x - 1 >=`",
            "change it to",
            recommendation,
            Applicability::MachineApplicable, // snippet
        );
    }
}

impl EarlyLintPass for IntPlusOne {
    fn check_expr(&mut self, cx: &EarlyContext<'_>, item: &Expr) {
        if let ExprKind::Binary(ref kind, ref lhs, ref rhs) = item.kind {
            if let Some(ref rec) = Self::check_binop(cx, kind.node, lhs, rhs) {
                Self::emit_warning(cx, item, rec.clone());
            }
        }
    }
}
