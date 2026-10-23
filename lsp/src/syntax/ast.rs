// Copyright 2026 Oxide Computer Company

//! P4-16 abstract syntax tree.
//!
//! Every node carries a `span` covering its tokens, and every declared or
//! referenced identifier is a [`Name`] with its own span. When a node's tokens
//! come from several files (an `#include` spliced into a body), its span
//! covers only the part in the file where the node starts.
//!
//! Scopes for name resolution:
//! - [`Program::decls`] is the top-level scope.
//! - [`ParserDecl`]: `type_params`, then `params` and `ctor_params`, then
//!   `locals` and `states` (state names are visible throughout the parser).
//!   Each [`ParserState::body`] is a nested scope.
//! - [`ControlDecl`]: `type_params`, `params` and `ctor_params`, `locals`;
//!   the `apply` block is a nested scope.
//! - [`ExternDecl`]: `type_params`; each [`MethodDecl`] adds its own
//!   `type_params` and `params`.
//! - [`FunctionDecl`] and [`ActionDecl`]: type parameters and parameters,
//!   then the body [`Block`]. Every [`Block`] is a scope; declarations in it
//!   appear as [`StmtKind::Decl`] and are visible from that point on.
//! - [`Instantiation::initializer`] is a scope holding the functions and
//!   instances that implement abstract methods.
//! - `for` loops scope their init declarations or loop variable.
//!
//! References appear as [`Path`] (in [`ExprKind::Path`], [`TypeKind::Named`],
//! table action references), as member names in [`ExprKind::Member`], as
//! state names in transitions and select cases, as match kinds in table keys,
//! and as argument names in [`Argument::name`].

use crate::lexer::Token;
use crate::source::Span;

/// An identifier with its location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub text: String,
    pub span: Span,
}

impl Name {
    /// Whether this is a placeholder for a name missing from the source.
    pub fn is_missing(&self) -> bool {
        self.text.is_empty()
    }
}

/// A reference to a declaration by name: `x`, or `.x` for the top-level
/// declaration named `x`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    /// Whether the name is prefixed with `.`.
    pub global: bool,
    pub name: Name,
    /// Covers the leading `.`, if any, and the name.
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationKind {
    /// `@name` with no body.
    Plain,
    /// `@name(tokens)`.
    Unstructured,
    /// `@name[expressions or key-value pairs]`.
    Structured,
    /// `@pragma name tokens` (to the end of the line).
    Pragma,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    /// The annotation's name; for `@pragma x ...` this is `x`.
    pub name: Name,
    pub kind: AnnotationKind,
    /// The raw tokens between the delimiters (or after the pragma name).
    pub body: Vec<Token>,
    /// Span of the body tokens, if any. Empty bodies have a zero-width span
    /// just inside the opening delimiter.
    pub body_span: Option<Span>,
    pub span: Span,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub decls: Vec<Decl>,
}

/// A declaration in any position: top level, parser or control locals,
/// extern initializer blocks, and (for `Const`, `Var`, and `Instantiation`)
/// statement blocks via [`StmtKind::Decl`].
#[derive(Clone, Debug, PartialEq)]
pub enum Decl {
    Const(ConstDecl),
    Var(VarDecl),
    /// `typedef` and `type`.
    Typedef(TypedefDecl),
    /// `struct`, `header`, and `header_union`.
    Struct(StructDecl),
    Enum(EnumDecl),
    /// `error { ... }`.
    Errors(MemberListDecl),
    /// `match_kind { ... }`.
    MatchKinds(MemberListDecl),
    Extern(ExternDecl),
    /// `extern T f(...);`
    ExternFunction(FunctionProto),
    Function(FunctionDecl),
    Action(ActionDecl),
    /// `parser P(...);`
    ParserType(BlockTypeDecl),
    Parser(ParserDecl),
    /// `control C(...);`
    ControlType(BlockTypeDecl),
    Control(ControlDecl),
    /// `package P(...);`
    Package(BlockTypeDecl),
    Instantiation(Instantiation),
    ValueSet(ValueSetDecl),
    Table(TableDecl),
    /// Tokens that could not be parsed as a declaration.
    Error(Span),
}

impl Decl {
    /// The declared name; `None` for `error`/`match_kind` lists (whose
    /// members are the declared names) and error placeholders.
    pub fn name(&self) -> Option<&Name> {
        Some(match self {
            Decl::Const(d) => &d.name,
            Decl::Var(d) => &d.name,
            Decl::Typedef(d) => &d.name,
            Decl::Struct(d) => &d.name,
            Decl::Enum(d) => &d.name,
            Decl::Extern(d) => &d.name,
            Decl::ExternFunction(d) => &d.name,
            Decl::Function(d) => &d.proto.name,
            Decl::Action(d) => &d.name,
            Decl::ParserType(d) | Decl::ControlType(d) | Decl::Package(d) => {
                &d.name
            }
            Decl::Parser(d) => &d.name,
            Decl::Control(d) => &d.name,
            Decl::Instantiation(d) => &d.name,
            Decl::ValueSet(d) => &d.name,
            Decl::Table(d) => &d.name,
            Decl::Errors(_) | Decl::MatchKinds(_) | Decl::Error(_) => {
                return None
            }
        })
    }

    /// The whole declaration, including annotations.
    pub fn span(&self) -> Span {
        match self {
            Decl::Const(d) => d.span,
            Decl::Var(d) => d.span,
            Decl::Typedef(d) => d.span,
            Decl::Struct(d) => d.span,
            Decl::Enum(d) => d.span,
            Decl::Errors(d) | Decl::MatchKinds(d) => d.span,
            Decl::Extern(d) => d.span,
            Decl::ExternFunction(d) => d.span,
            Decl::Function(d) => d.span,
            Decl::Action(d) => d.span,
            Decl::ParserType(d) | Decl::ControlType(d) | Decl::Package(d) => {
                d.span
            }
            Decl::Parser(d) => d.span,
            Decl::Control(d) => d.span,
            Decl::Instantiation(d) => d.span,
            Decl::ValueSet(d) => d.span,
            Decl::Table(d) => d.span,
            Decl::Error(s) => *s,
        }
    }

    pub fn annotations(&self) -> &[Annotation] {
        match self {
            Decl::Const(d) => &d.annotations,
            Decl::Var(d) => &d.annotations,
            Decl::Typedef(d) => &d.annotations,
            Decl::Struct(d) => &d.annotations,
            Decl::Enum(d) => &d.annotations,
            Decl::Extern(d) => &d.annotations,
            Decl::ExternFunction(d) => &d.annotations,
            Decl::Function(d) => &d.proto.annotations,
            Decl::Action(d) => &d.annotations,
            Decl::ParserType(d) | Decl::ControlType(d) | Decl::Package(d) => {
                &d.annotations
            }
            Decl::Parser(d) => &d.annotations,
            Decl::Control(d) => &d.annotations,
            Decl::Instantiation(d) => &d.annotations,
            Decl::ValueSet(d) => &d.annotations,
            Decl::Table(d) => &d.annotations,
            Decl::Errors(_) | Decl::MatchKinds(_) | Decl::Error(_) => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    None,
    In,
    Out,
    InOut,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub annotations: Vec<Annotation>,
    pub direction: Direction,
    pub ty: Type,
    pub name: Name,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConstDecl {
    pub annotations: Vec<Annotation>,
    pub ty: Type,
    pub name: Name,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VarDecl {
    pub annotations: Vec<Annotation>,
    pub ty: Type,
    pub name: Name,
    pub init: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypedefKind {
    /// `typedef T N;` — an alias.
    Typedef,
    /// `type T N;` — a new type.
    Type,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedefDecl {
    pub annotations: Vec<Annotation>,
    pub kind: TypedefKind,
    /// The aliased type. For `typedef struct S { ... } N;` this is a named
    /// reference to `S` and `inline_decl` holds the struct.
    pub ty: Type,
    pub inline_decl: Option<Box<Decl>>,
    pub name: Name,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructKind {
    Struct,
    Header,
    HeaderUnion,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub annotations: Vec<Annotation>,
    pub kind: StructKind,
    pub name: Name,
    pub type_params: Vec<Name>,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub annotations: Vec<Annotation>,
    pub ty: Type,
    pub name: Name,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub annotations: Vec<Annotation>,
    /// The underlying type of a serializable enum (`enum bit<8> E`).
    pub repr: Option<Type>,
    pub name: Name,
    pub members: Vec<EnumMember>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumMember {
    pub name: Name,
    pub value: Option<Expr>,
    pub span: Span,
}

/// `error { A, B }` or `match_kind { a, b }`; every member is a declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberListDecl {
    pub members: Vec<Name>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternDecl {
    pub annotations: Vec<Annotation>,
    pub name: Name,
    pub type_params: Vec<Name>,
    pub methods: Vec<MethodDecl>,
    /// `extern E;` with no body.
    pub forward: bool,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodDecl {
    pub annotations: Vec<Annotation>,
    pub is_abstract: bool,
    /// `None` for constructors.
    pub ret: Option<Type>,
    pub name: Name,
    pub type_params: Vec<Name>,
    pub params: Vec<Param>,
    pub span: Span,
}

impl MethodDecl {
    pub fn is_constructor(&self) -> bool {
        self.ret.is_none()
    }
}

/// A function signature: extern functions and the head of
/// [`FunctionDecl`].
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionProto {
    pub annotations: Vec<Annotation>,
    pub ret: Type,
    pub name: Name,
    pub type_params: Vec<Name>,
    pub params: Vec<Param>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDecl {
    pub proto: FunctionProto,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActionDecl {
    pub annotations: Vec<Annotation>,
    pub name: Name,
    pub params: Vec<Param>,
    pub body: Block,
    pub span: Span,
}

/// Parser, control, and package type declarations.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockTypeDecl {
    pub annotations: Vec<Annotation>,
    pub name: Name,
    pub type_params: Vec<Name>,
    pub params: Vec<Param>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParserDecl {
    pub annotations: Vec<Annotation>,
    pub name: Name,
    pub type_params: Vec<Name>,
    pub params: Vec<Param>,
    pub ctor_params: Vec<Param>,
    pub locals: Vec<Decl>,
    pub states: Vec<ParserState>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParserState {
    pub annotations: Vec<Annotation>,
    pub name: Name,
    pub body: Vec<Stmt>,
    pub transition: Option<Transition>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    pub kind: TransitionKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransitionKind {
    /// `transition s;`
    Direct(Name),
    Select(Select),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub exprs: Vec<Expr>,
    pub cases: Vec<SelectCase>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectCase {
    pub keyset: Keyset,
    /// The target state.
    pub state: Name,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Keyset {
    pub kind: KeysetKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum KeysetKind {
    /// A value, constant, or value set name.
    Expr(Expr),
    /// `value &&& mask`
    Mask(Expr, Expr),
    /// `lo .. hi`
    Range(Expr, Expr),
    Default,
    /// `_`
    DontCare,
    /// `(k1, k2, ...)`
    Tuple(Vec<Keyset>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlDecl {
    pub annotations: Vec<Annotation>,
    pub name: Name,
    pub type_params: Vec<Name>,
    pub params: Vec<Param>,
    pub ctor_params: Vec<Param>,
    pub locals: Vec<Decl>,
    /// `None` if the body has no `apply` block. p4c requires one, x4c does
    /// not; the parser reports nothing.
    pub apply: Option<Block>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Instantiation {
    pub annotations: Vec<Annotation>,
    /// The instantiated type; for an instance array `E() a[N];` this is a
    /// [`TypeKind::Stack`] of `E`.
    pub ty: Type,
    pub args: Vec<Argument>,
    pub name: Name,
    /// `= { ... }`: functions and instances implementing abstract methods.
    pub initializer: Option<Vec<Decl>>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValueSetDecl {
    pub annotations: Vec<Annotation>,
    pub elem_ty: Type,
    pub size: Expr,
    pub name: Name,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableDecl {
    pub annotations: Vec<Annotation>,
    pub name: Name,
    pub properties: Vec<TableProperty>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableProperty {
    pub annotations: Vec<Annotation>,
    pub is_const: bool,
    pub name: Name,
    pub value: PropertyValue,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PropertyValue {
    Key(Vec<KeyElement>),
    Actions(Vec<ActionRef>),
    Entries(Vec<Entry>),
    /// Any other property, such as `size`, `default_action` (whose value is
    /// a path or call expression), or `implementation`.
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyElement {
    pub expr: Expr,
    pub match_kind: Name,
    pub annotations: Vec<Annotation>,
    pub span: Span,
}

/// An action in an `actions` list or a table entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionRef {
    pub annotations: Vec<Annotation>,
    pub name: Path,
    pub args: Option<Vec<Argument>>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Annotations after the action (`... : a() @name("x");`).
    pub annotations: Vec<Annotation>,
    pub is_const: bool,
    /// `priority = n:`
    pub priority: Option<Expr>,
    pub keyset: Keyset,
    pub action: ActionRef,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Statements

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub annotations: Vec<Annotation>,
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    AddSat,
    SubSat,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    /// A local constant, variable, or instantiation.
    Decl(Decl),
    Assign {
        lhs: Expr,
        op: AssignOp,
        rhs: Expr,
    },
    /// A call statement; the expression is an [`ExprKind::Call`].
    Call(Expr),
    /// An expression that is neither a call nor an assignment (an error has
    /// been reported); kept so its names can still be resolved.
    Expr(Expr),
    If {
        cond: Expr,
        then: Box<Stmt>,
        els: Option<Box<Stmt>>,
    },
    Block(Block),
    Switch {
        expr: Expr,
        cases: Vec<SwitchCase>,
    },
    Return(Option<Expr>),
    Exit,
    Break,
    Continue,
    Empty,
    /// `for (init; cond; update) body`; `init` holds declarations and
    /// assignment/call statements.
    For {
        annotations: Vec<Annotation>,
        init: Vec<Stmt>,
        cond: Option<Expr>,
        update: Vec<Stmt>,
        body: Box<Stmt>,
    },
    /// `for (T x in collection) body`; a range collection is a
    /// [`BinOp::Range`] expression.
    ForIn {
        annotations: Vec<Annotation>,
        var: VarDecl,
        collection: Expr,
        body: Box<Stmt>,
    },
    /// A `transition` nested in a block inside a parser state, as x4c
    /// allows (`if (c) { transition a; }`). A state's final transition is
    /// [`ParserState::transition`] instead.
    Transition(Transition),
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SwitchCase {
    pub label: SwitchLabel,
    /// `None` for a fallthrough label (`a:` followed by another label).
    pub body: Option<Block>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SwitchLabel {
    Default(Span),
    /// An action name or an expression such as an enum member.
    Expr(Expr),
}

// ---------------------------------------------------------------------------
// Expressions

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

/// An integer literal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntLit {
    pub text: String,
    /// The value, or `None` if it does not fit in 128 bits.
    pub value: Option<u128>,
    /// The width of `8w5` or `8s5`; `None` for unsized literals.
    pub width: Option<u32>,
    /// Whether the literal has an `s` width prefix.
    pub signed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Not,
    BitNot,
    Neg,
    Plus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Mul,
    Div,
    Mod,
    Add,
    Sub,
    AddSat,
    SubSat,
    Concat,
    Shl,
    Shr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    And,
    Or,
    /// `lo .. hi`, only in `for (x in lo .. hi)`.
    Range,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Int(IntLit),
    Bool(bool),
    /// A string literal's contents without quotes, escapes unprocessed.
    Str(String),
    /// A name or `.name`. `error` (as in `error.NoError`) is a path whose
    /// name is `error`.
    Path(Path),
    /// `base.member`; `member` may be missing (empty) after a trailing dot.
    Member {
        base: Box<Expr>,
        member: Name,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    /// `base[hi:lo]`
    Slice {
        base: Box<Expr>,
        hi: Box<Expr>,
        lo: Box<Expr>,
    },
    /// `base[start +: width]`
    PlusSlice {
        base: Box<Expr>,
        start: Box<Expr>,
        width: Box<Expr>,
    },
    /// A function, method, or constructor call. Constructor calls such as
    /// `Register<bit<8>>(4)` have a type name as the callee path.
    Call {
        callee: Box<Expr>,
        type_args: Vec<Type>,
        args: Vec<Argument>,
    },
    /// A name with type arguments used as the base of a member access, as
    /// in `MyParser<bit<8>>.apply(...)`.
    Specialized {
        base: Box<Expr>,
        type_args: Vec<Type>,
    },
    Cast {
        ty: Type,
        expr: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Ternary {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
    /// `{a, b}`
    List(Vec<Expr>),
    /// `{f = 1, g = 2}`, with `dots` for a trailing `...`.
    Struct {
        fields: Vec<NamedExpr>,
        dots: bool,
    },
    /// `{#}`
    Invalid,
    /// `...`
    Dots,
    This,
    /// `_`
    DontCare,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NamedExpr {
    pub name: Name,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Argument {
    /// The parameter name for `f(x = 1)`.
    pub name: Option<Name>,
    /// The value; [`ExprKind::DontCare`] for `_`.
    pub value: Expr,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Types

#[derive(Clone, Debug, PartialEq)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    /// `bit` or `bit<W>`.
    Bit(Option<Box<Expr>>),
    /// `int` or `int<W>`.
    Int(Option<Box<Expr>>),
    Varbit(Box<Expr>),
    Bool,
    String,
    Void,
    /// The `error` type.
    ErrorType,
    MatchKind,
    /// `_`
    DontCare,
    /// A named type, possibly with type arguments: `T`, `.T`,
    /// `register<bit<32>, bit<8>>`.
    Named {
        path: Path,
        args: Vec<Type>,
    },
    /// A header stack `T[N]`.
    Stack {
        elem: Box<Type>,
        size: Box<Expr>,
    },
    Tuple(Vec<Type>),
    List(Box<Type>),
    Error,
}
