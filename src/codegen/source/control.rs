//! `switch` and `if`/`switch`-expression lowering for `BodyGen`.
//! Split out of `stmt.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    pub(super) fn gen_switch(
        &mut self,
        subject: &Expr,
        cases: &[Case],
        default: Option<&[Stmt]>,
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        let (subj, sty) = self.gen_expr(subject);
        // The subject may hoist statements (a call, a comprehension); emit them
        // before the switch rather than leaving them for the next statement.
        self.flush(out);
        // A subject that cannot be a C++ `case` label — a `String`, or a
        // floating-point value such as a non-integral `enum abstract` — lowers to an
        // `if`/`else if` chain comparing the subject (hoisted once) against each
        // case's pattern(s) with `==`.
        if matches!(sty.base.as_str(), "std::string" | "float" | "double") {
            let spell = self.decl_spelling(&sty);
            self.gen_equality_switch(&subj, &spell, cases, default, ind, out);
            return;
        }
        // An ADT subject (a tagged value class) dispatches on its `kind`; a
        // destructuring case (`case Add(a, b):`) binds payload fields to locals.
        // The subject is hoisted into a local unless it is already a bare name,
        // so payload reads never re-evaluate a side-effecting expression.
        let adt = sty
            .info
            .as_ref()
            .and_then(|i| self.adt_enum(i).map(|e| (i.clone(), e)));
        let (switch_subj, sv, acc) = match &adt {
            Some(_) => {
                let acc = if sty.is_ptr { "->" } else { "." };
                let sv = if matches!(subject, Expr::Ident(_)) {
                    subj.clone()
                } else {
                    let tmp = self.fresh("subj");
                    let spell = self.decl_spelling(&sty);
                    let _ = writeln!(out, "{t}{spell} {tmp} = {subj};");
                    tmp
                };
                (format!("{sv}{acc}kind"), sv, acc)
            }
            None => (subj.clone(), subj.clone(), "."),
        };
        // Haxe `switch` has no break semantics: a `break` in a case body exits
        // the enclosing *loop*. A bare C++ `break` inside the generated switch
        // would exit only the switch — so when a case body contains a loop-bound
        // break, hoist a flag, set it at the break, and re-break after the
        // switch. (Chained when this switch sits in an outer switch's case: the
        // post-check then sets the outer flag instead of breaking bare.)
        let needs_break_flag = self.loop_depth > 0
            && (cases.iter().any(|c| stmts_contain_loop_break(&c.body))
                || default.is_some_and(stmts_contain_loop_break));
        let break_flag = if needs_break_flag {
            let f = self.fresh("brk");
            let _ = writeln!(out, "{t}bool {f} = false;");
            Some(f)
        } else {
            None
        };
        let outer_flag = std::mem::replace(&mut self.switch_break_flag, break_flag.clone());
        let _ = writeln!(out, "{t}switch ({switch_subj}) {{");
        for case in cases {
            for pat in &case.patterns {
                // enum case labels need the enum-qualified constant; a
                // destructuring pattern labels with its variant's tag
                let label = match (&adt, pat) {
                    (Some((info, _)), Expr::Call(callee, _)) => {
                        self.enum_constant(info, &call_pattern_variant(callee))
                    }
                    _ => self.case_label(pat, &sty),
                };
                let _ = writeln!(out, "{t}\tcase {label}:");
            }
            let _ = writeln!(out, "{t}\t{{");
            self.push_scope();
            // Destructuring bindings: `case Add(a, b):` declares one typed local
            // per non-`_` capture, read from the variant's payload fields.
            // (Validation guarantees a destructuring pattern is its case's only
            // pattern, so the bindings are unambiguous.)
            if let Some((_, e)) = &adt {
                if let (1, Some(Expr::Call(callee, pargs))) =
                    (case.patterns.len(), case.patterns.first())
                {
                    let vname = call_pattern_variant(callee);
                    if let Some(v) = e.variants.iter().find(|v| v.name == vname) {
                        for (i, parg) in pargs.iter().enumerate() {
                            let Expr::Ident(bind) = parg else { continue };
                            if bind == "_" {
                                continue;
                            }
                            let Some(p) = v.params.get(i) else { continue };
                            let pty = p.ty.as_ref().map(|t| self.ty_of(t)).unwrap_or_default();
                            let spell = self.decl_spelling(&pty);
                            let _ = writeln!(
                                out,
                                "{t}\t\t{spell} {bind} = {sv}{acc}{vname}_{};",
                                p.name
                            );
                            self.define_local(bind, pty);
                        }
                    }
                }
            }
            for s in &case.body {
                self.gen_stmt(s, ind + 2, out);
            }
            self.pop_scope();
            let _ = writeln!(out, "{t}\t}}");
            let _ = writeln!(out, "{t}\tbreak;");
        }
        if let Some(d) = default {
            let _ = writeln!(out, "{t}\tdefault:");
            let _ = writeln!(out, "{t}\t{{");
            self.push_scope();
            for s in d {
                self.gen_stmt(s, ind + 2, out);
            }
            self.pop_scope();
            let _ = writeln!(out, "{t}\t}}");
            let _ = writeln!(out, "{t}\tbreak;");
        }
        let _ = writeln!(out, "{t}}}");
        // Re-raise a routed loop break: bare when the loop is the next enclosing
        // construct; through the outer switch's flag when this switch sits inside
        // another switch's case body (whose own post-check then breaks the loop).
        self.switch_break_flag = outer_flag;
        if let Some(f) = &break_flag {
            match &self.switch_break_flag {
                Some(of) => {
                    let _ = writeln!(out, "{t}if ({f}) {{ {of} = true; break; }}");
                }
                None => {
                    let _ = writeln!(out, "{t}if ({f}) break;");
                }
            }
        }
    }

    /// Lower a `switch` on a non-integral subject (a `String`, or a float-backed
    /// `enum abstract`) to an `if`/`else if`/`else` chain. The subject is hoisted
    /// into one local of type `spell` (so a side-effecting subject runs once), and
    /// each case's patterns become an OR-ed equality test. Haxe cases do not fall
    /// through, which the chain matches naturally.
    pub(super) fn gen_equality_switch(
        &mut self,
        subj: &str,
        spell: &str,
        cases: &[Case],
        default: Option<&[Stmt]>,
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        let sw = self.fresh("sw");
        let _ = writeln!(out, "{t}{spell} {sw} = {subj};");
        let mut started = false;
        for case in cases {
            // String case patterns are constants (literals), so they hoist nothing.
            let cond = case
                .patterns
                .iter()
                .map(|p| format!("{sw} == {}", self.gen_expr(p).0))
                .collect::<Vec<_>>()
                .join(" || ");
            let kw = if started { "} else if" } else { "if" };
            let _ = writeln!(out, "{t}{kw} ({cond}) {{");
            self.push_scope();
            for s in &case.body {
                self.gen_stmt(s, ind + 1, out);
            }
            self.pop_scope();
            started = true;
        }
        match default {
            Some(d) if started => {
                let _ = writeln!(out, "{t}}} else {{");
                self.push_scope();
                for s in d {
                    self.gen_stmt(s, ind + 1, out);
                }
                self.pop_scope();
                let _ = writeln!(out, "{t}}}");
            }
            // A `default` with no preceding `case` is just an unconditional block.
            Some(d) => {
                let _ = writeln!(out, "{t}{{");
                self.push_scope();
                for s in d {
                    self.gen_stmt(s, ind + 1, out);
                }
                self.pop_scope();
                let _ = writeln!(out, "{t}}}");
            }
            None if started => {
                let _ = writeln!(out, "{t}}}");
            }
            None => {}
        }
    }

    /// Desugar a value-position `switch` into a hoisted temporary plus a statement
    /// `switch` (reusing the integer/string/enum lowering): each arm assigns its
    /// trailing value expression to the temp, and the whole thing evaluates to it.
    pub(super) fn gen_switch_expr(
        &mut self,
        subject: &Expr,
        cases: &[Case],
        default: Option<&[Stmt]>,
    ) -> (String, Ty) {
        let tmp = self.fresh("swx");
        // The temporary's type is the switch's *expected* type when the context
        // supplies one (a typed `return`, `var x:T = …`, or assignment) — that is
        // the common type the arms unify to (e.g. a base class when the arms are
        // different subclasses). Only when there is no contextual type do we fall
        // back to inferring from the first arm, which would otherwise mistype a
        // polymorphic switch as its first subclass.
        let ty = match &self.expected {
            Some(t) if !t.base.is_empty() => t.clone(),
            _ => self.switch_expr_ty(cases, default),
        };
        let spell = self.decl_spelling(&ty);
        // Rewrite each arm so its trailing value expression assigns to the temp.
        let cases2: Vec<Case> = cases
            .iter()
            .map(|c| Case {
                patterns: c.patterns.clone(),
                body: assign_last_to(&c.body, &tmp),
            })
            .collect();
        let default2: Option<Vec<Stmt>> = default.map(|d| assign_last_to(d, &tmp));

        // Build the statement switch in an isolated prelude context so its internal
        // flushing does not move unrelated, already-pending prelude into the middle.
        let saved = std::mem::take(&mut self.prelude);
        let ind = self.prelude_ind;
        let mut buf = String::new();
        self.gen_switch(subject, &cases2, default2.as_deref(), ind, &mut buf);
        // `gen_switch` flushes its own prelude into `buf`; nothing should remain.
        let leftover = std::mem::replace(&mut self.prelude, saved);
        let t = "\t".repeat(ind);
        self.prelude.push_str(&format!("{t}{spell} {tmp};\n"));
        self.prelude.push_str(&leftover);
        self.prelude.push_str(&buf);
        (tmp, ty)
    }

    /// A value-position `if`/`else`, desugared like a value `switch`: a hoisted
    /// temporary, then a statement `if` whose branches assign their trailing value
    /// to it. `else if` chains and `{ … }` blocks nest naturally.
    pub(super) fn gen_if_expr(
        &mut self,
        cond: &Expr,
        then: &Expr,
        els: Option<&Expr>,
    ) -> (String, Ty) {
        let tmp = self.fresh("ifx");
        // The temp's type is the contextual expected type when present (the common
        // type the branches unify to), else inferred from the first branch's value.
        let ty = match &self.expected {
            Some(t) if !t.base.is_empty() => t.clone(),
            _ => self.if_expr_ty(then, els),
        };
        let spell = self.decl_spelling(&ty);
        let stmt = Stmt::If {
            cond: cond.clone(),
            then: Box::new(branch_assign_to(then, &tmp)),
            els: els.map(|e| Box::new(branch_assign_to(e, &tmp))),
            line: self.current_line,
        };
        // Build the statement `if` in an isolated prelude context (mirrors
        // `gen_switch_expr`) so its flushing does not reorder pending prelude.
        let saved = std::mem::take(&mut self.prelude);
        let ind = self.prelude_ind;
        let mut buf = String::new();
        self.gen_stmt(&stmt, ind, &mut buf);
        let leftover = std::mem::replace(&mut self.prelude, saved);
        let t = "\t".repeat(ind);
        self.prelude.push_str(&format!("{t}{spell} {tmp};\n"));
        self.prelude.push_str(&leftover);
        self.prelude.push_str(&buf);
        (tmp, ty)
    }

    /// The result type of a value-position `if`: the type of the first branch's
    /// trailing value expression, inferred without emitting.
    fn if_expr_ty(&mut self, then: &Expr, els: Option<&Expr>) -> Ty {
        let value = branch_value_expr(then)
            .or_else(|| els.and_then(branch_value_expr))
            .cloned();
        match value {
            Some(e) => self.dry_ty(&e),
            None => Ty::default(),
        }
    }

    /// A value-position `{ … }` block: hoist its statements, the trailing value
    /// assigned to a temporary that the expression evaluates to.
    pub(super) fn gen_block_expr(&mut self, stmts: &[Stmt]) -> (String, Ty) {
        let tmp = self.fresh("blk");
        let ty = match &self.expected {
            Some(t) if !t.base.is_empty() => t.clone(),
            _ => match case_value_expr(stmts) {
                Some(e) => self.dry_ty(&e.clone()),
                None => Ty::default(),
            },
        };
        let spell = self.decl_spelling(&ty);
        let body = Stmt::Block(assign_last_to(stmts, &tmp));
        let saved = std::mem::take(&mut self.prelude);
        let ind = self.prelude_ind;
        let mut buf = String::new();
        self.gen_stmt(&body, ind, &mut buf);
        let leftover = std::mem::replace(&mut self.prelude, saved);
        let t = "\t".repeat(ind);
        self.prelude.push_str(&format!("{t}{spell} {tmp};\n"));
        self.prelude.push_str(&leftover);
        self.prelude.push_str(&buf);
        (tmp, ty)
    }

    /// The result type of a value-position `switch`: the type of the first arm's
    /// (or the default's) trailing value expression, inferred without emitting.
    pub(super) fn switch_expr_ty(&mut self, cases: &[Case], default: Option<&[Stmt]>) -> Ty {
        let value = cases
            .iter()
            .find_map(|c| case_value_expr(&c.body))
            .or_else(|| default.and_then(case_value_expr))
            .cloned();
        match value {
            Some(e) => self.dry_ty(&e),
            None => Ty::default(),
        }
    }

    /// Infer an expression's type without emitting any code: the throwaway
    /// generation's prelude is discarded (a value expression has no side effects we
    /// need to keep). The fresh-name counter may advance, which is harmless.
    pub(super) fn dry_ty(&mut self, e: &Expr) -> Ty {
        let saved = std::mem::take(&mut self.prelude);
        let (_, ty) = self.gen_expr(e);
        self.prelude = saved;
        ty
    }

    pub(super) fn case_label(&mut self, pat: &Expr, subj_ty: &Ty) -> String {
        if let Expr::Ident(name) = pat {
            // bare enum variant → qualify with the subject's enum type
            if let Some(info) = &subj_ty.info {
                if info.kind == TypeKind::Enum {
                    return self.enum_constant(info, name).to_string();
                }
            }
        }
        // Qualified enum variant (`EnumType.Variant`): a `case` label is always
        // the tag — even for an ADT, whose *value*-position spelling is the
        // factory call (`Op::Halt()`), which cannot label a case.
        if let Expr::Field(recv, name) = pat {
            if let Expr::Ident(tname) = &**recv {
                if let Some(info) = self
                    .prog
                    .resolve_type(std::slice::from_ref(tname), self.mi)
                    .cloned()
                {
                    if info.kind == TypeKind::Enum {
                        return self.enum_constant(&info, name);
                    }
                }
            }
        }
        self.gen_expr(pat).0
    }
}
