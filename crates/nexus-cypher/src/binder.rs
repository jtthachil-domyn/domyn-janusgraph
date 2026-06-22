//! Binder and scope validation for parsed Cypher ASTs.
//!
//! The binder sits between parsing and planning. It validates variable
//! visibility, records coarse variable kinds, and provides a home for Cypher
//! semantic checks that do not belong in the parser or physical planner.

use crate::ast::*;
use crate::error::{CypherError, CypherResult};
use std::collections::{BTreeMap, BTreeSet};

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
    Integer,
    Float,
    String,
    List,
    Map,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversionKind {
    Boolean,
    Integer,
    Float,
    String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationPatternMode {
    Create,
    Merge,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    variables: BTreeMap<String, VarKind>,
    types: BTreeMap<String, StaticType>,
    list_items: BTreeMap<String, Vec<(VarKind, StaticType)>>,
    deleted: BTreeSet<String>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, kind: VarKind) {
        self.insert_typed(name, kind, StaticType::Unknown);
    }

    fn insert_typed(&mut self, name: impl Into<String>, kind: VarKind, static_type: StaticType) {
        let name = name.into();
        self.variables.insert(name.clone(), kind);
        self.types.insert(name.clone(), static_type);
        self.list_items.remove(&name);
        self.deleted.remove(&name);
    }

    fn insert_list_typed(&mut self, name: impl Into<String>, items: Vec<(VarKind, StaticType)>) {
        let name = name.into();
        self.variables.insert(name.clone(), VarKind::List);
        self.types.insert(name.clone(), StaticType::List);
        self.deleted.remove(&name);
        self.list_items.insert(name, items);
    }

    pub fn get(&self, name: &str) -> Option<VarKind> {
        self.variables.get(name).copied()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.variables.contains_key(name)
    }

    fn static_type(&self, name: &str) -> StaticType {
        self.types.get(name).copied().unwrap_or(StaticType::Unknown)
    }

    fn list_item(&self, name: &str, index: usize) -> Option<(VarKind, StaticType)> {
        self.list_items
            .get(name)
            .and_then(|items| items.get(index))
            .copied()
    }

    pub fn variables(&self) -> &BTreeMap<String, VarKind> {
        &self.variables
    }

    fn mark_deleted(&mut self, name: impl Into<String>) {
        let name = name.into();
        if self.variables.contains_key(&name) {
            self.deleted.insert(name);
        }
    }

    fn is_deleted(&self, name: &str) -> bool {
        self.deleted.contains(name)
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
        bind_match_clause(&mut scope, match_clause)?;
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Match,
            scope: scope.clone(),
        });
    }

    if let Some(where_clause) = &query.where_clause {
        validate_where_expr(&scope, &where_clause.expr)?;
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Where,
            scope: scope.clone(),
        });
    }

    for clause in &query.tail {
        match clause {
            ReadClause::Match { optional, clause } => {
                bind_match_clause(&mut scope, clause)?;
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
                validate_where_expr(&scope, &where_clause.expr)?;
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
                let (kind, static_type) = infer_unwind_item(&scope, expr);
                scope.insert_typed(alias.clone(), kind, static_type);
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Unwind,
                    scope: scope.clone(),
                });
            }
        }
    }

    bind_return_clause(&scope, &query.return_clause, false)?;
    let final_scope = bind_projection_scope(&scope, &query.return_clause)?;
    segments.push(BoundSegment {
        kind: BoundSegmentKind::Return,
        scope: final_scope.clone(),
    });

    if let Some(order_by) = &query.order_by {
        validate_order_by(
            &scope,
            &final_scope,
            order_by,
            Some(&query.return_clause.items),
            query.return_clause.distinct,
        )?;
    }

    if let Some(union) = &query.union {
        validate_union_tail(&final_scope, &query.return_clause, union, union.all)?;
    }

    Ok(BoundQuery {
        final_scope,
        segments,
    })
}

fn validate_union_tail(
    left_scope: &Scope,
    left_return: &ReturnClause,
    union: &UnionTail,
    expected_all: bool,
) -> CypherResult<()> {
    if union.all != expected_all {
        return Err(CypherError::Plan(
            "cannot mix UNION and UNION ALL in the same query".into(),
        ));
    }

    let right = bind_query(&union.right)?;
    let left_columns = return_column_names(left_scope, left_return);
    let right_columns = return_column_names(&right.final_scope, &union.right.return_clause);
    if left_columns != right_columns {
        return Err(CypherError::Plan(format!(
            "UNION columns differ: left {:?}, right {:?}",
            left_columns, right_columns
        )));
    }

    if let Some(next) = &union.right.union {
        validate_union_tail(
            &right.final_scope,
            &union.right.return_clause,
            next,
            expected_all,
        )?;
    }

    Ok(())
}

fn return_column_names(scope: &Scope, return_clause: &ReturnClause) -> Vec<String> {
    let mut columns = Vec::new();
    for item in &return_clause.items {
        if matches!(&item.expr, Expr::Variable(name) if name == "*") && item.alias.is_none() {
            columns.extend(scope.variables().keys().cloned());
        } else {
            columns.push(return_item_alias(item));
        }
    }
    columns
}

pub fn bind_write(query: &WriteQuery) -> CypherResult<BoundWriteQuery> {
    let mut scope = Scope::new();
    let mut segments = Vec::new();

    if let Some(match_clause) = &query.match_clause {
        bind_match_clause(&mut scope, match_clause)?;
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Match,
            scope: scope.clone(),
        });
    }

    if let Some(where_clause) = &query.where_clause {
        validate_where_expr(&scope, &where_clause.expr)?;
        segments.push(BoundSegment {
            kind: BoundSegmentKind::Where,
            scope: scope.clone(),
        });
    }

    bind_read_tail(&mut scope, &mut segments, &query.tail)?;

    for mutation in &query.mutations {
        match mutation {
            MutationClause::Read(clause) => {
                bind_read_tail(&mut scope, &mut segments, std::slice::from_ref(clause))?;
            }
            MutationClause::Create { patterns } => {
                bind_mutation_patterns(&mut scope, patterns, MutationPatternMode::Create)?;
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Create,
                    scope: scope.clone(),
                });
            }
            MutationClause::Merge {
                patterns,
                on_create,
                on_match,
            } => {
                bind_mutation_patterns(&mut scope, patterns, MutationPatternMode::Merge)?;
                validate_set_items(&scope, on_create)?;
                validate_set_items(&scope, on_match)?;
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Merge,
                    scope: scope.clone(),
                });
            }
            MutationClause::Set { items } => {
                validate_set_items(&scope, items)?;
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Set,
                    scope: scope.clone(),
                });
            }
            MutationClause::Remove { items } => {
                for item in items {
                    match item {
                        RemoveItem::Property(pa) => require_var(&scope, &pa.variable)?,
                        RemoveItem::Labels { variable, .. } => require_var(&scope, variable)?,
                    }
                }
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Remove,
                    scope: scope.clone(),
                });
            }
            MutationClause::Delete { targets, .. } => {
                for target in targets {
                    validate_expr(&scope, target)?;
                    validate_delete_target(target)?;
                }
                for target in targets {
                    mark_deleted_target(&mut scope, target);
                }
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Delete,
                    scope: scope.clone(),
                });
            }
        }
    }

    let final_scope = if let Some(return_clause) = &query.return_clause {
        bind_return_clause(&scope, return_clause, false)?;
        bind_projection_scope(&scope, return_clause)?
    } else {
        scope.clone()
    };

    Ok(BoundWriteQuery {
        final_scope,
        segments,
    })
}

fn validate_set_items(scope: &Scope, items: &[SetItem]) -> CypherResult<()> {
    for item in items {
        match item {
            SetItem::Property { target, value } => {
                require_var(scope, &target.variable)?;
                validate_expr(scope, value)?;
                if contains_pattern_predicate(value) {
                    return Err(CypherError::Plan(
                        "pattern predicates are not valid property values".into(),
                    ));
                }
                validate_graph_property_value_expr(value)?;
            }
            SetItem::Properties {
                variable, value, ..
            } => {
                require_var(scope, variable)?;
                validate_expr(scope, value)?;
                validate_property_map_assignment(value)?;
            }
            SetItem::Labels { variable, .. } => {
                require_var(scope, variable)?;
            }
        }
    }
    Ok(())
}

fn mark_deleted_target(scope: &mut Scope, target: &Expr) {
    match target {
        Expr::Variable(name) => scope.mark_deleted(name.clone()),
        Expr::List(items) => {
            for item in items {
                mark_deleted_target(scope, item);
            }
        }
        _ => {}
    }
}

fn validate_delete_target(target: &Expr) -> CypherResult<()> {
    match target {
        Expr::Variable(_) | Expr::Property(_) | Expr::Index { .. } | Expr::Parameter(_) => Ok(()),
        Expr::Literal(Literal::Null) => Ok(()),
        Expr::List(items) => {
            for item in items {
                validate_delete_target(item)?;
            }
            Ok(())
        }
        Expr::FunctionCall { name, .. } if name == "__label_test" => Err(CypherError::Plan(
            "DELETE cannot target labels or relationship types".into(),
        )),
        _ => Err(CypherError::Plan(
            "DELETE target must be a graph element, path, or null".into(),
        )),
    }
}

fn bind_read_tail(
    scope: &mut Scope,
    segments: &mut Vec<BoundSegment>,
    tail: &[ReadClause],
) -> CypherResult<()> {
    for clause in tail {
        match clause {
            ReadClause::Match { optional, clause } => {
                bind_match_clause(scope, clause)?;
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
                validate_where_expr(scope, &where_clause.expr)?;
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Where,
                    scope: scope.clone(),
                });
            }
            ReadClause::With(with_clause) => {
                *scope = bind_with_clause(scope, with_clause)?;
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::With,
                    scope: scope.clone(),
                });
            }
            ReadClause::Unwind { expr, alias } => {
                validate_expr(scope, expr)?;
                let (kind, static_type) = infer_unwind_item(scope, expr);
                scope.insert_typed(alias.clone(), kind, static_type);
                segments.push(BoundSegment {
                    kind: BoundSegmentKind::Unwind,
                    scope: scope.clone(),
                });
            }
        }
    }
    Ok(())
}

fn bind_with_clause(input_scope: &Scope, with_clause: &WithClause) -> CypherResult<Scope> {
    validate_with_projection_aliases(with_clause)?;
    bind_return_clause(
        input_scope,
        &ReturnClause {
            items: with_clause.items.clone(),
            distinct: with_clause.distinct,
        },
        true,
    )?;

    let projected = bind_projection_scope(
        input_scope,
        &ReturnClause {
            items: with_clause.items.clone(),
            distinct: with_clause.distinct,
        },
    )?;

    if let Some(where_clause) = &with_clause.where_clause {
        validate_expr_either_scope(input_scope, &projected, &where_clause.expr)?;
        if contains_aggregate(&where_clause.expr) {
            return Err(CypherError::Plan(
                "aggregate expressions are not allowed in WHERE".into(),
            ));
        }
    }

    if let Some(order_by) = &with_clause.order_by {
        validate_order_by(
            input_scope,
            &projected,
            order_by,
            Some(&with_clause.items),
            with_clause.distinct,
        )?;
    }

    Ok(projected)
}

fn validate_with_projection_aliases(with_clause: &WithClause) -> CypherResult<()> {
    for item in &with_clause.items {
        if item.alias.is_none()
            && !matches!(&item.expr, Expr::Variable(name) if name == "*")
            && !matches!(&item.expr, Expr::Variable(_))
        {
            return Err(CypherError::Plan(
                "expressions in WITH must be aliased".into(),
            ));
        }
    }
    Ok(())
}

fn validate_where_expr(scope: &Scope, expr: &Expr) -> CypherResult<()> {
    validate_expr(scope, expr)?;
    validate_boolean_context_in_scope(scope, expr, "WHERE")?;
    if contains_aggregate(expr) {
        return Err(CypherError::Plan(
            "aggregate expressions are not allowed in WHERE".into(),
        ));
    }
    Ok(())
}

fn bind_return_clause(
    scope: &Scope,
    return_clause: &ReturnClause,
    allow_empty_wildcard: bool,
) -> CypherResult<()> {
    let has_aggregate = return_clause
        .items
        .iter()
        .any(|item| contains_aggregate(&item.expr));

    // Cypher grouping rule: when a RETURN list contains aggregates, every
    // non-aggregate sub-expression of an aggregate-containing item must match
    // (or be functionally determined by) a top-level group key — i.e. an item
    // that is itself non-aggregate. We enforce a sufficient form of this:
    // each non-aggregate variable/property reference must appear standalone
    // OR be a property of a variable that appears standalone.
    if has_aggregate {
        let group_keys: Vec<&Expr> = return_clause
            .items
            .iter()
            .filter(|item| !contains_aggregate(&item.expr))
            .map(|item| &item.expr)
            .collect();
        for item in &return_clause.items {
            if !contains_aggregate(&item.expr) {
                continue;
            }
            let mut refs = Vec::new();
            collect_non_aggregate_refs(&item.expr, &mut refs);
            for r in &refs {
                if !ref_covered_by_group_keys(r, &group_keys) {
                    return Err(CypherError::Plan(format!(
                        "non-aggregate expression {} is not a grouping key",
                        describe_ref(r)
                    )));
                }
            }
        }
    }

    let mut result_names = BTreeSet::new();
    for item in &return_clause.items {
        if matches!(&item.expr, Expr::Variable(name) if name == "*") && item.alias.is_none() {
            if scope.variables().is_empty() && !allow_empty_wildcard {
                return Err(CypherError::Plan(
                    "RETURN * requires at least one variable in scope".into(),
                ));
            }
            for (name, _) in scope.variables() {
                if !result_names.insert(name.clone()) {
                    return Err(CypherError::Plan(format!(
                        "duplicate result column '{name}'"
                    )));
                }
            }
            continue;
        }
        validate_expr(scope, &item.expr)?;
        if contains_pattern_predicate(&item.expr) {
            return Err(CypherError::Plan(
                "pattern predicates are only allowed in boolean predicate contexts".into(),
            ));
        }
        if has_aggregate {
            validate_aggregate_shape(&item.expr)?;
        }
        let alias = return_item_alias(item);
        if !result_names.insert(alias.clone()) {
            return Err(CypherError::Plan(format!(
                "duplicate result column '{alias}'"
            )));
        }
    }
    Ok(())
}

fn bind_projection_scope(input_scope: &Scope, return_clause: &ReturnClause) -> CypherResult<Scope> {
    let mut projected = Scope::new();
    for item in &return_clause.items {
        if matches!(&item.expr, Expr::Variable(name) if name == "*") && item.alias.is_none() {
            for (name, kind) in input_scope.variables() {
                projected.insert(name.clone(), *kind);
                if input_scope.is_deleted(name) {
                    projected.mark_deleted(name.clone());
                }
            }
            continue;
        }
        let alias = return_item_alias(item);
        let alias_deleted =
            matches!(&item.expr, Expr::Variable(name) if input_scope.is_deleted(name));
        if let Expr::List(items) = &item.expr {
            let item_types = items
                .iter()
                .map(|item| {
                    (
                        infer_expr_kind(input_scope, item),
                        infer_static_expr_type(input_scope, item),
                    )
                })
                .collect();
            projected.insert_list_typed(alias.clone(), item_types);
        } else if let Expr::FunctionCall { name, args } = &item.expr {
            if name.eq_ignore_ascii_case("collect") {
                let item = args
                    .first()
                    .map_or((VarKind::Unknown, StaticType::Unknown), |arg| {
                        (
                            infer_expr_kind(input_scope, arg),
                            infer_static_expr_type(input_scope, arg),
                        )
                    });
                projected.insert_list_typed(alias.clone(), vec![item]);
                if alias_deleted {
                    projected.mark_deleted(alias);
                }
                continue;
            }
            projected.insert_typed(
                alias.clone(),
                infer_expr_kind(input_scope, &item.expr),
                infer_static_expr_type(input_scope, &item.expr),
            );
        } else {
            projected.insert_typed(
                alias.clone(),
                infer_expr_kind(input_scope, &item.expr),
                infer_static_expr_type(input_scope, &item.expr),
            );
        }
        if alias_deleted {
            projected.mark_deleted(alias);
        }
    }
    Ok(projected)
}

fn infer_unwind_item(scope: &Scope, expr: &Expr) -> (VarKind, StaticType) {
    match expr {
        Expr::FunctionCall { name, args }
            if args.len() == 1 && name.eq_ignore_ascii_case("nodes") =>
        {
            (VarKind::Node, StaticType::Unknown)
        }
        Expr::FunctionCall { name, args }
            if args.len() == 1 && name.eq_ignore_ascii_case("relationships") =>
        {
            (VarKind::Relationship, StaticType::Unknown)
        }
        Expr::Variable(name) => scope
            .list_item(name, 0)
            .unwrap_or((VarKind::Unknown, StaticType::Unknown)),
        Expr::List(items) => {
            items
                .first()
                .map_or((VarKind::Unknown, StaticType::Unknown), |item| {
                    (
                        infer_expr_kind(scope, item),
                        infer_static_expr_type(scope, item),
                    )
                })
        }
        _ => (VarKind::Unknown, StaticType::Unknown),
    }
}

fn infer_list_iteration_item(scope: &Scope, expr: &Expr) -> (VarKind, StaticType) {
    infer_unwind_item(scope, expr)
}

fn validate_order_by(
    input_scope: &Scope,
    projected_scope: &Scope,
    order_by: &OrderByClause,
    projection_items: Option<&[ReturnItem]>,
    distinct: bool,
) -> CypherResult<()> {
    // DISTINCT narrows the visible scope to projected items only. ORDER BY
    // expressions referencing input-scope variables that aren't projected
    // (or aren't projected as their bare variable) are invalid.
    if distinct {
        if let Some(items) = projection_items {
            for item in &order_by.items {
                let mut refs = Vec::new();
                collect_non_aggregate_refs(&item.expr, &mut refs);
                for r in &refs {
                    if !ref_visible_after_distinct(r, items, projected_scope) {
                        return Err(CypherError::Plan(format!(
                            "ORDER BY {} cannot reference values removed by DISTINCT",
                            describe_ref(r)
                        )));
                    }
                }
            }
        }
    }

    for item in &order_by.items {
        validate_expr_either_scope(input_scope, projected_scope, &item.expr)?;
        if contains_aggregate(&item.expr) {
            let non_aggregate_refs = non_aggregate_order_refs(&item.expr);
            for expr in &non_aggregate_refs {
                if !order_ref_is_projected(projected_scope, projection_items, &expr) {
                    let name = expr_alias(&expr);
                    return Err(CypherError::Plan(format!(
                        "ORDER BY aggregate expression cannot reference non-projected variable '{name}'"
                    )));
                }
            }
            if non_aggregate_refs.is_empty()
                && aggregate_order_refs(&item.expr)
                    .iter()
                    .any(|expr| !order_ref_is_projected(projected_scope, projection_items, expr))
            {
                let name = expr_alias(&item.expr);
                return Err(CypherError::Plan(format!(
                    "ORDER BY aggregate expression '{name}' must be projected"
                )));
            }
        }
    }
    Ok(())
}

fn bind_match_clause(scope: &mut Scope, clause: &MatchClause) -> CypherResult<()> {
    for pattern in &clause.patterns {
        bind_pattern(scope, pattern)?;
    }
    Ok(())
}

fn bind_mutation_patterns(
    scope: &mut Scope,
    patterns: &[Pattern],
    mode: MutationPatternMode,
) -> CypherResult<()> {
    for pattern in patterns {
        bind_mutation_pattern(scope, pattern, mode)?;
    }
    Ok(())
}

fn bind_mutation_pattern(
    scope: &mut Scope,
    pattern: &Pattern,
    mode: MutationPatternMode,
) -> CypherResult<()> {
    if let Some(var) = &pattern.path_variable {
        bind_variable(scope, var, VarKind::Path)?;
    }

    let relationship_pattern = pattern
        .elements
        .iter()
        .any(|element| matches!(element, PatternElement::Relationship(_)));

    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => {
                bind_mutation_node(scope, node, mode, relationship_pattern)?;
            }
            PatternElement::Relationship(rel) => {
                bind_mutation_relationship(scope, rel, mode)?;
            }
        }
    }
    Ok(())
}

fn bind_mutation_node(
    scope: &mut Scope,
    node: &NodePattern,
    mode: MutationPatternMode,
    relationship_pattern: bool,
) -> CypherResult<()> {
    if mode == MutationPatternMode::Merge {
        reject_null_properties(&node.properties, "MERGE node")?;
    }

    if let Some(var) = &node.variable {
        if scope.contains(var) {
            if !relationship_pattern
                || !node.labels.is_empty()
                || node.properties_specified
                || !node.properties.is_empty()
            {
                return Err(CypherError::Plan(format!(
                    "{mode:?} cannot create new predicates for already-bound node '{var}'"
                )));
            }
            return Ok(());
        }
    }

    validate_property_values(scope, &node.properties)?;
    if let Some(var) = &node.variable {
        bind_variable(scope, var, VarKind::Node)?;
    }
    Ok(())
}

fn bind_mutation_relationship(
    scope: &mut Scope,
    rel: &RelationshipPattern,
    mode: MutationPatternMode,
) -> CypherResult<()> {
    if rel.rel_types.len() != 1 {
        return Err(CypherError::Plan(format!(
            "{mode:?} relationship patterns must specify exactly one type"
        )));
    }
    if rel.min_hops.is_some() || rel.max_hops.is_some() {
        return Err(CypherError::Plan(format!(
            "{mode:?} relationship patterns cannot be variable-length"
        )));
    }
    if mode == MutationPatternMode::Merge {
        reject_null_properties(&rel.properties, "MERGE relationship")?;
    }
    if let Some(var) = &rel.variable {
        if scope.contains(var) {
            return Err(CypherError::Plan(format!(
                "{mode:?} cannot reuse already-bound relationship '{var}'"
            )));
        }
    }

    validate_property_values(scope, &rel.properties)?;
    if let Some(var) = &rel.variable {
        bind_variable(scope, var, VarKind::Relationship)?;
    }
    Ok(())
}

fn validate_property_values(scope: &Scope, properties: &[(String, Expr)]) -> CypherResult<()> {
    for (_, value) in properties {
        validate_expr(scope, value)?;
        validate_graph_property_value_expr(value)?;
    }
    Ok(())
}

fn validate_property_map_assignment(expr: &Expr) -> CypherResult<()> {
    if let Expr::Map(entries) = expr {
        for (_, value) in entries {
            validate_graph_property_value_expr(value)?;
        }
    }
    Ok(())
}

fn validate_graph_property_value_expr(expr: &Expr) -> CypherResult<()> {
    match expr {
        Expr::Map(_) => Err(CypherError::Plan(
            "graph properties cannot contain map values".into(),
        )),
        Expr::List(items) => {
            for item in items {
                if contains_map_literal(item) {
                    return Err(CypherError::Plan(
                        "graph properties cannot contain lists of maps".into(),
                    ));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn contains_map_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Map(_) => true,
        Expr::List(items) => items.iter().any(contains_map_literal),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee.as_deref().is_some_and(contains_map_literal)
                || arms.iter().any(|(when_expr, then_expr)| {
                    contains_map_literal(when_expr) || contains_map_literal(then_expr)
                })
                || default.as_deref().is_some_and(contains_map_literal)
        }
        _ => false,
    }
}

fn reject_null_properties(properties: &[(String, Expr)], context: &str) -> CypherResult<()> {
    for (key, value) in properties {
        if expr_is_static_null(value) {
            return Err(CypherError::Plan(format!(
                "{context} property '{key}' cannot be null"
            )));
        }
    }
    Ok(())
}

fn expr_is_static_null(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(Literal::Null))
}

fn bind_pattern(scope: &mut Scope, pattern: &Pattern) -> CypherResult<()> {
    if let Some(var) = &pattern.path_variable {
        bind_variable(scope, var, VarKind::Path)?;
    }
    let mut relationships_in_pattern = BTreeSet::new();
    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(var) = &node.variable {
                    bind_variable(scope, var, VarKind::Node)?;
                }
            }
            PatternElement::Relationship(rel) => {
                if let Some(var) = &rel.variable {
                    if !relationships_in_pattern.insert(var.clone()) {
                        return Err(CypherError::Plan(format!(
                            "relationship variable '{var}' cannot be reused in the same pattern"
                        )));
                    }
                    let kind = if rel.min_hops.is_some() || rel.max_hops.is_some() {
                        VarKind::List
                    } else {
                        VarKind::Relationship
                    };
                    bind_variable(scope, var, kind)?;
                }
            }
        }
    }
    Ok(())
}

fn bind_variable(scope: &mut Scope, name: &str, kind: VarKind) -> CypherResult<()> {
    if let Some(existing) = scope.get(name) {
        if existing != kind {
            if scope.static_type(name) == StaticType::Null {
                scope.insert_typed(name, kind, StaticType::Null);
                return Ok(());
            }
            return Err(CypherError::Parse {
                position: 0,
                message: format!(
                    "variable '{name}' is already bound as {existing:?}, cannot bind as {kind:?}"
                ),
            });
        }
    }
    scope.insert(name, kind);
    Ok(())
}

fn validate_expr(scope: &Scope, expr: &Expr) -> CypherResult<()> {
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::CountStar => Ok(()),
        Expr::Variable(var) => require_var(scope, var),
        Expr::Property(pa) => {
            require_var(scope, &pa.variable)?;
            if scope.is_deleted(&pa.variable) {
                return Err(CypherError::Plan(format!(
                    "cannot access property '{}.{}' after DELETE",
                    pa.variable, pa.property
                )));
            }
            validate_property_access_target(scope, &pa.variable)
        }
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
        Expr::FunctionCall { name, args } => {
            for arg in args {
                validate_expr(scope, arg)?;
            }
            validate_known_function_name(name)?;
            if name.eq_ignore_ascii_case("range") {
                validate_range_args(scope, args)?;
            }
            if let Some(kind) = conversion_kind(name) {
                validate_conversion_args(scope, kind, args)?;
            }
            validate_percentile_args(name, args)?;
            validate_graph_function_args(scope, name, args)?;
            validate_path_size_length_args(scope, name, args)?;
            Ok(())
        }
        Expr::List(args) => {
            for arg in args {
                validate_expr(scope, arg)?;
            }
            Ok(())
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => {
            validate_expr(scope, list)?;
            let mut local = scope.clone();
            let (kind, static_type) = infer_list_iteration_item(scope, list);
            local.insert_typed(variable.clone(), kind, static_type);
            if let Some(predicate) = predicate {
                if contains_aggregate(predicate) {
                    return Err(CypherError::Plan(
                        "aggregate expressions are not allowed in list comprehensions".into(),
                    ));
                }
                validate_expr(&local, predicate)?;
                validate_boolean_context(predicate, "list comprehension WHERE")?;
            }
            if let Some(projection) = projection {
                if contains_aggregate(projection) {
                    return Err(CypherError::Plan(
                        "aggregate expressions are not allowed in list comprehensions".into(),
                    ));
                }
                validate_expr(&local, projection)?;
                validate_list_conversion_projection(scope, variable, list, projection)?;
                validate_list_graph_function_projection(scope, variable, list, projection)?;
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
            validate_expr(scope, list)?;
            match infer_static_expr_type(scope, list) {
                StaticType::List | StaticType::Null | StaticType::Unknown => Ok(()),
                other => Err(CypherError::Plan(format!(
                    "IN requires a list expression on the right-hand side, got {other:?}"
                ))),
            }
        }
        Expr::Index { target, index } => {
            validate_expr(scope, target)?;
            validate_expr(scope, index)?;
            validate_index_access(scope, target, index)
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
        Expr::PatternPredicate(pattern) => {
            let mut has_relationship = false;
            let mut has_bound_node = false;
            for element in &pattern.elements {
                match element {
                    PatternElement::Node(node) => {
                        if let Some(var) = &node.variable {
                            if scope.contains(var) {
                                has_bound_node = true;
                                continue;
                            }
                            return Err(CypherError::Plan(format!("undefined variable '{var}'")));
                        }
                    }
                    PatternElement::Relationship(rel) => {
                        has_relationship = true;
                        if let Some(var) = &rel.variable {
                            if scope.contains(var) {
                                continue;
                            }
                            return Err(CypherError::Plan(format!("undefined variable '{var}'")));
                        }
                    }
                }
            }
            if !has_relationship {
                return Err(CypherError::Plan(
                    "pattern predicates must contain a relationship".into(),
                ));
            }
            if !has_bound_node {
                return Err(CypherError::Plan(
                    "pattern predicates must reference at least one bound node".into(),
                ));
            }
            Ok(())
        }
        Expr::PatternComprehension {
            variable,
            pattern,
            projection,
        } => {
            let mut local = scope.clone();
            if let Some(variable) = variable {
                bind_variable(&mut local, variable, VarKind::Path)?;
            }
            bind_pattern(&mut local, pattern)?;
            validate_expr(&local, projection)
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
                if scrutinee.is_none() {
                    validate_boolean_context(when_expr, "CASE WHEN")?;
                }
                validate_expr(scope, then_expr)?;
            }
            if let Some(default) = default {
                validate_expr(scope, default)?;
            }
            Ok(())
        }
        Expr::Exists(inner) => validate_expr(scope, inner),
        Expr::ExistsSubquery(_) => Ok(()),
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
                validate_list_predicate_element_use(variable, list, pred)?;
            }
            Ok(())
        }
    }
}

fn conversion_kind(name: &str) -> Option<ConversionKind> {
    match name.to_lowercase().as_str() {
        "toboolean" | "tobooleanornull" => Some(ConversionKind::Boolean),
        "tointeger" | "tointegerornull" => Some(ConversionKind::Integer),
        "tofloat" | "tofloatornull" => Some(ConversionKind::Float),
        "tostring" | "tostringornull" => Some(ConversionKind::String),
        _ => None,
    }
}

fn validate_known_function_name(name: &str) -> CypherResult<()> {
    let lname = name.to_lowercase();
    if is_known_temporal_function(&lname) {
        return Ok(());
    }
    let known = matches!(
        lname.as_str(),
        "__distinct"
            | "__label_test"
            | "abs"
            | "all"
            | "any"
            | "avg"
            | "ceil"
            | "coalesce"
            | "collect"
            | "count"
            | "date"
            | "datetime"
            | "document"
            | "documentfulltext"
            | "documents"
            | "documentsby"
            | "documentsfulltext"
            | "documentsprefix"
            | "documentsrange"
            | "duration"
            | "floor"
            | "head"
            | "id"
            | "keys"
            | "labels"
            | "last"
            | "length"
            | "localdatetime"
            | "localtime"
            | "max"
            | "min"
            | "nodes"
            | "none"
            | "percentilecont"
            | "percentiledisc"
            | "properties"
            | "rand"
            | "range"
            | "relationships"
            | "reverse"
            | "sign"
            | "single"
            | "size"
            | "split"
            | "sqrt"
            | "startnode"
            | "substring"
            | "sum"
            | "tail"
            | "time"
            | "toboolean"
            | "tobooleanornull"
            | "tofloat"
            | "tofloatornull"
            | "tointeger"
            | "tointegerornull"
            | "tolower"
            | "tostring"
            | "tostringornull"
            | "toupper"
            | "type"
            | "endnode"
            | "vectordistance"
            | "vectorsearch"
    );
    if known {
        Ok(())
    } else {
        Err(CypherError::Plan(format!("unknown function '{name}'")))
    }
}

fn is_known_temporal_function(name: &str) -> bool {
    matches!(
        name,
        "date.truncate"
            | "date.transaction"
            | "date.statement"
            | "date.realtime"
            | "localtime.truncate"
            | "localtime.transaction"
            | "localtime.statement"
            | "localtime.realtime"
            | "time.truncate"
            | "time.transaction"
            | "time.statement"
            | "time.realtime"
            | "localdatetime.truncate"
            | "localdatetime.transaction"
            | "localdatetime.statement"
            | "localdatetime.realtime"
            | "datetime.truncate"
            | "datetime.fromepoch"
            | "datetime.fromepochmillis"
            | "datetime.transaction"
            | "datetime.statement"
            | "datetime.realtime"
            | "duration.between"
            | "duration.inmonths"
            | "duration.indays"
            | "duration.inseconds"
    )
}

fn validate_percentile_args(name: &str, args: &[Expr]) -> CypherResult<()> {
    if !matches!(
        name.to_lowercase().as_str(),
        "percentiledisc" | "percentilecont"
    ) {
        return Ok(());
    }
    if args.len() != 2 {
        return Err(CypherError::Plan(format!("{name} requires two arguments")));
    }
    if !matches!(
        args.get(1),
        Some(Expr::Literal(Literal::Integer(_)) | Expr::Literal(Literal::Float(_)))
    ) {
        return Err(CypherError::Plan(format!(
            "{name} percentile argument must be a numeric literal"
        )));
    }
    Ok(())
}

fn validate_property_access_target(scope: &Scope, variable: &str) -> CypherResult<()> {
    match (scope.get(variable), scope.static_type(variable)) {
        (Some(VarKind::Path), _) => Err(CypherError::Plan(format!(
            "property access is not defined for path variable '{variable}'"
        ))),
        (Some(VarKind::Node | VarKind::Relationship | VarKind::Map | VarKind::Unknown), _) => {
            Ok(())
        }
        (_, StaticType::Map | StaticType::Null | StaticType::Unknown) => Ok(()),
        (kind, static_type) => Err(CypherError::Plan(format!(
            "property access requires a graph or map value, got {kind:?}/{static_type:?}"
        ))),
    }
}

fn validate_graph_function_args(scope: &Scope, name: &str, args: &[Expr]) -> CypherResult<()> {
    let Some(kind) = graph_function_kind(name) else {
        return Ok(());
    };
    for arg in args {
        validate_deleted_graph_function_arg(scope, name, kind, arg)?;
        if graph_function_arg_is_statically_invalid(scope, kind, arg) {
            return Err(CypherError::Plan(format!(
                "{name}() cannot accept {}",
                describe_expr_type(scope, arg)
            )));
        }
    }
    Ok(())
}

fn validate_deleted_graph_function_arg(
    scope: &Scope,
    name: &str,
    kind: GraphFunctionKind,
    arg: &Expr,
) -> CypherResult<()> {
    let Expr::Variable(var) = arg else {
        return Ok(());
    };
    if !scope.is_deleted(var) || kind == GraphFunctionKind::Type {
        return Ok(());
    }
    Err(CypherError::Plan(format!(
        "cannot evaluate {name}() on deleted variable '{var}'"
    )))
}

fn validate_path_size_length_args(scope: &Scope, name: &str, args: &[Expr]) -> CypherResult<()> {
    let lname = name.to_lowercase();
    if !matches!(lname.as_str(), "size" | "length") {
        return Ok(());
    }
    for arg in args {
        if let Expr::Variable(var) = arg {
            match (lname.as_str(), scope.get(var)) {
                ("length", Some(VarKind::Node | VarKind::Relationship)) => {
                    return Err(CypherError::Plan(
                        "length() requires a path, string, or list value".into(),
                    ));
                }
                ("size", Some(VarKind::Path)) => {
                    return Err(CypherError::Plan(
                        "size() cannot accept a path value; use length()".into(),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphFunctionKind {
    Labels,
    Type,
    Properties,
    Keys,
}

fn graph_function_kind(name: &str) -> Option<GraphFunctionKind> {
    match name.to_lowercase().as_str() {
        "labels" => Some(GraphFunctionKind::Labels),
        "type" => Some(GraphFunctionKind::Type),
        "properties" => Some(GraphFunctionKind::Properties),
        "keys" => Some(GraphFunctionKind::Keys),
        _ => None,
    }
}

fn graph_function_arg_is_statically_invalid(
    scope: &Scope,
    kind: GraphFunctionKind,
    expr: &Expr,
) -> bool {
    if let Expr::Index { target, index } = expr {
        if let (Expr::Variable(list), Some(index)) =
            (target.as_ref(), static_integer_literal(index))
        {
            if let Some((item_kind, item_type)) = scope.list_item(list, index as usize) {
                return graph_function_kind_rejects(kind, item_kind, item_type);
            }
        }
    }

    if let Expr::Variable(var) = expr {
        match (kind, scope.get(var)) {
            (GraphFunctionKind::Labels, Some(VarKind::Path | VarKind::Relationship)) => {
                return true;
            }
            (GraphFunctionKind::Type, Some(VarKind::Node | VarKind::Path)) => return true,
            (_, Some(VarKind::Node | VarKind::Relationship | VarKind::Map | VarKind::Unknown)) => {
                return false;
            }
            _ => {}
        }
    }

    match (kind, infer_static_expr_type(scope, expr)) {
        (_, StaticType::Null | StaticType::Unknown) => false,
        (GraphFunctionKind::Labels, StaticType::List | StaticType::Map) => true,
        (
            GraphFunctionKind::Labels,
            StaticType::Boolean | StaticType::Integer | StaticType::Float | StaticType::String,
        ) => true,
        (
            GraphFunctionKind::Type,
            StaticType::Boolean
            | StaticType::Integer
            | StaticType::Float
            | StaticType::String
            | StaticType::List
            | StaticType::Map,
        ) => true,
        (
            GraphFunctionKind::Properties,
            StaticType::Boolean
            | StaticType::Integer
            | StaticType::Float
            | StaticType::String
            | StaticType::List,
        ) => true,
        (
            GraphFunctionKind::Keys,
            StaticType::Boolean | StaticType::Integer | StaticType::Float | StaticType::String,
        ) => true,
        _ => false,
    }
}

fn graph_function_kind_rejects(
    kind: GraphFunctionKind,
    item_kind: VarKind,
    item_type: StaticType,
) -> bool {
    match (kind, item_kind, item_type) {
        (_, _, StaticType::Null | StaticType::Unknown) => false,
        (GraphFunctionKind::Labels, VarKind::Node, _) => false,
        (GraphFunctionKind::Type, VarKind::Relationship, _) => false,
        (
            GraphFunctionKind::Properties | GraphFunctionKind::Keys,
            VarKind::Node | VarKind::Relationship | VarKind::Map,
            _,
        ) => false,
        _ => true,
    }
}

fn validate_conversion_args(
    scope: &Scope,
    kind: ConversionKind,
    args: &[Expr],
) -> CypherResult<()> {
    for arg in args {
        if conversion_arg_is_statically_invalid(scope, kind, arg) {
            return Err(CypherError::Plan(format!(
                "{kind:?} conversion cannot accept {}",
                describe_expr_type(scope, arg)
            )));
        }
    }
    Ok(())
}

fn validate_list_graph_function_projection(
    scope: &Scope,
    variable: &str,
    list: &Expr,
    projection: &Expr,
) -> CypherResult<()> {
    let Some(kind) = graph_function_projection_kind(variable, projection) else {
        return Ok(());
    };
    let Expr::List(items) = list else {
        return Ok(());
    };

    for item in items {
        if graph_function_arg_is_statically_invalid(scope, kind, item) {
            return Err(CypherError::Plan(format!(
                "{kind:?} function cannot accept {}",
                describe_expr_type(scope, item)
            )));
        }
    }
    Ok(())
}

fn graph_function_projection_kind(variable: &str, expr: &Expr) -> Option<GraphFunctionKind> {
    match expr {
        Expr::FunctionCall { name, args }
            if args.len() == 1 && matches!(&args[0], Expr::Variable(var) if var == variable) =>
        {
            graph_function_kind(name)
        }
        _ => None,
    }
}

fn validate_list_conversion_projection(
    scope: &Scope,
    variable: &str,
    list: &Expr,
    projection: &Expr,
) -> CypherResult<()> {
    let Some(kind) = conversion_projection_kind(variable, projection) else {
        return Ok(());
    };
    let Expr::List(items) = list else {
        return Ok(());
    };

    for item in items {
        if conversion_arg_is_statically_invalid(scope, kind, item) {
            return Err(CypherError::Plan(format!(
                "{kind:?} conversion cannot accept {}",
                describe_expr_type(scope, item)
            )));
        }
    }
    Ok(())
}

fn conversion_projection_kind(variable: &str, expr: &Expr) -> Option<ConversionKind> {
    match expr {
        Expr::FunctionCall { name, args }
            if args.len() == 1 && matches!(&args[0], Expr::Variable(var) if var == variable) =>
        {
            conversion_kind(name)
        }
        Expr::List(items) => items
            .iter()
            .find_map(|item| conversion_projection_kind(variable, item)),
        Expr::Map(entries) => entries
            .iter()
            .find_map(|(_, value)| conversion_projection_kind(variable, value)),
        Expr::BinaryOp { left, right, .. } => conversion_projection_kind(variable, left)
            .or_else(|| conversion_projection_kind(variable, right)),
        Expr::UnaryOp { expr, .. } => conversion_projection_kind(variable, expr),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => scrutinee
            .as_deref()
            .and_then(|expr| conversion_projection_kind(variable, expr))
            .or_else(|| {
                arms.iter().find_map(|(when_expr, then_expr)| {
                    conversion_projection_kind(variable, when_expr)
                        .or_else(|| conversion_projection_kind(variable, then_expr))
                })
            })
            .or_else(|| {
                default
                    .as_deref()
                    .and_then(|expr| conversion_projection_kind(variable, expr))
            }),
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => conversion_projection_kind(variable, list)
            .or_else(|| {
                predicate
                    .as_deref()
                    .and_then(|expr| conversion_projection_kind(variable, expr))
            })
            .or_else(|| {
                projection
                    .as_deref()
                    .and_then(|expr| conversion_projection_kind(variable, expr))
            }),
        Expr::ListPredicate {
            list, predicate, ..
        } => conversion_projection_kind(variable, list).or_else(|| {
            predicate
                .as_deref()
                .and_then(|expr| conversion_projection_kind(variable, expr))
        }),
        Expr::In { expr, list } => conversion_projection_kind(variable, expr)
            .or_else(|| conversion_projection_kind(variable, list)),
        Expr::Index { target, index } => conversion_projection_kind(variable, target)
            .or_else(|| conversion_projection_kind(variable, index)),
        Expr::Slice { target, start, end } => conversion_projection_kind(variable, target)
            .or_else(|| {
                start
                    .as_deref()
                    .and_then(|expr| conversion_projection_kind(variable, expr))
            })
            .or_else(|| {
                end.as_deref()
                    .and_then(|expr| conversion_projection_kind(variable, expr))
            }),
        Expr::Exists(inner) => conversion_projection_kind(variable, inner),
        Expr::ExistsSubquery(_) => None,
        Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::Property(_)
        | Expr::Parameter(_)
        | Expr::PatternPredicate(_)
        | Expr::PatternComprehension { .. }
        | Expr::CountStar
        | Expr::FunctionCall { .. } => None,
    }
}

fn conversion_arg_is_statically_invalid(scope: &Scope, kind: ConversionKind, expr: &Expr) -> bool {
    if let Expr::Variable(var) = expr {
        if matches!(
            scope.get(var),
            Some(VarKind::Node | VarKind::Relationship | VarKind::Path)
        ) {
            return true;
        }
    }

    match (kind, infer_static_expr_type(scope, expr)) {
        (_, StaticType::List | StaticType::Map) => true,
        (ConversionKind::Boolean, StaticType::Integer | StaticType::Float) => true,
        (ConversionKind::Float, StaticType::Boolean) => true,
        _ => false,
    }
}

fn describe_expr_type(scope: &Scope, expr: &Expr) -> String {
    match expr {
        Expr::Variable(var) => scope
            .get(var)
            .map(|kind| format!("{kind:?} variable '{var}'"))
            .unwrap_or_else(|| format!("variable '{var}'")),
        _ => format!("{:?}", infer_static_expr_type(scope, expr)),
    }
}

fn validate_list_predicate_element_use(
    variable: &str,
    list: &Expr,
    predicate: &Expr,
) -> CypherResult<()> {
    if !expr_requires_numeric_var(predicate, variable) {
        return Ok(());
    }

    let Expr::List(items) = list else {
        return Ok(());
    };

    if items.iter().all(|item| {
        matches!(
            static_expr_type(item),
            StaticType::Integer | StaticType::Float | StaticType::Null | StaticType::Unknown
        )
    }) {
        return Ok(());
    }

    Err(CypherError::Plan(format!(
        "list predicate variable '{variable}' is used as a numeric operand but the list contains non-numeric values"
    )))
}

fn validate_index_access(scope: &Scope, target: &Expr, index: &Expr) -> CypherResult<()> {
    if let Expr::Variable(var) = target {
        match scope.get(var) {
            Some(VarKind::Node | VarKind::Relationship | VarKind::Map) => {
                return validate_property_key_index(scope, index);
            }
            Some(VarKind::Unknown) => {
                return match infer_static_expr_type(scope, index) {
                    StaticType::Integer
                    | StaticType::String
                    | StaticType::Null
                    | StaticType::Unknown => Ok(()),
                    other => Err(CypherError::Plan(format!(
                        "index or dynamic property access requires an integer or string key, got {other:?}"
                    ))),
                };
            }
            Some(VarKind::List) => return validate_integer_index(scope, index),
            Some(VarKind::Path) => return validate_integer_index(scope, index),
            Some(VarKind::Scalar) | None => {}
        }
    }

    match infer_static_expr_type(scope, target) {
        StaticType::Unknown => {
            return match infer_static_expr_type(scope, index) {
                StaticType::Integer
                | StaticType::String
                | StaticType::Null
                | StaticType::Unknown => Ok(()),
                other => Err(CypherError::Plan(format!(
                    "index or dynamic property access requires an integer or string key, got {other:?}"
                ))),
            };
        }
        StaticType::List => {}
        StaticType::Null => {
            return match infer_static_expr_type(scope, index) {
                StaticType::Integer
                | StaticType::String
                | StaticType::Null
                | StaticType::Unknown => Ok(()),
                other => Err(CypherError::Plan(format!(
                    "index or dynamic property access on null requires an integer or string key, got {other:?}"
                ))),
            };
        }
        StaticType::Map => return validate_property_key_index(scope, index),
        other => {
            return Err(CypherError::Plan(format!(
                "list indexing requires a list target, got {other:?}"
            )));
        }
    }

    validate_integer_index(scope, index)
}

fn validate_integer_index(scope: &Scope, index: &Expr) -> CypherResult<()> {
    match infer_static_expr_type(scope, index) {
        StaticType::Integer | StaticType::Null | StaticType::Unknown => Ok(()),
        other => Err(CypherError::Plan(format!(
            "list indexing requires an integer index, got {other:?}"
        ))),
    }
}

fn validate_property_key_index(scope: &Scope, index: &Expr) -> CypherResult<()> {
    match infer_static_expr_type(scope, index) {
        StaticType::String | StaticType::Null | StaticType::Unknown => Ok(()),
        other => Err(CypherError::Plan(format!(
            "dynamic property access requires a string key, got {other:?}"
        ))),
    }
}

fn validate_range_args(scope: &Scope, args: &[Expr]) -> CypherResult<()> {
    for arg in args {
        match infer_static_expr_type(scope, arg) {
            StaticType::Integer | StaticType::Unknown => {}
            other => {
                return Err(CypherError::Plan(format!(
                    "range() arguments must be integers, got {other:?}"
                )));
            }
        }
    }

    if let Some(step) = args.get(2).and_then(static_integer_literal) {
        if step == 0 {
            return Err(CypherError::Plan(
                "range() step must not be zero".to_string(),
            ));
        }
    }
    Ok(())
}

fn static_integer_literal(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(Literal::Integer(value)) => Some(*value),
        Expr::BinaryOp { left, op, right } if *op == BinaryOp::Sub => {
            match (static_integer_literal(left), static_integer_literal(right)) {
                (Some(left), Some(right)) => Some(left - right),
                _ => None,
            }
        }
        _ => None,
    }
}

fn expr_requires_numeric_var(expr: &Expr, variable: &str) -> bool {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            let numeric_here = matches!(
                op,
                BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod | BinaryOp::Pow
            ) && (expr_mentions_var(left, variable)
                || expr_mentions_var(right, variable));
            numeric_here
                || expr_requires_numeric_var(left, variable)
                || expr_requires_numeric_var(right, variable)
        }
        Expr::UnaryOp { expr, .. } => expr_requires_numeric_var(expr, variable),
        Expr::FunctionCall { args, .. } | Expr::List(args) => args
            .iter()
            .any(|arg| expr_requires_numeric_var(arg, variable)),
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            expr_requires_numeric_var(list, variable)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| expr_requires_numeric_var(expr, variable))
                || projection
                    .as_deref()
                    .is_some_and(|expr| expr_requires_numeric_var(expr, variable))
        }
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| expr_requires_numeric_var(value, variable)),
        Expr::In { expr, list } => {
            expr_requires_numeric_var(expr, variable) || expr_requires_numeric_var(list, variable)
        }
        Expr::Index { target, index } => {
            expr_requires_numeric_var(target, variable)
                || expr_requires_numeric_var(index, variable)
        }
        Expr::Slice { target, start, end } => {
            expr_requires_numeric_var(target, variable)
                || start
                    .as_deref()
                    .is_some_and(|expr| expr_requires_numeric_var(expr, variable))
                || end
                    .as_deref()
                    .is_some_and(|expr| expr_requires_numeric_var(expr, variable))
        }
        Expr::PatternComprehension { projection, .. } => {
            expr_requires_numeric_var(projection, variable)
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee
                .as_deref()
                .is_some_and(|expr| expr_requires_numeric_var(expr, variable))
                || arms.iter().any(|(when_expr, then_expr)| {
                    expr_requires_numeric_var(when_expr, variable)
                        || expr_requires_numeric_var(then_expr, variable)
                })
                || default
                    .as_deref()
                    .is_some_and(|expr| expr_requires_numeric_var(expr, variable))
        }
        Expr::Exists(inner) => expr_requires_numeric_var(inner, variable),
        Expr::ExistsSubquery(_) => false,
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            expr_requires_numeric_var(list, variable)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| expr_requires_numeric_var(expr, variable))
        }
        Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::Property(_)
        | Expr::Parameter(_)
        | Expr::PatternPredicate(_)
        | Expr::CountStar => false,
    }
}

fn expr_mentions_var(expr: &Expr, variable: &str) -> bool {
    match expr {
        Expr::Variable(var) => var == variable,
        Expr::Property(pa) => pa.variable == variable,
        Expr::BinaryOp { left, right, .. } => {
            expr_mentions_var(left, variable) || expr_mentions_var(right, variable)
        }
        Expr::UnaryOp { expr, .. } => expr_mentions_var(expr, variable),
        Expr::FunctionCall { args, .. } | Expr::List(args) => {
            args.iter().any(|arg| expr_mentions_var(arg, variable))
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            expr_mentions_var(list, variable)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| expr_mentions_var(expr, variable))
                || projection
                    .as_deref()
                    .is_some_and(|expr| expr_mentions_var(expr, variable))
        }
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| expr_mentions_var(value, variable)),
        Expr::In { expr, list } => {
            expr_mentions_var(expr, variable) || expr_mentions_var(list, variable)
        }
        Expr::Index { target, index } => {
            expr_mentions_var(target, variable) || expr_mentions_var(index, variable)
        }
        Expr::Slice { target, start, end } => {
            expr_mentions_var(target, variable)
                || start
                    .as_deref()
                    .is_some_and(|expr| expr_mentions_var(expr, variable))
                || end
                    .as_deref()
                    .is_some_and(|expr| expr_mentions_var(expr, variable))
        }
        Expr::PatternComprehension { projection, .. } => expr_mentions_var(projection, variable),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee
                .as_deref()
                .is_some_and(|expr| expr_mentions_var(expr, variable))
                || arms.iter().any(|(when_expr, then_expr)| {
                    expr_mentions_var(when_expr, variable) || expr_mentions_var(then_expr, variable)
                })
                || default
                    .as_deref()
                    .is_some_and(|expr| expr_mentions_var(expr, variable))
        }
        Expr::Exists(inner) => expr_mentions_var(inner, variable),
        Expr::ExistsSubquery(_) => false,
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            expr_mentions_var(list, variable)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| expr_mentions_var(expr, variable))
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::PatternPredicate(_) | Expr::CountStar => {
            false
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

fn validate_boolean_context_in_scope(
    scope: &Scope,
    expr: &Expr,
    context: &str,
) -> CypherResult<()> {
    if let Expr::Variable(var) = expr {
        if matches!(
            scope.get(var),
            Some(
                VarKind::Node
                    | VarKind::Relationship
                    | VarKind::Path
                    | VarKind::List
                    | VarKind::Map
            )
        ) {
            return Err(CypherError::Plan(format!(
                "{context} requires a boolean expression, got graph value"
            )));
        }
    }
    match infer_static_expr_type(scope, expr) {
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
        Expr::Literal(Literal::Integer(_)) => StaticType::Integer,
        Expr::Literal(Literal::Float(_)) => StaticType::Float,
        Expr::Literal(Literal::String(_)) => StaticType::String,
        Expr::List(_) | Expr::ListComprehension { .. } | Expr::PatternComprehension { .. } => {
            StaticType::List
        }
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
        | Expr::PatternPredicate(_)
        | Expr::Exists(_)
        | Expr::ExistsSubquery(_)
        | Expr::ListPredicate { .. } => StaticType::Boolean,
        Expr::BinaryOp { left, op, right }
            if matches!(
                op,
                BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Mod
                    | BinaryOp::Pow
            ) =>
        {
            match (static_expr_type(left), static_expr_type(right)) {
                (StaticType::Integer, StaticType::Integer) => StaticType::Integer,
                (
                    StaticType::Integer | StaticType::Float,
                    StaticType::Integer | StaticType::Float,
                ) => StaticType::Float,
                _ => StaticType::Unknown,
            }
        }
        Expr::FunctionCall { name, .. }
            if matches!(name.to_lowercase().as_str(), "exists" | "documentfulltext") =>
        {
            StaticType::Boolean
        }
        Expr::FunctionCall { name, .. }
            if matches!(
                name.to_lowercase().as_str(),
                "documents"
                    | "documentsby"
                    | "documentsfulltext"
                    | "documentsprefix"
                    | "documentsrange"
                    | "labels"
                    | "keys"
                    | "nodes"
                    | "relationships"
                    | "range"
                    | "tail"
                    | "vectorsearch"
            ) =>
        {
            StaticType::List
        }
        Expr::FunctionCall { name, .. }
            if matches!(name.to_lowercase().as_str(), "document" | "properties") =>
        {
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
            StaticType::Integer
        }
        Expr::FunctionCall { name, .. } if name.eq_ignore_ascii_case("vectordistance") => {
            StaticType::Float
        }
        Expr::FunctionCall { name, .. }
            if matches!(name.to_lowercase().as_str(), "tostring" | "type") =>
        {
            StaticType::String
        }
        Expr::CountStar => StaticType::Integer,
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

fn infer_static_expr_type(scope: &Scope, expr: &Expr) -> StaticType {
    match expr {
        Expr::Variable(var) => scope.static_type(var),
        _ => static_expr_type(expr),
    }
}

fn validate_expr_either_scope(
    left_scope: &Scope,
    right_scope: &Scope,
    expr: &Expr,
) -> CypherResult<()> {
    if validate_expr(right_scope, expr).is_ok() || validate_expr(left_scope, expr).is_ok() {
        return Ok(());
    }

    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::CountStar => Ok(()),
        Expr::Variable(var) => {
            if right_scope.contains(var) || left_scope.contains(var) {
                Ok(())
            } else {
                Err(CypherError::Plan(format!("undefined variable '{var}'")))
            }
        }
        Expr::Property(pa) => {
            if right_scope.contains(&pa.variable) {
                validate_property_access_target(right_scope, &pa.variable)
            } else if left_scope.contains(&pa.variable) {
                validate_property_access_target(left_scope, &pa.variable)
            } else {
                Err(CypherError::Plan(format!(
                    "undefined variable '{}'",
                    pa.variable
                )))
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            validate_expr_either_scope(left_scope, right_scope, left)?;
            validate_expr_either_scope(left_scope, right_scope, right)
        }
        Expr::UnaryOp { expr, .. } | Expr::Exists(expr) => {
            validate_expr_either_scope(left_scope, right_scope, expr)
        }
        Expr::ExistsSubquery(_) => Ok(()),
        Expr::FunctionCall { name, args } => {
            validate_known_function_name(name)?;
            for arg in args {
                validate_expr_either_scope(left_scope, right_scope, arg)?;
            }
            Ok(())
        }
        Expr::List(items) => {
            for item in items {
                validate_expr_either_scope(left_scope, right_scope, item)?;
            }
            Ok(())
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                validate_expr_either_scope(left_scope, right_scope, value)?;
            }
            Ok(())
        }
        Expr::In { expr, list } => {
            validate_expr_either_scope(left_scope, right_scope, expr)?;
            validate_expr_either_scope(left_scope, right_scope, list)
        }
        Expr::Index { target, index } => {
            validate_expr_either_scope(left_scope, right_scope, target)?;
            validate_expr_either_scope(left_scope, right_scope, index)
        }
        Expr::Slice { target, start, end } => {
            validate_expr_either_scope(left_scope, right_scope, target)?;
            if let Some(start) = start {
                validate_expr_either_scope(left_scope, right_scope, start)?;
            }
            if let Some(end) = end {
                validate_expr_either_scope(left_scope, right_scope, end)?;
            }
            Ok(())
        }
        _ => Err(CypherError::Plan(
            "expression cannot be resolved in either scope".into(),
        )),
    }
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
        Expr::List(_) | Expr::ListComprehension { .. } | Expr::PatternComprehension { .. } => {
            VarKind::List
        }
        Expr::Map(_) => VarKind::Map,
        Expr::FunctionCall { name, .. }
            if matches!(name.to_lowercase().as_str(), "nodes" | "relationships") =>
        {
            VarKind::List
        }
        Expr::FunctionCall { name, args } if name.eq_ignore_ascii_case("coalesce") => {
            let mut kinds = args.iter().map(|arg| infer_expr_kind(scope, arg));
            let Some(first) = kinds.next() else {
                return VarKind::Unknown;
            };
            if kinds.all(|kind| kind == first) {
                first
            } else {
                VarKind::Unknown
            }
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
        | Expr::PatternPredicate(_)
        | Expr::Case { .. }
        | Expr::CountStar
        | Expr::Exists(_)
        | Expr::ExistsSubquery(_)
        | Expr::ListPredicate { .. } => VarKind::Scalar,
    }
}

/// Walk an expression and collect Variable/Property references that are NOT
/// inside an aggregate function call. Used to enforce the grouping rule for
/// RETURN lists that mix aggregate and non-aggregate parts.
fn collect_non_aggregate_refs(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::FunctionCall { name, args } => {
            if is_aggregate_name(name) {
                // children are inside the aggregate — don't surface them
                return;
            }
            for arg in args {
                collect_non_aggregate_refs(arg, out);
            }
        }
        Expr::CountStar => {} // count(*) — no refs to surface
        Expr::Variable(_) | Expr::Property(_) => out.push(expr.clone()),
        Expr::BinaryOp { left, right, .. } => {
            collect_non_aggregate_refs(left, out);
            collect_non_aggregate_refs(right, out);
        }
        Expr::UnaryOp { expr, .. } => collect_non_aggregate_refs(expr, out),
        Expr::List(items) => {
            for it in items {
                collect_non_aggregate_refs(it, out);
            }
        }
        Expr::Map(entries) => {
            for (_, v) in entries {
                collect_non_aggregate_refs(v, out);
            }
        }
        Expr::In { expr, list } => {
            collect_non_aggregate_refs(expr, out);
            collect_non_aggregate_refs(list, out);
        }
        Expr::Index { target, index } => {
            collect_non_aggregate_refs(target, out);
            collect_non_aggregate_refs(index, out);
        }
        Expr::Slice { target, start, end } => {
            collect_non_aggregate_refs(target, out);
            if let Some(s) = start {
                collect_non_aggregate_refs(s, out);
            }
            if let Some(e) = end {
                collect_non_aggregate_refs(e, out);
            }
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            if let Some(s) = scrutinee {
                collect_non_aggregate_refs(s, out);
            }
            for (w, t) in arms {
                collect_non_aggregate_refs(w, out);
                collect_non_aggregate_refs(t, out);
            }
            if let Some(d) = default {
                collect_non_aggregate_refs(d, out);
            }
        }
        Expr::Exists(inner) => collect_non_aggregate_refs(inner, out),
        // List predicates and comprehensions introduce an inner iteration
        // variable. The predicate/projection bodies live in an inner scope
        // and shouldn't surface free refs to the outer grouping rule —
        // any aggregate inside the source list (e.g. `collect(n)`) is
        // already handled by descending into `list`.
        Expr::ListPredicate { list, .. } => {
            collect_non_aggregate_refs(list, out);
        }
        Expr::ListComprehension { list, .. } => {
            collect_non_aggregate_refs(list, out);
        }
        // Pattern comprehensions also introduce inner-scope bindings.
        Expr::PatternComprehension { .. } => {}
        // Literals, parameters, pattern predicates, etc. surface no refs.
        _ => {}
    }
}

/// True if `r` (a Variable or Property) is matched by some group key, either
/// by structural equality or by being a property of a variable that appears
/// standalone among the group keys (functional dependency on the group key).
fn ref_covered_by_group_keys(r: &Expr, group_keys: &[&Expr]) -> bool {
    for k in group_keys {
        if exprs_structurally_equal(r, k) {
            return true;
        }
        // n.foo is determined by `n`
        if let (Expr::Property(pa), Expr::Variable(name)) = (r, *k) {
            if &pa.variable == name {
                return true;
            }
        }
    }
    false
}

fn exprs_structurally_equal(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Variable(x), Expr::Variable(y)) => x == y,
        (Expr::Property(x), Expr::Property(y)) => {
            x.variable == y.variable && x.property == y.property
        }
        _ => false,
    }
}

fn describe_ref(r: &Expr) -> String {
    match r {
        Expr::Variable(name) => format!("'{name}'"),
        Expr::Property(pa) => format!("'{}.{}'", pa.variable, pa.property),
        other => format!("{other:?}"),
    }
}

/// After DISTINCT, ORDER BY can only see what's been projected. A reference
/// is visible if (a) the projection includes the variable as a bare item, or
/// (b) the projection includes the property exactly, or (c) the variable
/// itself appears in the projected scope.
fn ref_visible_after_distinct(
    r: &Expr,
    projection_items: &[ReturnItem],
    projected_scope: &Scope,
) -> bool {
    for item in projection_items {
        if exprs_structurally_equal(r, &item.expr) {
            return true;
        }
        // RETURN DISTINCT n ORDER BY n.foo — n is projected whole, so n.foo is OK
        if let (Expr::Property(pa), Expr::Variable(name)) = (r, &item.expr) {
            if &pa.variable == name {
                return true;
            }
        }
    }
    // Final fallback: the variable name appears in projected scope (covers
    // aliased projections like `RETURN n.name AS x ORDER BY x`).
    if let Expr::Variable(name) = r {
        if projected_scope.contains(name) {
            return true;
        }
    }
    false
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, args } => {
            is_aggregate_name(name) || args.iter().any(contains_aggregate)
        }
        Expr::CountStar => true,
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { expr, .. } => contains_aggregate(expr),
        Expr::List(items) => items.iter().any(contains_aggregate),
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            contains_aggregate(list)
                || predicate.as_deref().is_some_and(contains_aggregate)
                || projection.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Map(entries) => entries.iter().any(|(_, expr)| contains_aggregate(expr)),
        Expr::In { expr, list } => contains_aggregate(expr) || contains_aggregate(list),
        Expr::Index { target, index } => contains_aggregate(target) || contains_aggregate(index),
        Expr::Slice { target, start, end } => {
            contains_aggregate(target)
                || start.as_deref().is_some_and(contains_aggregate)
                || end.as_deref().is_some_and(contains_aggregate)
        }
        Expr::PatternPredicate(_) => false,
        Expr::PatternComprehension { projection, .. } => contains_aggregate(projection),
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
        Expr::ExistsSubquery(_) => false,
        Expr::ListPredicate {
            list, predicate, ..
        } => contains_aggregate(list) || predicate.as_deref().is_some_and(contains_aggregate),
        Expr::Literal(_) | Expr::Property(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
    }
}

fn contains_pattern_predicate(expr: &Expr) -> bool {
    match expr {
        Expr::PatternPredicate(_) => true,
        Expr::BinaryOp { left, right, .. } => {
            contains_pattern_predicate(left) || contains_pattern_predicate(right)
        }
        Expr::UnaryOp { expr, .. } | Expr::Exists(expr) => contains_pattern_predicate(expr),
        Expr::FunctionCall { args, .. } | Expr::List(args) => {
            args.iter().any(contains_pattern_predicate)
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            contains_pattern_predicate(list)
                || predicate.as_deref().is_some_and(contains_pattern_predicate)
                || projection
                    .as_deref()
                    .is_some_and(contains_pattern_predicate)
        }
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| contains_pattern_predicate(value)),
        Expr::In { expr, list } => {
            contains_pattern_predicate(expr) || contains_pattern_predicate(list)
        }
        Expr::Index { target, index } => {
            contains_pattern_predicate(target) || contains_pattern_predicate(index)
        }
        Expr::Slice { target, start, end } => {
            contains_pattern_predicate(target)
                || start.as_deref().is_some_and(contains_pattern_predicate)
                || end.as_deref().is_some_and(contains_pattern_predicate)
        }
        Expr::PatternComprehension { projection, .. } => contains_pattern_predicate(projection),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee.as_deref().is_some_and(contains_pattern_predicate)
                || arms.iter().any(|(when_expr, then_expr)| {
                    contains_pattern_predicate(when_expr) || contains_pattern_predicate(then_expr)
                })
                || default.as_deref().is_some_and(contains_pattern_predicate)
        }
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            contains_pattern_predicate(list)
                || predicate.as_deref().is_some_and(contains_pattern_predicate)
        }
        Expr::ExistsSubquery(_)
        | Expr::Literal(_)
        | Expr::Property(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::CountStar => false,
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "count" | "sum" | "avg" | "min" | "max" | "collect" | "percentiledisc" | "percentilecont"
    )
}

fn validate_aggregate_shape(expr: &Expr) -> CypherResult<()> {
    match expr {
        Expr::FunctionCall { name, args } => {
            if matches!(
                name.to_lowercase().as_str(),
                "count"
                    | "sum"
                    | "avg"
                    | "min"
                    | "max"
                    | "collect"
                    | "percentiledisc"
                    | "percentilecont"
            ) {
                for arg in args {
                    if contains_aggregate(arg) {
                        return Err(CypherError::Plan(
                            "nested aggregate expressions are not supported".into(),
                        ));
                    }
                    if contains_function_named(arg, "rand") {
                        return Err(CypherError::Plan(
                            "non-deterministic expressions are not allowed inside aggregates"
                                .into(),
                        ));
                    }
                }
            }
            for arg in args {
                validate_aggregate_shape(arg)?;
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
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            validate_aggregate_shape(list)?;
            if let Some(predicate) = predicate {
                validate_aggregate_shape(predicate)?;
            }
            if let Some(projection) = projection {
                validate_aggregate_shape(projection)?;
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
        Expr::PatternPredicate(_) => Ok(()),
        Expr::PatternComprehension { projection, .. } => validate_aggregate_shape(projection),
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
        Expr::ExistsSubquery(_) => Ok(()),
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

fn contains_function_named(expr: &Expr, wanted: &str) -> bool {
    match expr {
        Expr::FunctionCall { name, args } => {
            name.eq_ignore_ascii_case(wanted)
                || args.iter().any(|arg| contains_function_named(arg, wanted))
        }
        Expr::CountStar => false,
        Expr::BinaryOp { left, right, .. } => {
            contains_function_named(left, wanted) || contains_function_named(right, wanted)
        }
        Expr::UnaryOp { expr, .. } | Expr::Exists(expr) => contains_function_named(expr, wanted),
        Expr::List(items) => items
            .iter()
            .any(|item| contains_function_named(item, wanted)),
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            contains_function_named(list, wanted)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| contains_function_named(expr, wanted))
                || projection
                    .as_deref()
                    .is_some_and(|expr| contains_function_named(expr, wanted))
        }
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, expr)| contains_function_named(expr, wanted)),
        Expr::In { expr, list } => {
            contains_function_named(expr, wanted) || contains_function_named(list, wanted)
        }
        Expr::Index { target, index } => {
            contains_function_named(target, wanted) || contains_function_named(index, wanted)
        }
        Expr::Slice { target, start, end } => {
            contains_function_named(target, wanted)
                || start
                    .as_deref()
                    .is_some_and(|expr| contains_function_named(expr, wanted))
                || end
                    .as_deref()
                    .is_some_and(|expr| contains_function_named(expr, wanted))
        }
        Expr::PatternComprehension { projection, .. } => {
            contains_function_named(projection, wanted)
        }
        Expr::PatternPredicate(pattern) => pattern
            .elements
            .iter()
            .any(|element| pattern_element_contains_function_named(element, wanted)),
        Expr::ExistsSubquery(_) => false,
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee
                .as_deref()
                .is_some_and(|expr| contains_function_named(expr, wanted))
                || arms.iter().any(|(when_expr, then_expr)| {
                    contains_function_named(when_expr, wanted)
                        || contains_function_named(then_expr, wanted)
                })
                || default
                    .as_deref()
                    .is_some_and(|expr| contains_function_named(expr, wanted))
        }
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            contains_function_named(list, wanted)
                || predicate
                    .as_deref()
                    .is_some_and(|expr| contains_function_named(expr, wanted))
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) | Expr::Property(_) => false,
    }
}

fn pattern_element_contains_function_named(element: &PatternElement, wanted: &str) -> bool {
    match element {
        PatternElement::Node(node) => node
            .properties
            .iter()
            .any(|(_, expr)| contains_function_named(expr, wanted)),
        PatternElement::Relationship(rel) => rel
            .properties
            .iter()
            .any(|(_, expr)| contains_function_named(expr, wanted)),
    }
}

fn expr_alias(expr: &Expr) -> String {
    match expr {
        Expr::Variable(v) => v.clone(),
        Expr::Property(pa) => format!("{}.{}", pa.variable, pa.property),
        Expr::Literal(lit) => match lit {
            Literal::Integer(v) => v.to_string(),
            Literal::Float(v) => {
                if v.fract() == 0.0 {
                    format!("{v:.1}")
                } else {
                    v.to_string()
                }
            }
            Literal::String(v) => format!("'{v}'"),
            Literal::Bool(v) => v.to_string(),
            Literal::Null => "null".into(),
        },
        Expr::List(items) => format!(
            "[{}]",
            items.iter().map(expr_alias).collect::<Vec<_>>().join(", ")
        ),
        Expr::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(key, value)| format!("{key}: {}", expr_alias(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::In { expr, list } => format!("{} IN {}", expr_alias(expr), expr_alias(list)),
        Expr::Index { target, index } => format!("{}[{}]", expr_alias(target), expr_alias(index)),
        Expr::Slice { target, start, end } => format!(
            "{}[{}..{}]",
            expr_alias(target),
            start.as_deref().map(expr_alias).unwrap_or_default(),
            end.as_deref().map(expr_alias).unwrap_or_default()
        ),
        Expr::UnaryOp { op, expr } => match op {
            UnaryOp::Not => format!("NOT {}", expr_alias(expr)),
            UnaryOp::IsNull => format!("{} IS NULL", expr_alias(expr)),
            UnaryOp::IsNotNull => format!("{} IS NOT NULL", expr_alias(expr)),
        },
        Expr::BinaryOp { left, op, right } => {
            let op = match op {
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
                BinaryOp::Xor => "XOR",
                BinaryOp::Eq => "=",
                BinaryOp::Neq => "<>",
                BinaryOp::Lt => "<",
                BinaryOp::Lte => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::Gte => ">=",
                BinaryOp::Contains => "CONTAINS",
                BinaryOp::StartsWith => "STARTS WITH",
                BinaryOp::EndsWith => "ENDS WITH",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Pow => "^",
            };
            format!("{} {} {}", expr_alias(left), op, expr_alias(right))
        }
        Expr::FunctionCall { name, args } => {
            let arg_str: Vec<String> = args.iter().map(expr_alias).collect();
            format!("{}({})", name, arg_str.join(", "))
        }
        Expr::CountStar => "count(*)".into(),
        Expr::Parameter(name) => format!("${name}"),
        Expr::Exists(inner) => format!("EXISTS({})", expr_alias(inner)),
        Expr::ExistsSubquery(_) => "EXISTS { ... }".into(),
        _ => "expr".into(),
    }
}

fn return_item_alias(item: &ReturnItem) -> String {
    item.alias
        .clone()
        .or_else(|| item.raw.clone())
        .unwrap_or_else(|| expr_alias(&item.expr))
}

fn non_aggregate_order_refs(expr: &Expr) -> Vec<Expr> {
    let mut out = Vec::new();
    collect_non_aggregate_order_refs(expr, &mut out);
    out
}

fn collect_non_aggregate_order_refs(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::FunctionCall { name, .. } if is_aggregate_name(name) => {}
        Expr::CountStar => {}
        Expr::Variable(name) => {
            if name != "*" {
                out.push(expr.clone());
            }
        }
        Expr::Property(pa) => {
            out.push(Expr::Property(pa.clone()));
        }
        Expr::List(items) => {
            for item in items {
                collect_non_aggregate_order_refs(item, out);
            }
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => {
            collect_non_aggregate_order_refs(list, out);
            let mut local = Vec::new();
            if let Some(predicate) = predicate {
                collect_non_aggregate_order_refs(predicate, &mut local);
            }
            if let Some(projection) = projection {
                collect_non_aggregate_order_refs(projection, &mut local);
            }
            out.extend(
                local
                    .into_iter()
                    .filter(|expr| !matches!(expr, Expr::Variable(name) if name == variable)),
            );
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                collect_non_aggregate_order_refs(value, out);
            }
        }
        Expr::In { expr, list } => {
            collect_non_aggregate_order_refs(expr, out);
            collect_non_aggregate_order_refs(list, out);
        }
        Expr::Index { target, index } => {
            collect_non_aggregate_order_refs(target, out);
            collect_non_aggregate_order_refs(index, out);
        }
        Expr::Slice { target, start, end } => {
            collect_non_aggregate_order_refs(target, out);
            if let Some(start) = start {
                collect_non_aggregate_order_refs(start, out);
            }
            if let Some(end) = end {
                collect_non_aggregate_order_refs(end, out);
            }
        }
        Expr::UnaryOp { expr, .. } => collect_non_aggregate_order_refs(expr, out),
        Expr::BinaryOp { left, right, .. } => {
            collect_non_aggregate_order_refs(left, out);
            collect_non_aggregate_order_refs(right, out);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_non_aggregate_order_refs(arg, out);
            }
        }
        Expr::Exists(inner) => collect_non_aggregate_order_refs(inner, out),
        Expr::ExistsSubquery(_) => {}
        Expr::ListPredicate {
            variable,
            list,
            predicate,
            ..
        } => {
            collect_non_aggregate_order_refs(list, out);
            let mut local = Vec::new();
            if let Some(predicate) = predicate {
                collect_non_aggregate_order_refs(predicate, &mut local);
            }
            out.extend(
                local
                    .into_iter()
                    .filter(|expr| !matches!(expr, Expr::Variable(name) if name == variable)),
            );
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            if let Some(scrutinee) = scrutinee {
                collect_non_aggregate_order_refs(scrutinee, out);
            }
            for (when_expr, then_expr) in arms {
                collect_non_aggregate_order_refs(when_expr, out);
                collect_non_aggregate_order_refs(then_expr, out);
            }
            if let Some(default) = default {
                collect_non_aggregate_order_refs(default, out);
            }
        }
        Expr::PatternPredicate(_) | Expr::PatternComprehension { .. } => {}
        Expr::Literal(_) | Expr::Parameter(_) => {}
    }
}

fn aggregate_order_refs(expr: &Expr) -> Vec<Expr> {
    let mut out = Vec::new();
    collect_aggregate_order_refs(expr, &mut out);
    out
}

fn collect_aggregate_order_refs(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::CountStar => out.push(expr.clone()),
        Expr::FunctionCall { name, .. } if is_aggregate_name(name) => out.push(expr.clone()),
        Expr::List(items) => {
            for item in items {
                collect_aggregate_order_refs(item, out);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            collect_aggregate_order_refs(list, out);
            if let Some(predicate) = predicate {
                collect_aggregate_order_refs(predicate, out);
            }
            if let Some(projection) = projection {
                collect_aggregate_order_refs(projection, out);
            }
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                collect_aggregate_order_refs(value, out);
            }
        }
        Expr::In { expr, list } => {
            collect_aggregate_order_refs(expr, out);
            collect_aggregate_order_refs(list, out);
        }
        Expr::Index { target, index } => {
            collect_aggregate_order_refs(target, out);
            collect_aggregate_order_refs(index, out);
        }
        Expr::Slice { target, start, end } => {
            collect_aggregate_order_refs(target, out);
            if let Some(start) = start {
                collect_aggregate_order_refs(start, out);
            }
            if let Some(end) = end {
                collect_aggregate_order_refs(end, out);
            }
        }
        Expr::UnaryOp { expr, .. } => collect_aggregate_order_refs(expr, out),
        Expr::BinaryOp { left, right, .. } => {
            collect_aggregate_order_refs(left, out);
            collect_aggregate_order_refs(right, out);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_aggregate_order_refs(arg, out);
            }
        }
        Expr::Exists(inner) => collect_aggregate_order_refs(inner, out),
        Expr::ExistsSubquery(_) => {}
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            collect_aggregate_order_refs(list, out);
            if let Some(predicate) = predicate {
                collect_aggregate_order_refs(predicate, out);
            }
        }
        Expr::PatternComprehension { projection, .. } => {
            collect_aggregate_order_refs(projection, out);
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            if let Some(scrutinee) = scrutinee {
                collect_aggregate_order_refs(scrutinee, out);
            }
            for (when_expr, then_expr) in arms {
                collect_aggregate_order_refs(when_expr, out);
                collect_aggregate_order_refs(then_expr, out);
            }
            if let Some(default) = default {
                collect_aggregate_order_refs(default, out);
            }
        }
        _ => {}
    }
}

fn order_ref_is_projected(
    projected_scope: &Scope,
    projection_items: Option<&[ReturnItem]>,
    expr: &Expr,
) -> bool {
    if let Expr::Variable(name) = expr {
        return projected_scope.contains(name);
    }
    projection_items.is_some_and(|items| {
        let alias = expr_alias(expr);
        items.iter().any(|item| expr_alias(&item.expr) == alias)
    })
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
    fn bind_rejects_reused_relationship_variable_in_pattern() {
        let query = Parser::parse_read("MATCH (a)-[r]->()-[r]->(a) RETURN r").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("cannot be reused"));
    }

    #[test]
    fn bind_rejects_aggregate_in_where() {
        let query = Parser::parse_read("MATCH (a) WHERE count(a) > 10 RETURN a").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("not allowed in WHERE"));
    }

    #[test]
    fn bind_rejects_return_star_without_named_variables() {
        let query = Parser::parse_read("MATCH () RETURN *").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("RETURN * requires"));
    }

    #[test]
    fn bind_rejects_unaliased_with_expression() {
        let query = Parser::parse_read("MATCH (a) WITH a, count(*) RETURN a").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("WITH must be aliased"));
    }

    #[test]
    fn bind_rejects_non_projected_variable_in_aggregate_order_by() {
        let query = Parser::parse_read(
            "MATCH (me:Person)--(you:Person) RETURN count(you.age) AS agg ORDER BY me.age + count(you.age)",
        )
        .unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("non-projected variable"));
    }

    #[test]
    fn bind_rejects_dynamic_percentile_argument() {
        let query = Parser::parse_read(
            "MATCH (n:S) WITH n, size([(n)-->() | 1]) AS deg RETURN percentileDisc(0.90, deg)",
        )
        .unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("percentile argument"));
    }

    #[test]
    fn bind_unwind_introduces_alias() {
        let query = Parser::parse_read("UNWIND [1, 2] AS x RETURN x").unwrap();
        let bound = bind_query(&query).unwrap();
        assert_eq!(bound.final_scope.get("x"), Some(VarKind::Scalar));
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
    fn bind_rejects_property_and_labels_on_deleted_bindings() {
        let Statement::Write(query) = Parser::parse("MATCH (n) DELETE n RETURN n.num").unwrap()
        else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("after DELETE")
        );

        let Statement::Write(query) = Parser::parse("MATCH (n) DELETE n RETURN labels(n)").unwrap()
        else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("deleted variable")
        );
    }

    #[test]
    fn bind_allows_type_on_deleted_relationship() {
        let Statement::Write(query) =
            Parser::parse("MATCH ()-[r]->() DELETE r RETURN type(r)").unwrap()
        else {
            panic!("expected write");
        };
        bind_write(&query).unwrap();
    }

    #[test]
    fn bind_rejects_invalid_delete_targets() {
        for cypher in [
            "MATCH (n) DELETE n:Person",
            "MATCH ()-[r:T]-() DELETE r:T",
            "MATCH (n) DELETE 1 + 1",
        ] {
            let Statement::Write(query) = Parser::parse(cypher).unwrap() else {
                panic!("expected write");
            };
            assert!(bind_write(&query).is_err(), "{cypher} should fail");
        }
    }

    #[test]
    fn bind_rejects_illegal_create_patterns() {
        let Statement::Write(query) = Parser::parse("MATCH (a) CREATE (a)").unwrap() else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("already-bound node")
        );

        let Statement::Write(query) = Parser::parse("CREATE ()-->()").unwrap() else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("exactly one type")
        );

        let Statement::Write(query) = Parser::parse("CREATE (b {name: missing})").unwrap() else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("undefined variable")
        );

        let Statement::Write(query) =
            Parser::parse("CREATE (n:Foo) CREATE (n {})-[:OWNS]->(:Dog)").unwrap()
        else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("already-bound node")
        );
    }

    #[test]
    fn bind_rejects_property_access_on_path_variable() {
        let query =
            Parser::parse_read("MATCH (n) MATCH r = (n)-[*]->() WHERE r.name = 'apa' RETURN r")
                .unwrap();
        assert!(
            bind_query(&query)
                .unwrap_err()
                .to_string()
                .contains("path variable")
        );
    }

    #[test]
    fn bind_rejects_illegal_merge_patterns() {
        let Statement::Write(query) = Parser::parse("MERGE ({num: null})").unwrap() else {
            panic!("expected write");
        };
        assert!(bind_write(&query).unwrap_err().to_string().contains("null"));

        let Statement::Write(query) = Parser::parse("MERGE ()-[:A|:B]->()").unwrap() else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("exactly one type")
        );

        let Statement::Write(query) =
            Parser::parse("CREATE (a:Foo) MERGE (a)-[:KNOWS]->(a:Bar)").unwrap()
        else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("already-bound node")
        );
    }

    #[test]
    fn bind_rejects_undefined_merge_on_action_target() {
        let Statement::Write(query) = Parser::parse("MERGE (n) ON CREATE SET x.num = 1").unwrap()
        else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("undefined variable")
        );

        let Statement::Write(query) = Parser::parse("MERGE (n) ON MATCH SET x.num = 1").unwrap()
        else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("undefined variable")
        );
    }

    #[test]
    fn bind_rejects_map_values_in_graph_properties() {
        let Statement::Write(query) =
            Parser::parse("CREATE (a) SET a.maplist = [{num: 1}]").unwrap()
        else {
            panic!("expected write");
        };
        assert!(
            bind_write(&query)
                .unwrap_err()
                .to_string()
                .contains("lists of maps")
        );
    }

    #[test]
    fn bind_rejects_node_relationship_variable_type_conflict() {
        let query = Parser::parse_read("MATCH ()-[r]-() MATCH (r) RETURN r").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("already bound"));

        let query = Parser::parse_read("MATCH (r)-[r]-() RETURN r").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("already bound"));
    }

    #[test]
    fn bind_coalesce_preserves_node_kind_when_inputs_are_nodes() {
        let query = Parser::parse_read(
            "MATCH (a) OPTIONAL MATCH (a)-->(b) OPTIONAL MATCH (a)-->(c) WITH coalesce(b, c) AS x MATCH (x)-->(d) RETURN d",
        )
        .unwrap();
        bind_query(&query).unwrap();
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

    #[test]
    fn bind_rejects_numeric_quantifier_over_non_numeric_literal_list() {
        let query =
            Parser::parse_read("RETURN any(x IN ['Clara', 'Bob'] WHERE x % 2 = 0) AS result")
                .unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("numeric operand"));
    }

    #[test]
    fn bind_allows_numeric_quantifier_over_numeric_literal_list() {
        let query =
            Parser::parse_read("RETURN any(x IN [1, 2, null] WHERE x % 2 = 0) AS result").unwrap();
        bind_query(&query).unwrap();
    }

    #[test]
    fn bind_rejects_indexing_non_list_with_static_with_alias() {
        let query = Parser::parse_read("WITH true AS list, 0 AS idx RETURN list[idx]").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("list target"));
    }

    #[test]
    fn bind_rejects_range_zero_step_and_float_args() {
        let zero = Parser::parse_read("RETURN range(1, 5, 0)").unwrap();
        assert!(bind_query(&zero).unwrap_err().to_string().contains("step"));

        let float = Parser::parse_read("RETURN range(0 - 1.1, 5, 1)").unwrap();
        assert!(
            bind_query(&float)
                .unwrap_err()
                .to_string()
                .contains("integers")
        );
    }

    #[test]
    fn bind_rejects_in_against_non_list_rhs() {
        let query = Parser::parse_read("RETURN 1 IN true").unwrap();
        let err = bind_query(&query).unwrap_err();
        assert!(err.to_string().contains("right-hand side"));
    }
}
