//! Query planner: translates AST into a logical plan, then optimizes it.
//!
//! Logical plan operators:
//!   ScanVertices -> filter by label
//!   Expand      -> traverse one hop via CSR SpMV
//!   Filter      -> evaluate predicate on current bindings
//!   Project     -> select return columns
//!   Limit/Skip  -> cardinality control
//!
//! Optimizations:
//!   - Label pushdown: filter by vertex label at scan time
//!   - Predicate pushdown: push WHERE filters as close to scan as possible
//!   - Index selection: use composite/unique indexes when property equality detected
//!   - Traversal budgeting: enforce max-hop and max-node limits

use crate::ast::*;
use crate::error::{CypherError, CypherResult};
use nexus_core::Value;

/// A logical plan is a tree of operators evaluated bottom-up.
#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// One empty row. Used for RETURN/WITH/UNWIND queries that do not start
    /// with MATCH.
    Argument,
    ScanVertices {
        variable: String,
        label: Option<String>,
        index_lookup: Option<IndexLookup>,
    },
    Expand {
        input: Box<LogicalPlan>,
        src_var: String,
        dst_var: String,
        rel_var: Option<String>,
        edge_types: Vec<String>,
        rel_properties: Vec<(String, ProjectExpr)>,
        direction: ExpandDirection,
        min_hops: u32,
        max_hops: u32,
        dst_labels: Vec<String>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: Predicate,
    },
    ApplyMatch {
        input: Box<LogicalPlan>,
        clause: MatchClause,
        optional: bool,
        where_predicate: Option<Predicate>,
    },
    Unwind {
        input: Box<LogicalPlan>,
        expr: ProjectExpr,
        alias: String,
    },
    Project {
        input: Box<LogicalPlan>,
        columns: Vec<ProjectColumn>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<LogicalPlan>,
        count: RowCount,
    },
    Skip {
        input: Box<LogicalPlan>,
        count: RowCount,
    },
    Distinct {
        input: Box<LogicalPlan>,
    },
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<ProjectColumn>,
        aggregations: Vec<AggregateOp>,
    },
    Union {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        all: bool,
    },
}

#[derive(Debug, Clone)]
pub struct IndexLookup {
    pub property: String,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExpandDirection {
    Outgoing,
    Incoming,
    Both,
}

impl From<RelDirection> for ExpandDirection {
    fn from(d: RelDirection) -> Self {
        match d {
            RelDirection::Outgoing => ExpandDirection::Outgoing,
            RelDirection::Incoming => ExpandDirection::Incoming,
            RelDirection::Both => ExpandDirection::Both,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Predicate {
    Comparison {
        left: PropertyRef,
        op: CompareOp,
        right: PredicateValue,
    },
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
    IsNull(PropertyRef),
    IsNotNull(PropertyRef),
    StringOp {
        property: PropertyRef,
        op: StringPredOp,
        pattern: String,
    },
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct PropertyRef {
    pub variable: String,
    pub property: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompareOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone)]
pub enum PredicateValue {
    Literal(Value),
    Property(PropertyRef),
    Parameter(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StringPredOp {
    Contains,
    StartsWith,
    EndsWith,
}

#[derive(Debug, Clone)]
pub struct ProjectColumn {
    pub expr: ProjectExpr,
    pub alias: String,
}

#[derive(Debug, Clone)]
pub enum ProjectExpr {
    Wildcard,
    Variable(String),
    Property(PropertyRef),
    Function {
        name: String,
        args: Vec<ProjectExpr>,
    },
    Literal(Value),
    Expression(Expr),
}

#[derive(Debug, Clone)]
pub struct SortKey {
    pub expr: ProjectExpr,
    pub descending: bool,
}

#[derive(Debug, Clone)]
pub struct AggregateOp {
    pub function: String,
    pub inputs: Vec<ProjectExpr>,
    pub distinct: bool,
    pub alias: String,
}

/// Physical plan for a write statement.
///
/// `source` provides row bindings (from MATCH/WHERE) over which the
/// mutations are evaluated. When `source` is `None` the mutations run
/// exactly once (e.g. standalone CREATE).
#[derive(Debug, Clone)]
pub struct WritePlan {
    pub source: Option<LogicalPlan>,
    pub mutations: Vec<MutationOp>,
    pub return_columns: Option<Vec<ProjectColumn>>,
    pub return_order_by: Option<Vec<SortKey>>,
    pub return_skip: Option<RowCount>,
    pub return_limit: Option<RowCount>,
}

#[derive(Debug, Clone)]
pub enum MutationOp {
    ReadClause(ReadClause),
    BeginMerge,
    CreateNode {
        variable: String,
        labels: Vec<String>,
        properties: Vec<(String, PropertyValue)>,
    },
    CreateEdge {
        variable: Option<String>,
        src_var: String,
        dst_var: String,
        rel_type: String,
        properties: Vec<(String, PropertyValue)>,
    },
    MergeNode {
        variable: String,
        labels: Vec<String>,
        properties: Vec<(String, PropertyValue)>,
    },
    MergeEdge {
        variable: Option<String>,
        src_var: String,
        dst_var: String,
        rel_type: String,
        direction: RelDirection,
        properties: Vec<(String, PropertyValue)>,
    },
    BindPath {
        variable: String,
        node_vars: Vec<String>,
        edge_vars: Vec<String>,
    },
    ApplyMergeActions {
        on_create: Vec<MutationOp>,
        on_match: Vec<MutationOp>,
    },
    SetProperty {
        variable: String,
        key: String,
        value: PropertyValue,
    },
    RemoveProperty {
        variable: String,
        key: String,
    },
    /// `SET n = map` (`replace = true`) or `SET n += map`
    /// (`replace = false`).
    SetProperties {
        variable: String,
        value: PropertyValue,
        replace: bool,
    },
    /// `SET n:Foo:Bar` — add labels to an existing node binding.
    SetLabels {
        variable: String,
        labels: Vec<String>,
    },
    /// `REMOVE n:Foo:Bar` — remove labels from an existing node binding.
    RemoveLabels {
        variable: String,
        labels: Vec<String>,
    },
    Delete {
        target: ProjectExpr,
        variable: Option<String>,
        detach: bool,
    },
}

/// A literal or parameter value used in a mutation's property map.
#[derive(Debug, Clone)]
pub enum PropertyValue {
    Literal(Value),
    Parameter(String),
    Property(PropertyRef),
    /// Arbitrary AST expression — evaluated at execute time against the
    /// current row bindings and parameters. Used for arithmetic
    /// (`{x: 0 - 11}`), lists (`{xs: [1, 2]}`), function calls, etc.
    Expr(Box<Expr>),
}

/// Translate a parsed write statement into a physical write plan.
pub fn plan_write(wq: &WriteQuery) -> CypherResult<WritePlan> {
    // Source: MATCH/WHERE plus read-pipeline clauses (`WITH`, `UNWIND`,
    // tail `MATCH`) if present. No RETURN projection here — the write
    // executor consumes rows as binding sources for mutations.
    let source = if wq.match_clause.is_some() || !wq.tail.is_empty() {
        let mut plan = if wq.match_clause.is_some() {
            plan_initial_match(wq.match_clause.as_ref().unwrap())?
        } else {
            LogicalPlan::Argument
        };
        if let Some(ref where_clause) = wq.where_clause {
            let pred = translate_predicate(&where_clause.expr)?;
            plan = push_down_predicate(plan, pred);
        }
        let mut tail_idx = 0usize;
        while tail_idx < wq.tail.len() {
            match &wq.tail[tail_idx] {
                ReadClause::Match { optional, clause } => {
                    let where_predicate =
                        if let Some(ReadClause::Where(where_clause)) = wq.tail.get(tail_idx + 1) {
                            tail_idx += 1;
                            Some(translate_predicate(&where_clause.expr)?)
                        } else {
                            None
                        };
                    plan = LogicalPlan::ApplyMatch {
                        input: Box::new(plan),
                        clause: clause.clone(),
                        optional: *optional,
                        where_predicate,
                    };
                }
                clause => {
                    plan = plan_read_tail_clause(plan, clause)?;
                }
            }
            tail_idx += 1;
        }
        Some(plan)
    } else if wq.where_clause.is_some() {
        return Err(CypherError::Plan("WHERE requires a preceding MATCH".into()));
    } else {
        None
    };

    let mut mutations = Vec::new();
    for clause in &wq.mutations {
        match clause {
            MutationClause::Read(clause) => {
                mutations.push(MutationOp::ReadClause(clause.clone()));
            }
            MutationClause::Create { patterns } => {
                translate_create_patterns(patterns, &mut mutations)?;
            }
            MutationClause::Merge {
                patterns,
                on_create,
                on_match,
            } => {
                mutations.push(MutationOp::BeginMerge);
                translate_merge_patterns(patterns, &mut mutations)?;
                mutations.push(MutationOp::ApplyMergeActions {
                    on_create: translate_set_items(on_create)?,
                    on_match: translate_set_items(on_match)?,
                });
            }
            MutationClause::Set { items } => {
                mutations.extend(translate_set_items(items)?);
            }
            MutationClause::Remove { items } => {
                for item in items {
                    match item {
                        RemoveItem::Property(pa) => {
                            mutations.push(MutationOp::RemoveProperty {
                                variable: pa.variable.clone(),
                                key: pa.property.clone(),
                            });
                        }
                        RemoveItem::Labels { variable, labels } => {
                            mutations.push(MutationOp::RemoveLabels {
                                variable: variable.clone(),
                                labels: labels.clone(),
                            });
                        }
                    }
                }
            }
            MutationClause::Delete { targets, detach } => {
                for target in targets {
                    mutations.push(MutationOp::Delete {
                        target: translate_project_expr(target),
                        variable: match target {
                            Expr::Variable(name) => Some(name.clone()),
                            _ => None,
                        },
                        detach: *detach,
                    });
                }
            }
        }
    }

    let return_columns = wq.return_clause.as_ref().map(|return_clause| {
        return_clause
            .items
            .iter()
            .map(|item| ProjectColumn {
                expr: translate_project_expr(&item.expr),
                alias: return_item_alias(item),
            })
            .collect()
    });
    let return_order_by = wq.order_by.as_ref().map(|order_by| {
        order_by
            .items
            .iter()
            .map(|item| SortKey {
                expr: translate_project_expr(&rewrite_order_expr_for_return(
                    &item.expr,
                    wq.return_clause.as_ref(),
                )),
                descending: item.descending,
            })
            .collect()
    });

    Ok(WritePlan {
        source,
        mutations,
        return_columns,
        return_order_by,
        return_skip: wq.skip.clone(),
        return_limit: wq.limit.clone(),
    })
}

fn translate_create_patterns(
    patterns: &[Pattern],
    mutations: &mut Vec<MutationOp>,
) -> CypherResult<()> {
    for pattern in patterns {
        let mut prev_node_var: Option<String> = None;
        let mut pending_rel: Option<&RelationshipPattern> = None;

        for (idx, element) in pattern.elements.iter().enumerate() {
            match element {
                PatternElement::Node(np) => {
                    let var = np
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("_anon_create_{}_{}", mutations.len(), idx));

                    let labels = np.labels.clone();
                    let properties = translate_create_props(&np.properties)?;

                    mutations.push(MutationOp::CreateNode {
                        variable: var.clone(),
                        labels,
                        properties,
                    });

                    if let Some(rp) = pending_rel.take() {
                        let src = prev_node_var.clone().ok_or_else(|| {
                            CypherError::Plan("relationship has no source node".into())
                        })?;
                        let dst = var.clone();
                        let (src_v, dst_v) = match rp.direction {
                            RelDirection::Outgoing => (src, dst),
                            RelDirection::Incoming => (dst, src),
                            RelDirection::Both => {
                                return Err(CypherError::Plan(
                                    "CREATE requires a directed relationship".into(),
                                ));
                            }
                        };
                        let rel_type = rp.rel_types.first().cloned().unwrap_or_default();
                        mutations.push(MutationOp::CreateEdge {
                            variable: rp.variable.clone(),
                            src_var: src_v,
                            dst_var: dst_v,
                            rel_type,
                            properties: translate_create_props(&rp.properties)?,
                        });
                    }

                    prev_node_var = Some(var);
                }
                PatternElement::Relationship(rp) => {
                    pending_rel = Some(rp);
                }
            }
        }
    }
    Ok(())
}

fn translate_merge_patterns(
    patterns: &[Pattern],
    mutations: &mut Vec<MutationOp>,
) -> CypherResult<()> {
    for pattern in patterns {
        let mut prev_node_var: Option<String> = None;
        let mut pending_rel: Option<&RelationshipPattern> = None;
        let mut path_node_vars = Vec::new();
        let mut path_edge_vars = Vec::new();

        for (idx, element) in pattern.elements.iter().enumerate() {
            match element {
                PatternElement::Node(np) => {
                    let var = np
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("_anon_merge_{}_{}", mutations.len(), idx));
                    path_node_vars.push(var.clone());
                    let labels = np.labels.clone();

                    mutations.push(MutationOp::MergeNode {
                        variable: var.clone(),
                        labels,
                        properties: translate_create_props(&np.properties)?,
                    });

                    if let Some(rp) = pending_rel.take() {
                        let src = prev_node_var.clone().ok_or_else(|| {
                            CypherError::Plan("relationship has no source node".into())
                        })?;
                        let dst = var.clone();
                        let (src_v, dst_v) = match rp.direction {
                            RelDirection::Outgoing => (src, dst),
                            RelDirection::Incoming => (dst, src),
                            RelDirection::Both => (src, dst),
                        };
                        let rel_type = rp.rel_types.first().cloned().ok_or_else(|| {
                            CypherError::Plan("MERGE relationship must have a type".into())
                        })?;
                        let rel_var = rp.variable.clone().or_else(|| {
                            pattern
                                .path_variable
                                .as_ref()
                                .map(|_| format!("_anon_merge_rel_{}_{}", mutations.len(), idx))
                        });
                        if let Some(rel_var) = &rel_var {
                            path_edge_vars.push(rel_var.clone());
                        }
                        mutations.push(MutationOp::MergeEdge {
                            variable: rel_var,
                            src_var: src_v,
                            dst_var: dst_v,
                            rel_type,
                            direction: rp.direction,
                            properties: translate_create_props(&rp.properties)?,
                        });
                    }

                    prev_node_var = Some(var);
                }
                PatternElement::Relationship(rp) => {
                    pending_rel = Some(rp);
                }
            }
        }
        if let Some(path_variable) = &pattern.path_variable {
            mutations.push(MutationOp::BindPath {
                variable: path_variable.clone(),
                node_vars: path_node_vars,
                edge_vars: path_edge_vars,
            });
        }
    }
    Ok(())
}

fn translate_create_props(props: &[(String, Expr)]) -> CypherResult<Vec<(String, PropertyValue)>> {
    props
        .iter()
        .map(|(k, e)| {
            let pv = match e {
                _ => translate_mutation_value(e)?,
            };
            Ok((k.clone(), pv))
        })
        .collect()
}

fn translate_set_items(items: &[SetItem]) -> CypherResult<Vec<MutationOp>> {
    let mut mutations = Vec::new();
    for item in items {
        match item {
            SetItem::Property { target, value } => {
                mutations.push(MutationOp::SetProperty {
                    variable: target.variable.clone(),
                    key: target.property.clone(),
                    value: translate_mutation_value(value)?,
                });
            }
            SetItem::Properties {
                variable,
                value,
                replace,
            } => {
                mutations.push(MutationOp::SetProperties {
                    variable: variable.clone(),
                    value: translate_mutation_value(value)?,
                    replace: *replace,
                });
            }
            SetItem::Labels { variable, labels } => {
                mutations.push(MutationOp::SetLabels {
                    variable: variable.clone(),
                    labels: labels.clone(),
                });
            }
        }
    }
    Ok(mutations)
}

fn translate_mutation_value(expr: &Expr) -> CypherResult<PropertyValue> {
    match expr {
        Expr::Literal(lit) => Ok(PropertyValue::Literal(literal_to_value(lit))),
        Expr::Parameter(name) => Ok(PropertyValue::Parameter(name.clone())),
        Expr::Property(pa) => Ok(PropertyValue::Property(PropertyRef {
            variable: pa.variable.clone(),
            property: pa.property.clone(),
        })),
        // Any other expression (arithmetic, lists, maps, function calls) is
        // preserved and evaluated at mutation time. This unblocks TCK
        // scenarios that use e.g. `{num: 0 - 11}` or `{xs: [1, 2, 3]}`.
        other => Ok(PropertyValue::Expr(Box::new(other.clone()))),
    }
}

fn plan_match_clause_opt(match_clause: Option<&MatchClause>) -> CypherResult<LogicalPlan> {
    let match_clause =
        match_clause.ok_or_else(|| CypherError::Plan("expected MATCH clause".into()))?;

    if match_clause.patterns.is_empty() {
        return Err(CypherError::Plan("empty MATCH clause".into()));
    }

    let pattern = &match_clause.patterns[0];
    let mut plan: Option<LogicalPlan> = None;

    let mut i = 0;
    while i < pattern.elements.len() {
        match &pattern.elements[i] {
            PatternElement::Node(np) => {
                if plan.is_none() {
                    let variable = np.variable.clone().unwrap_or_else(|| format!("_anon_{i}"));
                    let label = if np.labels.is_empty() {
                        None
                    } else {
                        Some(np.labels.join(":"))
                    };

                    let index_lookup = np.properties.first().map(|(key, expr)| IndexLookup {
                        property: key.clone(),
                        value: expr_to_value(expr),
                    });

                    plan = Some(LogicalPlan::ScanVertices {
                        variable,
                        label,
                        index_lookup,
                    });
                }
                i += 1;
            }
            PatternElement::Relationship(rp) => {
                let next_node = if i + 1 < pattern.elements.len() {
                    if let PatternElement::Node(np) = &pattern.elements[i + 1] {
                        np.clone()
                    } else {
                        return Err(CypherError::Plan("expected node after relationship".into()));
                    }
                } else {
                    return Err(CypherError::Plan(
                        "relationship must end with a node".into(),
                    ));
                };

                let src_var = extract_last_variable(&plan);
                let dst_var = next_node
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("_anon_{}", i + 1));
                let variable_length = rp.min_hops.is_some() || rp.max_hops.is_some();

                let expand = LogicalPlan::Expand {
                    input: Box::new(plan.take().unwrap()),
                    src_var,
                    dst_var: dst_var.clone(),
                    rel_var: rp.variable.clone(),
                    edge_types: rp.rel_types.clone(),
                    rel_properties: rp
                        .properties
                        .iter()
                        .map(|(key, expr)| (key.clone(), translate_project_expr(expr)))
                        .collect(),
                    direction: rp.direction.into(),
                    min_hops: rp.min_hops.unwrap_or(1),
                    max_hops: if variable_length {
                        rp.max_hops.unwrap_or(u32::MAX)
                    } else {
                        1
                    },
                    dst_labels: next_node.labels.clone(),
                };

                let mut next_plan = expand;
                for (key, expr) in &next_node.properties {
                    let predicate = Predicate::Comparison {
                        left: PropertyRef {
                            variable: dst_var.clone(),
                            property: key.clone(),
                        },
                        op: CompareOp::Eq,
                        right: translate_predicate_value(expr)?,
                    };
                    next_plan = LogicalPlan::Filter {
                        input: Box::new(next_plan),
                        predicate,
                    };
                }

                plan = Some(next_plan);
                i += 2;
            }
        }
    }

    plan.ok_or_else(|| CypherError::Plan("empty pattern".into()))
}

/// Translate a parsed AST into a logical plan.
pub fn plan_query(query: &Query) -> CypherResult<LogicalPlan> {
    let mut plan = if query.match_clause.is_some() {
        plan_initial_match(query.match_clause.as_ref().unwrap())?
    } else {
        LogicalPlan::Argument
    };

    if let Some(ref where_clause) = query.where_clause {
        let pred = translate_predicate(&where_clause.expr)?;
        plan = push_down_predicate(plan, pred);
    }

    let mut tail_idx = 0usize;
    while tail_idx < query.tail.len() {
        match &query.tail[tail_idx] {
            ReadClause::Match { optional, clause } => {
                let where_predicate =
                    if let Some(ReadClause::Where(where_clause)) = query.tail.get(tail_idx + 1) {
                        tail_idx += 1;
                        Some(translate_predicate(&where_clause.expr)?)
                    } else {
                        None
                    };
                plan = LogicalPlan::ApplyMatch {
                    input: Box::new(plan),
                    clause: clause.clone(),
                    optional: *optional,
                    where_predicate,
                };
            }
            clause => {
                plan = plan_read_tail_clause(plan, clause)?;
            }
        }
        tail_idx += 1;
    }

    plan = plan_return_clause(plan, &query.return_clause)?;

    if let Some(ref order_by) = query.order_by {
        let keys: Vec<SortKey> = order_by
            .items
            .iter()
            .map(|item| SortKey {
                expr: translate_project_expr(&rewrite_order_expr_for_return(
                    &item.expr,
                    Some(&query.return_clause),
                )),
                descending: item.descending,
            })
            .collect();
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            keys,
        };
    }

    if let Some(skip) = &query.skip {
        plan = LogicalPlan::Skip {
            input: Box::new(plan),
            count: skip.clone(),
        };
    }

    if let Some(limit) = &query.limit {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            count: limit.clone(),
        };
    }

    if let Some(union) = &query.union {
        let right = plan_query(&union.right)?;
        plan = LogicalPlan::Union {
            left: Box::new(plan),
            right: Box::new(right),
            all: union.all,
        };
    }

    Ok(plan)
}

fn rewrite_order_expr_for_return(expr: &Expr, return_clause: Option<&ReturnClause>) -> Expr {
    if let Some(return_clause) = return_clause {
        let alias = expr_alias(expr);
        for item in &return_clause.items {
            if expr_alias(&item.expr) == alias {
                return Expr::Variable(return_item_alias(item));
            }
        }
    }

    match expr {
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(rewrite_order_expr_for_return(left, return_clause)),
            op: *op,
            right: Box::new(rewrite_order_expr_for_return(right, return_clause)),
        },
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op: *op,
            expr: Box::new(rewrite_order_expr_for_return(expr, return_clause)),
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| rewrite_order_expr_for_return(arg, return_clause))
                .collect(),
        },
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|item| rewrite_order_expr_for_return(item, return_clause))
                .collect(),
        ),
        Expr::Map(entries) => Expr::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        rewrite_order_expr_for_return(value, return_clause),
                    )
                })
                .collect(),
        ),
        Expr::In { expr, list } => Expr::In {
            expr: Box::new(rewrite_order_expr_for_return(expr, return_clause)),
            list: Box::new(rewrite_order_expr_for_return(list, return_clause)),
        },
        Expr::Index { target, index } => Expr::Index {
            target: Box::new(rewrite_order_expr_for_return(target, return_clause)),
            index: Box::new(rewrite_order_expr_for_return(index, return_clause)),
        },
        Expr::Slice { target, start, end } => Expr::Slice {
            target: Box::new(rewrite_order_expr_for_return(target, return_clause)),
            start: start
                .as_deref()
                .map(|expr| Box::new(rewrite_order_expr_for_return(expr, return_clause))),
            end: end
                .as_deref()
                .map(|expr| Box::new(rewrite_order_expr_for_return(expr, return_clause))),
        },
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => Expr::Case {
            scrutinee: scrutinee
                .as_deref()
                .map(|expr| Box::new(rewrite_order_expr_for_return(expr, return_clause))),
            arms: arms
                .iter()
                .map(|(when, then)| {
                    (
                        rewrite_order_expr_for_return(when, return_clause),
                        rewrite_order_expr_for_return(then, return_clause),
                    )
                })
                .collect(),
            default: default
                .as_deref()
                .map(|expr| Box::new(rewrite_order_expr_for_return(expr, return_clause))),
        },
        Expr::Exists(inner) => Expr::Exists(Box::new(rewrite_order_expr_for_return(
            inner,
            return_clause,
        ))),
        Expr::ListPredicate {
            kind,
            variable,
            list,
            predicate,
        } => Expr::ListPredicate {
            kind: *kind,
            variable: variable.clone(),
            list: Box::new(rewrite_order_expr_for_return(list, return_clause)),
            predicate: predicate
                .as_deref()
                .map(|expr| Box::new(rewrite_order_expr_for_return(expr, return_clause))),
        },
        _ => expr.clone(),
    }
}

fn plan_initial_match(match_clause: &MatchClause) -> CypherResult<LogicalPlan> {
    if match_clause.patterns.is_empty() {
        return Err(CypherError::Plan("empty MATCH clause".into()));
    }

    let first = MatchClause {
        patterns: vec![match_clause.patterns[0].clone()],
    };
    let mut plan = if pattern_needs_match_engine(&match_clause.patterns[0]) {
        LogicalPlan::ApplyMatch {
            input: Box::new(LogicalPlan::Argument),
            clause: first,
            optional: false,
            where_predicate: None,
        }
    } else {
        plan_match_clause_opt(Some(&first))?
    };
    if match_clause.patterns.len() > 1 {
        for pattern in match_clause.patterns.iter().skip(1) {
            plan = LogicalPlan::ApplyMatch {
                input: Box::new(plan),
                clause: MatchClause {
                    patterns: vec![pattern.clone()],
                },
                optional: false,
                where_predicate: None,
            };
        }
    }
    Ok(plan)
}

fn pattern_needs_match_engine(pattern: &Pattern) -> bool {
    if pattern.path_variable.is_some() {
        return true;
    }
    let mut seen_nodes = std::collections::HashSet::new();
    let mut seen_relationships = std::collections::HashSet::new();
    let mut relationships = 0usize;
    let mut variable_length_relationship_binding = false;
    for element in &pattern.elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(var) = &node.variable {
                    if !seen_nodes.insert(var) {
                        return true;
                    }
                }
            }
            PatternElement::Relationship(rel) => {
                relationships += 1;
                if let Some(var) = &rel.variable {
                    if !seen_relationships.insert(var) {
                        return true;
                    }
                    if rel.min_hops.is_some() || rel.max_hops.is_some() {
                        variable_length_relationship_binding = true;
                    }
                }
            }
        }
    }
    relationships > 1 || variable_length_relationship_binding
}

fn plan_read_tail_clause(input: LogicalPlan, clause: &ReadClause) -> CypherResult<LogicalPlan> {
    match clause {
        ReadClause::Match { optional, clause } => Ok(LogicalPlan::ApplyMatch {
            input: Box::new(input),
            clause: clause.clone(),
            optional: *optional,
            where_predicate: None,
        }),
        ReadClause::Where(where_clause) => Ok(LogicalPlan::Filter {
            input: Box::new(input),
            predicate: translate_predicate(&where_clause.expr)?,
        }),
        ReadClause::With(with_clause) => plan_with_clause(input, with_clause),
        ReadClause::Unwind { expr, alias } => Ok(LogicalPlan::Unwind {
            input: Box::new(input),
            expr: translate_project_expr(expr),
            alias: alias.clone(),
        }),
    }
}

fn plan_with_clause(input: LogicalPlan, with_clause: &WithClause) -> CypherResult<LogicalPlan> {
    let aliases: Vec<String> = with_clause.items.iter().map(return_item_alias).collect();

    let mut input = input;
    let projection_clause = ReturnClause {
        items: with_clause.items.clone(),
        distinct: with_clause.distinct,
    };
    let has_aggregation = with_clause
        .items
        .iter()
        .any(|item| expr_contains_aggregate(&item.expr));

    if !has_aggregation {
        if let Some(where_clause) = &with_clause.where_clause {
            let referenced = expr_referenced_variables(&where_clause.expr);
            let mixed_alias_and_input_refs = referenced.iter().any(|name| aliases.contains(name))
                && referenced.iter().any(|name| !aliases.contains(name));
            if mixed_alias_and_input_refs {
                let mut intermediate_items = with_clause.items.clone();
                for name in referenced.iter().filter(|name| !aliases.contains(*name)) {
                    intermediate_items.push(ReturnItem {
                        expr: Expr::Variable(name.clone()),
                        alias: Some(name.clone()),
                        raw: Some(name.clone()),
                    });
                }
                let intermediate_clause = ReturnClause {
                    items: intermediate_items,
                    distinct: false,
                };
                let mut plan = plan_return_clause(input, &intermediate_clause)?;
                plan = LogicalPlan::Filter {
                    input: Box::new(plan),
                    predicate: translate_predicate(&where_clause.expr)?,
                };
                plan = plan_return_clause(plan, &projection_clause)?;
                if with_clause.distinct {
                    plan = LogicalPlan::Distinct {
                        input: Box::new(plan),
                    };
                }
                if let Some(order_by) = &with_clause.order_by {
                    let keys: Vec<SortKey> = order_by
                        .items
                        .iter()
                        .map(|item| SortKey {
                            expr: translate_project_expr(&rewrite_order_expr_for_return(
                                &item.expr,
                                Some(&projection_clause),
                            )),
                            descending: item.descending,
                        })
                        .collect();
                    plan = LogicalPlan::Sort {
                        input: Box::new(plan),
                        keys,
                    };
                }
                if let Some(skip) = &with_clause.skip {
                    plan = LogicalPlan::Skip {
                        input: Box::new(plan),
                        count: skip.clone(),
                    };
                }
                if let Some(limit) = &with_clause.limit {
                    plan = LogicalPlan::Limit {
                        input: Box::new(plan),
                        count: limit.clone(),
                    };
                }
                return Ok(plan);
            }
        }
    }

    let filter_after_projection = with_clause
        .where_clause
        .as_ref()
        .is_some_and(|where_clause| {
            let referenced = expr_referenced_variables(&where_clause.expr);
            !referenced.is_empty() && referenced.iter().all(|name| aliases.contains(name))
        });

    if let Some(where_clause) = &with_clause.where_clause {
        if !filter_after_projection {
            input = LogicalPlan::Filter {
                input: Box::new(input),
                predicate: translate_predicate(&where_clause.expr)?,
            };
        }
    }

    let order_before_projection = !has_aggregation
        && with_clause.order_by.as_ref().is_some_and(|order_by| {
            order_by.items.iter().any(|item| {
                !order_expr_available_after_projection(&item.expr, &projection_clause, &aliases)
            })
        });

    if order_before_projection {
        if let Some(order_by) = &with_clause.order_by {
            let keys: Vec<SortKey> = order_by
                .items
                .iter()
                .map(|item| SortKey {
                    expr: translate_project_expr(&item.expr),
                    descending: item.descending,
                })
                .collect();
            input = LogicalPlan::Sort {
                input: Box::new(input),
                keys,
            };
        }

        if let Some(skip) = &with_clause.skip {
            input = LogicalPlan::Skip {
                input: Box::new(input),
                count: skip.clone(),
            };
        }

        if let Some(limit) = &with_clause.limit {
            input = LogicalPlan::Limit {
                input: Box::new(input),
                count: limit.clone(),
            };
        }
    }

    let mut plan = plan_return_clause(input, &projection_clause)?;

    if let Some(where_clause) = &with_clause.where_clause {
        if filter_after_projection {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: translate_predicate(&where_clause.expr)?,
            };
        }
    }

    if !order_before_projection {
        if let Some(order_by) = &with_clause.order_by {
            let keys: Vec<SortKey> = order_by
                .items
                .iter()
                .map(|item| SortKey {
                    expr: translate_project_expr(&rewrite_order_expr_for_return(
                        &item.expr,
                        Some(&projection_clause),
                    )),
                    descending: item.descending,
                })
                .collect();
            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                keys,
            };
        }
    }

    if !order_before_projection {
        if let Some(skip) = &with_clause.skip {
            plan = LogicalPlan::Skip {
                input: Box::new(plan),
                count: skip.clone(),
            };
        }

        if let Some(limit) = &with_clause.limit {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                count: limit.clone(),
            };
        }
    }

    Ok(plan)
}

fn order_expr_available_after_projection(
    expr: &Expr,
    projection_clause: &ReturnClause,
    aliases: &[String],
) -> bool {
    let order_alias = expr_alias(expr);
    if projection_clause.items.iter().any(|item| {
        item.alias.as_deref() == Some(order_alias.as_str()) || expr_alias(&item.expr) == order_alias
    }) {
        return true;
    }

    let referenced = expr_referenced_variables(expr);
    !referenced.is_empty() && referenced.iter().all(|name| aliases.contains(name))
}

fn extract_last_variable(plan: &Option<LogicalPlan>) -> String {
    match plan {
        Some(LogicalPlan::ScanVertices { variable, .. }) => variable.clone(),
        Some(LogicalPlan::Expand { dst_var, .. }) => dst_var.clone(),
        Some(LogicalPlan::Filter { input, .. }) => extract_last_variable(&Some(*input.clone())),
        Some(LogicalPlan::ApplyMatch { input, .. }) => extract_last_variable(&Some(*input.clone())),
        Some(LogicalPlan::Unwind { alias, .. }) => alias.clone(),
        Some(LogicalPlan::Project { columns, .. }) => columns
            .last()
            .map(|col| col.alias.clone())
            .unwrap_or_else(|| "_anon_0".into()),
        Some(LogicalPlan::Sort { input, .. })
        | Some(LogicalPlan::Limit { input, .. })
        | Some(LogicalPlan::Skip { input, .. })
        | Some(LogicalPlan::Distinct { input })
        | Some(LogicalPlan::Aggregate { input, .. }) => {
            extract_last_variable(&Some(*input.clone()))
        }
        _ => "_anon_0".into(),
    }
}

fn translate_predicate(expr: &Expr) -> CypherResult<Predicate> {
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOp::And => {
                let l = translate_predicate(left)?;
                let r = translate_predicate(right)?;
                Ok(Predicate::And(Box::new(l), Box::new(r)))
            }
            BinaryOp::Or => {
                let l = translate_predicate(left)?;
                let r = translate_predicate(right)?;
                Ok(Predicate::Or(Box::new(l), Box::new(r)))
            }
            BinaryOp::Contains => {
                let Ok(prop) = expr_to_property_ref(left) else {
                    return Ok(Predicate::Expr(expr.clone()));
                };
                let Ok(pattern) = expr_to_string_value(right) else {
                    return Ok(Predicate::Expr(expr.clone()));
                };
                Ok(Predicate::StringOp {
                    property: prop,
                    op: StringPredOp::Contains,
                    pattern,
                })
            }
            BinaryOp::StartsWith => {
                let Ok(prop) = expr_to_property_ref(left) else {
                    return Ok(Predicate::Expr(expr.clone()));
                };
                let Ok(pattern) = expr_to_string_value(right) else {
                    return Ok(Predicate::Expr(expr.clone()));
                };
                Ok(Predicate::StringOp {
                    property: prop,
                    op: StringPredOp::StartsWith,
                    pattern,
                })
            }
            BinaryOp::EndsWith => {
                let Ok(prop) = expr_to_property_ref(left) else {
                    return Ok(Predicate::Expr(expr.clone()));
                };
                let Ok(pattern) = expr_to_string_value(right) else {
                    return Ok(Predicate::Expr(expr.clone()));
                };
                Ok(Predicate::StringOp {
                    property: prop,
                    op: StringPredOp::EndsWith,
                    pattern,
                })
            }
            _ => {
                if matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::Neq
                        | BinaryOp::Lt
                        | BinaryOp::Lte
                        | BinaryOp::Gt
                        | BinaryOp::Gte
                ) {
                    if let Ok(left_ref) = expr_to_property_ref(left) {
                        let Ok(right_val) = translate_predicate_value(right) else {
                            return Ok(Predicate::Expr(expr.clone()));
                        };
                        let compare_op = match op {
                            BinaryOp::Eq => CompareOp::Eq,
                            BinaryOp::Neq => CompareOp::Neq,
                            BinaryOp::Lt => CompareOp::Lt,
                            BinaryOp::Lte => CompareOp::Lte,
                            BinaryOp::Gt => CompareOp::Gt,
                            BinaryOp::Gte => CompareOp::Gte,
                            _ => unreachable!(),
                        };
                        Ok(Predicate::Comparison {
                            left: left_ref,
                            op: compare_op,
                            right: right_val,
                        })
                    } else {
                        Ok(Predicate::Expr(expr.clone()))
                    }
                } else {
                    Ok(Predicate::Expr(expr.clone()))
                }
            }
        },
        Expr::UnaryOp { op, expr } => match op {
            UnaryOp::Not => Ok(Predicate::Expr(Expr::UnaryOp {
                op: UnaryOp::Not,
                expr: expr.clone(),
            })),
            UnaryOp::IsNull => expr_to_property_ref(expr)
                .map(Predicate::IsNull)
                .or_else(|_| {
                    Ok(Predicate::Expr(Expr::UnaryOp {
                        op: UnaryOp::IsNull,
                        expr: expr.clone(),
                    }))
                }),
            UnaryOp::IsNotNull => expr_to_property_ref(expr)
                .map(Predicate::IsNotNull)
                .or_else(|_| {
                    Ok(Predicate::Expr(Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        expr: expr.clone(),
                    }))
                }),
        },
        _ => Ok(Predicate::Expr(expr.clone())),
    }
}

fn expr_to_property_ref(expr: &Expr) -> CypherResult<PropertyRef> {
    match expr {
        Expr::Property(pa) => Ok(PropertyRef {
            variable: pa.variable.clone(),
            property: pa.property.clone(),
        }),
        _ => Err(CypherError::Plan(format!(
            "expected property access, got {expr:?}"
        ))),
    }
}

fn expr_to_string_value(expr: &Expr) -> CypherResult<String> {
    match expr {
        Expr::Literal(Literal::String(s)) => Ok(s.clone()),
        _ => Err(CypherError::Plan(format!(
            "expected string literal, got {expr:?}"
        ))),
    }
}

fn translate_predicate_value(expr: &Expr) -> CypherResult<PredicateValue> {
    match expr {
        Expr::Literal(lit) => Ok(PredicateValue::Literal(literal_to_value(lit))),
        Expr::Property(pa) => Ok(PredicateValue::Property(PropertyRef {
            variable: pa.variable.clone(),
            property: pa.property.clone(),
        })),
        Expr::Parameter(name) => Ok(PredicateValue::Parameter(name.clone())),
        _ => Err(CypherError::Plan(format!(
            "unsupported value expression: {expr:?}"
        ))),
    }
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Integer(i) => Value::Int64(*i),
        Literal::Float(f) => Value::Float64(*f),
        Literal::String(s) => Value::String(s.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

fn expr_to_value(expr: &Expr) -> Value {
    match expr {
        Expr::Literal(lit) => literal_to_value(lit),
        _ => Value::Null,
    }
}

/// Predicate pushdown: if a filter references only the scan variable,
/// attach it to the scan via an index lookup when possible.
fn push_down_predicate(plan: LogicalPlan, predicate: Predicate) -> LogicalPlan {
    if let Predicate::And(left, right) = predicate {
        let plan = push_down_predicate(plan, *left);
        return push_down_predicate(plan, *right);
    }

    match plan {
        LogicalPlan::ScanVertices {
            variable,
            label,
            index_lookup,
        } => {
            if index_lookup.is_none() {
                if let Predicate::Comparison {
                    ref left,
                    op: CompareOp::Eq,
                    ref right,
                } = predicate
                {
                    if left.variable == variable {
                        if let PredicateValue::Literal(val) = right {
                            return LogicalPlan::ScanVertices {
                                variable,
                                label,
                                index_lookup: Some(IndexLookup {
                                    property: left.property.clone(),
                                    value: val.clone(),
                                }),
                            };
                        }
                    }
                }
            }

            LogicalPlan::Filter {
                input: Box::new(LogicalPlan::ScanVertices {
                    variable,
                    label,
                    index_lookup,
                }),
                predicate,
            }
        }
        LogicalPlan::Expand {
            input,
            src_var,
            dst_var,
            rel_var,
            edge_types,
            rel_properties,
            direction,
            min_hops,
            max_hops,
            dst_labels,
        } => {
            let references_new_expand_binding = predicate_references_variable(&predicate, &dst_var)
                || rel_var
                    .as_ref()
                    .is_some_and(|var| predicate_references_variable(&predicate, var));

            if !references_new_expand_binding {
                return LogicalPlan::Expand {
                    input: Box::new(push_down_predicate(*input, predicate)),
                    src_var,
                    dst_var,
                    rel_var,
                    edge_types,
                    rel_properties,
                    direction,
                    min_hops,
                    max_hops,
                    dst_labels,
                };
            }

            LogicalPlan::Filter {
                input: Box::new(LogicalPlan::Expand {
                    input,
                    src_var,
                    dst_var,
                    rel_var,
                    edge_types,
                    rel_properties,
                    direction,
                    min_hops,
                    max_hops,
                    dst_labels,
                }),
                predicate,
            }
        }
        LogicalPlan::ApplyMatch {
            input,
            clause,
            optional,
            where_predicate,
        } => {
            if matches!(input.as_ref(), LogicalPlan::Argument) {
                if let Some((seed_var, seed_label)) = match_seed_node(&clause) {
                    if predicate_references_only_variable(&predicate, &seed_var) {
                        let seed_scan = LogicalPlan::ScanVertices {
                            variable: seed_var,
                            label: seed_label,
                            index_lookup: None,
                        };
                        return LogicalPlan::ApplyMatch {
                            input: Box::new(push_down_predicate(seed_scan, predicate)),
                            clause,
                            optional,
                            where_predicate,
                        };
                    }
                }
            }

            LogicalPlan::Filter {
                input: Box::new(LogicalPlan::ApplyMatch {
                    input,
                    clause,
                    optional,
                    where_predicate,
                }),
                predicate,
            }
        }
        other => LogicalPlan::Filter {
            input: Box::new(other),
            predicate,
        },
    }
}

fn match_seed_node(clause: &MatchClause) -> Option<(String, Option<String>)> {
    let pattern = clause.patterns.first()?;
    let PatternElement::Node(node) = pattern.elements.first()? else {
        return None;
    };
    let labels = if node.labels.is_empty() {
        None
    } else {
        Some(node.labels.join(":"))
    };
    Some((node.variable.clone()?, labels))
}

fn predicate_references_only_variable(predicate: &Predicate, variable: &str) -> bool {
    match predicate {
        Predicate::Comparison { left, right, .. } => {
            left.variable == variable
                && match right {
                    PredicateValue::Property(prop) => prop.variable == variable,
                    PredicateValue::Literal(_) | PredicateValue::Parameter(_) => true,
                }
        }
        Predicate::And(left, right) | Predicate::Or(left, right) => {
            predicate_references_only_variable(left, variable)
                && predicate_references_only_variable(right, variable)
        }
        Predicate::Not(inner) => predicate_references_only_variable(inner, variable),
        Predicate::IsNull(prop) | Predicate::IsNotNull(prop) => prop.variable == variable,
        Predicate::StringOp { property, .. } => property.variable == variable,
        Predicate::Expr(expr) => expr_references_only_variable(expr, variable),
    }
}

fn predicate_references_variable(predicate: &Predicate, variable: &str) -> bool {
    match predicate {
        Predicate::Comparison { left, right, .. } => {
            left.variable == variable
                || match right {
                    PredicateValue::Property(prop) => prop.variable == variable,
                    PredicateValue::Literal(_) | PredicateValue::Parameter(_) => false,
                }
        }
        Predicate::And(left, right) | Predicate::Or(left, right) => {
            predicate_references_variable(left, variable)
                || predicate_references_variable(right, variable)
        }
        Predicate::Not(inner) => predicate_references_variable(inner, variable),
        Predicate::IsNull(prop) | Predicate::IsNotNull(prop) => prop.variable == variable,
        Predicate::StringOp { property, .. } => property.variable == variable,
        Predicate::Expr(expr) => expr_references_variable(expr, variable),
    }
}

fn expr_references_only_variable(expr: &Expr, variable: &str) -> bool {
    let referenced = expr_referenced_variables(expr);
    !referenced.is_empty() && referenced.iter().all(|name| name == variable)
}

fn expr_references_variable(expr: &Expr, variable: &str) -> bool {
    expr_referenced_variables(expr)
        .iter()
        .any(|name| name == variable)
}

fn expr_referenced_variables(expr: &Expr) -> Vec<String> {
    let mut vars = Vec::new();
    collect_expr_variables(expr, &mut vars);
    vars.sort();
    vars.dedup();
    vars
}

fn collect_expr_variables(expr: &Expr, vars: &mut Vec<String>) {
    match expr {
        Expr::Variable(var) => vars.push(var.clone()),
        Expr::Property(prop) => vars.push(prop.variable.clone()),
        Expr::Parameter(_) | Expr::Literal(_) | Expr::CountStar => {}
        Expr::List(items) => {
            for item in items {
                collect_expr_variables(item, vars);
            }
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => {
            collect_expr_variables(list, vars);
            let mut local = Vec::new();
            if let Some(predicate) = predicate {
                collect_expr_variables(predicate, &mut local);
            }
            if let Some(projection) = projection {
                collect_expr_variables(projection, &mut local);
            }
            vars.extend(local.into_iter().filter(|name| name != variable));
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                collect_expr_variables(value, vars);
            }
        }
        Expr::In { expr, list } => {
            collect_expr_variables(expr, vars);
            collect_expr_variables(list, vars);
        }
        Expr::Index { target, index } => {
            collect_expr_variables(target, vars);
            collect_expr_variables(index, vars);
        }
        Expr::Slice { target, start, end } => {
            collect_expr_variables(target, vars);
            if let Some(start) = start {
                collect_expr_variables(start, vars);
            }
            if let Some(end) = end {
                collect_expr_variables(end, vars);
            }
        }
        Expr::PatternPredicate(pattern) => {
            for element in &pattern.elements {
                match element {
                    PatternElement::Node(node) => {
                        if let Some(var) = &node.variable {
                            vars.push(var.clone());
                        }
                    }
                    PatternElement::Relationship(rel) => {
                        if let Some(var) = &rel.variable {
                            vars.push(var.clone());
                        }
                    }
                }
            }
        }
        Expr::PatternComprehension {
            pattern,
            projection,
            ..
        } => {
            for element in &pattern.elements {
                match element {
                    PatternElement::Node(node) => {
                        if let Some(var) = &node.variable {
                            vars.push(var.clone());
                        }
                    }
                    PatternElement::Relationship(rel) => {
                        if let Some(var) = &rel.variable {
                            vars.push(var.clone());
                        }
                    }
                }
            }
            collect_expr_variables(projection, vars);
        }
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            if let Some(scrutinee) = scrutinee {
                collect_expr_variables(scrutinee, vars);
            }
            for (when_expr, then_expr) in arms {
                collect_expr_variables(when_expr, vars);
                collect_expr_variables(then_expr, vars);
            }
            if let Some(default) = default {
                collect_expr_variables(default, vars);
            }
        }
        Expr::UnaryOp { expr, .. } => collect_expr_variables(expr, vars),
        Expr::BinaryOp { left, right, .. } => {
            collect_expr_variables(left, vars);
            collect_expr_variables(right, vars);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                collect_expr_variables(arg, vars);
            }
        }
        Expr::Exists(inner) => collect_expr_variables(inner, vars),
        Expr::ExistsSubquery(_) => {}
        Expr::ListPredicate {
            variable,
            list,
            predicate,
            ..
        } => {
            collect_expr_variables(list, vars);
            let mut local = Vec::new();
            if let Some(predicate) = predicate {
                collect_expr_variables(predicate, &mut local);
            }
            vars.extend(local.into_iter().filter(|name| name != variable));
        }
    }
}

fn plan_return_clause(
    input: LogicalPlan,
    return_clause: &ReturnClause,
) -> CypherResult<LogicalPlan> {
    let mut has_aggregation = false;
    let mut aggregations = Vec::new();
    let mut group_by = Vec::new();
    let mut columns = Vec::new();
    let mut aggregate_projection = Vec::new();
    let mut next_agg_alias = 0usize;

    for item in &return_clause.items {
        let alias = return_item_alias(item);

        if is_aggregate_expr(&item.expr) {
            aggregations.push(translate_aggregate_op(&item.expr, alias.clone())?);
            has_aggregation = true;
            aggregate_projection.push(ProjectColumn {
                expr: ProjectExpr::Variable(alias.clone()),
                alias,
            });
        } else if expr_contains_aggregate(&item.expr) {
            let rewritten =
                extract_nested_aggregates(&item.expr, &mut aggregations, &mut next_agg_alias)?;
            has_aggregation = true;
            aggregate_projection.push(ProjectColumn {
                expr: translate_project_expr(&rewritten),
                alias,
            });
        } else {
            let project_expr = translate_project_expr(&item.expr);
            let column = ProjectColumn {
                expr: project_expr,
                alias: alias.clone(),
            };
            group_by.push(column.clone());
            columns.push(column);
            aggregate_projection.push(ProjectColumn {
                expr: ProjectExpr::Variable(alias.clone()),
                alias,
            });
        }
    }

    let plan = if has_aggregation {
        let aggregate = LogicalPlan::Aggregate {
            input: Box::new(input),
            group_by,
            aggregations,
        };
        let project = LogicalPlan::Project {
            input: Box::new(aggregate),
            columns: aggregate_projection,
        };
        if return_clause.distinct {
            LogicalPlan::Distinct {
                input: Box::new(project),
            }
        } else {
            project
        }
    } else {
        let plan = LogicalPlan::Project {
            input: Box::new(input),
            columns,
        };

        if return_clause.distinct {
            LogicalPlan::Distinct {
                input: Box::new(plan),
            }
        } else {
            plan
        }
    };

    Ok(plan)
}

fn return_item_alias(item: &ReturnItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    if matches!(item.expr, Expr::CountStar) && item.raw.as_deref() == Some("*") {
        return expr_alias(&item.expr);
    }
    item.raw.clone().unwrap_or_else(|| expr_alias(&item.expr))
}

fn translate_aggregate_op(expr: &Expr, alias: String) -> CypherResult<AggregateOp> {
    match expr {
        Expr::CountStar => Ok(AggregateOp {
            function: "count".into(),
            inputs: Vec::new(),
            distinct: false,
            alias,
        }),
        Expr::FunctionCall { name, args } if is_aggregate_name(name) => {
            let mut distinct = false;
            let inputs = args
                .iter()
                .cloned()
                .map(|arg| {
                    if let Expr::FunctionCall {
                        name: marker,
                        args: marker_args,
                    } = &arg
                    {
                        if marker == "__distinct" {
                            distinct = true;
                            return marker_args
                                .first()
                                .cloned()
                                .unwrap_or(Expr::Literal(Literal::Null));
                        }
                    }
                    arg
                })
                .map(|expr| translate_project_expr(&expr))
                .collect();
            Ok(AggregateOp {
                function: name.to_lowercase(),
                inputs,
                distinct,
                alias,
            })
        }
        _ => Err(CypherError::Plan(format!(
            "expected aggregate expression, got {expr:?}"
        ))),
    }
}

fn translate_project_expr(expr: &Expr) -> ProjectExpr {
    match expr {
        Expr::Variable(v) if v == "*" => ProjectExpr::Wildcard,
        Expr::Variable(v) => ProjectExpr::Variable(v.clone()),
        Expr::Property(pa) => ProjectExpr::Property(PropertyRef {
            variable: pa.variable.clone(),
            property: pa.property.clone(),
        }),
        Expr::FunctionCall { name, args } if name == "__distinct" => args
            .first()
            .map(translate_project_expr)
            .unwrap_or(ProjectExpr::Literal(Value::Null)),
        Expr::FunctionCall { name, args } => ProjectExpr::Function {
            name: name.clone(),
            args: args.iter().map(translate_project_expr).collect(),
        },
        Expr::Literal(lit) => ProjectExpr::Literal(literal_to_value(lit)),
        _ => ProjectExpr::Expression(expr.clone()),
    }
}

fn is_aggregate_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::CountStar)
        || matches!(expr, Expr::FunctionCall { name, .. } if is_aggregate_name(name))
}

fn expr_contains_aggregate(expr: &Expr) -> bool {
    if is_aggregate_expr(expr) {
        return true;
    }
    match expr {
        Expr::Literal(_) | Expr::Property(_) | Expr::Variable(_) | Expr::Parameter(_) => false,
        Expr::List(items) => items.iter().any(expr_contains_aggregate),
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            expr_contains_aggregate(list)
                || predicate.as_deref().is_some_and(expr_contains_aggregate)
                || projection.as_deref().is_some_and(expr_contains_aggregate)
        }
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| expr_contains_aggregate(value)),
        Expr::In { expr, list } => expr_contains_aggregate(expr) || expr_contains_aggregate(list),
        Expr::Index { target, index } => {
            expr_contains_aggregate(target) || expr_contains_aggregate(index)
        }
        Expr::Slice { target, start, end } => {
            expr_contains_aggregate(target)
                || start.as_deref().is_some_and(expr_contains_aggregate)
                || end.as_deref().is_some_and(expr_contains_aggregate)
        }
        Expr::PatternPredicate(_) => false,
        Expr::PatternComprehension { projection, .. } => expr_contains_aggregate(projection),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            scrutinee.as_deref().is_some_and(expr_contains_aggregate)
                || arms.iter().any(|(when_expr, then_expr)| {
                    expr_contains_aggregate(when_expr) || expr_contains_aggregate(then_expr)
                })
                || default.as_deref().is_some_and(expr_contains_aggregate)
        }
        Expr::UnaryOp { expr, .. } => expr_contains_aggregate(expr),
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_aggregate),
        Expr::CountStar => true,
        Expr::Exists(inner) => expr_contains_aggregate(inner),
        Expr::ExistsSubquery(_) => false,
        Expr::ListPredicate {
            list, predicate, ..
        } => {
            expr_contains_aggregate(list)
                || predicate.as_deref().is_some_and(expr_contains_aggregate)
        }
    }
}

fn extract_nested_aggregates(
    expr: &Expr,
    aggregations: &mut Vec<AggregateOp>,
    next_alias: &mut usize,
) -> CypherResult<Expr> {
    if is_aggregate_expr(expr) {
        let alias = format!("__agg_{next_alias}");
        *next_alias += 1;
        aggregations.push(translate_aggregate_op(expr, alias.clone())?);
        return Ok(Expr::Variable(alias));
    }

    match expr {
        Expr::Literal(_)
        | Expr::Property(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::CountStar => Ok(expr.clone()),
        Expr::List(items) => Ok(Expr::List(
            items
                .iter()
                .map(|item| extract_nested_aggregates(item, aggregations, next_alias))
                .collect::<CypherResult<Vec<_>>>()?,
        )),
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => Ok(Expr::ListComprehension {
            variable: variable.clone(),
            list: Box::new(extract_nested_aggregates(list, aggregations, next_alias)?),
            predicate: predicate
                .as_deref()
                .map(|expr| extract_nested_aggregates(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
            projection: projection
                .as_deref()
                .map(|expr| extract_nested_aggregates(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
        Expr::Map(entries) => Ok(Expr::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    Ok((
                        key.clone(),
                        extract_nested_aggregates(value, aggregations, next_alias)?,
                    ))
                })
                .collect::<CypherResult<Vec<_>>>()?,
        )),
        Expr::In { expr, list } => Ok(Expr::In {
            expr: Box::new(extract_nested_aggregates(expr, aggregations, next_alias)?),
            list: Box::new(extract_nested_aggregates(list, aggregations, next_alias)?),
        }),
        Expr::Index { target, index } => Ok(Expr::Index {
            target: Box::new(extract_nested_aggregates(target, aggregations, next_alias)?),
            index: Box::new(extract_nested_aggregates(index, aggregations, next_alias)?),
        }),
        Expr::Slice { target, start, end } => Ok(Expr::Slice {
            target: Box::new(extract_nested_aggregates(target, aggregations, next_alias)?),
            start: start
                .as_deref()
                .map(|expr| extract_nested_aggregates(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
            end: end
                .as_deref()
                .map(|expr| extract_nested_aggregates(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
        Expr::PatternPredicate(_) => Ok(expr.clone()),
        Expr::PatternComprehension {
            variable,
            pattern,
            projection,
        } => Ok(Expr::PatternComprehension {
            variable: variable.clone(),
            pattern: pattern.clone(),
            projection: Box::new(extract_nested_aggregates(
                projection,
                aggregations,
                next_alias,
            )?),
        }),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => Ok(Expr::Case {
            scrutinee: scrutinee
                .as_deref()
                .map(|expr| extract_nested_aggregates(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
            arms: arms
                .iter()
                .map(|(when_expr, then_expr)| {
                    Ok((
                        extract_nested_aggregates(when_expr, aggregations, next_alias)?,
                        extract_nested_aggregates(then_expr, aggregations, next_alias)?,
                    ))
                })
                .collect::<CypherResult<Vec<_>>>()?,
            default: default
                .as_deref()
                .map(|expr| extract_nested_aggregates(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
        Expr::UnaryOp { op, expr } => Ok(Expr::UnaryOp {
            op: *op,
            expr: Box::new(extract_nested_aggregates(expr, aggregations, next_alias)?),
        }),
        Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
            left: Box::new(extract_nested_aggregates(left, aggregations, next_alias)?),
            op: *op,
            right: Box::new(extract_nested_aggregates(right, aggregations, next_alias)?),
        }),
        Expr::FunctionCall { name, args } => Ok(Expr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| extract_nested_aggregates(arg, aggregations, next_alias))
                .collect::<CypherResult<Vec<_>>>()?,
        }),
        Expr::Exists(inner) => Ok(Expr::Exists(Box::new(extract_nested_aggregates(
            inner,
            aggregations,
            next_alias,
        )?))),
        Expr::ExistsSubquery(_) => Ok(expr.clone()),
        Expr::ListPredicate {
            kind,
            variable,
            list,
            predicate,
        } => Ok(Expr::ListPredicate {
            kind: *kind,
            variable: variable.clone(),
            list: Box::new(extract_nested_aggregates(list, aggregations, next_alias)?),
            predicate: predicate
                .as_deref()
                .map(|expr| extract_nested_aggregates(expr, aggregations, next_alias))
                .transpose()?
                .map(Box::new),
        }),
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "count" | "sum" | "avg" | "min" | "max" | "collect" | "percentiledisc" | "percentilecont"
    )
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
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => {
            let mut out = format!("[{variable} IN {}", expr_alias(list));
            if let Some(predicate) = predicate {
                out.push_str(&format!(" WHERE {}", expr_alias(predicate)));
            }
            if let Some(projection) = projection {
                out.push_str(&format!(" | {}", expr_alias(projection)));
            }
            out.push(']');
            out
        }
        Expr::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(key, value)| format!("{key}: {}", expr_alias(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::In { expr, list } => format!("{} IN {}", expr_alias(expr), expr_alias(list)),
        Expr::Index { target, index } => {
            if let Expr::Literal(Literal::String(property)) = index.as_ref() {
                if matches!(target.as_ref(), Expr::Index { .. }) {
                    return format!("({}).{property}", expr_alias(target));
                }
            }
            format!("{}[{}]", expr_alias(target), expr_alias(index))
        }
        Expr::Slice { target, start, end } => format!(
            "{}[{}..{}]",
            expr_alias(target),
            start.as_deref().map(expr_alias).unwrap_or_default(),
            end.as_deref().map(expr_alias).unwrap_or_default()
        ),
        Expr::PatternPredicate(_) => "expr".into(),
        Expr::PatternComprehension { .. } => "expr".into(),
        Expr::Case {
            scrutinee,
            arms,
            default,
        } => {
            let mut out = String::from("CASE");
            if let Some(scrutinee) = scrutinee {
                out.push(' ');
                out.push_str(&expr_alias(scrutinee));
            }
            for (when_expr, then_expr) in arms {
                out.push_str(&format!(
                    " WHEN {} THEN {}",
                    expr_alias(when_expr),
                    expr_alias(then_expr)
                ));
            }
            if let Some(default) = default {
                out.push_str(&format!(" ELSE {}", expr_alias(default)));
            }
            out.push_str(" END");
            out
        }
        Expr::UnaryOp { op, expr } => match op {
            UnaryOp::Not => format!("NOT {}", expr_alias(expr)),
            UnaryOp::IsNull => format!("{} IS NULL", expr_alias(expr)),
            UnaryOp::IsNotNull => format!("{} IS NOT NULL", expr_alias(expr)),
        },
        Expr::BinaryOp { left, op, right } => {
            let prec = binary_precedence(*op);
            format!(
                "{} {} {}",
                expr_alias_binary_child(left, prec, false),
                binary_op_str(*op),
                expr_alias_binary_child(right, prec, true)
            )
        }
        Expr::CountStar => "count(*)".into(),
        Expr::FunctionCall { name, args } => {
            if name == "__label_test" {
                if let Some(Expr::Variable(variable)) = args.first() {
                    let labels = args
                        .iter()
                        .skip(1)
                        .filter_map(|arg| match arg {
                            Expr::Literal(Literal::String(label)) => Some(format!(":{label}")),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    return format!("({variable}{labels})");
                }
            }
            if let Some(Expr::FunctionCall {
                name: marker,
                args: marker_args,
            }) = args.first()
            {
                if marker == "__distinct" {
                    let inner = marker_args
                        .first()
                        .map(expr_alias)
                        .unwrap_or_else(|| "expr".into());
                    return format!("{name}(DISTINCT {inner})");
                }
            }
            let arg_str: Vec<String> = args.iter().map(expr_alias).collect();
            format!("{}({})", name, arg_str.join(", "))
        }
        Expr::Parameter(name) => format!("${name}"),
        Expr::Exists(inner) => format!("EXISTS({})", expr_alias(inner)),
        Expr::ExistsSubquery(_) => "EXISTS { ... }".into(),
        Expr::ListPredicate {
            kind,
            variable,
            list,
            predicate,
        } => {
            let name = match kind {
                ListPredicateKind::Any => "any",
                ListPredicateKind::All => "ALL",
                ListPredicateKind::None => "none",
                ListPredicateKind::Single => "single",
            };
            let mut out = format!("{name}({variable} IN {}", expr_alias(list));
            if let Some(predicate) = predicate {
                out.push_str(&format!(" WHERE {}", expr_alias(predicate)));
            }
            out.push(')');
            out
        }
    }
}

fn expr_alias_binary_child(expr: &Expr, parent_precedence: u8, right_child: bool) -> String {
    let child = expr_alias(expr);
    let Expr::BinaryOp { op, .. } = expr else {
        return child;
    };
    let child_precedence = binary_precedence(*op);
    if child_precedence < parent_precedence
        || (right_child && child_precedence == parent_precedence)
    {
        format!("({child})")
    } else {
        child
    }
}

fn binary_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 1,
        BinaryOp::Xor => 2,
        BinaryOp::And => 3,
        BinaryOp::Eq
        | BinaryOp::Neq
        | BinaryOp::Lt
        | BinaryOp::Lte
        | BinaryOp::Gt
        | BinaryOp::Gte
        | BinaryOp::Contains
        | BinaryOp::StartsWith
        | BinaryOp::EndsWith => 4,
        BinaryOp::Add | BinaryOp::Sub => 5,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 6,
        BinaryOp::Pow => 7,
    }
}

fn binary_op_str(op: BinaryOp) -> &'static str {
    match op {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    #[test]
    fn plan_simple_scan() {
        let q = Parser::parse_read("MATCH (n:Entity) RETURN n").unwrap();
        let plan = plan_query(&q).unwrap();
        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::ScanVertices {
                variable, label, ..
            } = *input
            {
                assert_eq!(variable, "n");
                assert_eq!(label, Some("Entity".into()));
                return;
            }
        }
        panic!("expected Project -> ScanVertices");
    }

    #[test]
    fn plan_expand() {
        let q =
            Parser::parse_read("MATCH (a:Entity)-[:DISCLOSES]->(b:Metric) RETURN a, b").unwrap();
        let plan = plan_query(&q).unwrap();
        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::Expand {
                edge_types,
                direction,
                ..
            } = *input
            {
                assert_eq!(edge_types, vec!["DISCLOSES"]);
                assert_eq!(direction, ExpandDirection::Outgoing);
                return;
            }
        }
        panic!("expected Project -> Expand");
    }

    #[test]
    fn plan_with_where_predicate_pushdown() {
        let q =
            Parser::parse_read("MATCH (n:Entity) WHERE n.external_id = 'AAPL:Apple:ORG' RETURN n")
                .unwrap();
        let plan = plan_query(&q).unwrap();
        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::ScanVertices {
                index_lookup: Some(lookup),
                ..
            } = *input
            {
                assert_eq!(lookup.property, "external_id");
                return;
            }
        }
        panic!("expected predicate pushdown to ScanVertices index_lookup");
    }

    #[test]
    fn plan_pushes_source_predicate_below_expand() {
        let q = Parser::parse_read(
            "MATCH (s:Entity)-[]-(n) WHERE s.external_id = 'AAPL:Apple:ORG' RETURN n",
        )
        .unwrap();
        let plan = plan_query(&q).unwrap();

        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::Expand { input, .. } = *input {
                if let LogicalPlan::ScanVertices {
                    index_lookup: Some(lookup),
                    ..
                } = *input
                {
                    assert_eq!(lookup.property, "external_id");
                    return;
                }
            }
        }
        panic!("expected Project -> Expand -> indexed ScanVertices");
    }

    #[test]
    fn plan_pushes_and_source_predicates_below_expand() {
        let q = Parser::parse_read(
            "MATCH (s:Entity)-[]-(n) WHERE s.tenant_id = 'AAPL' AND s.external_id = 'AAPL:Apple:ORG' RETURN n",
        )
        .unwrap();
        let plan = plan_query(&q).unwrap();

        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::Expand { input, .. } = *input {
                if let LogicalPlan::Filter {
                    input: scan,
                    predicate,
                } = *input
                {
                    if let Predicate::Comparison { left, .. } = predicate {
                        assert_eq!(left.property, "external_id");
                    } else {
                        panic!("expected residual external_id filter");
                    }
                    if let LogicalPlan::ScanVertices {
                        index_lookup: Some(lookup),
                        ..
                    } = *scan
                    {
                        assert_eq!(lookup.property, "tenant_id");
                        return;
                    }
                }
            }
        }
        panic!("expected AND predicate pushdown through Expand to Filter -> indexed ScanVertices");
    }

    #[test]
    fn plan_pushes_root_predicate_through_two_hop_expand() {
        let q = Parser::parse_read(
            "MATCH (s:Entity)-[]->()-[]->(n) WHERE s.external_id = 'AAPL:Apple:ORG' RETURN n",
        )
        .unwrap();
        let plan = plan_query(&q).unwrap();

        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::ApplyMatch { input, .. } = *input {
                if let LogicalPlan::ScanVertices {
                    index_lookup: Some(lookup),
                    ..
                } = *input
                {
                    assert_eq!(lookup.property, "external_id");
                    return;
                }
            }
        }
        panic!("expected two-hop ApplyMatch to be seeded by indexed root ScanVertices");
    }

    #[test]
    fn plan_keeps_relationship_function_predicate_above_expand() {
        let q = Parser::parse_read("MATCH (n)-[r]->(x) WHERE type(r) = 'KNOWS' RETURN x").unwrap();
        let plan = plan_query(&q).unwrap();

        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::Filter { input, .. } = *input {
                if matches!(*input, LogicalPlan::Expand { .. }) {
                    return;
                }
            }
        }
        panic!("expected relationship function predicate to remain above Expand");
    }

    #[test]
    fn plan_fuses_optional_match_where_into_apply_match() {
        let q = Parser::parse_read(
            "MATCH (a:Entity) OPTIONAL MATCH (a)-[:DISCLOSES]->(b) WHERE b.name = 'Revenue' RETURN a, b",
        )
        .unwrap();
        let plan = plan_query(&q).unwrap();

        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::ApplyMatch {
                optional,
                where_predicate,
                ..
            } = *input
            {
                assert!(optional);
                assert!(where_predicate.is_some());
                return;
            }
        }
        panic!("expected OPTIONAL MATCH WHERE to be fused into ApplyMatch");
    }

    #[test]
    fn plan_with_limit() {
        let q = Parser::parse_read("MATCH (n) RETURN n LIMIT 10").unwrap();
        let plan = plan_query(&q).unwrap();
        if let LogicalPlan::Limit { count, .. } = plan {
            assert_eq!(count, RowCount::Literal(10));
        } else {
            panic!("expected Limit");
        }
    }

    #[test]
    fn plan_aggregate() {
        let q = Parser::parse_read("MATCH (n:Entity) RETURN count(n)").unwrap();
        let plan = plan_query(&q).unwrap();
        if let LogicalPlan::Project { input, .. } = plan {
            if let LogicalPlan::Aggregate { aggregations, .. } = *input {
                assert_eq!(aggregations.len(), 1);
                assert_eq!(aggregations[0].function, "count");
                return;
            }
        }
        panic!("expected Project -> Aggregate");
    }

    fn into_write(input: &str) -> WriteQuery {
        let stmt = Parser::parse(input).unwrap();
        match stmt {
            crate::ast::Statement::Write(wq) => wq,
            _ => panic!("expected write statement"),
        }
    }

    #[test]
    fn plan_create_single_node() {
        let wq = into_write("CREATE (n:Entity {name: 'Apple'})");
        let wp = plan_write(&wq).unwrap();
        assert!(wp.source.is_none());
        assert_eq!(wp.mutations.len(), 1);
        let MutationOp::CreateNode {
            variable,
            labels,
            properties,
        } = &wp.mutations[0]
        else {
            panic!("expected CreateNode");
        };
        assert_eq!(variable, "n");
        assert_eq!(labels, &vec!["Entity".to_string()]);
        assert_eq!(properties.len(), 1);
    }

    #[test]
    fn plan_create_edge_between_two_nodes() {
        let wq = into_write("CREATE (a:X {n: 1})-[:REL]->(b:Y {n: 2})");
        let wp = plan_write(&wq).unwrap();
        assert_eq!(wp.mutations.len(), 3); // node, node, edge
        matches!(wp.mutations[0], MutationOp::CreateNode { .. });
        matches!(wp.mutations[1], MutationOp::CreateNode { .. });
        let MutationOp::CreateEdge {
            src_var,
            dst_var,
            rel_type,
            ..
        } = &wp.mutations[2]
        else {
            panic!("expected CreateEdge");
        };
        assert_eq!(src_var, "a");
        assert_eq!(dst_var, "b");
        assert_eq!(rel_type, "REL");
    }

    #[test]
    fn plan_match_delete() {
        let wq = into_write("MATCH (n:Entity) WHERE n.name = 'Apple' DELETE n");
        let wp = plan_write(&wq).unwrap();
        assert!(wp.source.is_some());
        assert_eq!(wp.mutations.len(), 1);
        let MutationOp::Delete {
            variable, detach, ..
        } = &wp.mutations[0]
        else {
            panic!("expected Delete");
        };
        assert_eq!(variable.as_deref(), Some("n"));
        assert!(!detach);
    }

    #[test]
    fn plan_detach_delete_sets_flag() {
        let wq = into_write("MATCH (n:Entity) DETACH DELETE n");
        let wp = plan_write(&wq).unwrap();
        let MutationOp::Delete { detach, .. } = &wp.mutations[0] else {
            panic!("expected Delete");
        };
        assert!(detach);
    }

    #[test]
    fn plan_match_set_property() {
        let wq = into_write("MATCH (n:Entity) SET n.name = 'Bob'");
        let wp = plan_write(&wq).unwrap();
        let MutationOp::SetProperty {
            variable,
            key,
            value,
        } = &wp.mutations[0]
        else {
            panic!("expected SetProperty");
        };
        assert_eq!(variable, "n");
        assert_eq!(key, "name");
        assert!(matches!(value, PropertyValue::Literal(Value::String(s)) if s == "Bob"));
    }

    #[test]
    fn plan_match_remove_property() {
        let wq = into_write("MATCH (n:Entity) REMOVE n.name");
        let wp = plan_write(&wq).unwrap();
        let MutationOp::RemoveProperty { variable, key } = &wp.mutations[0] else {
            panic!("expected RemoveProperty");
        };
        assert_eq!(variable, "n");
        assert_eq!(key, "name");
    }

    #[test]
    fn plan_merge_node() {
        let wq = into_write("MERGE (n:Entity {name: 'Alice'})");
        let wp = plan_write(&wq).unwrap();
        assert!(matches!(wp.mutations[0], MutationOp::BeginMerge));
        let MutationOp::MergeNode {
            variable,
            labels,
            properties,
        } = &wp.mutations[1]
        else {
            panic!("expected MergeNode");
        };
        assert_eq!(variable, "n");
        assert_eq!(labels, &vec!["Entity".to_string()]);
        assert_eq!(properties.len(), 1);
        assert!(matches!(
            wp.mutations[2],
            MutationOp::ApplyMergeActions { .. }
        ));
    }

    #[test]
    fn plan_relationship_delete_target() {
        let wq = into_write("MATCH (a)-[r:REL]->(b) DELETE r");
        let wp = plan_write(&wq).unwrap();
        let Some(LogicalPlan::Expand { rel_var, .. }) = &wp.source else {
            panic!("expected expand source");
        };
        assert_eq!(rel_var.as_deref(), Some("r"));
        assert!(matches!(wp.mutations[0], MutationOp::Delete { .. }));
    }
}
