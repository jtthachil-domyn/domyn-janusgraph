//! Abstract Syntax Tree for a subset of openCypher.
//!
//! Supports the patterns needed by GraphRAG:
//!   MATCH (n:Label)-[:REL]->(m:Label)
//!   WHERE n.prop = value
//!   RETURN n, m
//!   LIMIT k
//! Plus mutations:
//!   CREATE (n:Label {k: v})
//!   CREATE (a)-[:REL]->(b)
//!   MATCH (n) SET n.prop = value
//!   MATCH (n) REMOVE n.prop
//!   MERGE (n:Label {k: v})
//!   MATCH (n) [WHERE ...] [DETACH] DELETE n

/// Top-level parsed statement. Reads and writes share lexer/parser but
/// diverge at the planner and executor.
#[derive(Debug, Clone)]
pub enum Statement {
    Read(Query),
    Write(WriteQuery),
}

/// A read-only Cypher query.
#[derive(Debug, Clone)]
pub struct Query {
    pub match_clause: Option<MatchClause>,
    pub where_clause: Option<WhereClause>,
    /// Additional clauses between the initial MATCH/WHERE and the final
    /// RETURN: WITH, UNWIND, subsequent MATCH / OPTIONAL MATCH, WHERE.
    pub tail: Vec<ReadClause>,
    pub return_clause: ReturnClause,
    pub order_by: Option<OrderByClause>,
    pub limit: Option<RowCount>,
    pub skip: Option<RowCount>,
    /// If set, this query is the left side of a UNION [ALL] with another
    /// query. Represented recursively so UNION chains form a right-leaning
    /// list.
    pub union: Option<Box<UnionTail>>,
}

#[derive(Debug, Clone)]
pub struct UnionTail {
    pub all: bool,
    pub right: Query,
}

/// A read-side clause appearing in the body of a query between the initial
/// MATCH and the final RETURN. Covers Cypher's linear-composition model.
#[derive(Debug, Clone)]
pub enum ReadClause {
    Match { optional: bool, clause: MatchClause },
    Where(WhereClause),
    With(WithClause),
    Unwind { expr: Expr, alias: String },
}

#[derive(Debug, Clone)]
pub struct WithClause {
    pub items: Vec<ReturnItem>,
    pub distinct: bool,
    pub where_clause: Option<WhereClause>,
    pub order_by: Option<OrderByClause>,
    pub skip: Option<RowCount>,
    pub limit: Option<RowCount>,
}

/// A write query: optional MATCH/WHERE to bind rows, followed by one or
/// more mutation clauses, optionally followed by RETURN.
#[derive(Debug, Clone)]
pub struct WriteQuery {
    pub match_clause: Option<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub tail: Vec<ReadClause>,
    pub mutations: Vec<MutationClause>,
    pub return_clause: Option<ReturnClause>,
    pub order_by: Option<OrderByClause>,
    pub skip: Option<RowCount>,
    pub limit: Option<RowCount>,
}

#[derive(Debug, Clone)]
pub enum RowCount {
    Literal(u64),
    Parameter(String),
    Expr(Box<Expr>),
}

impl PartialEq for RowCount {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Literal(left), Self::Literal(right)) => left == right,
            (Self::Parameter(left), Self::Parameter(right)) => left == right,
            (Self::Expr(left), Self::Expr(right)) => format!("{left:?}") == format!("{right:?}"),
            _ => false,
        }
    }
}

impl Eq for RowCount {}

#[derive(Debug, Clone)]
pub enum MutationClause {
    /// Read-side row-transform clause inside a read/write pipeline, e.g.
    /// `CREATE ... WITH ... UNWIND ... CREATE`.
    Read(ReadClause),
    /// CREATE (a:Label {k: v}), (b)-[:REL]->(c), ...
    Create { patterns: Vec<Pattern> },
    /// MERGE (a:Label {k: v}), (a)-[:REL]->(b)
    Merge {
        patterns: Vec<Pattern>,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    /// SET a.name = 'Alice', r.weight = 0.5
    Set { items: Vec<SetItem> },
    /// REMOVE a.name, r.weight
    Remove { items: Vec<RemoveItem> },
    /// [DETACH] DELETE a, b, c
    Delete { targets: Vec<Expr>, detach: bool },
}

#[derive(Debug, Clone)]
pub enum SetItem {
    /// `SET a.name = 'Alice'`
    Property { target: PropertyAccess, value: Expr },
    /// `SET n = {k: v}` or `SET n += {k: v}`.
    Properties {
        variable: String,
        value: Expr,
        replace: bool,
    },
    /// `SET n:Foo:Bar` — add labels to a node binding.
    Labels {
        variable: String,
        labels: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub enum RemoveItem {
    Property(PropertyAccess),
    /// `REMOVE n:Foo:Bar` — remove labels from a node binding.
    Labels {
        variable: String,
        labels: Vec<String>,
    },
}

/// MATCH clause: one or more pattern chains.
#[derive(Debug, Clone)]
pub struct MatchClause {
    pub patterns: Vec<Pattern>,
}

/// A pattern is a chain of nodes and relationships:
/// (a:Entity)-[:DISCLOSES]->(b:Metric)
#[derive(Debug, Clone)]
pub struct Pattern {
    pub path_variable: Option<String>,
    pub elements: Vec<PatternElement>,
}

#[derive(Debug, Clone)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelationshipPattern),
}

#[derive(Debug, Clone)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: Vec<(String, Expr)>,
    pub properties_specified: bool,
}

#[derive(Debug, Clone)]
pub struct RelationshipPattern {
    pub variable: Option<String>,
    pub rel_types: Vec<String>,
    pub properties: Vec<(String, Expr)>,
    pub direction: RelDirection,
    pub min_hops: Option<u32>,
    pub max_hops: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RelDirection {
    Outgoing, // -[r:TYPE]->
    Incoming, // <-[r:TYPE]-
    Both,     // -[r:TYPE]-
}

/// WHERE clause: a boolean expression tree.
#[derive(Debug, Clone)]
pub struct WhereClause {
    pub expr: Expr,
}

/// Expression types.
#[derive(Debug, Clone)]
pub enum Expr {
    Literal(Literal),
    Property(PropertyAccess),
    Variable(String),
    BinaryOp {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
    Parameter(String),
    /// `[e1, e2, ...]`
    List(Vec<Expr>),
    /// `[x IN xs WHERE pred | proj]`
    ListComprehension {
        variable: String,
        list: Box<Expr>,
        predicate: Option<Box<Expr>>,
        projection: Option<Box<Expr>>,
    },
    /// `{k1: v1, k2: v2, ...}`
    Map(Vec<(String, Expr)>),
    /// `e IN list`
    In {
        expr: Box<Expr>,
        list: Box<Expr>,
    },
    /// `xs[i]`
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    /// `xs[start..end]`
    Slice {
        target: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
    },
    /// Existential pattern predicate: `(n)-[:REL]->()`.
    PatternPredicate(Pattern),
    /// Pattern comprehension: `[p = (n)-[:REL]->() | expr]`.
    PatternComprehension {
        variable: Option<String>,
        pattern: Pattern,
        projection: Box<Expr>,
    },
    /// CASE WHEN...THEN...ELSE...END
    Case {
        scrutinee: Option<Box<Expr>>,
        arms: Vec<(Expr, Expr)>,
        default: Option<Box<Expr>>,
    },
    /// `count(*)`
    CountStar,
    /// `EXISTS(expr)` — legacy function-form exists.
    Exists(Box<Expr>),
    /// `EXISTS { MATCH ... }` or `EXISTS { (n)-->() }`.
    /// Evaluated as a correlated read subquery against the current row.
    ExistsSubquery(Box<Query>),
    /// `any/all/none/single(x IN xs WHERE pred)` — list predicates with a
    /// bound iteration variable scoped over the predicate.
    ListPredicate {
        kind: ListPredicateKind,
        variable: String,
        list: Box<Expr>,
        predicate: Option<Box<Expr>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ListPredicateKind {
    Any,
    All,
    None,
    Single,
}

#[derive(Debug, Clone)]
pub enum Literal {
    Integer(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone)]
pub struct PropertyAccess {
    pub variable: String,
    pub property: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    And,
    Or,
    Xor,
    Contains,
    StartsWith,
    EndsWith,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Not,
    IsNull,
    IsNotNull,
}

/// RETURN clause.
#[derive(Debug, Clone)]
pub struct ReturnClause {
    pub items: Vec<ReturnItem>,
    pub distinct: bool,
}

#[derive(Debug, Clone)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
    /// Original projection text for unaliased output-column names. Cypher
    /// clients expect `RETURN cOuNt( * )` to report exactly that expression
    /// text as the column title unless an explicit `AS` alias is present.
    pub raw: Option<String>,
}

/// ORDER BY clause.
#[derive(Debug, Clone)]
pub struct OrderByClause {
    pub items: Vec<OrderByItem>,
}

#[derive(Debug, Clone)]
pub struct OrderByItem {
    pub expr: Expr,
    pub descending: bool,
}
