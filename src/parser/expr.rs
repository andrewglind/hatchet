//! Expression parsing: precedence-climbing core, postfix/primary, lambdas, literals, comprehensions. Part of the `Parser` impl, split out of `parser.rs`.

use crate::ast::*;
use crate::lexer::*;

use super::{Parser, PResult};

impl<'a> Parser<'a> {
    // ---- expressions ----------------------------------------------------

    pub(super) fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_assign()
    }

    pub(super) fn parse_assign(&mut self) -> PResult<Expr> {
        let lhs = self.parse_ternary()?;
        let op = match self.peek() {
            TokKind::Sym(Sym::Assign) => Some(None),
            TokKind::Sym(Sym::PlusEq) => Some(Some(BinOp::Add)),
            TokKind::Sym(Sym::MinusEq) => Some(Some(BinOp::Sub)),
            TokKind::Sym(Sym::StarEq) => Some(Some(BinOp::Mul)),
            TokKind::Sym(Sym::SlashEq) => Some(Some(BinOp::Div)),
            TokKind::Sym(Sym::PercentEq) => Some(Some(BinOp::Mod)),
            TokKind::Sym(Sym::AmpEq) => Some(Some(BinOp::BitAnd)),
            TokKind::Sym(Sym::PipeEq) => Some(Some(BinOp::BitOr)),
            TokKind::Sym(Sym::CaretEq) => Some(Some(BinOp::BitXor)),
            TokKind::Sym(Sym::ShlEq) => Some(Some(BinOp::Shl)),
            TokKind::Sym(Sym::ShrEq) => Some(Some(BinOp::Shr)),
            TokKind::Sym(Sym::UShrEq) => Some(Some(BinOp::UShr)),
            TokKind::Sym(Sym::QuestionQuestionEq) => {
                // x ??= y  → desugars later; model as assign of a coalesce
                self.bump();
                let value = self.parse_assign()?;
                return Ok(Expr::Assign {
                    op: None,
                    target: Box::new(lhs.clone()),
                    value: Box::new(Expr::NullCoalesce(Box::new(lhs), Box::new(value))),
                });
            }
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let value = self.parse_assign()?;
            Ok(Expr::Assign {
                op,
                target: Box::new(lhs),
                value: Box::new(value),
            })
        } else {
            Ok(lhs)
        }
    }

    pub(super) fn parse_ternary(&mut self) -> PResult<Expr> {
        let cond = self.parse_coalesce()?;
        if self.eat_sym(Sym::Question) {
            let then = self.parse_assign()?;
            self.expect_sym(Sym::Colon)?;
            let els = self.parse_assign()?;
            Ok(Expr::Ternary {
                cond: Box::new(cond),
                then: Box::new(then),
                els: Box::new(els),
            })
        } else {
            Ok(cond)
        }
    }

    pub(super) fn parse_coalesce(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_binary(0)?;
        while self.eat_sym(Sym::QuestionQuestion) {
            let rhs = self.parse_binary(0)?;
            lhs = Expr::NullCoalesce(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    pub(super) fn parse_binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            // Haxe 4.2 `expr is Type`. `is` is a *soft* keyword — it lexes as an
            // identifier so that ordinary variables named `is` keep working — so it
            // is recognised here, in operator position, rather than in the lexer.
            if matches!(self.peek(), TokKind::Ident(n) if n == "is") {
                self.bump();
                let ty = self.parse_type()?;
                lhs = Expr::Is {
                    expr: Box::new(lhs),
                    ty,
                };
                continue;
            }
            let Some((op, prec)) = self.peek_binop() else {
                break;
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.parse_binary(prec + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    pub(super) fn peek_binop(&self) -> Option<(BinOp, u8)> {
        let s = match self.peek() {
            TokKind::Sym(s) => *s,
            _ => return None,
        };
        Some(match s {
            Sym::PipePipe => (BinOp::Or, 1),
            Sym::AmpAmp => (BinOp::And, 2),
            Sym::Pipe => (BinOp::BitOr, 3),
            Sym::Caret => (BinOp::BitXor, 4),
            Sym::Amp => (BinOp::BitAnd, 5),
            Sym::Eq => (BinOp::Eq, 6),
            Sym::Ne => (BinOp::Ne, 6),
            Sym::Lt => (BinOp::Lt, 7),
            Sym::Gt => (BinOp::Gt, 7),
            Sym::Le => (BinOp::Le, 7),
            Sym::Ge => (BinOp::Ge, 7),
            Sym::Shl => (BinOp::Shl, 8),
            Sym::Shr => (BinOp::Shr, 8),
            Sym::UShr => (BinOp::UShr, 8),
            Sym::Plus => (BinOp::Add, 9),
            Sym::Minus => (BinOp::Sub, 9),
            Sym::Star => (BinOp::Mul, 10),
            Sym::Slash => (BinOp::Div, 10),
            Sym::Percent => (BinOp::Mod, 10),
            _ => return None,
        })
    }

    pub(super) fn parse_unary(&mut self) -> PResult<Expr> {
        if self.eat_kw(Kw::Untyped) {
            // `untyped EXPR` — Haxe's typer escape hatch. The operand is a normal
            // expression (still transpiled); `untyped` only marks its type as opaque.
            // Binds as a unary prefix to the following expression. Raw C++ injection
            // is the separate `__cpp__(…)` intrinsic, handled in codegen.
            let expr = self.parse_unary()?;
            return Ok(Expr::Untyped(Box::new(expr)));
        }
        if self.eat_kw(Kw::Cast) {
            // cast(expr, Type) | cast expr
            if self.at_sym(Sym::LParen) {
                // Could be cast(expr, Type) or cast (grouped expr). Peek for a
                // top-level comma to decide.
                if self.paren_has_top_level_comma() {
                    self.expect_sym(Sym::LParen)?;
                    let expr = self.parse_expr()?;
                    self.expect_sym(Sym::Comma)?;
                    let ty = self.parse_type()?;
                    self.expect_sym(Sym::RParen)?;
                    return Ok(Expr::Cast {
                        expr: Box::new(expr),
                        ty: Some(ty),
                    });
                }
            }
            let expr = self.parse_unary()?;
            return Ok(Expr::Cast {
                expr: Box::new(expr),
                ty: None,
            });
        }
        let op = match self.peek() {
            TokKind::Sym(Sym::Minus) => Some(UnOp::Neg),
            TokKind::Sym(Sym::Bang) => Some(UnOp::Not),
            TokKind::Sym(Sym::Tilde) => Some(UnOp::BitNot),
            TokKind::Sym(Sym::PlusPlus) => Some(UnOp::Incr),
            TokKind::Sym(Sym::MinusMinus) => Some(UnOp::Decr),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let expr = self.parse_unary()?;
            return Ok(Expr::Unary {
                op,
                expr: Box::new(expr),
                prefix: true,
            });
        }
        self.parse_postfix()
    }

    pub(super) fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek() {
                TokKind::Sym(Sym::Dot) => {
                    self.bump();
                    let name = self.member_name()?;
                    e = Expr::Field(Box::new(e), name);
                }
                TokKind::Sym(Sym::QuestionDot) => {
                    self.bump();
                    let name = self.member_name()?;
                    e = Expr::SafeField(Box::new(e), name);
                }
                TokKind::Sym(Sym::LParen) => {
                    let args = self.parse_call_args()?;
                    e = Expr::Call(Box::new(e), args);
                }
                TokKind::Sym(Sym::LBracket) => {
                    self.bump();
                    let idx = self.parse_expr()?;
                    self.expect_sym(Sym::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                TokKind::Sym(Sym::PlusPlus) => {
                    self.bump();
                    e = Expr::Unary {
                        op: UnOp::Incr,
                        expr: Box::new(e),
                        prefix: false,
                    };
                }
                TokKind::Sym(Sym::MinusMinus) => {
                    self.bump();
                    e = Expr::Unary {
                        op: UnOp::Decr,
                        expr: Box::new(e),
                        prefix: false,
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// A member name after `.` may be an identifier or a contextual keyword
    /// (e.g. `new` in `Type.new`, or other reserved words used as field names).
    pub(super) fn member_name(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokKind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            TokKind::Kw(Kw::New) => {
                self.bump();
                Ok("new".to_string())
            }
            other => Err(self.err(&format!("expected member name, found {:?}", other))),
        }
    }

    pub(super) fn parse_call_args(&mut self) -> PResult<Vec<Expr>> {
        self.expect_sym(Sym::LParen)?;
        let mut args = Vec::new();
        if self.eat_sym(Sym::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            if self.eat_sym(Sym::Comma) {
                continue;
            }
            break;
        }
        self.expect_sym(Sym::RParen)?;
        Ok(args)
    }

    pub(super) fn parse_primary(&mut self) -> PResult<Expr> {
        match self.peek().clone() {
            TokKind::Int(s) => {
                self.bump();
                Ok(Expr::Int(s))
            }
            TokKind::Float(s) => {
                self.bump();
                Ok(Expr::Float(s))
            }
            TokKind::Str { raw, interpolated } => {
                self.bump();
                Ok(Expr::Str { raw, interpolated })
            }
            TokKind::Regex { pattern, flags } => {
                self.bump();
                Ok(Expr::Regex { pattern, flags })
            }
            TokKind::Kw(Kw::True) => {
                self.bump();
                Ok(Expr::Bool(true))
            }
            TokKind::Kw(Kw::False) => {
                self.bump();
                Ok(Expr::Bool(false))
            }
            TokKind::Kw(Kw::Null) => {
                self.bump();
                Ok(Expr::Null)
            }
            TokKind::Kw(Kw::This) => {
                self.bump();
                Ok(Expr::This)
            }
            TokKind::Kw(Kw::Super) => {
                self.bump();
                Ok(Expr::Super)
            }
            TokKind::Kw(Kw::New) => self.parse_new(),
            TokKind::Kw(Kw::If) => self.parse_if_expr(),
            TokKind::Kw(Kw::Function) => self.parse_anon_function(),
            TokKind::Kw(Kw::Switch) => {
                // `switch` in value position (`var x = switch (e) { … }`): same shape
                // as the statement form, carried as an expression for codegen to
                // desugar into a hoisted temp + statement switch.
                let (subject, cases, default) = self.parse_switch_parts()?;
                Ok(Expr::Switch {
                    subject: Box::new(subject),
                    cases,
                    default,
                })
            }
            TokKind::Ident(name) => {
                // single-parameter lambda `x -> expr`
                if matches!(self.peek2(), TokKind::Sym(Sym::Arrow)) {
                    self.bump();
                    self.bump();
                    let body = self.parse_lambda_body()?;
                    return Ok(Expr::Lambda {
                        params: vec![Param {
                            name,
                            ty: None,
                            optional: false,
                            default: None,
                            rest: false,
                            meta: Vec::new(),
                        }],
                        ret: None,
                        body: Box::new(body),
                    });
                }
                self.bump();
                Ok(Expr::Ident(name))
            }
            TokKind::Sym(Sym::LParen) => self.parse_paren_or_lambda(),
            TokKind::Sym(Sym::LBracket) => self.parse_array_or_comprehension(),
            TokKind::Sym(Sym::LBrace) => self.parse_object_literal(),
            other => Err(self.err(&format!("unexpected token in expression: {:?}", other))),
        }
    }

    pub(super) fn parse_new(&mut self) -> PResult<Expr> {
        self.expect_sym_kw(Kw::New)?;
        let ty = self.parse_type()?;
        let args = self.parse_call_args()?;
        Ok(Expr::New(ty, args))
    }

    /// Anonymous function expression: `function (params) [:Ret] { body }`.
    /// Modelled as a `Lambda` with an explicit return type and block body.
    pub(super) fn parse_anon_function(&mut self) -> PResult<Expr> {
        self.expect_sym_kw(Kw::Function)?;
        // an optional local name is allowed in Haxe; ignore it
        if let TokKind::Ident(_) = self.peek() {
            self.bump();
        }
        let params = self.parse_params()?;
        let ret = if self.eat_sym(Sym::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_lambda_body()?;
        Ok(Expr::Lambda {
            params,
            ret,
            body: Box::new(body),
        })
    }

    pub(super) fn parse_lambda_body(&mut self) -> PResult<LambdaBody> {
        // `-> { x: ... }` is an object literal; `-> { stmt; ... }` is a block.
        if self.at_sym(Sym::LBrace) && !self.looks_like_object_literal() {
            Ok(LambdaBody::Block(self.parse_block()?))
        } else {
            Ok(LambdaBody::Expr(self.parse_expr()?))
        }
    }

    /// With the cursor on `{`, decide whether it begins an object literal:
    /// `{}`, or `{ key : ... }` where `key` is an identifier or string.
    pub(super) fn looks_like_object_literal(&self) -> bool {
        if !self.at_sym(Sym::LBrace) {
            return false;
        }
        match self.toks.get(self.pos + 1).map(|t| &t.kind) {
            Some(TokKind::Sym(Sym::RBrace)) => true,
            Some(TokKind::Ident(_) | TokKind::Str { .. }) => {
                matches!(
                    self.toks.get(self.pos + 2).map(|t| &t.kind),
                    Some(TokKind::Sym(Sym::Colon))
                )
            }
            _ => false,
        }
    }

    /// `(expr)`, `(expr : Type)`, or a lambda `(params) -> body`.
    pub(super) fn parse_paren_or_lambda(&mut self) -> PResult<Expr> {
        if self.parens_introduce_lambda() {
            let params = self.parse_params()?;
            self.expect_sym(Sym::Arrow)?;
            let body = self.parse_lambda_body()?;
            return Ok(Expr::Lambda {
                params,
                ret: None,
                body: Box::new(body),
            });
        }
        self.expect_sym(Sym::LParen)?;
        let e = self.parse_expr()?;
        if self.eat_sym(Sym::Colon) {
            let ty = self.parse_type()?;
            self.expect_sym(Sym::RParen)?;
            return Ok(Expr::TypeCheck {
                expr: Box::new(e),
                ty,
            });
        }
        self.expect_sym(Sym::RParen)?;
        Ok(Expr::Paren(Box::new(e)))
    }

    pub(super) fn parse_array_or_comprehension(&mut self) -> PResult<Expr> {
        self.expect_sym(Sym::LBracket)?;
        if self.at_kw(Kw::For) {
            return self.parse_comprehension_tail();
        }
        if self.eat_sym(Sym::RBracket) {
            return Ok(Expr::ArrayLit(Vec::new()));
        }
        let first = self.parse_expr()?;
        if self.eat_sym(Sym::FatArrow) {
            // map literal
            let v = self.parse_expr()?;
            let mut entries = vec![(first, v)];
            while self.eat_sym(Sym::Comma) {
                if self.at_sym(Sym::RBracket) {
                    break;
                }
                let k = self.parse_expr()?;
                self.expect_sym(Sym::FatArrow)?;
                let val = self.parse_expr()?;
                entries.push((k, val));
            }
            self.expect_sym(Sym::RBracket)?;
            return Ok(Expr::MapLit(entries));
        }
        let mut elems = vec![first];
        while self.eat_sym(Sym::Comma) {
            if self.at_sym(Sym::RBracket) {
                break;
            }
            elems.push(self.parse_expr()?);
        }
        self.expect_sym(Sym::RBracket)?;
        Ok(Expr::ArrayLit(elems))
    }

    /// After consuming `[`, parse `for (v in iter) [if (g)] body]`.
    pub(super) fn parse_comprehension_tail(&mut self) -> PResult<Expr> {
        self.expect_sym_kw(Kw::For)?;
        self.expect_sym(Sym::LParen)?;
        let var = self.expect_ident()?;
        // `[for (key => value in iter) …]` — the optional key-value binding (this
        // `=>` precedes `in`, distinct from a `=>` map-comprehension body).
        let value_var = if self.eat_sym(Sym::FatArrow) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect_sym_kw(Kw::In)?;
        let iter = self.parse_iterable()?;
        self.expect_sym(Sym::RParen)?;
        // A leading `if (cond) <body>` with no `else` is a *filter* — the element is
        // produced only when `cond` holds. With an `else`, the `if` is an ordinary
        // value expression (every iteration yields a value), so it stays the body.
        let (guard, key_or_val) = if self.at_kw(Kw::If) {
            match self.parse_if_expr()? {
                Expr::If {
                    cond,
                    then,
                    els: None,
                } => (Some(cond), *then),
                other => (None, other),
            }
        } else {
            (None, self.parse_expr()?)
        };
        let body = if self.eat_sym(Sym::FatArrow) {
            let v = self.parse_expr()?;
            ComprBody::KeyValue(Box::new(key_or_val), Box::new(v))
        } else {
            ComprBody::Value(Box::new(key_or_val))
        };
        self.expect_sym(Sym::RBracket)?;
        Ok(Expr::Comprehension {
            var,
            value_var,
            iter: Box::new(iter),
            guard,
            body,
        })
    }

    pub(super) fn parse_object_literal(&mut self) -> PResult<Expr> {
        self.expect_sym(Sym::LBrace)?;
        let mut fields = Vec::new();
        if self.eat_sym(Sym::RBrace) {
            return Ok(Expr::ObjectLit(fields));
        }
        loop {
            let key = self.field_key()?;
            self.expect_sym(Sym::Colon)?;
            let value = self.parse_expr()?;
            fields.push((key, value));
            if self.eat_sym(Sym::Comma) {
                if self.at_sym(Sym::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(Expr::ObjectLit(fields))
    }

    // ---- lookahead helpers ---------------------------------------------

    /// Does the `(` at the cursor enclose a top-level `,` before its matching `)`?
    pub(super) fn paren_has_top_level_comma(&self) -> bool {
        let mut depth = 0i32;
        let mut i = self.pos;
        // expects cursor at '('
        while i < self.toks.len() {
            match &self.toks[i].kind {
                TokKind::Sym(Sym::LParen) => depth += 1,
                TokKind::Sym(Sym::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                }
                TokKind::Sym(Sym::Comma) if depth == 1 => return true,
                TokKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Is the `(` at the cursor the start of a lambda parameter list, i.e. its
    /// matching `)` is immediately followed by `->`?
    pub(super) fn parens_introduce_lambda(&self) -> bool {
        let mut depth = 0i32;
        let mut i = self.pos;
        while i < self.toks.len() {
            match &self.toks[i].kind {
                TokKind::Sym(Sym::LParen) => depth += 1,
                TokKind::Sym(Sym::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.toks.get(i + 1).map(|t| &t.kind),
                            Some(TokKind::Sym(Sym::Arrow))
                        );
                    }
                }
                TokKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }
}
