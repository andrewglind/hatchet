//! Type-expression parsing (`parse_type` and friends). Part of the `Parser` impl, split out of `parser.rs`.

use crate::ast::*;
use crate::lexer::*;

use super::{Parser, PResult};

impl<'a> Parser<'a> {
    // ---- types ----------------------------------------------------------

    pub(super) fn parse_type(&mut self) -> PResult<Type> {
        // Parenthesized function type `(A, B) -> R` (modern Haxe). Parameters may
        // carry a `name:` label (`(a:Int, b:Int) -> Int`), which is ignored — only
        // the types matter for the C++ signature.
        if self.at_sym(Sym::LParen) {
            self.bump();
            let mut params = Vec::new();
            if !self.at_sym(Sym::RParen) {
                loop {
                    if matches!(self.peek(), TokKind::Ident(_))
                        && matches!(self.peek2(), TokKind::Sym(Sym::Colon))
                    {
                        self.bump(); // label
                        self.bump(); // ':'
                    }
                    params.push(self.parse_type()?);
                    if self.eat_sym(Sym::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect_sym(Sym::RParen)?;
            self.expect_sym(Sym::Arrow)?;
            let ret = self.parse_type()?;
            return Ok(Type::Func {
                params,
                ret: Box::new(ret),
            });
        }
        let first = self.parse_type_atom()?;
        // function type `A -> B`
        if self.at_sym(Sym::Arrow) {
            let mut params = vec![first];
            while self.eat_sym(Sym::Arrow) {
                params.push(self.parse_type_atom()?);
            }
            let ret = params.pop().unwrap();
            return Ok(Type::Func {
                params,
                ret: Box::new(ret),
            });
        }
        Ok(first)
    }

    pub(super) fn parse_type_atom(&mut self) -> PResult<Type> {
        let optional = self.eat_sym(Sym::Question);
        let line = self.line();
        if self.at_sym(Sym::LBrace) {
            let fields = self.parse_struct_fields()?;
            return Ok(Type::Anon(fields));
        }
        let mut path = vec![self.expect_ident()?];
        while self.at_sym(Sym::Dot) && matches!(self.peek2(), TokKind::Ident(_)) {
            self.bump();
            path.push(self.expect_ident()?);
        }
        let mut params = Vec::new();
        if self.eat_sym(Sym::Lt) {
            loop {
                params.push(self.parse_type()?);
                if self.eat_sym(Sym::Comma) {
                    continue;
                }
                break;
            }
            self.expect_type_gt()?;
        }
        Ok(Type::Named {
            path,
            params,
            optional,
            line,
        })
    }

    pub(super) fn parse_struct_fields(&mut self) -> PResult<Vec<StructField>> {
        self.expect_sym(Sym::LBrace)?;
        let mut fields = Vec::new();
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            // Both short notation (`name:Type,`) and class notation
            // (`var name:Type;`, `var ?name:Type;`) are accepted.
            self.eat_kw(Kw::Var);
            let optional = self.eat_sym(Sym::Question);
            let name = self.field_key()?;
            self.expect_sym(Sym::Colon)?;
            let ty = self.parse_type()?;
            fields.push(StructField { name, optional, ty });
            if !self.eat_sym(Sym::Comma) {
                self.eat_sym(Sym::Semi);
            }
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(fields)
    }

    pub(super) fn field_key(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokKind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            TokKind::Str { raw, .. } => {
                self.bump();
                Ok(raw)
            }
            other => Err(self.err(&format!("expected field name, found {:?}", other))),
        }
    }

    /// Consume a `>` that closes a type parameter list, splitting `>>`/`>=`/`>>=`
    /// as needed (the lexer greedily merges them).
    pub(super) fn expect_type_gt(&mut self) -> PResult<()> {
        match self.peek() {
            TokKind::Sym(Sym::Gt) => {
                self.bump();
                Ok(())
            }
            TokKind::Sym(Sym::Shr) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::Gt);
                Ok(())
            }
            TokKind::Sym(Sym::UShr) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::Shr);
                Ok(())
            }
            TokKind::Sym(Sym::Ge) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::Assign);
                Ok(())
            }
            TokKind::Sym(Sym::ShrEq) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::Ge);
                Ok(())
            }
            TokKind::Sym(Sym::UShrEq) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::ShrEq);
                Ok(())
            }
            other => Err(self.err(&format!("expected '>', found {:?}", other))),
        }
    }

}
