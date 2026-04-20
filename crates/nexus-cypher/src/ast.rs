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
    pub limit: Option<u64>,
    pub skip: Option<u64>,
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
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}

/// A write query: optional MATCH/WHERE to bind rows, followed by one or
/// more mutation clauses, optionally followed by RETURN.
#[derive(Debug, Clone)]
pub struct WriteQuery {
    pub match_clause: Option<MatchClause>,
    pub where_clause: Option<WhereClause>,
    pub mutations: Vec<MutationClause>,
    pub return_clause: Option<ReturnClause>,
}

#[derive(Debug, Clone)]
pub enum MutationClause {
    /// CREATE (a:Label {k: v}), (b)-[:REL]->(c), ...
    Create { patterns: Vec<Pattern> },
    /// MERGE (a:Label {k: v}), (a)-[:REL]->(b)
    Merge { patterns: Vec<Pattern> },
    /// SET a.name = 'Alice', r.weight = 0.5
    Set { items: Vec<SetItem> },
    /// REMOVE a.name, r.weight
    Remove { items: Vec<RemoveItem> },
    /// [DETACH] DELETE a, b, c
    Delete {
        variables: Vec<String>,
        detach: bool,
    },
}

#[derive(Debug, Clone)]
pub struct SetItem {
    pub target: PropertyAccess,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub enum RemoveItem {
    Property(PropertyAccess),
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
    /// CASE WHEN...THEN...ELSE...END
    Case {
        scrutinee: Option<Box<Expr>>,
        arms: Vec<(Expr, Expr)>,
        default: Option<Box<Expr>>,
    },
    /// `count(*)`
    CountStar,
    /// `EXISTS { MATCH ... }` — full pattern predicates not evaluated yet;
    /// captured so the query can parse and reach the planner.
    Exists(Box<Expr>),
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
