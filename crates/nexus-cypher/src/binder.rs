//! Binder and scope validation for parsed Cypher ASTs.
//!
//! The binder sits between parsing and planning. It validates variable
//! visibility, records coarse variable kinds, and provides a home for Cypher
//! semantic checks that do not belong in the parser or physical planner.

use crate::ast::*;
use crate::error::{CypherError, CypherResult};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Node,
    Relationship,
    Path,
    Scalar,
    List,
    Map,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticType {
    Boolean,
    Null,
    Number,
    String,
    List,
    Map,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    variables: BTreeMap<String, VarKind>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, kind: VarKind) {
        self.variables.insert(name.into(), kind);
    }

    pub fn get(&self, name: &str) -> Option<VarKind> {
        self.variables.get(name).copied()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.variables.contains_key(name)
    }

    pub fn variables(&self) -> &BTreeMap<String, VarKind> {
        &self.variables
    }
}

#[derive(Debug, Clone)]
pub struct BoundQuery {
    pub final_scope: Scope,
    pub segments: Vec<BoundSegment>,
}

#[derive(Debug, Clone)]
pub struct BoundWriteQuery {
    pub final_scope: Scope,
    pub segments: Vec<BoundSegment>,
}

#[derive(Debug, Clone)]
pub struct BoundSegment {
    pub kind: BoundSegmentKind,
    pub scope: Scope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundSegmentKind {
    Match,
    OptionalMatch,
    Where,
    With,
    Unwind,
    Return,
    Create,
    Merge,
    Set,
    Remove,
    Delete,
}

pub fn bind_statement(statement: &Statement) -> CypherResult<()> {
    match statement {
        Statement::Read(query) => bind_query(query).map(|_| ()),
        Statement::Write(query) => bind_write(query).map(|_| ()),
    }
}

pub fn bind_query(query: &Query) -> CypherResult<BoundQuery> {
    let mut scope = Scope::new();
    let mut segments = Vec::new();

    if let Some(match_clause) = &query.match_clause {
        bind_match_clause(&mut scope, match_clause);
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Match,
            scope: scope.clone(),
        });
    }

    if let Some(where_clause) = &query.where_clause {
        validate_expr(&scope, &where_clause.expr)?;
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Where,
            scope: scope.clone(),
        });
    }

    for clause in &query.tail {
        match clause {
            ReadClause::Match { optional, clause } => {
                bind_match_clause(&mut scope, clause);
                segments.push(BoundSegment {
                    kind: if *optional {
                        BoundSegmentKind::OptionalMatch
                    } else {
                        BoundSegmentKind::Match
                    },
                    scope: scope.clone(),
                });
            }
            ReadClause::Where(where_clause) => {
                validate_expr(&scope, &where_clause.expr)?;
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Where,
                    scope: scope.clone(),
                });
            }
            ReadClause::With(with_clause) => {
                scope = bind_with_clause(&scope, with_clause)?;
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::With,
                    scope: scope.clone(),
                });
            }
            ReadClause::Unwind { expr, alias } => {
                validate_expr(&scope, expr)?;
                scope.insert(alias, VarKind::Unknown);
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Unwind,
                    scope: scope.clone(),
                });
            }
        }
    }

    bind_return_clause(&scope, &query.return_clause)?;
    let final_scope = bind_projection_scope(&scope, &query.return_clause)?;
    segments.push(BoundSegment {
        kind: BoundSegmentKind::Return,
        scope: final_scope.clone(),
    });

    if let Some(order_by) = &query.order_by {
        validate_order_by(&scope, &final_scope, order_by)?;
    }

    if let Some(union) = &query.union {
        bind_query(&union.right)?;
    }

    Ok(BoundQuery {
        final_scope,
        segments,
    })
}

pub fn bind_write(query: &WriteQuery) -> CypherResult<BoundWriteQuery> {
    let mut scope = Scope::new();
    let mut segments = Vec::new();

    if let Some(match_clause) = &query.match_clause {
        bind_match_clause(&mut scope, match_clause);
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Match,
            scope: scope.clone(),
        });
    }

    if let Some(where_clause) = &query.where_clause {
        validate_expr(&scope, &where_clause.expr)?;
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Where,
            scope: scope.clone(),
        });
    }

    for mutation in &query.mutations {
        match mutation {
            MutationClause::Create { patterns } => {
                bind_mutation_patterns(&mut scope, patterns);
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Create,
                    scope: scope.clone(),
                });
            }
            MutationClause::Merge { patterns } => {
                bind_mutation_patterns(&mut scope, patterns);
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Merge,
                    scope: scope.clone(),
                });
            }
            MutationClause::Set { items } => {
                for item in items {
                    require_var(&scope, &item.target.variable)?;
                    validate_expr(&scope, &item.value)?;
                }
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Set,
                    scope: scope.clone(),
                });
            }
            MutationClause::Remove { items } => {
                for item in items {
                    match item {
                        RemoveItem::Property(pa) => require_var(&scope, &pa.variable)?,
                    }
                }
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Remove,
                    scope: scope.clone(),
                });
            }
            MutationClause::Delete { variables, .. } => {
                for var in variables {
                    require_var(&scope, var)?;
                }
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Delete,
                    scope: scope.clone(),
                });
            }
        }
    }

    let final_scope = if let Some(return_clause) = &query.return_clause {
        bind_return_clause(&scope, return_clause)?;
        bind_projection_scope(&scope, return_clause)?
    } else {
        scope.clone()
    };

    Ok(BoundWriteQuery {
        final_scope,
        segments,
    })
}

fn bind_with_clause(input_scope: &Scope, with_clause: &WithClause) -> CypherResult<Scope> {
    bind_return_clause(
        input_scope,
        &ReturnClause {
            items: with_clause.items.clone(),
            distinct: with_clause.distinct,
        },
    )?;

    let projected = bind_projection_scope(
        input_scope,
        &ReturnClause {
            items: with_clause.items.clone(),
            distinct: with_clause.distinct,
        },
    )?;

    if let Some(where_clause) = &with_clause.where_clause {
        validate_expr(&projected, &where_clause.expr)?;
    }

    if let Some(order_by) = &with_clause.order_by {
        validate_order_by(input_scope, &projected, order_by)?;
    }

    Ok(projected)
}

fn bind_return_clause(scope: &Scope, return_clause: &ReturnClause) -> CypherResult<()> {
    let has_aggregate = return_clause
        .items
        .iter()
        .any(|item| contains_aggregate(&item.expr));
    for item in &return_clause.items {
        validate_expr(scope, &item.expr)?;
        if has_aggregate {
            validate_aggregate_shape(&item.expr)?;
        }
    }
    Ok(())
}

fn bind_projection_scope(input_scope: &Scope, return_clause: &ReturnClause) -> CypherResult<Scope> {
    let mut projected = Scope::new();
    for item in &return_clause.items {
        let alias = item.alias.clone().unwrap_or_else(|| expr_alias(&item.expr));
        projected.insert(alias, infer_expr_kind(input_scope, &item.expr));
    }
    Ok(projected)
}

fn validate_order_by(
    input_scope: &Scope,
    projected_scope: &Scope,
    order_by: &OrderByClause,
) -> CypherResult<()> {
    for item in &order_by.items {
        validate_expr_either_scope(input_scope, projected_scope, &item.expr)?;
    }
    Ok(())
}

fn bind_match_clause(scope: &mut Scope, clause: &MatchClause) {
    for pattern in &clause.patterns {
        bind_pattern(scope, pattern);
    }
}

fn bind_mutation_patterns(scope: &mut Scope, patterns: &[Pattern]) {
    for pattern in patterns {
        bind_pattern(scope, pattern);
    }
}

fn bind_pattern(scope: &mut Scope, pattern: &Pattern) {
    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(var) = &node.variable {
                    scope.insert(var, VarKind::Node);
                }
            }
            PatternElement::Relationship(rel) => {
                if let Some(var) = &rel.variable {
                    scope.insert(var, VarKind::Relationship);
                }
            }
        }
    }
}

fn validate_expr(scope: &Scope, expr: &Expr) -> CypherResult<()> {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::CountStar => Ok(()),
        Expr::Variable(var) => require_var(scope, var),
        Expr::Property(pa) => require_var(scope, &pa.variable),
        Expr::BinaryOp { left, op, right } => {
            validate_expr(scope, left)?;
            validate_expr(scope, right)?;
            match op {
                BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                    validate_boolean_operand(left, *op)?;
                    validate_boolean_operand(right, *op)
                }
                _ => Ok(()),
            }
        }
        Expr::UnaryOp { op, expr } => {
            validate_expr(scope, expr)?;
            match op {
                UnaryOp::Not => validate_boolean_operand(expr, *op),
                UnaryOp::IsNull | UnaryOp::IsNotNull => Ok(()),
            }
        }
        Expr::FunctionCall { args, .. } | Expr::List(args) => {
            for arg in args {
                validate_expr(scope, arg)?;
            }
            Ok(())
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                validate_expr(scope, value)?;
            }
            Ok(())
        }
        Expr::In { expr, list } => {
            validate_expr(scope, expr)?;
            validate_expr(scope, list)
        }
        Expr::Index { target, index } => {
            validate_expr(scope, target)?;
            validate_expr(scope, index)
        }
        Expr::Slice { target, start, end } => {
            validate_expr(scope, target)?;
            if let Some(start) = start {
                validate_expr(scope, start)?;
            }
            if let Some(end) = end {
                validate_expr(scope, end)?;
            }
            Ok(())
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            if let Some(scrutinee) = scrutinee {
                validate_expr(scope, scrutinee)?;
            }
            for (when_expr, then_expr) in arms {
                validate_expr(scope, when_expr)?;
                validate_boolean_context(when_expr, "CASE WHEN")?;
                validate_expr(scope, then_expr)?;
            }
            if let Some(default) = default {
                validate_expr(scope, default)?;
            }
            Ok(())
        }
        Expr::Exists(inner) => validate_expr(scope, inner),
        Expr::ListPredicate {
            variable,
            list,
            predicate,
            ..
        } => {
            validate_expr(scope, list)?;
            if let Some(pred) = predicate {
                let mut local = scope.clone();
                local.insert(variable.clone(), VarKind::Scalar);
                validate_expr(&local, pred)?;
                validate_boolean_context(pred, "list predicate WHERE")?;
            }
            Ok(())
        }
    }
}

fn validate_boolean_context(expr: &Expr, context: &str) -> CypherResult<()> {
    match static_expr_type(expr) {
        StaticType::Boolean | StaticType::Null | StaticType::Unknown => Ok(()),
        other => Err(CypherError::Plan(format!(
            "{context} requires a boolean expression, got {other:?}"
        ))),
    }
}

fn validate_boolean_operand<T: std::fmt::Debug>(expr: &Expr, op: T) -> CypherResult<()> {
    match static_expr_type(expr) {
        StaticType::Boolean | StaticType::Null | StaticType::Unknown => Ok(()),
        other => Err(CypherError::Plan(format!(
            "{op:?} requires boolean operands, got {other:?}"
        ))),
    }
}

fn static_expr_type(expr: &Expr) -> StaticType {
    match expr {
        Expr::Literal(Literal::Bool(_)) => StaticType::Boolean,
        Expr::Literal(Literal::Null) => StaticType::Null,
        Expr::Literal(Literal::Integer(_) | Literal::Float(_)) => StaticType::Number,
        Expr::Literal(Literal::String(_)) => StaticType::String,
        Expr::List(_) => StaticType::List,
        Expr::Map(_) => StaticType::Map,
        Expr::UnaryOp {
            op: UnaryOp::Not | UnaryOp::IsNull | UnaryOp::IsNotNull,
            ..
        }
        | Expr::BinaryOp {
            op:
                BinaryOp::Eq
                | BinaryOp::Neq
                | BinaryOp::Lt
                | BinaryOp::Lte
                | BinaryOp::Gt
                | BinaryOp::Gte
                | BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Xor
                | BinaryOp::Contains
                | BinaryOp::StartsWith
                | BinaryOp::EndsWith,
            ..
        }
        | Expr::In { .. }
        | Expr::Exists(_)
        | Expr::ListPredicate { .. } => StaticType::Boolean,
        Expr::FunctionCall { name, .. } if matches!(name.to_lowercase().as_str(), "exists") => {
            StaticType::Boolean
        }
        Expr::FunctionCall { name, .. }
            if matches!(
                name.to_lowercase().as_str(),
                "labels" | "keys" | "nodes" | "relationships" | "range" | "tail"
            ) =>
        {
            StaticType::List
        }
        Expr::FunctionCall { name, .. } if matches!(name.to_lowercase().as_str(), "properties") => {
            StaticType::Map
        }
        Expr::FunctionCall { name, .. }
            if matches!(
                name.to_lowercase().as_str(),
                "id" | "size"
                    | "length"
                    | "tointeger"
                    | "tofloat"
                    | "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
            ) =>
        {
            StaticType::Number
        }
        Expr::FunctionCall { name, .. }
            if matches!(name.to_lowercase().as_str(), "tostring" | "type") =>
        {
            StaticType::String
        }
        Expr::CountStar => StaticType::Number,
        Expr::Variable(_)
        | Expr::Property(_)
        | Expr::Parameter(_)
        | Expr::BinaryOp { .. }
        | Expr::FunctionCall { .. }
        | Expr::Index { .. }
        | Expr::Slice { .. }
        | Expr::Case { .. } => StaticType::Unknown,
    }
}

fn validate_expr_either_scope(
    left_scope: &Scope,
    right_scope: &Scope,
    expr: &Expr,
) -> CypherResult<()> {
    validate_expr(right_scope, expr).or_else(|_| validate_expr(left_scope, expr))
}

fn require_var(scope: &Scope, var: &str) -> CypherResult<()> {
    if scope.contains(var) {
        Ok(())
    } else {
        Err(CypherError::Plan(format!("undefined variable '{var}'")))
    }
}

fn infer_expr_kind(scope: &Scope, expr: &Expr) -> VarKind {
    match expr {
        Expr::Variable(var) => scope.get(var).unwrap_or(VarKind::Unknown),
        Expr::List(_) => VarKind::List,
        Expr::Map(_) => VarKind::Map,
        Expr::FunctionCall { name, .. }
            if matches!(name.to_lowercase().as_str(), "nodes" | "relationships") =>
        {
            VarKind::List
        }
        Expr::FunctionCall { .. }
        | Expr::Property(_)
        | Expr::Literal(_)
        | Expr::Parameter(_)
        | Expr::BinaryOp { .. }
        | Expr::UnaryOp { .. }
        | Expr::In { .. }
        | Expr::Index { .. }
        | Expr::Slice { .. }
        | Expr::Case { .. }
        | Expr::CountStar
        | Expr::Exists(_)
        | Expr::ListPredicate { .. } => VarKind::Scalar,
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, args } => {
            matches!(
                name.to_lowercase().as_str(),
                "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
                    | "collect"
                    | "percentiledisc"
                    | "percentilecont"
            ) || args.iter().any(contains_aggregate)
        }
        Expr::CountStar => true,
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { expr, .. } => contains_aggregate(expr),
        Expr::List(items) => items.iter().any(contains_aggregate),
        Expr::Map(entries) => entries.iter().any(|(_, expr)| contains_aggregate(expr)),
        Expr::In { expr, list } => contains_aggregate(expr) || contains_aggregate(list),
        Expr::Index { target, index } => contains_aggregate(target) || contains_aggregate(index),
        Expr::Slice { target, start, end } => {
            contains_aggregate(target)
                || start.as_deref().is_some_and(contains_aggregate)
                || end.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee.as_deref().is_some_and(contains_aggregate)
                || arms.iter().any(|(when_expr, then_expr)| {
                    contains_aggregate(when_expr) || contains_aggregate(then_expr)
                })
                || default.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Exists(inner) => contains_aggregate(inner),
        Expr::ListPredicate {
            list, predicate, ..
        } => contains_aggregate(list) || predicate.as_deref().is_some_and(contains_aggregate),
        Expr::Literal(_) | Expr::Property(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
    }
}

fn validate_aggregate_shape(expr: &Expr) -> CypherResult<()> {
    match expr {
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                if contains_aggregate(arg) {
                    return Err(CypherError::Plan(
                        "nested aggregate expressions are not supported".into(),
                    ));
                }
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_aggregate_shape(left)?;
            validate_aggregate_shape(right)
        }
        Expr::UnaryOp { expr, .. } => validate_aggregate_shape(expr),
        Expr::List(items) => {
            for item in items {
                validate_aggregate_shape(item)?;
            }
            Ok(())
        }
        Expr::Map(entries) => {
            for (_, item) in entries {
                validate_aggregate_shape(item)?;
            }
            Ok(())
        }
        Expr::In { expr, list } => {
            validate_aggregate_shape(expr)?;
            validate_aggregate_shape(list)
        }
        Expr::Index { target, index } => {
            validate_aggregate_shape(target)?;
            validate_aggregate_shape(index)
        }
        Expr::Slice { target, start, end } => {
            validate_aggregate_shape(target)?;
            if let Some(start) = start {
                validate_aggregate_shape(start)?;
            }
            if let Some(end) = end {
                validate_aggregate_shape(end)?;
            }
            Ok(())
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            if let Some(scrutinee) = scrutinee {
                validate_aggregate_shape(scrutinee)?;
            }
            for (when_expr, then_expr) in arms {
                validate_aggregate_shape(when_expr)?;
                validate_aggregate_shape(then_expr)?;
            }
            if let Some(default) = default {
                validate_aggregate_shape(default)?;
            }
            Ok(())
        }
        Expr::Exists(inner) => validate_aggregate_shape(inner),
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            validate_aggregate_shape(list)?;
            if let Some(pred) = predicate {
                validate_aggregate_shape(pred)?;
            }
            Ok(())
        }
        Expr::Literal(_)
        | Expr::Property(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::CountStar => Ok(()),
    }
}

fn expr_alias(expr: &Expr) -> String {
    match expr {
        Expr::Variable(v) => v.clone(),
        Expr::Property(pa) => format!("{}.{}", pa.variable, pa.property),
        Expr::FunctionCall { name, args } => {
            let arg_str: Vec<String> = args.iter().map(expr_alias).collect();
            format!("{}({})", name, arg_str.join(", "))
        }
        Expr::CountStar => "count(*)".into(),
        _ => "expr".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    #[test]
    fn bind_with_projects_scope() {
        let query = Parser::parse_read(
            "MATCH (n:Entity) WITH n.name AS name WHERE name = 'Apple' RETURN name",
        )
        .unwrap();
        let bound = bind_query(&query).unwrap();
        assert!(bound.final_scope.contains("name"));
        assert!(!bound.final_scope.contains("n"));
    }

    #[test]
    fn bind_rejects_variable_hidden_by_with() {
        let query = Parser::parse_read("MATCH (n:Entity) WITH n.name AS name RETURN n").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("undefined variable 'n'"));
    }

    #[test]
    fn bind_unwind_introduces_alias() {
        let query = Parser::parse_read("UNWIND [1, 2] AS x RETURN x").unwrap();
        let bound = bind_query(&query).unwrap();
        assert_eq!(bound.final_scope.get("x"), Some(VarKind::Unknown));
    }

    #[test]
    fn bind_write_rejects_undefined_set_target() {
        let Statement::Write(query) = Parser::parse("MATCH (m) SET n.name = 'Alice'").unwrap()
        else {
            panic!("expected write");
        };
        let err = bind_write(&query).unwrap_err();
        assert!(err.to_string().contains("undefined variable 'n'"));
    }

    #[test]
    fn bind_create_then_delete_sees_created_variable() {
        let Statement::Write(query) =
            Parser::parse("CREATE (n:Entity {name: 'A'}) DELETE n").unwrap()
        else {
            panic!("expected write");
        };
        bind_write(&query).unwrap();
    }

    #[test]
    fn bind_rejects_not_on_non_boolean_literal() {
        let query = Parser::parse_read("RETURN NOT 1 AS result").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("requires boolean"));
    }

    #[test]
    fn bind_rejects_boolean_op_with_non_boolean_literal() {
        let query = Parser::parse_read("RETURN true AND 'x' AS result").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("requires boolean"));
    }

    #[test]
    fn bind_allows_boolean_ops_with_null() {
        let query =
            Parser::parse_read("RETURN NOT null AS a, true OR null AS b, false AND null AS c")
                .unwrap();
        bind_query(&query).unwrap();
    }

    #[test]
    fn bind_rejects_non_boolean_case_when_literal() {
        let query =
            Parser::parse_read("RETURN CASE WHEN 1 THEN 'bad' ELSE 'ok' END AS result").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("CASE WHEN requires"));
    }
}
