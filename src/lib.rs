//! Hatchet — a Haxe 4.x → C++98 transpiler for legacy platforms.
//!
//! A transpiler from Haxe 4.x to C++98 source that compiles under Visual C++ 6.0
//! (and so targets Windows 9x and older Unix toolchains). This is a *transpiler*,
//! not a compiler: it emits portable C++ source and never produces a custom C++
//! runtime — every Haxe construct maps to a hand-writable C++ idiom. The
//! transpilation rules live in `SKILL.md`.
//!
//! Pipeline:
//!   discover → lex → parse → semantic analysis → code generation

pub mod ast;
pub mod cli;
pub mod codegen;
pub mod diag;
pub mod discover;
pub mod finals;
pub mod lexer;
pub mod parser;
pub mod scan;
pub mod sema;
pub mod stdafx;
