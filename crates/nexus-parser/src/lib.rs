//! Parser adapter experiments for Domyn Nexus.
//!
//! This crate exists so external openCypher frontends can be evaluated without
//! destabilizing the production `nexus-cypher` parser. The first spike wires
//! `kyu-parser` in as a syntax frontend and maps currently supported Domyn
//! semantics into the existing logical planner.

use kyu_parser::ast as kyu;
use nexus_cypher::ast as domyn;
use nexus_cypher::ast::Query;
use nexus_cypher::error::CypherError;
use nexus_cypher::parser::Parser as NativeParser;
use nexus_cypher::planner::{LogicalPlan, plan_query};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserBackend {
    Native,
    Kyu,
}

#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub backend: ParserBackend,
    pub ast: Query,
}

#[derive(Debug, Clone)]
pub enum ParsedStatement {
    Read(ParsedQuery),
    Write(ParsedWrite),
}

#[derive(Debug, Clone)]
pub struct ParsedWrite {
    pub backend: ParserBackend,
    pub statement: WriteStatement,
}

#[derive(Debug, Clone)]
pub struct WriteStatement {
    pub clauses: Vec<WriteClause>,
    pub return_clause: Option<domyn::ReturnClause>,
}

#[derive(Debug, Clone)]
pub enum WriteClause {
    Create(Vec<domyn::Pattern>),
}

#[derive(Debug, thiserror::Error)]
pub enum NexusParserError {
    #[error("native parser error: {0}")]
    Native(#[from] CypherError),
    #[error("kyu parser accepted the query but Domyn mapping failed: {0}")]
    Mapping(String),
    #[error("kyu parser rejected the query: {0}")]
    Kyu(String),
}

pub type NexusParserResult<T> = Result<T, NexusParserError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    pub query: String,
    pub native_parses: bool,
    pub kyu_parses: bool,
    pub kyu_maps: bool,
    pub kyu_plans: bool,
    pub detail: Option<String>,
}

pub struct NexusParser;

impl NexusParser {
    pub fn parse_native(query: &str) -> NexusParserResult<ParsedQuery> {
        Ok(ParsedQuery {
            backend: ParserBackend::Native,
            ast: NativeParser::parse_read(query)?,
        })
    }

    pub fn parse_kyu(query: &str) -> NexusParserResult<ParsedQuery> {
        let statement = parse_kyu_statement(query)?;

        Ok(ParsedQuery {
            backend: ParserBackend::Kyu,
            ast: map_statement(statement)?,
        })
    }

    pub fn plan_kyu(query: &str) -> NexusParserResult<LogicalPlan> {
        let parsed = Self::parse_kyu(query)?;
        nexus_cypher::binder::bind_query(&parsed.ast).map_err(NexusParserError::Native)?;
        plan_query(&parsed.ast).map_err(NexusParserError::Native)
    }

    pub fn parse_statement_kyu(query: &str) -> NexusParserResult<ParsedStatement> {
        let statement = parse_kyu_statement(query)?;
        map_statement_any(statement)
    }

    pub fn probe(query: &str) -> CoverageReport {
        let native_parses = Self::parse_native(query).is_ok();
        let kyu_parse_result = parse_kyu_statement(query);
        let kyu_parses = kyu_parse_result.is_ok();

        let mut kyu_maps = false;
        let mut kyu_plans = false;
        let mut detail = None;

        match kyu_parse_result.and_then(map_statement_any) {
            Ok(ParsedStatement::Read(parsed)) => {
                kyu_maps = true;
                match nexus_cypher::binder::bind_query(&parsed.ast)
                    .and_then(|_| plan_query(&parsed.ast))
                {
                    Ok(_) => kyu_plans = true,
                    Err(err) => detail = Some(format!("planning failed: {err}")),
                }
            }
            Ok(ParsedStatement::Write(_)) => {
                kyu_maps = true;
                detail =
                    Some("write statement mapped; execution is handled by nexus-server".into());
            }
            Err(err) => detail = Some(err.to_string()),
        }

        CoverageReport {
            query: query.to_string(),
            native_parses,
            kyu_parses,
            kyu_maps,
            kyu_plans,
            detail,
        }
    }

    pub fn compare_coverage<'a>(queries: impl IntoIterator<Item = &'a str>) -> Vec<CoverageReport> {
        queries.into_iter().map(Self::probe).collect()
    }
}

fn parse_kyu_statement(query: &str) -> NexusParserResult<kyu::Statement> {
    let result = kyu_parser::parse(query);
    if !result.errors.is_empty() {
        let rendered = result
            .errors
            .iter()
            .map(|err| err.render("query.cypher", query))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(NexusParserError::Kyu(rendered));
    }

    result
        .ast
        .ok_or_else(|| NexusParserError::Kyu("kyu parser returned no AST".into()))
}

fn map_statement(statement: kyu::Statement) -> NexusParserResult<Query> {
    match statement {
        kyu::Statement::Query(query) => map_query(query),
        kyu::Statement::Explain(inner) | kyu::Statement::Profile(inner) => map_statement(*inner),
        other => unsupported(format!("statement kind is parse-only for now: {other:?}")),
    }
}

fn map_statement_any(statement: kyu::Statement) -> NexusParserResult<ParsedStatement> {
    match statement {
        kyu::Statement::Query(query) => map_query_any(query),
        kyu::Statement::Explain(inner) | kyu::Statement::Profile(inner) => {
            map_statement_any(*inner)
        }
        other => unsupported(format!("statement kind is parse-only for now: {other:?}")),
    }
}

fn map_query_any(query: kyu::Query) -> NexusParserResult<ParsedStatement> {
    let has_updates = query
        .parts
        .iter()
        .any(|part| !part.updating_clauses.is_empty());

    if has_updates {
        Ok(ParsedStatement::Write(ParsedWrite {
            backend: ParserBackend::Kyu,
            statement: map_write_query(query)?,
        }))
    } else {
        Ok(ParsedStatement::Read(ParsedQuery {
            backend: ParserBackend::Kyu,
            ast: map_query(query)?,
        }))
    }
}

fn map_query(query: kyu::Query) -> NexusParserResult<Query> {
    if !query.union_all.is_empty() {
        return unsupported("UNION / UNION ALL needs logical-plan support");
    }
    if query.parts.len() != 1 {
        return unsupported("multi-part queries with WITH need pipeline planning");
    }

    let part = query
        .parts
        .into_iter()
        .next()
        .ok_or_else(|| NexusParserError::Mapping("query has no parts".into()))?;

    if !part.updating_clauses.is_empty() {
        return unsupported(
            "write clauses require parse_statement_kyu() and the write execution path",
        );
    }
    if !part.is_return {
        return unsupported("WITH projection needs pipeline planning");
    }

    let projection = part
        .projection
        .ok_or_else(|| NexusParserError::Mapping("RETURN clause is required".into()))?;

    let mut match_clause = None;
    let mut where_clause = None;

    for clause in part.reading_clauses {
        match clause {
            kyu::ReadingClause::Match(kyu_match) => {
                if match_clause.is_some() {
                    return unsupported("multiple MATCH clauses need pipeline planning");
                }
                let (mapped_match, mapped_where) = map_match_clause(kyu_match)?;
                match_clause = Some(mapped_match);
                where_clause = mapped_where;
            }
            kyu::ReadingClause::Unwind(_) => {
                return unsupported("UNWIND needs row-expansion planning");
            }
            kyu::ReadingClause::InQueryCall(_) => {
                return unsupported("CALL subqueries need procedure planning");
            }
            kyu::ReadingClause::LoadFrom(_) => {
                return unsupported("LOAD FROM needs source planning");
            }
        }
    }

    let (return_clause, order_by, skip, limit) = map_projection_body(projection)?;

    Ok(Query {
        match_clause,
        where_clause,
        tail: Vec::new(),
        return_clause,
        order_by,
        limit,
        skip,
        union: None,
    })
}

fn map_write_query(query: kyu::Query) -> NexusParserResult<WriteStatement> {
    if !query.union_all.is_empty() {
        return unsupported("UNION / UNION ALL needs write pipeline support");
    }
    if query.parts.len() != 1 {
        return unsupported("multi-part write queries with WITH need pipeline planning");
    }

    let part = query
        .parts
        .into_iter()
        .next()
        .ok_or_else(|| NexusParserError::Mapping("query has no parts".into()))?;

    if !part.reading_clauses.is_empty() {
        return unsupported("MATCH + write clauses need bound-row write planning");
    }

    let mut clauses = Vec::new();
    for clause in part.updating_clauses {
        match clause {
            kyu::UpdatingClause::Create(patterns) => {
                let patterns = patterns
                    .into_iter()
                    .map(map_pattern)
                    .collect::<NexusParserResult<Vec<_>>>()?;
                clauses.push(WriteClause::Create(patterns));
            }
            kyu::UpdatingClause::Merge(_) => {
                return unsupported("MERGE needs match-or-create planning");
            }
            kyu::UpdatingClause::Set(_) => {
                return unsupported("SET needs bound entity mutation planning");
            }
            kyu::UpdatingClause::Delete(_) => {
                return unsupported("DELETE needs bound entity mutation planning");
            }
            kyu::UpdatingClause::Remove(_) => {
                return unsupported("REMOVE needs bound entity mutation planning");
            }
        }
    }

    let return_clause = match part.projection {
        Some(projection) => {
            if !part.is_return {
                return unsupported("WITH projection needs pipeline planning");
            }
            let (return_clause, order_by, skip, limit) = map_projection_body(projection)?;
            if order_by.is_some() || skip.is_some() || limit.is_some() {
                return unsupported("ORDER BY/SKIP/LIMIT after write needs row pipeline planning");
            }
            Some(return_clause)
        }
        None => None,
    };

    Ok(WriteStatement {
        clauses,
        return_clause,
    })
}

fn map_match_clause(
    clause: kyu::MatchClause,
) -> NexusParserResult<(domyn::MatchClause, Option<domyn::WhereClause>)> {
    if clause.is_optional {
        return unsupported("OPTIONAL MATCH needs nullable-row executor semantics");
    }
    if clause.patterns.len() != 1 {
        return unsupported("comma-separated MATCH patterns need join planning");
    }

    let where_clause = clause
        .where_clause
        .map(|expr| map_expr(expr).map(|expr| domyn::WhereClause { expr }))
        .transpose()?;

    let patterns = clause
        .patterns
        .into_iter()
        .map(map_pattern)
        .collect::<NexusParserResult<Vec<_>>>()?;

    Ok((domyn::MatchClause { patterns }, where_clause))
}

fn map_pattern(pattern: kyu::Pattern) -> NexusParserResult<domyn::Pattern> {
    if pattern.variable.is_some() {
        return unsupported("path variables need path-value planning");
    }

    let elements = pattern
        .elements
        .into_iter()
        .map(map_pattern_element)
        .collect::<NexusParserResult<Vec<_>>>()?;

    Ok(domyn::Pattern {
        path_variable: None,
        elements,
    })
}

fn map_pattern_element(element: kyu::PatternElement) -> NexusParserResult<domyn::PatternElement> {
    match element {
        kyu::PatternElement::Node(node) => Ok(domyn::PatternElement::Node(map_node(node)?)),
        kyu::PatternElement::Relationship(rel) => {
            Ok(domyn::PatternElement::Relationship(map_relationship(rel)?))
        }
    }
}

fn map_node(node: kyu::NodePattern) -> NexusParserResult<domyn::NodePattern> {
    let properties_specified = node.properties.is_some();
    let properties = match node.properties {
        Some(props) => props
            .into_iter()
            .map(|(key, expr)| Ok((key.0.to_string(), map_expr(expr)?)))
            .collect::<NexusParserResult<Vec<_>>>()?,
        None => Vec::new(),
    };

    Ok(domyn::NodePattern {
        variable: node.variable.map(|(name, _)| name.to_string()),
        labels: node
            .labels
            .into_iter()
            .map(|(label, _)| label.to_string())
            .collect(),
        properties,
        properties_specified,
    })
}

fn map_relationship(
    rel: kyu::RelationshipPattern,
) -> NexusParserResult<domyn::RelationshipPattern> {
    if rel.properties.is_some() {
        return unsupported("relationship properties need relationship-value planning");
    }

    let (min_hops, max_hops) = rel.range.unwrap_or((Some(1), Some(1)));

    Ok(domyn::RelationshipPattern {
        variable: rel.variable.map(|(name, _)| name.to_string()),
        rel_types: rel
            .rel_types
            .into_iter()
            .map(|(rel_type, _)| rel_type.to_string())
            .collect(),
        properties: Vec::new(),
        direction: match rel.direction {
            kyu::Direction::Left => domyn::RelDirection::Incoming,
            kyu::Direction::Right => domyn::RelDirection::Outgoing,
            kyu::Direction::Both => domyn::RelDirection::Both,
        },
        min_hops,
        max_hops,
    })
}

fn map_projection_body(
    projection: kyu::ProjectionBody,
) -> NexusParserResult<(
    domyn::ReturnClause,
    Option<domyn::OrderByClause>,
    Option<domyn::RowCount>,
    Option<domyn::RowCount>,
)> {
    let items = match projection.items {
        kyu::ProjectionItems::Expressions(items) => items
            .into_iter()
            .map(|(expr, alias)| {
                Ok(domyn::ReturnItem {
                    expr: map_expr(expr)?,
                    alias: alias.map(|(alias, _)| alias.to_string()),
                    raw: None,
                })
            })
            .collect::<NexusParserResult<Vec<_>>>()?,
        kyu::ProjectionItems::All => {
            return unsupported("RETURN * needs row-shape projection planning");
        }
    };

    let order_by_items = projection
        .order_by
        .into_iter()
        .map(|(expr, sort)| {
            Ok(domyn::OrderByItem {
                expr: map_expr(expr)?,
                descending: matches!(sort, kyu::SortOrder::Descending),
            })
        })
        .collect::<NexusParserResult<Vec<_>>>()?;

    let order_by = if order_by_items.is_empty() {
        None
    } else {
        Some(domyn::OrderByClause {
            items: order_by_items,
        })
    };

    let skip = projection
        .skip
        .map(map_u64_expression)
        .transpose()?
        .map(domyn::RowCount::Literal);
    let limit = projection
        .limit
        .map(map_u64_expression)
        .transpose()?
        .map(domyn::RowCount::Literal);

    Ok((
        domyn::ReturnClause {
            items,
            distinct: projection.distinct,
        },
        order_by,
        skip,
        limit,
    ))
}

fn map_u64_expression(expr: (kyu::Expression, kyu_parser::span::Span)) -> NexusParserResult<u64> {
    match expr.0 {
        kyu::Expression::Literal(kyu::Literal::Integer(value)) if value >= 0 => Ok(value as u64),
        other => unsupported(format!(
            "SKIP/LIMIT must be non-negative integer literals, got {other:?}"
        )),
    }
}

fn map_expr(expr: (kyu::Expression, kyu_parser::span::Span)) -> NexusParserResult<domyn::Expr> {
    match expr.0 {
        kyu::Expression::Literal(lit) => Ok(domyn::Expr::Literal(map_literal(lit))),
        kyu::Expression::Variable(name) => Ok(domyn::Expr::Variable(name.to_string())),
        kyu::Expression::Parameter(name) => Ok(domyn::Expr::Parameter(name.to_string())),
        kyu::Expression::Property { object, key } => map_property_expr(*object, key),
        kyu::Expression::FunctionCall {
            name,
            distinct,
            args,
        } => map_function_call(name, distinct, args),
        kyu::Expression::CountStar => Ok(domyn::Expr::FunctionCall {
            name: "count".into(),
            args: Vec::new(),
        }),
        kyu::Expression::UnaryOp { op, operand } => map_unary_expr(op, *operand),
        kyu::Expression::BinaryOp { left, op, right } => map_binary_expr(*left, op, *right),
        kyu::Expression::Comparison { left, ops } => map_comparison_expr(*left, ops),
        kyu::Expression::IsNull { expr, negated } => Ok(domyn::Expr::UnaryOp {
            op: if negated {
                domyn::UnaryOp::IsNotNull
            } else {
                domyn::UnaryOp::IsNull
            },
            expr: Box::new(map_expr(*expr)?),
        }),
        kyu::Expression::StringOp { left, op, right } => Ok(domyn::Expr::BinaryOp {
            left: Box::new(map_expr(*left)?),
            op: match op {
                kyu::StringOp::StartsWith => domyn::BinaryOp::StartsWith,
                kyu::StringOp::EndsWith => domyn::BinaryOp::EndsWith,
                kyu::StringOp::Contains => domyn::BinaryOp::Contains,
            },
            right: Box::new(map_expr(*right)?),
        }),
        other => unsupported(format!("expression is parse-only for now: {other:?}")),
    }
}

fn map_literal(lit: kyu::Literal) -> domyn::Literal {
    match lit {
        kyu::Literal::Integer(value) => domyn::Literal::Integer(value),
        kyu::Literal::Float(value) => domyn::Literal::Float(value),
        kyu::Literal::String(value) => domyn::Literal::String(value.to_string()),
        kyu::Literal::Bool(value) => domyn::Literal::Bool(value),
        kyu::Literal::Null => domyn::Literal::Null,
    }
}

fn map_property_expr(
    object: (kyu::Expression, kyu_parser::span::Span),
    key: (smol_str::SmolStr, kyu_parser::span::Span),
) -> NexusParserResult<domyn::Expr> {
    match object.0 {
        kyu::Expression::Variable(variable) => Ok(domyn::Expr::Property(domyn::PropertyAccess {
            variable: variable.to_string(),
            property: key.0.to_string(),
        })),
        other => unsupported(format!(
            "nested property object is not planned yet: {other:?}"
        )),
    }
}

fn map_function_call(
    name: Vec<(smol_str::SmolStr, kyu_parser::span::Span)>,
    distinct: bool,
    args: Vec<(kyu::Expression, kyu_parser::span::Span)>,
) -> NexusParserResult<domyn::Expr> {
    if distinct {
        return unsupported("DISTINCT inside aggregate/function calls needs aggregate planning");
    }

    Ok(domyn::Expr::FunctionCall {
        name: name
            .into_iter()
            .map(|(part, _)| part.to_string())
            .collect::<Vec<_>>()
            .join("."),
        args: args
            .into_iter()
            .map(map_expr)
            .collect::<NexusParserResult<Vec<_>>>()?,
    })
}

fn map_unary_expr(
    op: kyu::UnaryOp,
    operand: (kyu::Expression, kyu_parser::span::Span),
) -> NexusParserResult<domyn::Expr> {
    match op {
        kyu::UnaryOp::Not => Ok(domyn::Expr::UnaryOp {
            op: domyn::UnaryOp::Not,
            expr: Box::new(map_expr(operand)?),
        }),
        kyu::UnaryOp::Minus | kyu::UnaryOp::BitwiseNot => {
            unsupported("numeric/bitwise unary expressions need expression evaluation")
        }
    }
}

fn map_binary_expr(
    left: (kyu::Expression, kyu_parser::span::Span),
    op: kyu::BinaryOp,
    right: (kyu::Expression, kyu_parser::span::Span),
) -> NexusParserResult<domyn::Expr> {
    match op {
        kyu::BinaryOp::And | kyu::BinaryOp::Or => Ok(domyn::Expr::BinaryOp {
            left: Box::new(map_expr(left)?),
            op: if matches!(op, kyu::BinaryOp::And) {
                domyn::BinaryOp::And
            } else {
                domyn::BinaryOp::Or
            },
            right: Box::new(map_expr(right)?),
        }),
        other => unsupported(format!(
            "binary operator needs expression evaluation: {other:?}"
        )),
    }
}

fn map_comparison_expr(
    left: (kyu::Expression, kyu_parser::span::Span),
    ops: Vec<(kyu::ComparisonOp, (kyu::Expression, kyu_parser::span::Span))>,
) -> NexusParserResult<domyn::Expr> {
    if ops.len() != 1 {
        return unsupported("chained comparisons need expression lowering");
    }

    let (op, right) = ops
        .into_iter()
        .next()
        .ok_or_else(|| NexusParserError::Mapping("comparison has no operator".into()))?;

    Ok(domyn::Expr::BinaryOp {
        left: Box::new(map_expr(left)?),
        op: match op {
            kyu::ComparisonOp::Eq => domyn::BinaryOp::Eq,
            kyu::ComparisonOp::Neq => domyn::BinaryOp::Neq,
            kyu::ComparisonOp::Lt => domyn::BinaryOp::Lt,
            kyu::ComparisonOp::Le => domyn::BinaryOp::Lte,
            kyu::ComparisonOp::Gt => domyn::BinaryOp::Gt,
            kyu::ComparisonOp::Ge => domyn::BinaryOp::Gte,
            kyu::ComparisonOp::RegexMatch => {
                return unsupported("regex comparisons need predicate support");
            }
        },
        right: Box::new(map_expr(right)?),
    })
}

fn unsupported<T>(message: impl Into<String>) -> NexusParserResult<T> {
    Err(NexusParserError::Mapping(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_cypher::ast::{BinaryOp, Expr, Literal, PatternElement};

    #[test]
    fn kyu_frontend_accepts_current_match_where_return_subset() {
        let query = "MATCH (n:Entity) WHERE n.name = 'Alice' RETURN n.name LIMIT 10";
        let parsed = NexusParser::parse_kyu(query).unwrap();

        assert_eq!(parsed.backend, ParserBackend::Kyu);
        assert!(parsed.ast.match_clause.is_some());
        assert_eq!(parsed.ast.return_clause.items.len(), 1);
    }

    #[test]
    fn kyu_frontend_uses_kyu_ast_not_native_fallback() {
        let query = concat!(
            "MATCH (n:Entity) WHERE n.name STARTS WITH 'A' ",
            "RETURN n.name ORDER BY n.name DESC LIMIT 5"
        );
        let parsed = NexusParser::parse_kyu(query).unwrap();

        let where_expr = &parsed.ast.where_clause.as_ref().unwrap().expr;
        match where_expr {
            Expr::BinaryOp { op, right, .. } => {
                assert_eq!(*op, BinaryOp::StartsWith);
                assert!(matches!(**right, Expr::Literal(Literal::String(ref s)) if s == "A"));
            }
            other => panic!("expected STARTS WITH predicate, got {other:?}"),
        }

        assert_eq!(parsed.ast.limit, Some(domyn::RowCount::Literal(5)));
        assert!(parsed.ast.order_by.as_ref().unwrap().items[0].descending);
    }

    #[test]
    fn kyu_frontend_maps_to_existing_logical_plan() {
        let query = "MATCH (n:Entity)-[:KNOWS]->(m:Entity) RETURN n, m";
        let plan = NexusParser::plan_kyu(query).unwrap();

        assert!(matches!(plan, LogicalPlan::Project { .. }));
    }

    #[test]
    fn kyu_adapter_maps_pattern_details() {
        let query = "MATCH (n:Entity {name: $name})<-[:KNOWS*1..3]-(m:Person) RETURN m";
        let parsed = NexusParser::parse_kyu(query).unwrap();
        let pattern = &parsed.ast.match_clause.as_ref().unwrap().patterns[0];

        match &pattern.elements[..] {
            [
                PatternElement::Node(start),
                PatternElement::Relationship(rel),
                PatternElement::Node(end),
            ] => {
                assert_eq!(start.variable.as_deref(), Some("n"));
                assert_eq!(start.labels, vec!["Entity"]);
                assert!(matches!(start.properties[0].1, Expr::Parameter(ref p) if p == "name"));
                assert_eq!(rel.rel_types, vec!["KNOWS"]);
                assert_eq!(rel.direction, nexus_cypher::ast::RelDirection::Incoming);
                assert_eq!(rel.min_hops, Some(1));
                assert_eq!(rel.max_hops, Some(3));
                assert_eq!(end.labels, vec!["Person"]);
            }
            other => panic!("unexpected pattern elements: {other:?}"),
        }
    }

    #[test]
    fn kyu_coverage_probe_marks_create_as_mapped_write() {
        let report = NexusParser::probe("CREATE (n:Entity {name: 'Alice'}) RETURN n");

        assert!(!report.native_parses);
        assert!(report.kyu_parses);
        assert!(report.kyu_maps);
        assert!(!report.kyu_plans);
        assert!(report.detail.unwrap().contains("write statement mapped"));
    }

    #[test]
    fn kyu_adapter_maps_simple_create_write_statement() {
        let parsed =
            NexusParser::parse_statement_kyu("CREATE (n:Entity {name: $name}) RETURN n.name")
                .unwrap();

        let ParsedStatement::Write(write) = parsed else {
            panic!("expected write statement");
        };

        assert_eq!(write.backend, ParserBackend::Kyu);
        assert_eq!(write.statement.clauses.len(), 1);
        assert!(write.statement.return_clause.is_some());

        let WriteClause::Create(patterns) = &write.statement.clauses[0];
        match &patterns[0].elements[..] {
            [PatternElement::Node(node)] => {
                assert_eq!(node.variable.as_deref(), Some("n"));
                assert_eq!(node.labels, vec!["Entity"]);
                assert!(matches!(node.properties[0].1, Expr::Parameter(ref p) if p == "name"));
            }
            other => panic!("unexpected create pattern: {other:?}"),
        }
    }

    #[test]
    fn kyu_adapter_rejects_optional_match_until_executor_semantics_exist() {
        let report = NexusParser::probe("OPTIONAL MATCH (n) RETURN n");

        assert!(report.kyu_parses);
        assert!(!report.kyu_maps);
        assert!(report.detail.unwrap().contains("OPTIONAL MATCH"));
    }

    #[test]
    fn compare_coverage_reports_parse_and_plan_status() {
        let reports = NexusParser::compare_coverage([
            "MATCH (n:Entity) RETURN n",
            "MATCH (n) WITH n RETURN n",
            "CREATE (n:Entity {name: 'Alice'}) RETURN n",
        ]);

        assert_eq!(reports.len(), 3);
        assert!(reports[0].kyu_plans);
        assert!(reports[1].kyu_parses);
        assert!(!reports[1].kyu_maps);
        assert!(reports[2].kyu_parses);
        assert!(reports[2].kyu_maps);
        assert!(!reports[2].kyu_plans);
    }
}
