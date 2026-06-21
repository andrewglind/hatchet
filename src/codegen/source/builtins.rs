//! Standard-library lowering for `BodyGen`: Haxe container/string/`Std`/`Math`
//! calls and string building → C++98. Split out of `source.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    pub(super) fn container_call(
        &mut self,
        rcode: &str,
        _rty: &Ty,
        method: &str,
        args: &[Expr],
    ) -> Option<(String, Ty)> {
        match method {
            "push" => {
                // push([]) pushes a default-constructed element
                if matches!(args.first(), Some(Expr::ArrayLit(v)) if v.is_empty()) {
                    let spell = self.decl_spelling(&self.element_ty(_rty));
                    return Some((format!("{rcode}.push_back({spell}())"), Ty::default()));
                }
                // The pushed value is typed by the element type (so an object
                // literal becomes a temp of the element struct, not an anon one).
                let elem = self.elem_member_ty(_rty);
                let a = self.gen_args_typed(args, &[Some(elem)], false);
                Some((format!("{rcode}.push_back({a})"), Ty::default()))
            }
            "insert" => {
                let pos = self.gen_expr(&args[0]).0;
                let elem = self.elem_member_ty(_rty);
                let val = self.gen_args_typed(&args[1..2], &[Some(elem)], false);
                Some((
                    format!("{rcode}.insert({rcode}.begin() + {pos}, {val})"),
                    Ty::default(),
                ))
            }
            "pop" => {
                // Haxe `Array.pop()` removes AND returns the last element; C++
                // `back()` only *reads* it and `pop_back()` returns `void`, so capture
                // the value into a temp first, then shrink the vector.
                let elem = self.element_ty(_rty);
                let spell = self.decl_spelling(&elem);
                let tmp = self.fresh("pop");
                let t = "\t".repeat(self.prelude_ind);
                self.prelude
                    .push_str(&format!("{t}{spell} {tmp} = {rcode}.back();\n"));
                self.prelude.push_str(&format!("{t}{rcode}.pop_back();\n"));
                Some((tmp, elem))
            }
            // Map.get(k) → m[k]; element type is the map's value type.
            "get" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                Some((format!("{rcode}[{k}]"), self.map_value_ty(_rty)))
            }
            "exists" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                Some((
                    format!("({rcode}.find({k}) != {rcode}.end())"),
                    Ty {
                        base: "bool".into(),
                        ..Default::default()
                    },
                ))
            }
            // Array.map(f) → a hoisted std::vector populated by a loop that applies
            // the lambda to each element (the Map-comprehension + Lambda composition).
            "map" if matches!(args.first(), Some(Expr::Lambda { .. })) => {
                if let Some(Expr::Lambda { params, body, .. }) = args.first() {
                    Some(self.gen_array_map(rcode, _rty, params, body))
                } else {
                    None
                }
            }
            // Array.filter(p) → a hoisted std::vector of the elements the predicate
            // keeps (same element type as the receiver).
            "filter" if matches!(args.first(), Some(Expr::Lambda { .. })) => {
                if let Some(Expr::Lambda { params, body, .. }) = args.first() {
                    Some(self.gen_array_filter(rcode, _rty, params, body))
                } else {
                    None
                }
            }
            // Array.sort(cmp) → an in-place insertion sort (no `<algorithm>`) driven
            // by the comparator lambda; mutates the receiver, returns Void.
            "sort" if matches!(args.first(), Some(Expr::Lambda { .. })) => {
                if let Some(Expr::Lambda { params, body, .. }) = args.first() {
                    Some(self.gen_array_sort(rcode, _rty, params, body))
                } else {
                    None
                }
            }

            // ---- Map (std::map) methods ------------------------------------
            // `Map.set(k, v)` → `m[k] = v`.
            "set" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                let v = self.gen_expr(&args[1]).0;
                Some((format!("{rcode}[{k}] = {v}"), Ty::default()))
            }
            // `Map.remove(k)` → `m.erase(k)`; Haxe returns Bool (was it present?).
            "remove" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                Some((format!("({rcode}.erase({k}) != 0)"), bool_ty()))
            }
            // `Map.keys()` → a hoisted std::vector<K> of the keys (iterable via the
            // ordinary collection `for`).
            "keys" if rcode_is_map(_rty) => {
                let kspell = self.decl_spelling(&self.map_key_ty(_rty));
                let it = self.fresh("it");
                let acc = self.fresh("keys");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::vector<{kspell} > {acc};");
                let _ = writeln!(
                    pre,
                    "{t}for ({}::iterator {it} = {rcode}.begin(); {it} != {rcode}.end(); ++{it}) {{ {acc}.push_back({it}->first); }}",
                    _rty.base
                );
                self.prelude.push_str(&pre);
                Some((
                    acc,
                    Ty {
                        base: format!("std::vector<{kspell} >"),
                        ..Default::default()
                    },
                ))
            }

            // ---- Array (std::vector) methods -------------------------------
            // `Array.contains(x)` → linear scan, no <algorithm> dependency.
            "contains" => {
                let x = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let has = self.fresh("has");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}bool {has} = false;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({rcode}[{i}] == {x}) {{ {has} = true; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((has, bool_ty()))
            }
            // `Array.indexOf(x[, fromIndex])` → first matching index or -1.
            "indexOf" => {
                let x = self.gen_expr(&args[0]).0;
                let start = if args.len() > 1 {
                    self.gen_expr(&args[1]).0
                } else {
                    "0".to_string()
                };
                let i = self.fresh("i");
                let idx = self.fresh("idx");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}int {idx} = -1;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = (size_t)({start}); {i} < {rcode}.size(); ++{i}) {{ if ({rcode}[{i}] == {x}) {{ {idx} = (int){i}; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((idx, int_ty()))
            }
            // `Array.remove(x)` → erase first match; Haxe returns Bool.
            "remove" => {
                let x = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let rem = self.fresh("rem");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}bool {rem} = false;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({rcode}[{i}] == {x}) {{ {rcode}.erase({rcode}.begin() + {i}); {rem} = true; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((rem, bool_ty()))
            }
            // `Array.reverse()` → in-place swap loop (Void).
            "reverse" => {
                let espell = self.decl_spelling(&self.element_ty(_rty));
                let i = self.fresh("i");
                let tmp = self.fresh("tmp");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {rcode}.size() / 2; ++{i}) {{ {espell} {tmp} = {rcode}[{i}]; {rcode}[{i}] = {rcode}[{rcode}.size() - 1 - {i}]; {rcode}[{rcode}.size() - 1 - {i}] = {tmp}; }}"
                );
                self.prelude.push_str(&pre);
                Some(("((void)0)".to_string(), Ty::default()))
            }
            // `Array.copy()` → a shallow copy via the vector copy constructor.
            "copy" => Some((format!("{}({rcode})", _rty.base), _rty.clone())),
            // `Array.join(sep)` → concatenate elements (stringified) with `sep`.
            "join" => {
                let sep = self.gen_expr(&args[0]).0;
                let elem = self.element_ty(_rty);
                let i = self.fresh("i");
                let acc = self.fresh("join");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc};");
                if elem.base == "std::string" {
                    let _ = writeln!(
                        pre,
                        "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({i}) {acc} += {sep}; {acc} += {rcode}[{i}]; }}"
                    );
                } else {
                    let (spec, _) = self.spec_for("", &elem);
                    let buf = self.fresh("buf");
                    let _ = writeln!(
                        pre,
                        "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({i}) {acc} += {sep}; char {buf}[64]; sprintf({buf}, \"{spec}\", {rcode}[{i}]); {acc} += {buf}; }}"
                    );
                }
                self.prelude.push_str(&pre);
                Some((
                    acc,
                    Ty {
                        base: "std::string".into(),
                        ..Default::default()
                    },
                ))
            }
            // `Array.concat(other)` → a new vector: a copy of this with `other`'s
            // elements appended (Haxe returns a fresh array, leaving both operands).
            "concat" => {
                let other = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let acc = self.fresh("cat");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}{} {acc} = {rcode};", _rty.base);
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < ({other}).size(); ++{i}) {{ {acc}.push_back(({other})[{i}]); }}"
                );
                self.prelude.push_str(&pre);
                Some((acc, _rty.clone()))
            }
            // `Array.slice(pos, ?end)` → a new vector of `[pos, end)`; negative
            // indices count from the end, and the range is clamped to the array.
            "slice" => {
                let pos = self.gen_expr(&args[0]).0;
                let end = if args.len() > 1 {
                    Some(self.gen_expr(&args[1]).0)
                } else {
                    None
                };
                let acc = self.fresh("slc");
                let a = self.fresh("a");
                let b = self.fresh("b");
                let i = self.fresh("i");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}{} {acc};", _rty.base);
                let _ = writeln!(pre, "{t}int {a} = (int)({pos}); if ({a} < 0) {a} += (int){rcode}.size(); if ({a} < 0) {a} = 0; if ((size_t){a} > {rcode}.size()) {a} = (int){rcode}.size();");
                match end {
                    Some(end) => {
                        let _ = writeln!(pre, "{t}int {b} = (int)({end}); if ({b} < 0) {b} += (int){rcode}.size(); if ({b} < 0) {b} = 0; if ((size_t){b} > {rcode}.size()) {b} = (int){rcode}.size();");
                    }
                    None => {
                        let _ = writeln!(pre, "{t}int {b} = (int){rcode}.size();");
                    }
                }
                let _ = writeln!(pre, "{t}for (size_t {i} = (size_t){a}; {i} < (size_t){b}; ++{i}) {{ {acc}.push_back({rcode}[{i}]); }}");
                self.prelude.push_str(&pre);
                Some((acc, _rty.clone()))
            }
            // `Array.shift()` → remove and return the first element.
            "shift" => {
                let elem = self.element_ty(_rty);
                let spell = self.decl_spelling(&elem);
                let tmp = self.fresh("shift");
                let t = "\t".repeat(self.prelude_ind);
                self.prelude
                    .push_str(&format!("{t}{spell} {tmp} = {rcode}.front();\n"));
                self.prelude
                    .push_str(&format!("{t}{rcode}.erase({rcode}.begin());\n"));
                Some((tmp, elem))
            }
            // `Array.unshift(x)` → insert `x` at the front (Void).
            "unshift" => {
                let elem = self.elem_member_ty(_rty);
                let val = self.gen_args_typed(args, &[Some(elem)], false);
                Some((
                    format!("{rcode}.insert({rcode}.begin(), {val})"),
                    Ty::default(),
                ))
            }
            // `Array.lastIndexOf(x[, fromIndex])` → last matching index or -1,
            // searching backward from `fromIndex` (default: the last element).
            "lastIndexOf" => {
                let x = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let idx = self.fresh("idx");
                let t = "\t".repeat(self.prelude_ind);
                let start = if args.len() > 1 {
                    let from = self.gen_expr(&args[1]).0;
                    format!("(size_t)({from}) + 1")
                } else {
                    format!("{rcode}.size()")
                };
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}int {idx} = -1;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = {start}; {i}-- > 0; ) {{ if ({i} < {rcode}.size() && {rcode}[{i}] == {x}) {{ {idx} = (int){i}; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((idx, int_ty()))
            }
            _ => None,
        }
    }

    /// Lower a `Map` operation on an `@orderedMap` field to scans over its parallel
    /// key/value vectors (no `std::map`): linear find for `get`/`exists`, find-or-
    /// append for `set`, paired erase for `remove`, the keys vector for `keys`.
    /// Insertion order is preserved because both vectors only ever grow at the end.
    /// Returns `None` for an unrecognised method (the caller then fails loudly).
    pub(super) fn ordered_map_call(
        &mut self,
        om: &OrderedMapRef,
        method: &str,
        args: &[Expr],
    ) -> Option<(String, Ty)> {
        let t = "\t".repeat(self.prelude_ind);
        let keys = om.keys.clone();
        let vals = om.vals.clone();
        match method {
            // get(k) → the matching value, else a default (`NULL` / `V()`).
            "get" => {
                let k = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let out = self.fresh("get");
                let vspell = self.decl_spelling(&om.val_ty);
                let dflt = if om.val_ty.is_ptr {
                    "NULL".to_string()
                } else {
                    format!("{}()", om.val_ty.base)
                };
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}{vspell} {out} = {dflt};");
                let _ = writeln!(pre, "{t}for (size_t {i} = 0; {i} < {keys}.size(); ++{i}) {{ if ({keys}[{i}] == {k}) {{ {out} = {vals}[{i}]; break; }} }}");
                self.prelude.push_str(&pre);
                Some((out, om.val_ty.clone()))
            }
            "exists" => {
                let k = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let has = self.fresh("has");
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}bool {has} = false;");
                let _ = writeln!(pre, "{t}for (size_t {i} = 0; {i} < {keys}.size(); ++{i}) {{ if ({keys}[{i}] == {k}) {{ {has} = true; break; }} }}");
                self.prelude.push_str(&pre);
                Some((has, bool_ty()))
            }
            // set(k, v) → replace in place if the key is present, else append. Void.
            "set" => {
                let k = self.gen_expr(&args[0]).0;
                let v = self.gen_expr(&args[1]).0;
                let i = self.fresh("i");
                let found = self.fresh("found");
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}bool {found} = false;");
                let _ = writeln!(pre, "{t}for (size_t {i} = 0; {i} < {keys}.size(); ++{i}) {{ if ({keys}[{i}] == {k}) {{ {vals}[{i}] = {v}; {found} = true; break; }} }}");
                let _ = writeln!(pre, "{t}if (!{found}) {{ {keys}.push_back({k}); {vals}.push_back({v}); }}");
                self.prelude.push_str(&pre);
                Some((String::new(), Ty::default()))
            }
            // remove(k) → erase from both vectors; Haxe returns Bool (was it present?).
            "remove" => {
                let k = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let rem = self.fresh("rem");
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}bool {rem} = false;");
                let _ = writeln!(pre, "{t}for (size_t {i} = 0; {i} < {keys}.size(); ++{i}) {{ if ({keys}[{i}] == {k}) {{ {keys}.erase({keys}.begin() + {i}); {vals}.erase({vals}.begin() + {i}); {rem} = true; break; }} }}");
                self.prelude.push_str(&pre);
                Some((rem, bool_ty()))
            }
            // keys() → the keys vector itself (iterable via the ordinary collection
            // `for`); it preserves insertion order.
            "keys" => {
                let kspell = self.decl_spelling(&om.key_ty);
                Some((
                    keys,
                    Ty {
                        base: format!("std::vector<{kspell} >"),
                        ..Default::default()
                    },
                ))
            }
            _ => None,
        }
    }

    /// Tier-1 Haxe `String` methods on a `std::string` receiver, each mapped to a
    /// single C++98 expression. Byte/ASCII semantics (VC6 narrow `char`); an
    /// out-of-range index makes `charAt`/`charCodeAt` *throw* via `.at()`/`substr`
    /// rather than returning `""`/`null` — an error-path divergence from Haxe.
    /// Returns `None` for methods not in Tier 1 (e.g. `split`, the `startIndex`
    /// form of `lastIndexOf`), which fall through to a later tier.
    pub(super) fn string_call(
        &mut self,
        rcode: &str,
        method: &str,
        args: &[Expr],
    ) -> Option<(String, Ty)> {
        let str_ty = Ty {
            base: "std::string".into(),
            ..Default::default()
        };
        match method {
            "toString" => Some((rcode.to_string(), str_ty)),
            "charAt" => {
                let i = self.gen_expr(&args[0]).0;
                Some((format!("{rcode}.substr({i}, 1)"), str_ty))
            }
            // `npos` (size_t(-1)) casts to int `-1` on 32- and 64-bit, matching
            // Haxe's "not found" sentinel.
            "indexOf" => {
                let needle = self.gen_expr(&args[0]).0;
                let call = if args.len() > 1 {
                    let start = self.gen_expr(&args[1]).0;
                    format!("{rcode}.find({needle}, {start})")
                } else {
                    format!("{rcode}.find({needle})")
                };
                Some((format!("((int){call})"), int_ty()))
            }
            // Tier 1 handles only the no-`startIndex` form; the search-window rule
            // for `lastIndexOf(str, startIndex)` is Tier 2.
            "lastIndexOf" if args.len() <= 1 => {
                let needle = self.gen_expr(&args[0]).0;
                Some((format!("((int){rcode}.rfind({needle}))"), int_ty()))
            }
            // Haxe returns `Null<Int>`; as an intrinsic this yields plain `int`
            // (the usual `var c:Int = s.charCodeAt(i)` form). `unsigned char` cast
            // is required for correct code values on MSVC.
            "charCodeAt" => {
                let i = self.gen_expr(&args[0]).0;
                Some((format!("((int)(unsigned char){rcode}.at({i}))"), int_ty()))
            }
            // Tier 2: ASCII case mapping done in-place on a copy (no <cctype>).
            "toUpperCase" | "toLowerCase" => {
                let upper = method == "toUpperCase";
                let acc = self.fresh("case");
                let i = self.fresh("i");
                let t = "\t".repeat(self.prelude_ind);
                let (lo, hi, delta) = if upper {
                    ('a', 'z', "- 'a' + 'A'")
                } else {
                    ('A', 'Z', "- 'A' + 'a'")
                };
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc} = {rcode};");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {acc}.size(); ++{i}) {{ if ({acc}[{i}] >= '{lo}' && {acc}[{i}] <= '{hi}') {acc}[{i}] = (char)({acc}[{i}] {delta}); }}"
                );
                self.prelude.push_str(&pre);
                Some((acc, str_ty))
            }
            // Tier 2: `split(delim)` → a std::vector<std::string>. An empty delimiter
            // splits into individual characters (Haxe semantics).
            "split" => {
                let delim = self.gen_expr(&args[0]).0;
                let acc = self.fresh("spl");
                let s = self.fresh("s");
                let d = self.fresh("d");
                let i = self.fresh("i");
                let start = self.fresh("start");
                let found = self.fresh("found");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::vector<std::string > {acc};");
                let _ = writeln!(pre, "{t}std::string {s} = {rcode};");
                let _ = writeln!(pre, "{t}std::string {d} = {delim};");
                let _ = writeln!(pre, "{t}if ({d}.empty()) {{");
                let _ = writeln!(pre, "{t}\tfor (size_t {i} = 0; {i} < {s}.size(); ++{i}) {{ {acc}.push_back({s}.substr({i}, 1)); }}");
                let _ = writeln!(pre, "{t}}} else {{");
                let _ = writeln!(pre, "{t}\tsize_t {start} = 0;");
                let _ = writeln!(pre, "{t}\tsize_t {found};");
                let _ = writeln!(
                    pre,
                    "{t}\twhile (({found} = {s}.find({d}, {start})) != std::string::npos) {{"
                );
                let _ = writeln!(
                    pre,
                    "{t}\t\t{acc}.push_back({s}.substr({start}, {found} - {start}));"
                );
                let _ = writeln!(pre, "{t}\t\t{start} = {found} + {d}.size();");
                let _ = writeln!(pre, "{t}\t}}");
                let _ = writeln!(pre, "{t}\t{acc}.push_back({s}.substr({start}));");
                let _ = writeln!(pre, "{t}}}");
                self.prelude.push_str(&pre);
                Some((
                    acc,
                    Ty {
                        base: "std::vector<std::string >".into(),
                        ..Default::default()
                    },
                ))
            }
            // `substr(pos, ?len)` — `pos` may be negative (counted from the end);
            // `len` omitted means "to the end". Indices are clamped to the string.
            "substr" => {
                let pos = self.gen_expr(&args[0]).0;
                let len = if args.len() > 1 {
                    Some(self.gen_expr(&args[1]).0)
                } else {
                    None
                };
                let s = self.fresh("s");
                let p = self.fresh("p");
                let res = self.fresh("sub");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {s} = {rcode};");
                let _ = writeln!(pre, "{t}int {p} = (int)({pos});");
                let _ = writeln!(pre, "{t}if ({p} < 0) {p} += (int){s}.size();");
                let _ = writeln!(pre, "{t}if ({p} < 0) {p} = 0;");
                let _ = writeln!(
                    pre,
                    "{t}if ((size_t){p} > {s}.size()) {p} = (int){s}.size();"
                );
                if let Some(len) = len {
                    let n = self.fresh("n");
                    let _ = writeln!(pre, "{t}int {n} = (int)({len}); if ({n} < 0) {n} = 0;");
                    let _ = writeln!(
                        pre,
                        "{t}std::string {res} = {s}.substr((size_t){p}, (size_t){n});"
                    );
                } else {
                    let _ = writeln!(pre, "{t}std::string {res} = {s}.substr((size_t){p});");
                }
                self.prelude.push_str(&pre);
                Some((res, str_ty))
            }
            // `substring(start, ?end)` — negative indices clamp to 0, and start/end
            // are swapped when start > end (Haxe semantics).
            "substring" => {
                let start = self.gen_expr(&args[0]).0;
                let end = if args.len() > 1 {
                    Some(self.gen_expr(&args[1]).0)
                } else {
                    None
                };
                let s = self.fresh("s");
                let a = self.fresh("a");
                let b = self.fresh("b");
                let res = self.fresh("sub");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {s} = {rcode};");
                let _ = writeln!(pre, "{t}int {a} = (int)({start}); if ({a} < 0) {a} = 0; if ((size_t){a} > {s}.size()) {a} = (int){s}.size();");
                match end {
                    Some(end) => {
                        let _ = writeln!(pre, "{t}int {b} = (int)({end}); if ({b} < 0) {b} = 0; if ((size_t){b} > {s}.size()) {b} = (int){s}.size();");
                    }
                    None => {
                        let _ = writeln!(pre, "{t}int {b} = (int){s}.size();");
                    }
                }
                let _ = writeln!(
                    pre,
                    "{t}if ({a} > {b}) {{ int t = {a}; {a} = {b}; {b} = t; }}"
                );
                let _ = writeln!(
                    pre,
                    "{t}std::string {res} = {s}.substr((size_t){a}, (size_t)({b} - {a}));"
                );
                self.prelude.push_str(&pre);
                Some((res, str_ty))
            }
            // `StringBuf.add(x)` → append `x` stringified (reuses the `Std.string`
            // lowering); `StringBuf` is a `std::string` accumulator. Returns Void.
            "add" => {
                let (sv, _) = self.gen_std_string(&args[0]);
                Some((format!("{rcode} += {sv}"), Ty::default()))
            }
            // `StringBuf.addChar(c)` → append a single byte.
            "addChar" => Some((
                format!("{rcode} += (char)({})", self.gen_expr(&args[0]).0),
                Ty::default(),
            )),
            _ => None,
        }
    }

    pub(super) fn callee_param_types(&self, recv: &Ty, method: &str) -> Vec<Option<Ty>> {
        let Some(info) = &recv.info else {
            return Vec::new();
        };
        let cmi = info.module_index;
        let Some(decl) = self.prog.type_decl(info) else {
            return Vec::new();
        };
        let methods = match decl {
            Decl::Class(c) => &c.methods,
            Decl::Interface(i) => &i.methods,
            _ => return Vec::new(),
        };
        match methods.iter().find(|m| m.name.as_deref() == Some(method)) {
            // Parameter types resolve in the callee's declaring module.
            Some(m) => m.params.iter().map(|p| self.param_ty_in(p, cmi)).collect(),
            None => Vec::new(),
        }
    }

    pub(super) fn own_method_param_types(&self, name: &str) -> Vec<Option<Ty>> {
        match self
            .class
            .methods
            .iter()
            .find(|m| m.name.as_deref() == Some(name))
        {
            Some(m) => m
                .params
                .iter()
                .map(|p| self.param_ty_in(p, self.mi))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Per-position `@sink` flags for a method on `recv`'s type: positions the
    /// callee consumes, so a `new` passed there is emitted inline (the receiver
    /// frees it) rather than freed by the caller.
    pub(super) fn callee_sink_params(&self, recv: &Ty, method: &str) -> Vec<bool> {
        let Some(info) = &recv.info else {
            return Vec::new();
        };
        let Some(decl) = self.prog.type_decl(info) else {
            return Vec::new();
        };
        let methods = match decl {
            Decl::Class(c) => &c.methods,
            Decl::Interface(i) => &i.methods,
            _ => return Vec::new(),
        };
        match methods.iter().find(|m| m.name.as_deref() == Some(method)) {
            Some(m) => param_sink_flags(&m.params),
            None => Vec::new(),
        }
    }

    /// `@sink` flags for a bare call: an own-class method, else a module-level
    /// free function of that name.
    pub(super) fn bare_sink_params(&self, name: &str) -> Vec<bool> {
        if let Some(m) = self
            .class
            .methods
            .iter()
            .find(|m| m.name.as_deref() == Some(name))
        {
            return param_sink_flags(&m.params);
        }
        for d in &self.prog.modules[self.mi].file.decls {
            if let Decl::Function(f) = d {
                if f.name.as_deref() == Some(name) {
                    return param_sink_flags(&f.params);
                }
            }
        }
        Vec::new()
    }

    // ---- intrinsics ----------------------------------------------------

    pub(super) fn intrinsic_call(
        &mut self,
        obj: &str,
        method: &str,
        args: &[Expr],
    ) -> Option<(String, Ty)> {
        let f = |this: &mut Self, i: usize| this.gen_expr(&args[i]).0;
        match (obj, method) {
            // Direct <math.h> functions (Float → Float).
            ("Math", "sqrt") => Some((format!("sqrt({})", f(self, 0)), float_ty())),
            ("Math", "sin") => Some((format!("sin({})", f(self, 0)), float_ty())),
            ("Math", "cos") => Some((format!("cos({})", f(self, 0)), float_ty())),
            ("Math", "tan") => Some((format!("tan({})", f(self, 0)), float_ty())),
            ("Math", "asin") => Some((format!("asin({})", f(self, 0)), float_ty())),
            ("Math", "acos") => Some((format!("acos({})", f(self, 0)), float_ty())),
            ("Math", "atan") => Some((format!("atan({})", f(self, 0)), float_ty())),
            ("Math", "exp") => Some((format!("exp({})", f(self, 0)), float_ty())),
            ("Math", "log") => Some((format!("log({})", f(self, 0)), float_ty())),
            ("Math", "atan2") => {
                Some((format!("atan2({}, {})", f(self, 0), f(self, 1)), float_ty()))
            }
            ("Math", "pow") => Some((format!("pow({}, {})", f(self, 0), f(self, 1)), float_ty())),
            // Float-returning rounding (ffloor/fceil/fround).
            ("Math", "ffloor") => Some((format!("floor({})", f(self, 0)), float_ty())),
            ("Math", "fceil") => Some((format!("ceil({})", f(self, 0)), float_ty())),
            ("Math", "fround") => Some((format!("floor(({}) + 0.5)", f(self, 0)), float_ty())),
            // Int-returning rounding (Haxe `floor`/`ceil`/`round` return Int).
            ("Math", "floor") => Some((format!("((int)floor({}))", f(self, 0)), int_ty())),
            ("Math", "ceil") => Some((format!("((int)ceil({}))", f(self, 0)), int_ty())),
            ("Math", "round") => Some((format!("((int)floor(({}) + 0.5))", f(self, 0)), int_ty())),
            ("Math", "abs") => {
                // abs for Int, fabs for Float — choose by inferred argument type
                let (c, ty) = self.gen_expr(&args[0]);
                let fname = if matches!(ty.base.as_str(), "float" | "double") {
                    "fabs"
                } else {
                    "abs"
                };
                Some((format!("{fname}({c})"), ty))
            }
            ("Math", "min") => Some((self.min_max("<", args), float_ty())),
            ("Math", "max") => Some((self.min_max(">", args), float_ty())),
            // Math.random() ∈ [0, 1).
            ("Math", "random") => Some(("(rand() / (RAND_MAX + 1.0))".into(), float_ty())),
            // Predicates → bool (portable C++98, no <cmath> isnan/isfinite needed).
            ("Math", "isNaN") => {
                let a = f(self, 0);
                Some((format!("(({a}) != ({a}))"), bool_ty()))
            }
            ("Math", "isFinite") => Some((format!("((({}) * 0.0) == 0.0)", f(self, 0)), bool_ty())),
            ("Std", "int") => Some((
                format!("(int)({})", f(self, 0)),
                Ty {
                    base: "int".into(),
                    ..Default::default()
                },
            )),
            ("Std", "string") => Some(self.gen_std_string(&args[0])),
            // `Std.parseInt` accepts decimal and `0x` hex (strtol base 0). Haxe
            // returns `Null<Int>`; the C++98 lowering yields a plain `int` (0 on a
            // fully unparseable string).
            ("Std", "parseInt") => {
                let s = self.cstr_arg(&args[0]);
                Some((format!("(int)strtol({s}, NULL, 0)"), int_ty()))
            }
            ("Std", "parseFloat") => {
                let s = self.cstr_arg(&args[0]);
                // atof already returns double — Haxe `Float`.
                Some((format!("atof({s})"), float_ty()))
            }
            // `Std.random(x)` → a non-negative int in `[0, x)` (0 when `x <= 0`, as in
            // Haxe). The argument is virtually always pure, so re-using it is safe.
            ("Std", "random") => {
                let n = f(self, 0);
                Some((
                    format!("(((int)({n})) > 0 ? (rand() % (int)({n})) : 0)"),
                    int_ty(),
                ))
            }
            // `StringTools.replace(s, sub, by)` → replace every occurrence of `sub`.
            ("StringTools", "replace") => {
                let s = self.gen_expr(&args[0]).0;
                let sub = self.gen_expr(&args[1]).0;
                let by = self.gen_expr(&args[2]).0;
                let acc = self.fresh("rep");
                let needle = self.fresh("sub");
                let repl = self.fresh("by");
                let pos = self.fresh("pos");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc} = {s};");
                let _ = writeln!(pre, "{t}std::string {needle} = {sub};");
                let _ = writeln!(pre, "{t}std::string {repl} = {by};");
                let _ = writeln!(pre, "{t}if (!{needle}.empty()) {{");
                let _ = writeln!(pre, "{t}\tsize_t {pos} = 0;");
                let _ = writeln!(pre, "{t}\twhile (({pos} = {acc}.find({needle}, {pos})) != std::string::npos) {{ {acc}.replace({pos}, {needle}.size(), {repl}); {pos} += {repl}.size(); }}");
                let _ = writeln!(pre, "{t}}}");
                self.prelude.push_str(&pre);
                Some((acc, str_ty()))
            }
            // `StringTools.trim(s)` → strip leading/trailing ASCII whitespace.
            ("StringTools", "trim") | ("StringTools", "ltrim") | ("StringTools", "rtrim") => {
                let s = self.gen_expr(&args[0]).0;
                let acc = self.fresh("trm");
                let a = self.fresh("a");
                let b = self.fresh("b");
                let res = self.fresh("res");
                let t = "\t".repeat(self.prelude_ind);
                let ws = "== ' ' || {C} == '\\t' || {C} == '\\n' || {C} == '\\r'";
                let lo = ws.replace("{C}", &format!("{acc}[{a}]"));
                let hi = ws.replace("{C}", &format!("{acc}[{b} - 1]"));
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc} = {s};");
                let _ = writeln!(pre, "{t}size_t {a} = 0; size_t {b} = {acc}.size();");
                if method != "rtrim" {
                    let _ = writeln!(pre, "{t}while ({a} < {b} && ({acc}[{a}] {lo})) ++{a};");
                }
                if method != "ltrim" {
                    let _ = writeln!(pre, "{t}while ({b} > {a} && ({acc}[{b} - 1] {hi})) --{b};");
                }
                let _ = writeln!(pre, "{t}std::string {res} = {acc}.substr({a}, {b} - {a});");
                self.prelude.push_str(&pre);
                Some((res, str_ty()))
            }
            // `StringTools.startsWith(s, start)` / `endsWith(s, end)` → a bool temp
            // (hoisted so the operands are evaluated exactly once).
            ("StringTools", "startsWith") | ("StringTools", "endsWith") => {
                let starts = method == "startsWith";
                let s = self.gen_expr(&args[0]).0;
                let sub = self.gen_expr(&args[1]).0;
                let sv = self.fresh("s");
                let ss = self.fresh("sub");
                let res = self.fresh("res");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {sv} = {s};");
                let _ = writeln!(pre, "{t}std::string {ss} = {sub};");
                let cmp = if starts {
                    format!("{sv}.compare(0, {ss}.size(), {ss}) == 0")
                } else {
                    format!("{sv}.compare({sv}.size() - {ss}.size(), {ss}.size(), {ss}) == 0")
                };
                let _ = writeln!(
                    pre,
                    "{t}bool {res} = ({sv}.size() >= {ss}.size() && {cmp});"
                );
                self.prelude.push_str(&pre);
                Some((res, bool_ty()))
            }
            // `StringTools.hex(n, ?digits)` → uppercase hex, zero-padded to `digits`.
            ("StringTools", "hex") => {
                let n = self.gen_expr(&args[0]).0;
                let buf = self.fresh("hex");
                let res = self.fresh("res");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}char {buf}[32];");
                if args.len() > 1 {
                    let digits = self.gen_expr(&args[1]).0;
                    let _ = writeln!(
                        pre,
                        "{t}sprintf({buf}, \"%0*X\", (int)({digits}), (unsigned int)({n}));"
                    );
                } else {
                    let _ = writeln!(pre, "{t}sprintf({buf}, \"%X\", (unsigned int)({n}));");
                }
                let _ = writeln!(pre, "{t}std::string {res} = {buf};");
                self.prelude.push_str(&pre);
                Some((res, str_ty()))
            }
            ("Sys", "cpuTime") => Some((
                "((double) clock() / (double) CLOCKS_PER_SEC)".into(),
                float_ty(),
            )),
            // `String.fromCharCode(c)` → a one-char string (low byte only on VC6).
            ("String", "fromCharCode") => Some((
                format!("std::string(1, (char)(({}) & 0xFF))", f(self, 0)),
                Ty {
                    base: "std::string".into(),
                    ..Default::default()
                },
            )),
            _ => None,
        }
    }

    pub(super) fn min_max(&mut self, cmp: &str, args: &[Expr]) -> String {
        let (a, _) = self.gen_expr(&args[0]);
        let (b, _) = self.gen_expr(&args[1]);
        // Inline ternary that propagates NaN exactly as Haxe does (NaN in either
        // operand → NaN result) — no `haxe_min`/`haxe_max` helper.
        format!("(({a}) {cmp} ({b}) ? ({a}) : (({a}) == ({a}) ? ({b}) : ({a})))")
    }

    pub(super) fn gen_string(&mut self, raw: &str, interpolated: bool) -> (String, Ty) {
        let str_ty = Ty {
            base: "std::string".into(),
            ..Default::default()
        };
        if !interpolated || !has_interpolation(raw) {
            return (format!("\"{}\"", escape_str(raw)), str_ty);
        }
        let (segments, exprs) = split_interpolation(raw);
        if exprs.is_empty() {
            return (format!("\"{}\"", escape_str(raw)), str_ty);
        }
        // Build the result by appending each piece to a `std::string`: literal and string
        // segments append directly (`s += part`) — `std::string` grows itself, so an
        // arbitrarily long interpolated string is safe — while each numeric segment is
        // formatted into a type-bounded buffer. There is no single value-guessed buffer,
        // so this cannot overflow regardless of the runtime values.
        let acc = self.fresh("str");
        let t = "\t".repeat(self.prelude_ind);
        self.prelude.push_str(&format!("{t}std::string {acc};\n"));
        for seg in segments {
            let part = match seg {
                Seg::Lit(s) => format!("\"{}\"", escape_str(&s)),
                Seg::Expr(src) => match crate::parser::parse_expression(&src) {
                    Ok(e) => {
                        let (code, ty) = self.gen_expr(&e);
                        if ty.base == "std::string" {
                            code
                        } else {
                            self.format_scalar(&code, &ty)
                        }
                    }
                    Err(_) => format!("\"{}\"", escape_str(&src)),
                },
            };
            self.prelude.push_str(&format!("{t}{acc} += {part};\n"));
        }
        (acc, str_ty)
    }

    /// Lower a Haxe `trace(args...)` call. Like Haxe, the output is prefixed with
    /// the source `file:line` and the arguments follow, comma-separated. It reuses
    /// the string-interpolation plumbing (`spec_for`) to pick a printf conversion
    /// per argument, emitting a single `printf` to stdout. Under `--no-traces` the
    /// whole call (and its argument evaluation) is stripped to a no-op.
    pub(super) fn gen_trace(&mut self, args: &[Expr]) -> (String, Ty) {
        if self.no_trace {
            return ("((void)0)".to_string(), Ty::default());
        }
        let file = self.prog.modules[self.mi]
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?");
        let mut fmt = printf_escape(&format!("{file}:{}: ", self.current_line));
        let mut printf_args: Vec<String> = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                fmt.push_str(", ");
            }
            let (code, ty) = self.gen_expr(a);
            // A bare string literal is already a `const char*`; everything else
            // goes through the interpolation type→spec mapping (a `std::string`
            // value needs `.c_str()`, which `spec_for` supplies).
            let (spec, arg) = if matches!(
                a,
                Expr::Str {
                    interpolated: false,
                    ..
                }
            ) {
                ("%s".to_string(), code)
            } else {
                self.spec_for(&code, &ty)
            };
            fmt.push_str(&spec);
            printf_args.push(arg);
        }
        fmt.push_str("\\n");
        let call = if printf_args.is_empty() {
            format!("printf(\"{fmt}\")")
        } else {
            format!("printf(\"{fmt}\", {})", printf_args.join(", "))
        };
        (call, Ty::default())
    }

    /// `Std.string(x)` → a `std::string` holding x's textual form. A value that is
    /// already a string passes through; a bool maps to `"true"`/`"false"`; a numeric
    /// value is formatted via `sprintf` into a stack buffer (reusing `spec_for`'s
    /// type→conversion mapping).
    pub(super) fn gen_std_string(&mut self, arg: &Expr) -> (String, Ty) {
        let str_ty = Ty {
            base: "std::string".into(),
            ..Default::default()
        };
        // A bare string literal is emitted as a `const char*`; wrap it so the
        // result is a genuine `std::string` value.
        if matches!(
            arg,
            Expr::Str {
                interpolated: false,
                ..
            }
        ) {
            let (code, _) = self.gen_expr(arg);
            return (format!("std::string({code})"), str_ty);
        }
        let (code, ty) = self.gen_expr(arg);
        if ty.base == "std::string" {
            return (code, str_ty);
        }
        if ty.base == "bool" {
            return (
                format!("(({code}) ? std::string(\"true\") : std::string(\"false\"))"),
                str_ty,
            );
        }
        let buf = self.format_scalar(&code, &ty);
        (format!("std::string({buf})"), str_ty)
    }

    /// Evaluate a string-typed argument as a C++ `const char*` expression: a
    /// `std::string` value gets `.c_str()`; a bare string literal is already one.
    pub(super) fn cstr_arg(&mut self, arg: &Expr) -> String {
        let (code, ty) = self.gen_expr(arg);
        // A bare string literal is already a `const char*` — `.c_str()` on it is
        // invalid; everything else of string type is a `std::string` value.
        if matches!(
            arg,
            Expr::Str {
                interpolated: false,
                ..
            }
        ) {
            code
        } else if ty.base == "std::string" {
            format!("{code}.c_str()")
        } else {
            code
        }
    }

    /// Choose a printf conversion and the matching argument expression for an
    /// interpolated value, based on its inferred C++ type.
    pub(super) fn spec_for(&self, code: &str, ty: &Ty) -> (String, String) {
        if ty.base == "std::string" {
            ("%s".to_string(), format!("{code}.c_str()"))
        } else if ty.base == "float" || ty.base == "double" {
            ("%f".to_string(), code.to_string())
        } else {
            ("%d".to_string(), code.to_string())
        }
    }

    /// Format a non-string scalar (`code` of type `ty`) into a hoisted stack buffer and
    /// return the buffer name (a `const char*`). The buffer size is fixed by the *type*,
    /// never guessed from the runtime value, so it can never overflow: a 32-bit `int`
    /// prints ≤ 11 chars, a `float`/`double` via `%f` ≤ ~48 / ~316. This is the one place
    /// that turns a number into text, shared by interpolation, concatenation and
    /// `Std.string`. (Strings are never formatted through here — they are appended
    /// directly, which is unbounded-safe.)
    pub(super) fn format_scalar(&mut self, code: &str, ty: &Ty) -> String {
        let (spec, arg) = self.spec_for(code, ty);
        let size = match ty.base.as_str() {
            "double" => 320, // %f of DBL_MAX ≈ 316 chars
            "float" => 64,   // %f of FLT_MAX ≈ 48 chars
            _ => 24,         // a 64-bit integer ≤ 20 chars
        };
        let buf = self.fresh("buf");
        let t = "\t".repeat(self.prelude_ind);
        self.prelude.push_str(&format!(
            "{t}char {buf}[{size}]; sprintf({buf}, \"{spec}\", {arg});\n"
        ));
        buf
    }

    /// One operand of a string concatenation, as a C++ expression that participates in
    /// `std::string` `operator+`. A `String` operand (variable or literal) is used as-is;
    /// a non-string (numeric) operand is formatted into a type-bounded buffer and wrapped
    /// as a `std::string` so it anchors the chain (`std::string(buf) + ","`).
    pub(super) fn concat_part(&mut self, code: &str, ty: &Ty) -> String {
        if ty.base == "std::string" {
            return code.to_string();
        }
        let buf = self.format_scalar(code, ty);
        format!("std::string({buf})")
    }
}
