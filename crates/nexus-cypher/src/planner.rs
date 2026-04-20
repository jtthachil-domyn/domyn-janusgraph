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
        count: u64,
    },
    Skip {
        input: Box<LogicalPlan>,
        count: u64,
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
}

#[derive(Debug, Clone)]
pub enum MutationOp {
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
        properties: Vec<(String, PropertyValue)>,
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
    Delete {
        variable: String,
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
    // Source: MATCH (+ WHERE), if present. No RETURN projection — the
    // write executor consumes rows as binding sources for mutations.
    let source = if wq.match_clause.is_some() {
        let mut plan = plan_match_clause_opt(wq.match_clause.as_ref())?;
        if let Some(ref where_clause) = wq.where_clause {
            let pred = translate_predicate(&where_clause.expr)?;
            plan = push_down_predicate(plan, pred);
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
            MutationClause::Create { patterns } => {
                translate_create_patterns(patterns, &mut mutations)?;
            }
            MutationClause::Merge { patterns } => {
                translate_merge_patterns(patterns, &mut mutations)?;
            }
            MutationClause::Set { items } => {
                for item in items {
                    mutations.push(MutationOp::SetProperty {
                        variable: item.target.variable.clone(),
                        key: item.target.property.clone(),
                        value: translate_mutation_value(&item.value)?,
                    });
                }
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
                    }
                }
            }
            MutationClause::Delete { variables, detach } => {
                for var in variables {
                    mutations.push(MutationOp::Delete {
                        variable: var.clone(),
                        detach: *detach,
                    });
                }
            }
        }
    }

    Ok(WritePlan { source, mutations })
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
                        let rel_type = rp.rel_types.first().cloned().ok_or_else(|| {
                            CypherError::Plan("CREATE relationship must have a type".into())
                        })?;
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

        for element in &pattern.elements {
            match element {
                PatternElement::Node(np) => {
                    let var = np.variable.clone().ok_or_else(|| {
                        CypherError::Plan("MERGE nodes must have a variable".into())
                    })?;
                    let labels = np.labels.clone();
                    if labels.is_empty() && pending_rel.is_none() {
                        return Err(CypherError::Plan(
                            "MERGE of a bare node requires a label".into(),
                        ));
                    }

                    if !labels.is_empty() {
                        mutations.push(MutationOp::MergeNode {
                            variable: var.clone(),
                            labels,
                            properties: translate_create_props(&np.properties)?,
                        });
                    }

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
                                    "MERGE requires a directed relationship".into(),
                                ));
                            }
                        };
                        let rel_type = rp.rel_types.first().cloned().ok_or_else(|| {
                            CypherError::Plan("MERGE relationship must have a type".into())
                        })?;
                        mutations.push(MutationOp::MergeEdge {
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
                    let label = np.labels.first().cloned();

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

                plan = Some(expand);
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

    for clause in &query.tail {
        plan = plan_read_tail_clause(plan, clause)?;
    }

    plan = plan_return_clause(plan, &query.return_clause)?;

    if let Some(ref order_by) = query.order_by {
        let keys: Vec<SortKey> = order_by
            .items
            .iter()
            .map(|item| SortKey {
                expr: translate_project_expr(&item.expr),
                descending: item.descending,
            })
            .collect();
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            keys,
        };
    }

    if let Some(skip) = query.skip {
        plan = LogicalPlan::Skip {
            input: Box::new(plan),
            count: skip,
        };
    }

    if let Some(limit) = query.limit {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            count: limit,
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
            };
        }
    }
    Ok(plan)
}

fn pattern_needs_match_engine(pattern: &Pattern) -> bool {
    let mut relationships = 0usize;
    let mut variable_length_relationship_binding = false;
    for element in &pattern.elements {
        if let PatternElement::Relationship(rel) = element {
            relationships += 1;
            if rel.variable.is_some() && (rel.min_hops.is_some() || rel.max_hops.is_some()) {
                variable_length_relationship_binding = true;
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
    let mut plan = plan_return_clause(
        input,
        &ReturnClause {
            items: with_clause.items.clone(),
            distinct: with_clause.distinct,
        },
    )?;

    if let Some(where_clause) = &with_clause.where_clause {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate: translate_predicate(&where_clause.expr)?,
        };
    }

    if let Some(order_by) = &with_clause.order_by {
        let keys: Vec<SortKey> = order_by
            .items
            .iter()
            .map(|item| SortKey {
                expr: translate_project_expr(&item.expr),
                descending: item.descending,
            })
            .collect();
        plan = LogicalPlan::Sort {
            input: Box::new(plan),
            keys,
        };
    }

    if let Some(skip) = with_clause.skip {
        plan = LogicalPlan::Skip {
            input: Box::new(plan),
            count: skip,
        };
    }

    if let Some(limit) = with_clause.limit {
        plan = LogicalPlan::Limit {
            input: Box::new(plan),
            count: limit,
        };
    }

    Ok(plan)
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
                let prop = expr_to_property_ref(left)?;
                let pattern = expr_to_string_value(right)?;
                Ok(Predicate::StringOp {
                    property: prop,
                    op: StringPredOp::Contains,
                    pattern,
                })
            }
            BinaryOp::StartsWith => {
                let prop = expr_to_property_ref(left)?;
                let pattern = expr_to_string_value(right)?;
                Ok(Predicate::StringOp {
                    property: prop,
                    op: StringPredOp::StartsWith,
                    pattern,
                })
            }
            BinaryOp::EndsWith => {
                let prop = expr_to_property_ref(left)?;
                let pattern = expr_to_string_value(right)?;
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
                        let right_val = translate_predicate_value(right)?;
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
            UnaryOp::Not => Ok(Predicate::Not(Box::new(translate_predicate(expr)?))),
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
                        };
                    }
                }
            }

            LogicalPlan::Filter {
                input: Box::new(LogicalPlan::ApplyMatch {
                    input,
                    clause,
                    optional,
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
    Some((node.variable.clone()?, node.labels.first().cloned()))
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
        Predicate::Expr(_) => false,
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
        Predicate::Expr(_) => false,
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

    for item in &return_clause.items {
        let alias = item.alias.clone().unwrap_or_else(|| expr_alias(&item.expr));
        let project_expr = translate_project_expr(&item.expr);

        if is_aggregate_expr(&item.expr) {
            aggregations.push(translate_aggregate_op(&item.expr, alias.clone())?);
            has_aggregation = true;
        } else {
            let column = ProjectColumn {
                expr: project_expr,
                alias,
            };
            group_by.push(column.clone());
            columns.push(column);
        }
    }

    let plan = if has_aggregation {
        LogicalPlan::Aggregate {
            input: Box::new(input),
            group_by,
            aggregations,
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
        Expr::CountStar => "count(*)".into(),
        Expr::FunctionCall { name, args } => {
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
        _ => "expr".into(),
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
    fn plan_with_limit() {
        let q = Parser::parse_read("MATCH (n) RETURN n LIMIT 10").unwrap();
        let plan = plan_query(&q).unwrap();
        if let LogicalPlan::Limit { count, .. } = plan {
            assert_eq!(count, 10);
        } else {
            panic!("expected Limit");
        }
    }

    #[test]
    fn plan_aggregate() {
        let q = Parser::parse_read("MATCH (n:Entity) RETURN count(n)").unwrap();
        let plan = plan_query(&q).unwrap();
        if let LogicalPlan::Aggregate { aggregations, .. } = plan {
            assert_eq!(aggregations.len(), 1);
            assert_eq!(aggregations[0].function, "count");
        } else {
            panic!("expected Aggregate");
        }
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
        let MutationOp::Delete { variable, detach } = &wp.mutations[0] else {
            panic!("expected Delete");
        };
        assert_eq!(variable, "n");
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
        let MutationOp::MergeNode {
            variable,
            labels,
            properties,
        } = &wp.mutations[0]
        else {
            panic!("expected MergeNode");
        };
        assert_eq!(variable, "n");
        assert_eq!(labels, &vec!["Entity".to_string()]);
        assert_eq!(properties.len(), 1);
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
