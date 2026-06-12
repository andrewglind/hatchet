//! Abstract syntax tree for the Haxe subset Hatchet transpiles.
//!
//! The tree models the Haxe subset Hatchet supports, independent of any particular
//! project. Metadata is attached to the construct it decorates; semantic meaning
//! (e.g. `@:native`, `@:include`) is interpreted later in the `sema` layer, not
//! here.

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// A `@:name(args...)` (or `@name`) annotation. `args` holds the raw token text
/// of each top-level argument, which is enough for `@:native("x")`,
/// `@:include("p.h")`, etc. `@:overload(function(...){})` keeps its argument
/// source so the overload signature can be parsed on demand.
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub name: String,
    pub args: Vec<String>,
}

impl Meta {
    pub fn first_arg(&self) -> Option<&str> {
        self.args.first().map(|s| s.as_str())
    }
}

/// Helper: does a metadata list contain `name`?
pub fn has_meta(metas: &[Meta], name: &str) -> bool {
    metas.iter().any(|m| m.name == name)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    /// `Int`, `Array<T>`, a dotted path like `pack.Module.Type`, `?Foo` (optional).
    Named {
        path: Vec<String>,
        params: Vec<Type>,
        optional: bool,
        /// Source line (1-based) where the type name was written, for diagnostics.
        /// `0` when synthesized rather than parsed.
        line: usize,
    },
    /// Anonymous structure type: `{ x:Int, ?y:Float }`.
    Anon(Vec<StructField>),
    /// Function type `A -> B -> C`.
    Func {
        params: Vec<Type>,
        ret: Box<Type>,
    },
}

impl Type {
    /// The final identifier of a named type (e.g. `Type` for `pack.Module.Type`).
    pub fn base_name(&self) -> Option<&str> {
        match self {
            Type::Named { path, .. } => path.last().map(|s| s.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructField {
    pub name: String,
    pub optional: bool,
    pub ty: Type,
}

// ---------------------------------------------------------------------------
// Access / modifiers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Public,
    Private,
    /// `@:protected` (or `@:protected private`).
    Protected,
    /// No explicit modifier — Haxe instance default is private.
    Default,
}

/// Property access kind in `(get, set)` style declarations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropAccess {
    Default,
    Null,
    Get,
    Set,
    Never,
    Dynamic,
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(String),
    Float(String),
    Str { raw: String, interpolated: bool },
    Bool(bool),
    Null,
    This,
    Super,
    Ident(String),

    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    New(Type, Vec<Expr>),

    Unary { op: UnOp, expr: Box<Expr>, prefix: bool },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Ternary { cond: Box<Expr>, then: Box<Expr>, els: Box<Expr> },
    Assign { op: Option<BinOp>, target: Box<Expr>, value: Box<Expr> },

    /// `x ?? y`
    NullCoalesce(Box<Expr>, Box<Expr>),
    /// `a?.b`
    SafeField(Box<Expr>, String),

    ArrayLit(Vec<Expr>),
    MapLit(Vec<(Expr, Expr)>),
    ObjectLit(Vec<(String, Expr)>),

    /// `[for (v in iter) body]` (array) or with `=>` body (map).
    Comprehension {
        var: String,
        iter: Box<Iterable>,
        guard: Option<Box<Expr>>,
        body: ComprBody,
    },

    Lambda {
        params: Vec<Param>,
        ret: Option<Type>,
        body: Box<LambdaBody>,
    },

    /// `cast expr` or `cast(expr, Type)`.
    Cast { expr: Box<Expr>, ty: Option<Type> },
    /// `(expr : Type)`.
    TypeCheck { expr: Box<Expr>, ty: Type },
    /// The Haxe 4.2 `expr is Type` runtime type-check operator. Hatchet does not
    /// transpile it yet; this is carried only so the validation pass can flag it as
    /// `Unsupported` with a precise location (a class-type check would need
    /// `dynamic_cast` / RTTI, which is a separate, target-sensitive feature).
    Is { expr: Box<Expr>, ty: Type },

    /// A `switch` used in value position (`var x = switch (e) { … }`). Carries the
    /// same shape as the statement form; codegen desugars it to a hoisted temporary
    /// plus a statement `switch` whose arms assign their trailing value to the temp.
    Switch { subject: Box<Expr>, cases: Vec<Case>, default: Option<Vec<Stmt>> },
    /// Parenthesised expression (grouping preserved for fidelity).
    Paren(Box<Expr>),

    /// `untyped <rest of statement>` — the operand is captured as raw source and
    /// emitted to C++ verbatim, bypassing all type checking and transpilation.
    /// Used to drop down to C/C++ APIs the Haxe side cannot see (e.g. a
    /// platform intrinsic inside a `#if`/`#end` block).
    Verbatim(String),

    /// A regular-expression literal `~/pattern/flags`. Hatchet does not transpile
    /// regex; this is carried only so the validation pass can flag it as
    /// `Unsupported` with a precise location.
    Regex { pattern: String, flags: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComprBody {
    Value(Box<Expr>),
    KeyValue(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LambdaBody {
    Expr(Expr),
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Iterable {
    /// `start...end`
    Range(Expr, Expr),
    /// iterate over a collection value
    Coll(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
    Incr,
    Decr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, Ne, Lt, Gt, Le, Ge,
    And, Or,
    BitAnd, BitOr, BitXor, Shl, Shr,
    /// Haxe `>>>` (unsigned right shift). C++98 has no `>>>`; codegen lowers it
    /// through an `unsigned int` cast: `(int)((unsigned int)a >> b)`.
    UShr,
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Var {
        name: String,
        ty: Option<Type>,
        init: Option<Expr>,
        is_final: bool,
        /// `@delete var x = …`: the developer's explicit request to free `x` at the
        /// end of this scope (the local-scope counterpart to `@owned` on a field).
        /// Overrides the ownership analysis for this local.
        delete: bool,
        /// Source line (1-based) of the `var`/`final`, for diagnostics.
        line: usize,
    },
    /// An expression statement, tagged with its source line for diagnostics.
    Expr(Expr, usize),
    If {
        cond: Expr,
        then: Box<Stmt>,
        els: Option<Box<Stmt>>,
        line: usize,
    },
    For {
        /// The loop binding. For `for (v in coll)` this is the element/value; for
        /// the key/value form `for (k => v in map)` this is the **key**.
        var: String,
        /// The value binding of a `for (k => v in map)` loop; `None` otherwise.
        value_var: Option<String>,
        iter: Iterable,
        body: Box<Stmt>,
        line: usize,
    },
    While {
        cond: Expr,
        body: Box<Stmt>,
        do_while: bool,
        line: usize,
    },
    Switch {
        subject: Expr,
        cases: Vec<Case>,
        default: Option<Vec<Stmt>>,
        line: usize,
    },
    /// A `return`, tagged with its source line for diagnostics.
    Return(Option<Expr>, usize),
    Break,
    Continue,
    Throw(Expr, usize),
    /// A `try { … } catch (e:T) { … }` block, lowered to C++ `try`/`catch` (a
    /// typed catch maps the exception type; an untyped/`Dynamic` catch becomes the
    /// non-binding `catch (...)`).
    Try { body: Box<Stmt>, catches: Vec<Catch>, line: usize },
    Block(Vec<Stmt>),
    /// Verbatim C++ injected at this point in the body, from `@:cppFileCode('...')`
    /// statement-level metadata. The code is emitted exactly as written (at column
    /// 0, so it can carry preprocessor directives like `#ifdef`/`#else`/`#endif`).
    Verbatim { code: String, line: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    /// One or more patterns share a body: `case A, B:`.
    pub patterns: Vec<Expr>,
    pub body: Vec<Stmt>,
}

/// One `catch (name:Type) { … }` clause of a [`Stmt::Try`]. `ty` is `None` for the
/// Haxe 4.2 type-less `catch (e)` form.
#[derive(Debug, Clone, PartialEq)]
pub struct Catch {
    pub name: String,
    pub ty: Option<Type>,
    pub body: Vec<Stmt>,
}

// ---------------------------------------------------------------------------
// Functions / fields / declarations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Option<Type>,
    pub optional: bool,
    pub default: Option<Expr>,
    /// A Haxe 4.2 rest parameter (`...vals:Int`). Parsed so the validation pass
    /// can flag it as `Unsupported` (varargs have no C++98 lowering here) instead
    /// of dying with a parse error.
    pub rest: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FnModifiers {
    pub is_static: bool,
    pub is_inline: bool,
    pub is_extern: bool,
    pub is_override: bool,
    pub is_final: bool,
    pub is_dynamic: bool,
    pub is_abstract: bool,
    pub is_macro: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// `None` for the constructor (`new`).
    pub name: Option<String>,
    /// Generic parameters of a generic method/function (`function first<T>(…)`).
    /// Hatchet has no template lowering, so a non-empty list is flagged as
    /// `Unsupported` by the validation pass.
    pub type_params: Vec<String>,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub body: Option<Vec<Stmt>>,
    pub access: Access,
    pub modifiers: FnModifiers,
    pub meta: Vec<Meta>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Option<Type>,
    pub init: Option<Expr>,
    pub access: Access,
    pub is_static: bool,
    pub is_final: bool,
    pub get: PropAccess,
    pub set: PropAccess,
    pub meta: Vec<Meta>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub name: String,
    /// Source line (1-based) of the `class` keyword, for diagnostics. `0` when
    /// synthesized rather than parsed.
    pub line: usize,
    /// Generic parameters (`class Box<T>`). Hatchet has no template lowering, so a
    /// non-empty list is flagged as `Unsupported` by the validation pass.
    pub type_params: Vec<String>,
    pub extends: Option<Type>,
    pub implements: Vec<Type>,
    pub is_extern: bool,
    pub is_final: bool,
    pub is_abstract: bool,
    pub meta: Vec<Meta>,
    pub fields: Vec<Field>,
    pub methods: Vec<Function>,
    pub ctor: Option<Function>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    pub name: String,
    /// Source line (1-based) of the `interface` keyword, for diagnostics. `0`
    /// when synthesized rather than parsed.
    pub line: usize,
    /// Generic parameters (`interface I<T>`). Hatchet has no template lowering, so
    /// a non-empty list is flagged as `Unsupported` by the validation pass.
    pub type_params: Vec<String>,
    pub extends: Vec<Type>,
    pub meta: Vec<Meta>,
    pub methods: Vec<Function>,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub params: Vec<Param>,
    /// The explicit constant value of an `enum abstract` member (`var Red = 0;`).
    /// Always `None` for a plain Haxe `enum` (whose variants are nominal); `None`
    /// for an `enum abstract` member that omits its value (the C++ enum then
    /// auto-increments, matching Haxe).
    pub value: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Enum {
    pub name: String,
    pub meta: Vec<Meta>,
    pub variants: Vec<EnumVariant>,
    /// The underlying type of an `enum abstract X(T)` (`None` for a plain `enum`).
    /// An integral backing lowers to a C++ `enum`; a `String`/`Float` backing
    /// lowers to a `namespace X_ { static const T … }` of typed constants.
    pub underlying: Option<Type>,
}

impl Enum {
    /// Whether this is an algebraic enum — a plain `enum` with at least one
    /// parameterized variant (`Add(a:Int, b:Int)`). ADTs lower to a tagged value
    /// class (tag + per-variant payload fields + static factory functions)
    /// instead of a bare C++ enum.
    pub fn is_adt(&self) -> bool {
        self.underlying.is_none() && self.variants.iter().any(|v| !v.params.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedefTarget {
    Alias(Type),
    Struct(Vec<StructField>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Typedef {
    pub name: String,
    pub meta: Vec<Meta>,
    pub target: TypedefTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalVar {
    pub name: String,
    pub ty: Option<Type>,
    pub init: Option<Expr>,
    pub is_final: bool,
    pub access: Access,
    pub meta: Vec<Meta>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decl {
    /// Boxed: `Class` is by far the largest declaration (fields + methods +
    /// ctor), and `Decl`s live in per-file vectors — keep the enum small.
    Class(Box<Class>),
    Interface(Interface),
    Enum(Enum),
    Typedef(Typedef),
    Global(GlobalVar),
    /// Top-level function (e.g. `extern inline function MCreateScene(...) {}`).
    Function(Function),
    /// A recognised but not-yet-transpiled top-level declaration (an `abstract`
    /// type or `enum abstract`). Its body is skipped at parse time; `feature` is a
    /// human label and `line` its location, so the validation pass can flag it as
    /// `Unsupported` rather than dying with a parse error.
    Unsupported { feature: String, line: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    /// Dotted path; `wildcard` is true for `import a.b.*`.
    pub path: Vec<String>,
    pub wildcard: bool,
    pub alias: Option<String>,
}

/// A `using` static-extension declaration. Hatchet has no lowering for static
/// extensions (they rewrite `a.f(b)` into `Module.f(a, b)` at the call site by
/// type), so the `line` is retained to report it as unsupported.
#[derive(Debug, Clone, PartialEq)]
pub struct Using {
    pub path: Vec<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub package: Vec<String>,
    pub imports: Vec<Import>,
    pub usings: Vec<Using>,
    pub decls: Vec<Decl>,
    /// File-level metadata that precedes no declaration — a class-less Haxe file
    /// (e.g. a `StdAfx.hx` carrying only `@:headerCode`). Empty for ordinary files.
    pub meta: Vec<Meta>,
}
