//! Statement parsing: blocks, if/for/while/switch/try and control-flow sugar. Part of the `Parser` impl, split out of `parser.rs`.

use crate::ast::*;
use crate::lexer::*;

use super::{Parser, PResult};

impl<'a> Parser<'a> {
    // ---- statements -----------------------------------------------------

    pub(super) fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_sym(Sym::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            if self.eat_sym(Sym::Semi) {
                continue;
            }
            stmts.push(self.parse_stmt()?);
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(stmts)
    }

    /// Parse a `var`/`final` local declaration (the keyword is current). `delete`
    /// carries an `@delete` marker requesting a scope-close free.
    pub(super) fn parse_var_stmt(&mut self, line: usize, delete: bool) -> PResult<Stmt> {
        let is_final = self.at_kw(Kw::Final);
        self.bump();
        let name = self.expect_ident()?;
        let ty = if self.eat_sym(Sym::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let init = if self.eat_sym(Sym::Assign) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.eat_sym(Sym::Semi);
        Ok(Stmt::Var {
            name,
            ty,
            init,
            is_final,
            delete,
            line,
        })
    }

    pub(super) fn parse_stmt(&mut self) -> PResult<Stmt> {
        let line = self.line();
        match self.peek().clone() {
            TokKind::Kw(Kw::Var) | TokKind::Kw(Kw::Final) => self.parse_var_stmt(line, false),
            // `@delete var x = …` — the developer asks for `x` to be freed at the
            // end of this scope (the local-scope counterpart to `@owned`). It must
            // sit immediately before a local `var`/`final`.
            TokKind::Meta(name) if name == "delete" => {
                self.bump();
                if !(self.at_kw(Kw::Var) || self.at_kw(Kw::Final)) {
                    return Err(
                        self.err("@delete must immediately precede a local `var` declaration")
                    );
                }
                self.parse_var_stmt(line, true)
            }
            TokKind::Kw(Kw::If) => self.parse_if(),
            TokKind::Kw(Kw::For) => self.parse_for(),
            TokKind::Kw(Kw::While) => {
                self.bump();
                self.expect_sym(Sym::LParen)?;
                let cond = self.parse_expr()?;
                self.expect_sym(Sym::RParen)?;
                let body = Box::new(self.parse_stmt()?);
                Ok(Stmt::While {
                    cond,
                    body,
                    do_while: false,
                    line,
                })
            }
            TokKind::Kw(Kw::Do) => {
                self.bump();
                let body = Box::new(self.parse_stmt()?);
                self.expect_sym_kw(Kw::While)?;
                self.expect_sym(Sym::LParen)?;
                let cond = self.parse_expr()?;
                self.expect_sym(Sym::RParen)?;
                self.eat_sym(Sym::Semi);
                Ok(Stmt::While {
                    cond,
                    body,
                    do_while: true,
                    line,
                })
            }
            TokKind::Kw(Kw::Switch) => self.parse_switch(),
            TokKind::Kw(Kw::Return) => {
                self.bump();
                let e = if self.at_sym(Sym::Semi) || self.at_sym(Sym::RBrace) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Return(e, line))
            }
            TokKind::Kw(Kw::Break) => {
                self.bump();
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Break)
            }
            TokKind::Kw(Kw::Continue) => {
                self.bump();
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Continue)
            }
            TokKind::Kw(Kw::Throw) => {
                self.bump();
                let e = self.parse_expr()?;
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Throw(e, line))
            }
            TokKind::Kw(Kw::Try) => self.parse_try(line),
            TokKind::Sym(Sym::LBrace) => Ok(Stmt::Block(self.parse_block()?)),
            // Statement-level `@:cppFileCode('...')` injects verbatim C++ here.
            TokKind::Meta(name) if name == "cppFileCode" => {
                self.bump();
                let args = if self.at_sym(Sym::LParen) {
                    self.parse_meta_args()?
                } else {
                    Vec::new()
                };
                let code = args.into_iter().next().unwrap_or_default();
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Verbatim { code, line })
            }
            // Statement-level `@:include("X")` emits an `#include` directive at
            // this point (so it can sit inside a `#if`/`#end` block). Angle-form
            // (`<...>`) is emitted unquoted; everything else is quoted, matching
            // the header include convention.
            TokKind::Meta(name) if name == "include" => {
                self.bump();
                let args = if self.at_sym(Sym::LParen) {
                    self.parse_meta_args()?
                } else {
                    Vec::new()
                };
                let path = args.into_iter().next().unwrap_or_default();
                self.eat_sym(Sym::Semi);
                let code = if path.starts_with('<') {
                    format!("#include {path}")
                } else {
                    format!("#include \"{path}\"")
                };
                Ok(Stmt::Verbatim { code, line })
            }
            // Conditional-compilation directives, repurposed as a front end for
            // the C++ preprocessor. A bare flag keeps the classic spelling
            // (`#if FLAG` → `#ifdef FLAG`); a boolean condition over flags
            // (`!`, `&&`, `||`, parentheses) maps each flag through `defined(…)`
            // (`#if (A && !B)` → `#if (defined(A) && !defined(B))`). Anything else
            // (version comparisons, values) raises an error, per the "raise, do
            // not guess" rule. C++98 has no `#elifdef`, so `#elseif` lowers via
            // `#elif defined(...)`.
            TokKind::Pp(kind, cond) => {
                self.bump();
                let code = match kind {
                    PpKind::If => match self.bare_flag(&cond) {
                        Ok(flag) => format!("#ifdef {flag}"),
                        Err(_) => format!("#if {}", self.pp_condition(&cond)?),
                    },
                    PpKind::ElseIf => match self.bare_flag(&cond) {
                        Ok(flag) => format!("#elif defined({flag})"),
                        Err(_) => format!("#elif {}", self.pp_condition(&cond)?),
                    },
                    PpKind::Else => "#else".to_string(),
                    PpKind::End => "#endif".to_string(),
                };
                Ok(Stmt::Verbatim { code, line })
            }
            _ => {
                let e = self.parse_expr()?;
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Expr(e, line))
            }
        }
    }

    pub(super) fn parse_if(&mut self) -> PResult<Stmt> {
        let line = self.line();
        self.expect_sym_kw(Kw::If)?;
        self.expect_sym(Sym::LParen)?;
        let cond = self.parse_expr()?;
        self.expect_sym(Sym::RParen)?;
        let then = Box::new(self.parse_stmt()?);
        self.eat_sym(Sym::Semi);
        let els = if self.eat_kw(Kw::Else) {
            Some(Box::new(self.parse_stmt()?))
        } else {
            None
        };
        Ok(Stmt::If {
            cond,
            then,
            els,
            line,
        })
    }

    /// An `if`/`else` reached in expression position (`var x = if (c) a else b`, a
    /// `return if (…) …`, or an array-comprehension body). Each branch is a block
    /// (`{ … }`), a nested `if`, or a plain value expression; codegen desugars the
    /// whole thing to a hoisted temporary like a value `switch`.
    pub(super) fn parse_if_expr(&mut self) -> PResult<Expr> {
        self.expect_sym_kw(Kw::If)?;
        self.expect_sym(Sym::LParen)?;
        let cond = Box::new(self.parse_expr()?);
        self.expect_sym(Sym::RParen)?;
        let then = Box::new(self.parse_branch_expr()?);
        self.eat_sym(Sym::Semi);
        let els = if self.eat_kw(Kw::Else) {
            Some(Box::new(self.parse_branch_expr()?))
        } else {
            None
        };
        Ok(Expr::If { cond, then, els })
    }

    /// A single branch of a value `if`: a `{ … }` block, a chained `else if`, or a
    /// plain value expression.
    pub(super) fn parse_branch_expr(&mut self) -> PResult<Expr> {
        if self.at_sym(Sym::LBrace) {
            Ok(Expr::Block(self.parse_block()?))
        } else if self.at_kw(Kw::If) {
            self.parse_if_expr()
        } else {
            self.parse_expr()
        }
    }

    /// Parse `try <stmt> (catch (name[:Type]) <block>)*`. The structure is captured
    /// so the validation pass can flag it as unsupported with a location (Hatchet
    /// does not transpile exception handling yet).
    pub(super) fn parse_try(&mut self, line: usize) -> PResult<Stmt> {
        self.expect_sym_kw(Kw::Try)?;
        let body = Box::new(self.parse_stmt()?);
        let mut catches = Vec::new();
        while self.at_kw(Kw::Catch) {
            self.bump(); // catch
            self.expect_sym(Sym::LParen)?;
            let name = self.expect_ident()?;
            // The exception type is optional in Haxe 4.2 (`catch (e)`).
            let ty = if self.eat_sym(Sym::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            self.expect_sym(Sym::RParen)?;
            let body = self.parse_block()?;
            catches.push(Catch { name, ty, body });
        }
        Ok(Stmt::Try {
            body,
            catches,
            line,
        })
    }

    pub(super) fn parse_for(&mut self) -> PResult<Stmt> {
        let line = self.line();
        self.expect_sym_kw(Kw::For)?;
        self.expect_sym(Sym::LParen)?;
        let var = self.expect_ident()?;
        // `for (key => value in map)` — the optional value binding.
        let value_var = if self.eat_sym(Sym::FatArrow) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect_sym_kw(Kw::In)?;
        let iter = self.parse_iterable()?;
        self.expect_sym(Sym::RParen)?;
        let body = Box::new(self.parse_stmt()?);
        Ok(Stmt::For {
            var,
            value_var,
            iter,
            body,
            line,
        })
    }

    pub(super) fn parse_iterable(&mut self) -> PResult<Iterable> {
        let start = self.parse_expr()?;
        if self.eat_sym(Sym::DotDotDot) {
            let end = self.parse_expr()?;
            Ok(Iterable::Range(start, end))
        } else {
            Ok(Iterable::Coll(start))
        }
    }

    pub(super) fn parse_switch(&mut self) -> PResult<Stmt> {
        let line = self.line();
        let (subject, cases, default) = self.parse_switch_parts()?;
        Ok(Stmt::Switch {
            subject,
            cases,
            default,
            line,
        })
    }

    /// Parse `switch (subject) { case …: …; default: … }`, shared by the statement
    /// and expression forms. The leading `switch` keyword is current.
    pub(super) fn parse_switch_parts(&mut self) -> PResult<(Expr, Vec<Case>, Option<Vec<Stmt>>)> {
        self.expect_sym_kw(Kw::Switch)?;
        let paren = self.eat_sym(Sym::LParen);
        let subject = self.parse_expr()?;
        if paren {
            self.expect_sym(Sym::RParen)?;
        }
        self.expect_sym(Sym::LBrace)?;
        let mut cases = Vec::new();
        let mut default = None;
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            if self.eat_kw(Kw::Case) {
                let mut patterns = vec![self.parse_expr()?];
                while self.eat_sym(Sym::Comma) {
                    patterns.push(self.parse_expr()?);
                }
                self.expect_sym(Sym::Colon)?;
                let body = self.parse_case_body()?;
                // In pattern position `|` is Haxe's or-pattern — patterns are never
                // evaluated, so it is not a bitwise OR. Flatten `case A | B:` into
                // the same alternatives list as `case A, B:`.
                let patterns: Vec<Expr> =
                    patterns.into_iter().flat_map(flatten_or_pattern).collect();
                // `case _:` is Haxe's wildcard — the spelling of `default`.
                if patterns.iter().any(is_wildcard_pattern) {
                    if default.is_some() {
                        return Err(self.err(
                            "duplicate catch-all: this switch already has a `default:` or `case _:`",
                        ));
                    }
                    default = Some(body);
                } else {
                    cases.push(Case { patterns, body });
                }
            } else if self.eat_kw(Kw::Default) {
                self.expect_sym(Sym::Colon)?;
                if default.is_some() {
                    return Err(self.err(
                        "duplicate catch-all: this switch already has a `default:` or `case _:`",
                    ));
                }
                default = Some(self.parse_case_body()?);
            } else {
                return Err(self.err("expected 'case' or 'default' in switch"));
            }
        }
        self.expect_sym(Sym::RBrace)?;
        Ok((subject, cases, default))
    }

    /// Case bodies run until the next `case`/`default`/`}`. A single brace-block
    /// body is unwrapped into its statements.
    pub(super) fn parse_case_body(&mut self) -> PResult<Vec<Stmt>> {
        if self.at_sym(Sym::LBrace) {
            return self.parse_block();
        }
        let mut body = Vec::new();
        while !self.at_kw(Kw::Case)
            && !self.at_kw(Kw::Default)
            && !self.at_sym(Sym::RBrace)
            && !self.at_eof()
        {
            if self.eat_sym(Sym::Semi) {
                continue;
            }
            body.push(self.parse_stmt()?);
        }
        Ok(body)
    }

}

/// Split a top-level `|` pattern into its alternatives (recursively, so
/// `A | B | C` yields all three). Anything else passes through as-is.
fn flatten_or_pattern(e: Expr) -> Vec<Expr> {
    match e {
        Expr::Binary {
            op: BinOp::BitOr,
            lhs,
            rhs,
        } => {
            let mut out = flatten_or_pattern(*lhs);
            out.extend(flatten_or_pattern(*rhs));
            out
        }
        other => vec![other],
    }
}

/// Is this pattern Haxe's `_` wildcard?
fn is_wildcard_pattern(e: &Expr) -> bool {
    matches!(e, Expr::Ident(n) if n == "_")
}
