//! Recursive descent parser for openCypher subset.
//!
//! Parses: MATCH ... WHERE ... RETURN ... ORDER BY ... LIMIT ... SKIP

use crate::ast::*;
use crate::error::{CypherError, CypherResult};
use crate::lexer::{Lexer, Token};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    /// Parse an arbitrary Cypher statement (read or write).
    pub fn parse(input: &str) -> CypherResult<Statement> {
        let mut lexer = Lexer::new(input);
        let tokens = lexer.tokenize()?;
        let mut parser = Parser { tokens, pos: 0 };
        let mut statement = parser.parse_statement()?;
        parser.expect(&Token::Eof)?;
        annotate_projection_sources(&mut statement, input);
        Ok(statement)
    }

    /// Parse a read-only query. Returns an error if the input contains
    /// mutation clauses (CREATE / DELETE).
    pub fn parse_read(input: &str) -> CypherResult<Query> {
        match Self::parse(input)? {
            Statement::Read(q) => Ok(q),
            Statement::Write(_) => Err(CypherError::Parse {
                position: 0,
                message: "expected a read-only query, got a write statement".into(),
            }),
        }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> CypherResult<Token> {
        let tok = self.advance();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            Ok(tok)
        } else {
            Err(CypherError::UnexpectedToken {
                expected: format!("{expected:?}"),
                got: format!("{tok:?}"),
            })
        }
    }

    fn parse_statement(&mut self) -> CypherResult<Statement> {
        // Collect any leading MATCH/WHERE/WITH/UNWIND/OPTIONAL MATCH clauses.
        // The first MATCH/WHERE populate the back-compat `match_clause` /
        // `where_clause` fields; everything else goes into `tail`.
        let mut match_clause: Option<MatchClause> = None;
        let mut where_clause: Option<WhereClause> = None;
        let mut tail: Vec<ReadClause> = Vec::new();

        loop {
            match self.peek() {
                Token::Match => {
                    if self.match_starts_document_scan() {
                        tail.extend(self.parse_document_match_scan()?);
                    } else {
                        let clause = self.parse_match()?;
                        if match_clause.is_none() && tail.is_empty() {
                            match_clause = Some(clause);
                        } else {
                            tail.push(ReadClause::Match {
                                optional: false,
                                clause,
                            });
                        }
                    }
                }
                Token::Optional => {
                    self.advance();
                    self.expect(&Token::Match)?;
                    let clause = self.parse_match_body()?;
                    tail.push(ReadClause::Match {
                        optional: true,
                        clause,
                    });
                }
                Token::Where => {
                    let wh = self.parse_where()?;
                    if where_clause.is_none() && tail.is_empty() {
                        where_clause = Some(wh);
                    } else {
                        tail.push(ReadClause::Where(wh));
                    }
                }
                Token::With => {
                    let wc = self.parse_with_clause()?;
                    tail.push(ReadClause::With(wc));
                }
                Token::Unwind => {
                    self.advance();
                    let expr = self.parse_or_expr()?;
                    self.expect_keyword("AS")?;
                    let alias = self.expect_ident("UNWIND alias")?;
                    tail.push(ReadClause::Unwind { expr, alias });
                }
                _ => break,
            }
        }

        let is_write = matches!(
            self.peek(),
            Token::Create
                | Token::Merge
                | Token::Set
                | Token::Remove
                | Token::Detach
                | Token::Delete
        );

        if is_write {
            let mutations = self.parse_mutations()?;
            let return_clause = if *self.peek() == Token::Return {
                Some(self.parse_return()?)
            } else {
                None
            };
            let order_by = if *self.peek() == Token::Order {
                Some(self.parse_order_by()?)
            } else {
                None
            };
            let skip = if *self.peek() == Token::Skip {
                self.advance();
                Some(self.parse_row_count()?)
            } else {
                None
            };
            let limit = if *self.peek() == Token::Limit {
                self.advance();
                Some(self.parse_row_count()?)
            } else {
                None
            };
            return Ok(Statement::Write(WriteQuery {
                match_clause,
                where_clause,
                tail,
                mutations,
                return_clause,
                order_by,
                skip,
                limit,
            }));
        }

        let return_clause = self.parse_return()?;

        let order_by = if *self.peek() == Token::Order {
            Some(self.parse_order_by()?)
        } else {
            None
        };

        let skip = if *self.peek() == Token::Skip {
            self.advance();
            Some(self.parse_row_count()?)
        } else {
            None
        };

        let limit = if *self.peek() == Token::Limit {
            self.advance();
            Some(self.parse_row_count()?)
        } else {
            None
        };

        // UNION tail
        let union = if *self.peek() == Token::Union {
            self.advance();
            let all = if *self.peek() == Token::All {
                self.advance();
                true
            } else {
                false
            };
            let Statement::Read(right) = self.parse_statement()? else {
                return Err(CypherError::Parse {
                    position: self.pos,
                    message: "UNION right side must be a read query".into(),
                });
            };
            Some(Box::new(UnionTail { all, right }))
        } else {
            None
        };

        Ok(Statement::Read(Query {
            match_clause,
            where_clause,
            tail,
            return_clause,
            order_by,
            limit,
            skip,
            union,
        }))
    }

    fn parse_match_body(&mut self) -> CypherResult<MatchClause> {
        let mut patterns = vec![self.parse_pattern()?];
        while *self.peek() == Token::Comma {
            self.advance();
            patterns.push(self.parse_pattern()?);
        }
        Ok(MatchClause { patterns })
    }

    fn parse_with_clause(&mut self) -> CypherResult<WithClause> {
        self.expect(&Token::With)?;
        let distinct = if *self.peek() == Token::Distinct {
            self.advance();
            true
        } else {
            false
        };
        let mut items = vec![self.parse_return_item()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_return_item()?);
        }

        let order_by = if *self.peek() == Token::Order {
            Some(self.parse_order_by()?)
        } else {
            None
        };

        let skip = if *self.peek() == Token::Skip {
            self.advance();
            Some(self.parse_row_count()?)
        } else {
            None
        };

        let limit = if *self.peek() == Token::Limit {
            self.advance();
            Some(self.parse_row_count()?)
        } else {
            None
        };

        let where_clause = if *self.peek() == Token::Where {
            Some(self.parse_where()?)
        } else {
            None
        };

        Ok(WithClause {
            items,
            distinct,
            where_clause,
            order_by,
            skip,
            limit,
        })
    }

    fn expect_keyword(&mut self, word: &str) -> CypherResult<()> {
        match self.advance() {
            Token::As if word.eq_ignore_ascii_case("AS") => Ok(()),
            Token::Ident(name) if name.eq_ignore_ascii_case(word) => Ok(()),
            other => Err(CypherError::UnexpectedToken {
                expected: word.to_string(),
                got: format!("{other:?}"),
            }),
        }
    }

    fn expect_ident(&mut self, ctx: &str) -> CypherResult<String> {
        match self.advance() {
            Token::Ident(s) => Ok(s),
            other => Err(CypherError::UnexpectedToken {
                expected: format!("identifier for {ctx}"),
                got: format!("{other:?}"),
            }),
        }
    }

    fn parse_mutations(&mut self) -> CypherResult<Vec<MutationClause>> {
        let mut mutations = Vec::new();
        loop {
            match self.peek() {
                Token::Create => {
                    self.advance();
                    let mut patterns = vec![self.parse_pattern()?];
                    while *self.peek() == Token::Comma {
                        self.advance();
                        patterns.push(self.parse_pattern()?);
                    }
                    mutations.push(MutationClause::Create { patterns });
                }
                Token::Merge => {
                    self.advance();
                    let mut patterns = vec![self.parse_pattern()?];
                    while *self.peek() == Token::Comma {
                        self.advance();
                        patterns.push(self.parse_pattern()?);
                    }
                    let (on_create, on_match) = self.parse_merge_actions()?;
                    mutations.push(MutationClause::Merge {
                        patterns,
                        on_create,
                        on_match,
                    });
                }
                Token::Set => {
                    self.advance();
                    let mut items = vec![self.parse_set_item()?];
                    while *self.peek() == Token::Comma {
                        self.advance();
                        items.push(self.parse_set_item()?);
                    }
                    mutations.push(MutationClause::Set { items });
                }
                Token::Remove => {
                    self.advance();
                    let mut items = vec![self.parse_remove_item()?];
                    while *self.peek() == Token::Comma {
                        self.advance();
                        items.push(self.parse_remove_item()?);
                    }
                    mutations.push(MutationClause::Remove { items });
                }
                Token::Detach => {
                    self.advance();
                    self.expect(&Token::Delete)?;
                    let targets = self.parse_delete_targets()?;
                    mutations.push(MutationClause::Delete {
                        targets,
                        detach: true,
                    });
                }
                Token::Delete => {
                    self.advance();
                    let targets = self.parse_delete_targets()?;
                    mutations.push(MutationClause::Delete {
                        targets,
                        detach: false,
                    });
                }
                Token::With => {
                    let clause = self.parse_with_clause()?;
                    mutations.push(MutationClause::Read(ReadClause::With(clause)));
                }
                Token::Unwind => {
                    self.advance();
                    let expr = self.parse_or_expr()?;
                    self.expect_keyword("AS")?;
                    let alias = self.expect_ident("UNWIND alias")?;
                    mutations.push(MutationClause::Read(ReadClause::Unwind { expr, alias }));
                }
                Token::Match => {
                    if self.match_starts_document_scan() {
                        for clause in self.parse_document_match_scan()? {
                            mutations.push(MutationClause::Read(clause));
                        }
                    } else {
                        let clause = self.parse_match()?;
                        mutations.push(MutationClause::Read(ReadClause::Match {
                            optional: false,
                            clause,
                        }));
                    }
                }
                Token::Optional => {
                    self.advance();
                    self.expect(&Token::Match)?;
                    let clause = self.parse_match_body()?;
                    mutations.push(MutationClause::Read(ReadClause::Match {
                        optional: true,
                        clause,
                    }));
                }
                Token::Where => {
                    let clause = self.parse_where()?;
                    mutations.push(MutationClause::Read(ReadClause::Where(clause)));
                }
                _ => break,
            }
        }
        if mutations.is_empty() {
            return Err(CypherError::Parse {
                position: self.pos,
                message: "expected at least one CREATE, MERGE, SET, REMOVE, or DELETE clause"
                    .into(),
            });
        }
        Ok(mutations)
    }

    fn parse_merge_actions(&mut self) -> CypherResult<(Vec<SetItem>, Vec<SetItem>)> {
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();

        while *self.peek() == Token::On {
            self.advance();
            let target = match self.advance() {
                Token::Create => MergeActionTarget::Create,
                Token::Match => MergeActionTarget::Match,
                other => {
                    return Err(CypherError::UnexpectedToken {
                        expected: "CREATE or MATCH after ON".into(),
                        got: format!("{other:?}"),
                    });
                }
            };
            self.expect(&Token::Set)?;
            let mut items = vec![self.parse_set_item()?];
            while *self.peek() == Token::Comma {
                self.advance();
                items.push(self.parse_set_item()?);
            }
            match target {
                MergeActionTarget::Create => on_create.extend(items),
                MergeActionTarget::Match => on_match.extend(items),
            }
        }

        Ok((on_create, on_match))
    }

    fn parse_set_item(&mut self) -> CypherResult<SetItem> {
        // Detect `<ident> :Label[:Label]*` for label-add SET.
        if let Token::Ident(_) = self.peek() {
            if matches!(self.tokens.get(self.pos + 1), Some(Token::Colon)) {
                let variable = self.expect_ident("SET target variable")?;
                let labels = self.parse_label_list()?;
                return Ok(SetItem::Labels { variable, labels });
            }
            if matches!(
                self.tokens.get(self.pos + 1),
                Some(Token::Eq | Token::PlusEq)
            ) {
                let variable = self.expect_ident("SET target variable")?;
                let replace = match self.peek() {
                    Token::Eq => {
                        self.advance();
                        true
                    }
                    Token::PlusEq => {
                        self.advance();
                        false
                    }
                    _ => unreachable!("lookahead already checked assignment operator"),
                };
                let value = self.parse_or_expr()?;
                return Ok(SetItem::Properties {
                    variable,
                    value,
                    replace,
                });
            }
        }
        let target = self.parse_property_access_target()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_or_expr()?;
        Ok(SetItem::Property { target, value })
    }

    fn parse_remove_item(&mut self) -> CypherResult<RemoveItem> {
        if let Token::Ident(_) = self.peek() {
            if matches!(self.tokens.get(self.pos + 1), Some(Token::Colon)) {
                let variable = self.expect_ident("REMOVE target variable")?;
                let labels = self.parse_label_list()?;
                return Ok(RemoveItem::Labels { variable, labels });
            }
        }
        Ok(RemoveItem::Property(self.parse_property_access_target()?))
    }

    /// Consume `:Label (:Label)*` and return the list of label names.
    fn parse_label_list(&mut self) -> CypherResult<Vec<String>> {
        let mut labels = Vec::new();
        while *self.peek() == Token::Colon {
            self.advance();
            labels.push(label_name_from_token(self.advance()).ok_or_else(|| {
                CypherError::Parse {
                    position: self.pos,
                    message: "expected label".into(),
                }
            })?);
        }
        if labels.is_empty() {
            return Err(CypherError::Parse {
                position: self.pos,
                message: "expected at least one ':Label'".into(),
            });
        }
        Ok(labels)
    }

    fn parse_property_access_target(&mut self) -> CypherResult<PropertyAccess> {
        // Use parse_postfix so `(n).prop` is folded into a Property access
        // (parse_primary alone returns only the inner Variable("n")).
        match self.parse_postfix()? {
            Expr::Property(pa) => Ok(pa),
            other => Err(CypherError::Parse {
                position: self.pos,
                message: format!("expected property access, got {other:?}"),
            }),
        }
    }

    fn parse_delete_targets(&mut self) -> CypherResult<Vec<Expr>> {
        let mut targets = vec![self.parse_or_expr()?];
        while *self.peek() == Token::Comma {
            self.advance();
            targets.push(self.parse_or_expr()?);
        }
        Ok(targets)
    }

    fn parse_match(&mut self) -> CypherResult<MatchClause> {
        self.expect(&Token::Match)?;
        let mut patterns = vec![self.parse_pattern()?];
        while *self.peek() == Token::Comma {
            self.advance();
            patterns.push(self.parse_pattern()?);
        }
        Ok(MatchClause { patterns })
    }

    fn match_starts_document_scan(&self) -> bool {
        matches!(
            self.tokens.get(self.pos + 1),
            Some(Token::Ident(word))
                if word.eq_ignore_ascii_case("DOCUMENT")
                    || word.eq_ignore_ascii_case("DOCUMENTS")
        )
    }

    fn parse_document_match_scan(&mut self) -> CypherResult<Vec<ReadClause>> {
        self.expect(&Token::Match)?;
        match self.advance() {
            Token::Ident(word)
                if word.eq_ignore_ascii_case("DOCUMENT")
                    || word.eq_ignore_ascii_case("DOCUMENTS") => {}
            other => {
                return Err(CypherError::UnexpectedToken {
                    expected: "DOCUMENT".into(),
                    got: format!("{other:?}"),
                });
            }
        }
        let alias = self.expect_ident("document alias")?;
        self.expect(&Token::In)?;
        let collection = match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Expr::Literal(Literal::String(name))
            }
            _ => self.parse_or_expr()?,
        };
        if *self.peek() == Token::Where {
            let where_clause = self.parse_where()?;
            if let Some(pushdown) = document_index_pushdown(&alias, &collection, &where_clause.expr)
            {
                let mut clauses = vec![ReadClause::Unwind {
                    expr: pushdown.expr,
                    alias,
                }];
                if pushdown.keep_where {
                    clauses.push(ReadClause::Where(where_clause));
                }
                return Ok(clauses);
            }
            return Ok(vec![
                ReadClause::Unwind {
                    expr: document_scan_expr(collection),
                    alias,
                },
                ReadClause::Where(where_clause),
            ]);
        }
        Ok(vec![ReadClause::Unwind {
            expr: document_scan_expr(collection),
            alias,
        }])
    }

    fn parse_pattern(&mut self) -> CypherResult<Pattern> {
        let mut path_variable = None;
        // Optional path binding: `p = (a)-[:R]->(b)`.
        if let Token::Ident(_) = self.peek() {
            if let Some(Token::Eq) = self.tokens.get(self.pos + 1) {
                if let Token::Ident(name) = self.advance() {
                    path_variable = Some(name);
                }
                self.advance(); // =
            }
        }

        let mut elements = Vec::new();
        elements.push(PatternElement::Node(self.parse_node_pattern()?));

        loop {
            match self.peek() {
                Token::Dash | Token::ArrowLeftDash => {
                    let rel = self.parse_relationship_pattern()?;
                    elements.push(PatternElement::Relationship(rel));
                    elements.push(PatternElement::Node(self.parse_node_pattern()?));
                }
                _ => break,
            }
        }
        Ok(Pattern {
            path_variable,
            elements,
        })
    }

    fn parse_node_pattern(&mut self) -> CypherResult<NodePattern> {
        self.expect(&Token::LParen)?;

        let variable = if let Token::Ident(_) = self.peek() {
            if let Token::Ident(name) = self.advance() {
                Some(name)
            } else {
                None
            }
        } else {
            None
        };

        let mut labels = Vec::new();
        while *self.peek() == Token::Colon {
            self.advance();
            labels.push(label_name_from_token(self.advance()).ok_or_else(|| {
                CypherError::Parse {
                    position: self.pos,
                    message: "expected label name after ':'".into(),
                }
            })?);
        }

        let properties_specified = *self.peek() == Token::LBrace;
        let properties = if properties_specified {
            self.parse_property_map()?
        } else {
            Vec::new()
        };

        self.expect(&Token::RParen)?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
            properties_specified,
        })
    }

    fn parse_relationship_pattern(&mut self) -> CypherResult<RelationshipPattern> {
        let left_arrow = *self.peek() == Token::ArrowLeftDash;
        if left_arrow {
            self.advance(); // consume <-
        } else {
            self.expect(&Token::Dash)?;
        }

        let (variable, rel_types, properties, min_hops, max_hops) =
            if *self.peek() == Token::LBracket {
                self.advance();
                let var = if let Token::Ident(_) = self.peek() {
                    if let Token::Ident(name) = self.advance() {
                        Some(name)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let mut types = Vec::new();
                if *self.peek() == Token::Colon {
                    self.advance();
                    if let Some(t) = label_name_from_token(self.advance()) {
                        types.push(t);
                    }
                    while *self.peek() == Token::Pipe {
                        self.advance();
                        if *self.peek() == Token::Colon {
                            self.advance();
                        }
                        if let Some(t) = label_name_from_token(self.advance()) {
                            types.push(t);
                        }
                    }
                }

                let (min_h, max_h) = if *self.peek() == Token::Star {
                    self.advance();
                    let (min_h, max_h) = self.parse_hop_range()?;
                    (Some(min_h.unwrap_or(1)), max_h)
                } else {
                    (None, None)
                };

                let properties = if *self.peek() == Token::LBrace {
                    self.parse_property_map()?
                } else {
                    Vec::new()
                };

                self.expect(&Token::RBracket)?;
                (var, types, properties, min_h, max_h)
            } else {
                (None, Vec::new(), Vec::new(), None, None)
            };

        let direction = if left_arrow {
            if *self.peek() == Token::DashArrowRight {
                self.advance();
                RelDirection::Both
            } else if *self.peek() == Token::Dash {
                self.advance();
                RelDirection::Incoming
            } else {
                RelDirection::Incoming
            }
        } else if *self.peek() == Token::DashArrowRight {
            self.advance();
            RelDirection::Outgoing
        } else if *self.peek() == Token::Dash {
            self.advance();
            RelDirection::Both
        } else {
            RelDirection::Outgoing
        };

        Ok(RelationshipPattern {
            variable,
            rel_types,
            properties,
            direction,
            min_hops: min_hops,
            max_hops: max_hops,
        })
    }

    fn parse_hop_range(&mut self) -> CypherResult<(Option<u32>, Option<u32>)> {
        let min = if let Token::IntLiteral(_) = self.peek() {
            Some(self.parse_integer()? as u32)
        } else {
            None
        };

        if *self.peek() == Token::DotDot {
            self.advance();
            let max = if let Token::IntLiteral(_) = self.peek() {
                Some(self.parse_integer()? as u32)
            } else {
                None
            };
            Ok((min, max))
        } else {
            Ok((min, min))
        }
    }

    fn parse_property_map(&mut self) -> CypherResult<Vec<(String, Expr)>> {
        self.expect(&Token::LBrace)?;
        let mut props = Vec::new();
        if *self.peek() != Token::RBrace {
            loop {
                let key = if let Some(key) = property_key_from_token(self.advance()) {
                    key
                } else {
                    return Err(CypherError::Parse {
                        position: self.pos,
                        message: "expected property key".into(),
                    });
                };
                self.expect(&Token::Colon)?;
                let value = self.parse_or_expr()?;
                props.push((key, value));
                if *self.peek() != Token::Comma {
                    break;
                }
                self.advance();
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(props)
    }

    fn parse_where(&mut self) -> CypherResult<WhereClause> {
        self.expect(&Token::Where)?;
        let expr = self.parse_or_expr()?;
        Ok(WhereClause { expr })
    }

    fn parse_or_expr(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_xor_expr()?;
        while *self.peek() == Token::Or {
            self.advance();
            let right = self.parse_xor_expr()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Or,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_comparison()?;
        while *self.peek() == Token::And {
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::And,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_xor_expr(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_and_expr()?;
        loop {
            let is_xor = matches!(
                self.peek(),
                Token::Ident(s) if s.eq_ignore_ascii_case("XOR")
            );
            if !is_xor {
                break;
            }
            self.advance();
            let right = self.parse_and_expr()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Xor,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> CypherResult<Expr> {
        if *self.peek() == Token::Not {
            self.advance();
            let expr = self.parse_comparison()?;
            return Ok(Expr::UnaryOp {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            });
        }

        let mut expr = self.parse_in_expr()?;
        let mut previous_rhs: Option<Expr> = None;

        while let Some(op) = self.parse_comparison_operator()? {
            self.advance();
            if matches!(op, BinaryOp::StartsWith | BinaryOp::EndsWith) {
                match self.peek() {
                    Token::With => {
                        self.advance();
                    }
                    Token::Ident(word) if word.eq_ignore_ascii_case("WITH") => {
                        self.advance();
                    }
                    tok => {
                        return Err(CypherError::UnexpectedToken {
                            expected: "WITH".into(),
                            got: format!("{tok:?}"),
                        });
                    }
                }
            }
            let right = self.parse_in_expr()?;
            let comparison_left = previous_rhs.clone().unwrap_or_else(|| expr.clone());
            let comparison = Expr::BinaryOp {
                left: Box::new(comparison_left),
                op,
                right: Box::new(right.clone()),
            };
            expr = if previous_rhs.is_some() {
                Expr::BinaryOp {
                    left: Box::new(expr),
                    op: BinaryOp::And,
                    right: Box::new(comparison),
                }
            } else {
                comparison
            };
            previous_rhs = Some(right);
        }

        Ok(expr)
    }

    fn parse_comparison_operator(&mut self) -> CypherResult<Option<BinaryOp>> {
        let op = match self.peek() {
            Token::Eq => Some(BinaryOp::Eq),
            Token::Neq => Some(BinaryOp::Neq),
            Token::Lt => Some(BinaryOp::Lt),
            Token::Lte => Some(BinaryOp::Lte),
            Token::Gt => Some(BinaryOp::Gt),
            Token::Gte => Some(BinaryOp::Gte),
            Token::Contains => Some(BinaryOp::Contains),
            Token::StartsWith => Some(BinaryOp::StartsWith),
            Token::EndsWith => Some(BinaryOp::EndsWith),
            Token::Is => {
                return Err(CypherError::UnexpectedToken {
                    expected: "comparison operator".into(),
                    got: "IS".into(),
                });
            }
            _ => None,
        };
        Ok(op)
    }

    /// `IN` binds tighter than comparison operators in openCypher.
    fn parse_in_expr(&mut self) -> CypherResult<Expr> {
        let left = self.parse_is_null_postfix()?;
        if *self.peek() == Token::In {
            self.advance();
            let list = self.parse_is_null_postfix()?;
            return Ok(Expr::In {
                expr: Box::new(left),
                list: Box::new(list),
            });
        }
        Ok(left)
    }

    /// Apply optional `IS NULL` / `IS NOT NULL` as a postfix operator on top
    /// of an additive expression. This lets `(a OR b) IS NULL = (b OR a) IS NULL`
    /// compose properly: both sides are unary-on-additive, then compared.
    fn parse_is_null_postfix(&mut self) -> CypherResult<Expr> {
        let base = self.parse_additive()?;
        if *self.peek() == Token::Is {
            self.advance();
            if *self.peek() == Token::Not {
                self.advance();
                self.expect(&Token::Null)?;
                return Ok(Expr::UnaryOp {
                    op: UnaryOp::IsNotNull,
                    expr: Box::new(base),
                });
            }
            self.expect(&Token::Null)?;
            return Ok(Expr::UnaryOp {
                op: UnaryOp::IsNull,
                expr: Box::new(base),
            });
        }
        Ok(base)
    }

    /// `+ -` (left-associative).
    fn parse_additive(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => Some(BinaryOp::Add),
                Token::Dash => Some(BinaryOp::Sub),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_multiplicative()?;
                left = Expr::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// `* / %` (left-associative).
    fn parse_multiplicative(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_power()?;
        loop {
            let op = match self.peek() {
                Token::Star => Some(BinaryOp::Mul),
                Token::Slash => Some(BinaryOp::Div),
                Token::Percent => Some(BinaryOp::Mod),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_power()?;
                left = Expr::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// `^` (left-associative in openCypher TCK, higher than * / %).
    fn parse_power(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_unary()?;
        while *self.peek() == Token::Caret {
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Pow,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// Unary `-` and `+`.
    fn parse_unary(&mut self) -> CypherResult<Expr> {
        if *self.peek() == Token::Dash {
            self.advance();
            match self.peek().clone() {
                Token::IntLiteral(v) => {
                    self.advance();
                    return Ok(Expr::Literal(Literal::Integer(-v)));
                }
                Token::IntLiteralMinMagnitude => {
                    self.advance();
                    return Ok(Expr::Literal(Literal::Integer(i64::MIN)));
                }
                Token::FloatLiteral(v) => {
                    self.advance();
                    return Ok(Expr::Literal(Literal::Float(-v)));
                }
                _ => {}
            }
            let e = self.parse_unary()?;
            return Ok(Expr::BinaryOp {
                left: Box::new(Expr::Literal(Literal::Integer(0))),
                op: BinaryOp::Sub,
                right: Box::new(e),
            });
        }
        if *self.peek() == Token::Plus {
            self.advance();
            return self.parse_unary();
        }
        self.parse_postfix()
    }

    /// Postfix operators: indexing `xs[i]`, slicing `xs[a..b]`, chained `.prop`.
    fn parse_postfix(&mut self) -> CypherResult<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek() {
                Token::LBracket => {
                    self.advance();
                    // Range? [a..b]
                    if *self.peek() == Token::DotDot {
                        self.advance();
                        let end = if *self.peek() != Token::RBracket {
                            Some(Box::new(self.parse_or_expr()?))
                        } else {
                            None
                        };
                        self.expect(&Token::RBracket)?;
                        e = Expr::Slice {
                            target: Box::new(e),
                            start: None,
                            end,
                        };
                        continue;
                    }
                    let idx = self.parse_or_expr()?;
                    if *self.peek() == Token::DotDot {
                        self.advance();
                        let end = if *self.peek() != Token::RBracket {
                            Some(Box::new(self.parse_or_expr()?))
                        } else {
                            None
                        };
                        self.expect(&Token::RBracket)?;
                        e = Expr::Slice {
                            target: Box::new(e),
                            start: Some(Box::new(idx)),
                            end,
                        };
                    } else {
                        self.expect(&Token::RBracket)?;
                        e = Expr::Index {
                            target: Box::new(e),
                            index: Box::new(idx),
                        };
                    }
                }
                Token::Dot => {
                    // Chained property access after any expression.
                    self.advance();
                    let prop = symbolic_name_from_token(self.advance()).ok_or_else(|| {
                        CypherError::Parse {
                            position: self.pos,
                            message: "expected identifier for property name".into(),
                        }
                    })?;
                    // Fold into Property if the target was a simple Variable.
                    // For expression targets like `(list[1]).name`, represent
                    // static property access as dynamic string-key lookup.
                    e = match e {
                        Expr::Variable(v) => Expr::Property(PropertyAccess {
                            variable: v,
                            property: prop,
                        }),
                        other => Expr::Index {
                            target: Box::new(other),
                            index: Box::new(Expr::Literal(Literal::String(prop))),
                        },
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> CypherResult<Expr> {
        match self.peek().clone() {
            Token::IntLiteral(v) => {
                self.advance();
                Ok(Expr::Literal(Literal::Integer(v)))
            }
            Token::IntLiteralMinMagnitude => Err(CypherError::Parse {
                position: self.pos,
                message: "integer out of range".into(),
            }),
            Token::FloatLiteral(v) => {
                self.advance();
                Ok(Expr::Literal(Literal::Float(v)))
            }
            Token::StringLiteral(s) => {
                self.advance();
                Ok(Expr::Literal(Literal::String(s)))
            }
            Token::True => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(true)))
            }
            Token::False => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(false)))
            }
            Token::Null => {
                self.advance();
                Ok(Expr::Literal(Literal::Null))
            }
            Token::Parameter(name) => {
                self.advance();
                Ok(Expr::Parameter(name))
            }
            Token::Count => {
                // `count(*)` or `count(DISTINCT ...)` or regular `count(x)`.
                self.advance();
                if *self.peek() != Token::LParen {
                    return Ok(Expr::Variable("count".into()));
                }
                self.expect(&Token::LParen)?;
                if *self.peek() == Token::Star {
                    self.advance();
                    self.expect(&Token::RParen)?;
                    return Ok(Expr::CountStar);
                }
                let distinct = if *self.peek() == Token::Distinct {
                    self.advance();
                    true
                } else {
                    false
                };
                let mut args = Vec::new();
                if *self.peek() != Token::RParen {
                    loop {
                        args.push(self.parse_or_expr()?);
                        if *self.peek() != Token::Comma {
                            break;
                        }
                        self.advance();
                    }
                }
                self.expect(&Token::RParen)?;
                if distinct {
                    args = args
                        .into_iter()
                        .map(|arg| Expr::FunctionCall {
                            name: "__distinct".into(),
                            args: vec![arg],
                        })
                        .collect();
                }
                Ok(Expr::FunctionCall {
                    name: "count".into(),
                    args,
                })
            }
            Token::Exists => {
                // EXISTS { MATCH ... } or EXISTS(...)
                self.advance();
                if *self.peek() == Token::LBrace {
                    Ok(Expr::ExistsSubquery(Box::new(
                        self.parse_exists_subquery()?,
                    )))
                } else if *self.peek() == Token::LParen {
                    self.advance();
                    let inner = self.parse_or_expr()?;
                    self.expect(&Token::RParen)?;
                    Ok(Expr::Exists(Box::new(inner)))
                } else {
                    Err(CypherError::Parse {
                        position: self.pos,
                        message: "expected '(' or '{' after EXISTS".into(),
                    })
                }
            }
            Token::Case => self.parse_case_expr(),
            // `ALL` is a hard keyword for UNION ALL, but in expression position
            // it's the list-predicate function `all(x IN xs WHERE pred)`.
            Token::All => {
                self.advance();
                if *self.peek() == Token::LParen {
                    return self.parse_predicate_function("all".to_string());
                }
                Ok(Expr::Variable("all".into()))
            }
            Token::LBracket => self.parse_list_literal_or_comprehension(),
            Token::LBrace => {
                // Map literal
                let entries = self.parse_property_map()?;
                Ok(Expr::Map(entries))
            }
            Token::Ident(name) => {
                let name = name.clone();
                self.advance();
                if *self.peek() == Token::Dot {
                    self.advance();
                    let prop = symbolic_name_from_token(self.advance()).ok_or_else(|| {
                        CypherError::Parse {
                            position: self.pos,
                            message: "expected identifier for property name".into(),
                        }
                    })?;
                    if *self.peek() == Token::LParen {
                        self.advance();
                        let args = self.parse_function_args_after_lparen()?;
                        return Ok(Expr::FunctionCall {
                            name: format!("{name}.{prop}"),
                            args,
                        });
                    }
                    Ok(Expr::Property(PropertyAccess {
                        variable: name,
                        property: prop,
                    }))
                } else if *self.peek() == Token::LParen {
                    // Predicate functions `any/all/none/single(x IN xs WHERE p)`.
                    let lname = name.to_lowercase();
                    if matches!(lname.as_str(), "any" | "all" | "none" | "single") {
                        return self.parse_predicate_function(name);
                    }
                    self.advance();
                    let args = self.parse_function_args_after_lparen()?;
                    Ok(Expr::FunctionCall { name, args })
                } else if *self.peek() == Token::Colon {
                    // Label-test expression: `n:Label[:Label2...]`.
                    let mut labels = Vec::new();
                    while *self.peek() == Token::Colon {
                        self.advance();
                        labels.push(label_name_from_token(self.advance()).ok_or_else(|| {
                            CypherError::Parse {
                                position: self.pos,
                                message: "expected label".into(),
                            }
                        })?);
                    }
                    Ok(Expr::FunctionCall {
                        name: "__label_test".into(),
                        args: std::iter::once(Expr::Variable(name))
                            .chain(
                                labels
                                    .into_iter()
                                    .map(|l| Expr::Literal(Literal::String(l))),
                            )
                            .collect(),
                    })
                } else {
                    Ok(Expr::Variable(name))
                }
            }
            Token::LParen => {
                if self.starts_pattern_predicate() {
                    return self.parse_pattern().map(Expr::PatternPredicate);
                }
                self.advance();
                let expr = self.parse_or_expr()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::Star => {
                self.advance();
                Ok(Expr::Variable("*".into()))
            }
            _ => Err(CypherError::Parse {
                position: self.pos,
                message: format!("unexpected token in expression: {:?}", self.peek()),
            }),
        }
    }

    fn parse_function_args_after_lparen(&mut self) -> CypherResult<Vec<Expr>> {
        let distinct = if *self.peek() == Token::Distinct {
            self.advance();
            true
        } else {
            false
        };
        let mut args = Vec::new();
        if *self.peek() != Token::RParen {
            loop {
                args.push(self.parse_or_expr()?);
                if *self.peek() != Token::Comma {
                    break;
                }
                self.advance();
            }
        }
        self.expect(&Token::RParen)?;
        if distinct {
            args = args
                .into_iter()
                .map(|arg| Expr::FunctionCall {
                    name: "__distinct".into(),
                    args: vec![arg],
                })
                .collect();
        }
        Ok(args)
    }

    fn parse_exists_subquery(&mut self) -> CypherResult<Query> {
        let tokens = self.take_balanced_brace_tokens()?;
        let mut parser = Parser { tokens, pos: 0 };

        match parser.peek() {
            Token::Match | Token::Optional | Token::With | Token::Unwind => {
                match parser.parse_statement()? {
                    Statement::Read(query) => Ok(query),
                    Statement::Write(_) => Err(CypherError::Parse {
                        position: parser.pos,
                        message: "write clauses are not allowed in existential subqueries".into(),
                    }),
                }
            }
            Token::LParen => {
                let match_clause = parser.parse_match_body()?;
                let where_clause = if *parser.peek() == Token::Where {
                    Some(parser.parse_where()?)
                } else {
                    None
                };
                parser.expect(&Token::Eof)?;
                Ok(Query {
                    match_clause: Some(match_clause),
                    where_clause,
                    tail: Vec::new(),
                    return_clause: true_return_clause(),
                    order_by: None,
                    limit: None,
                    skip: None,
                    union: None,
                })
            }
            _ => Err(CypherError::Parse {
                position: parser.pos,
                message: "expected MATCH, WITH, UNWIND, or a pattern in EXISTS subquery".into(),
            }),
        }
    }

    fn take_balanced_brace_tokens(&mut self) -> CypherResult<Vec<Token>> {
        self.expect(&Token::LBrace)?;
        let mut depth = 1usize;
        let mut body = Vec::new();
        while depth > 0 {
            match self.advance() {
                Token::LBrace => {
                    depth += 1;
                    body.push(Token::LBrace);
                }
                Token::RBrace => {
                    depth -= 1;
                    if depth > 0 {
                        body.push(Token::RBrace);
                    }
                }
                Token::Eof => {
                    return Err(CypherError::Parse {
                        position: self.pos,
                        message: "unterminated '{'".into(),
                    });
                }
                token => body.push(token),
            }
        }
        body.push(Token::Eof);
        Ok(body)
    }

    fn starts_pattern_predicate(&self) -> bool {
        if *self.peek() != Token::LParen {
            return false;
        }
        if matches!(
            self.tokens.get(self.pos + 1),
            Some(
                Token::Dash
                    | Token::Plus
                    | Token::IntLiteral(_)
                    | Token::IntLiteralMinMagnitude
                    | Token::FloatLiteral(_)
                    | Token::StringLiteral(_)
                    | Token::True
                    | Token::False
                    | Token::Null
            )
        ) {
            return false;
        }

        let mut depth = 0i32;
        for idx in self.pos..self.tokens.len() {
            match &self.tokens[idx] {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.tokens.get(idx + 1),
                            Some(Token::Dash | Token::ArrowLeftDash)
                        );
                    }
                }
                Token::Eof => return false,
                _ => {}
            }
        }
        false
    }

    /// Parse a predicate function: `any/all/none/single(x IN xs WHERE pred)`.
    /// Parsed-but-flattened: the parser accepts the form; execution is deferred.
    fn parse_predicate_function(&mut self, name: String) -> CypherResult<Expr> {
        self.expect(&Token::LParen)?;
        let var = self.expect_ident("predicate variable")?;
        self.expect(&Token::In)?;
        let list = self.parse_or_expr()?;
        let pred = if *self.peek() == Token::Where {
            self.advance();
            Some(Box::new(self.parse_or_expr()?))
        } else {
            None
        };
        self.expect(&Token::RParen)?;
        let kind = match name.to_lowercase().as_str() {
            "any" => ListPredicateKind::Any,
            "all" => ListPredicateKind::All,
            "none" => ListPredicateKind::None,
            "single" => ListPredicateKind::Single,
            _ => unreachable!("parse_predicate_function called with non-predicate name"),
        };
        Ok(Expr::ListPredicate {
            kind,
            variable: var,
            list: Box::new(list),
            predicate: pred,
        })
    }

    fn parse_case_expr(&mut self) -> CypherResult<Expr> {
        self.expect(&Token::Case)?;
        // Optional scrutinee: CASE expr WHEN ... vs CASE WHEN ...
        let scrutinee = if *self.peek() != Token::When {
            Some(Box::new(self.parse_or_expr()?))
        } else {
            None
        };
        let mut arms = Vec::new();
        while *self.peek() == Token::When {
            self.advance();
            let when_expr = self.parse_or_expr()?;
            self.expect(&Token::Then)?;
            let then_expr = self.parse_or_expr()?;
            arms.push((when_expr, then_expr));
        }
        let default = if *self.peek() == Token::Else {
            self.advance();
            Some(Box::new(self.parse_or_expr()?))
        } else {
            None
        };
        self.expect(&Token::End)?;
        Ok(Expr::Case {
            scrutinee,
            arms,
            default,
        })
    }

    fn parse_list_literal_or_comprehension(&mut self) -> CypherResult<Expr> {
        self.expect(&Token::LBracket)?;
        if *self.peek() == Token::RBracket {
            self.advance();
            return Ok(Expr::List(Vec::new()));
        }
        if self.starts_pattern_comprehension() {
            let variable = if matches!(self.peek(), Token::Ident(_))
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Eq))
            {
                let var = match self.advance() {
                    Token::Ident(name) => name,
                    other => {
                        return Err(CypherError::Parse {
                            position: self.pos,
                            message: format!(
                                "expected pattern-comprehension variable, got {other:?}"
                            ),
                        });
                    }
                };
                self.advance(); // =
                Some(var)
            } else {
                None
            };
            let mut pattern = self.parse_pattern()?;
            if pattern.path_variable.is_none() {
                pattern.path_variable = variable.clone();
            }
            self.expect(&Token::Pipe)?;
            let projection = self.parse_or_expr()?;
            self.expect(&Token::RBracket)?;
            return Ok(Expr::PatternComprehension {
                variable,
                pattern,
                projection: Box::new(projection),
            });
        }
        // Detect comprehension syntactically before the expression parser
        // can consume IN as a binary operator: `[ <ident> IN ... ]`.
        let is_comprehension = matches!(self.peek(), Token::Ident(_))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::In));

        if is_comprehension {
            let variable = match self.advance() {
                Token::Ident(name) => name,
                other => {
                    return Err(CypherError::Parse {
                        position: self.pos,
                        message: format!("expected list-comprehension variable, got {other:?}"),
                    });
                }
            };
            self.advance(); // IN
            let source = self.parse_or_expr()?;
            let mut predicate = None;
            if *self.peek() == Token::Where {
                self.advance();
                predicate = Some(Box::new(self.parse_or_expr()?));
            }
            let mut projection = None;
            if *self.peek() == Token::Pipe {
                self.advance();
                projection = Some(Box::new(self.parse_or_expr()?));
            }
            self.expect(&Token::RBracket)?;
            return Ok(Expr::ListComprehension {
                variable,
                list: Box::new(source),
                predicate,
                projection,
            });
        }

        let mut items = vec![self.parse_or_expr()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_or_expr()?);
        }
        self.expect(&Token::RBracket)?;
        Ok(Expr::List(items))
    }

    fn starts_pattern_comprehension(&self) -> bool {
        matches!(self.peek(), Token::LParen)
            || (matches!(self.peek(), Token::Ident(_))
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Eq))
                && matches!(self.tokens.get(self.pos + 2), Some(Token::LParen)))
    }

    fn parse_return(&mut self) -> CypherResult<ReturnClause> {
        self.expect(&Token::Return)?;

        let distinct = if *self.peek() == Token::Distinct {
            self.advance();
            true
        } else {
            false
        };

        let mut items = vec![self.parse_return_item()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_return_item()?);
        }
        Ok(ReturnClause { items, distinct })
    }

    fn parse_return_item(&mut self) -> CypherResult<ReturnItem> {
        let expr = self.parse_or_expr()?;
        let alias = if *self.peek() == Token::As {
            self.advance();
            Some(
                symbolic_name_from_token(self.advance()).ok_or_else(|| CypherError::Parse {
                    position: self.pos,
                    message: "expected alias".into(),
                })?,
            )
        } else {
            None
        };
        Ok(ReturnItem {
            expr,
            alias,
            raw: None,
        })
    }

    fn parse_order_by(&mut self) -> CypherResult<OrderByClause> {
        self.expect(&Token::Order)?;
        self.expect(&Token::By)?;
        let mut items = vec![self.parse_order_by_item()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_order_by_item()?);
        }
        Ok(OrderByClause { items })
    }

    fn parse_order_by_item(&mut self) -> CypherResult<OrderByItem> {
        let expr = self.parse_or_expr()?;
        let descending = if *self.peek() == Token::Desc {
            self.advance();
            true
        } else {
            if *self.peek() == Token::Asc {
                self.advance();
            }
            false
        };
        Ok(OrderByItem { expr, descending })
    }

    fn parse_integer(&mut self) -> CypherResult<u64> {
        if let Token::IntLiteral(v) = self.advance() {
            Ok(v as u64)
        } else {
            Err(CypherError::Parse {
                position: self.pos,
                message: "expected integer".into(),
            })
        }
    }

    fn parse_row_count(&mut self) -> CypherResult<RowCount> {
        match self.peek() {
            Token::IntLiteral(v) if *v >= 0 => {
                let v = *v;
                self.advance();
                Ok(RowCount::Literal(v as u64))
            }
            Token::Parameter(name) => {
                let name = name.clone();
                self.advance();
                Ok(RowCount::Parameter(name))
            }
            Token::IntLiteral(_) => Err(CypherError::Parse {
                position: self.pos,
                message: "expected non-negative integer".into(),
            }),
            _ => Ok(RowCount::Expr(Box::new(self.parse_or_expr()?))),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MergeActionTarget {
    Create,
    Match,
}

fn property_key_from_token(token: Token) -> Option<String> {
    match token {
        Token::Ident(value) => Some(value),
        Token::Exists => Some("exists".into()),
        Token::Match => Some("match".into()),
        Token::Where => Some("where".into()),
        Token::Return => Some("return".into()),
        Token::As => Some("as".into()),
        Token::Order => Some("order".into()),
        Token::By => Some("by".into()),
        Token::Limit => Some("limit".into()),
        Token::Skip => Some("skip".into()),
        Token::Distinct => Some("distinct".into()),
        Token::And => Some("and".into()),
        Token::Or => Some("or".into()),
        Token::Not => Some("not".into()),
        Token::Null => Some("null".into()),
        Token::True => Some("true".into()),
        Token::False => Some("false".into()),
        Token::Is => Some("is".into()),
        Token::Contains => Some("contains".into()),
        Token::StartsWith => Some("starts".into()),
        Token::EndsWith => Some("ends".into()),
        Token::Asc => Some("asc".into()),
        Token::Desc => Some("desc".into()),
        Token::Create => Some("create".into()),
        Token::Delete => Some("delete".into()),
        Token::Detach => Some("detach".into()),
        Token::Set => Some("set".into()),
        Token::Remove => Some("remove".into()),
        Token::Merge => Some("merge".into()),
        Token::On => Some("on".into()),
        Token::With => Some("with".into()),
        Token::Unwind => Some("unwind".into()),
        Token::Optional => Some("optional".into()),
        Token::In => Some("in".into()),
        Token::Case => Some("case".into()),
        Token::When => Some("when".into()),
        Token::Then => Some("then".into()),
        Token::Else => Some("else".into()),
        Token::End => Some("end".into()),
        Token::Union => Some("union".into()),
        Token::All => Some("all".into()),
        Token::Count => Some("count".into()),
        _ => None,
    }
}

fn symbolic_name_from_token(token: Token) -> Option<String> {
    match token {
        Token::Ident(value) => Some(value),
        Token::Match => Some("match".into()),
        Token::Where => Some("where".into()),
        Token::Return => Some("return".into()),
        Token::As => Some("as".into()),
        Token::Order => Some("order".into()),
        Token::By => Some("by".into()),
        Token::Limit => Some("limit".into()),
        Token::Skip => Some("skip".into()),
        Token::Distinct => Some("distinct".into()),
        Token::And => Some("and".into()),
        Token::Or => Some("or".into()),
        Token::Not => Some("not".into()),
        Token::Null => Some("null".into()),
        Token::True => Some("true".into()),
        Token::False => Some("false".into()),
        Token::Is => Some("is".into()),
        Token::Contains => Some("contains".into()),
        Token::StartsWith => Some("starts".into()),
        Token::EndsWith => Some("ends".into()),
        Token::Asc => Some("asc".into()),
        Token::Desc => Some("desc".into()),
        Token::Create => Some("create".into()),
        Token::Delete => Some("delete".into()),
        Token::Detach => Some("detach".into()),
        Token::Set => Some("set".into()),
        Token::Remove => Some("remove".into()),
        Token::Merge => Some("merge".into()),
        Token::On => Some("on".into()),
        Token::With => Some("with".into()),
        Token::Unwind => Some("unwind".into()),
        Token::Optional => Some("optional".into()),
        Token::In => Some("in".into()),
        Token::Case => Some("case".into()),
        Token::When => Some("when".into()),
        Token::Then => Some("then".into()),
        Token::Else => Some("else".into()),
        Token::End => Some("end".into()),
        Token::Union => Some("union".into()),
        Token::All => Some("all".into()),
        Token::Count => Some("count".into()),
        Token::Exists => Some("exists".into()),
        _ => None,
    }
}

fn label_name_from_token(token: Token) -> Option<String> {
    match token {
        Token::Ident(value) => Some(value),
        Token::End => Some("End".into()),
        Token::Match => Some("MATCH".into()),
        Token::Where => Some("WHERE".into()),
        Token::Return => Some("RETURN".into()),
        Token::As => Some("AS".into()),
        Token::Order => Some("ORDER".into()),
        Token::By => Some("BY".into()),
        Token::Limit => Some("LIMIT".into()),
        Token::Skip => Some("SKIP".into()),
        Token::Distinct => Some("DISTINCT".into()),
        Token::And => Some("AND".into()),
        Token::Or => Some("OR".into()),
        Token::Not => Some("NOT".into()),
        Token::Null => Some("NULL".into()),
        Token::True => Some("TRUE".into()),
        Token::False => Some("FALSE".into()),
        Token::Is => Some("IS".into()),
        Token::Contains => Some("CONTAINS".into()),
        Token::StartsWith => Some("STARTS".into()),
        Token::EndsWith => Some("ENDS".into()),
        Token::Asc => Some("ASC".into()),
        Token::Desc => Some("DESC".into()),
        Token::Create => Some("CREATE".into()),
        Token::Delete => Some("DELETE".into()),
        Token::Detach => Some("DETACH".into()),
        Token::Set => Some("SET".into()),
        Token::Remove => Some("REMOVE".into()),
        Token::Merge => Some("MERGE".into()),
        Token::On => Some("ON".into()),
        Token::With => Some("WITH".into()),
        Token::Unwind => Some("UNWIND".into()),
        Token::Optional => Some("OPTIONAL".into()),
        Token::In => Some("IN".into()),
        Token::Case => Some("CASE".into()),
        Token::When => Some("WHEN".into()),
        Token::Then => Some("THEN".into()),
        Token::Else => Some("ELSE".into()),
        Token::Union => Some("UNION".into()),
        Token::All => Some("ALL".into()),
        Token::Count => Some("COUNT".into()),
        Token::Exists => Some("EXISTS".into()),
        _ => None,
    }
}

fn true_return_clause() -> ReturnClause {
    ReturnClause {
        items: vec![ReturnItem {
            expr: Expr::Literal(Literal::Bool(true)),
            alias: None,
            raw: Some("true".into()),
        }],
        distinct: false,
    }
}

fn annotate_projection_sources(statement: &mut Statement, input: &str) {
    let mut sources = extract_projection_sources(input).into_iter();
    match statement {
        Statement::Read(query) => annotate_query_projection_sources(query, &mut sources),
        Statement::Write(query) => annotate_write_projection_sources(query, &mut sources),
    }
}

fn annotate_query_projection_sources(
    query: &mut Query,
    sources: &mut impl Iterator<Item = Vec<String>>,
) {
    for clause in &mut query.tail {
        if let ReadClause::With(with_clause) = clause {
            if let Some(raw_items) = sources.next() {
                apply_raw_projection_items(&mut with_clause.items, raw_items);
            }
        }
    }
    if let Some(raw_items) = sources.next() {
        apply_raw_projection_items(&mut query.return_clause.items, raw_items);
    }
    if let Some(union) = &mut query.union {
        annotate_query_projection_sources(&mut union.right, sources);
    }
}

fn annotate_write_projection_sources(
    query: &mut WriteQuery,
    sources: &mut impl Iterator<Item = Vec<String>>,
) {
    for clause in &mut query.tail {
        if let ReadClause::With(with_clause) = clause {
            if let Some(raw_items) = sources.next() {
                apply_raw_projection_items(&mut with_clause.items, raw_items);
            }
        }
    }
    for mutation in &mut query.mutations {
        if let MutationClause::Read(ReadClause::With(with_clause)) = mutation {
            if let Some(raw_items) = sources.next() {
                apply_raw_projection_items(&mut with_clause.items, raw_items);
            }
        }
    }
    if let Some(return_clause) = &mut query.return_clause {
        if let Some(raw_items) = sources.next() {
            apply_raw_projection_items(&mut return_clause.items, raw_items);
        }
    }
}

fn apply_raw_projection_items(items: &mut [ReturnItem], raw_items: Vec<String>) {
    if raw_items.len() != items.len() {
        return;
    }
    for (item, raw) in items.iter_mut().zip(raw_items) {
        if item.alias.is_none() && !raw.is_empty() {
            item.raw = Some(raw);
        }
    }
}

fn extract_projection_sources(input: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    while let Some((keyword_start, keyword_end)) = find_next_projection_keyword(input, idx) {
        let mut segment_start = skip_ascii_ws(input, keyword_end);
        if let Some(end) = keyword_end_at(input, segment_start, "DISTINCT") {
            segment_start = skip_ascii_ws(input, end);
        }

        let segment_end = find_projection_end(input, segment_start);
        let raw_items = split_top_level_commas(&input[segment_start..segment_end])
            .into_iter()
            .map(|item| strip_projection_alias(item).trim().to_string())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        out.push(raw_items);
        idx = segment_end.max(keyword_start + 1);
    }
    out
}

fn find_next_projection_keyword(input: &str, from: usize) -> Option<(usize, usize)> {
    scan_top_level_keywords(input, from, &["RETURN", "WITH"])
        .into_iter()
        .next()
}

fn find_projection_end(input: &str, from: usize) -> usize {
    scan_top_level_keywords(
        input,
        from,
        &[
            "MATCH", "OPTIONAL", "WHERE", "WITH", "UNWIND", "RETURN", "ORDER", "SKIP", "LIMIT",
            "UNION", "CREATE", "MERGE", "SET", "REMOVE", "DELETE", "DETACH", "ON",
        ],
    )
    .into_iter()
    .next()
    .map(|(start, _)| start)
    .unwrap_or(input.len())
}

fn scan_top_level_keywords(input: &str, from: usize, keywords: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut iter = input.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if idx < from {
            continue;
        }
        if in_string {
            if ch == '\'' {
                if iter.peek().is_some_and(|(_, next)| *next == '\'') {
                    iter.next();
                } else {
                    in_string = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => {
                in_string = true;
                continue;
            }
            '(' | '[' | '{' => {
                depth += 1;
                continue;
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ => {}
        }
        if depth != 0 {
            continue;
        }
        for keyword in keywords {
            if let Some(end) = keyword_end_at(input, idx, keyword) {
                if keyword.eq_ignore_ascii_case("WITH")
                    && previous_word(input, idx).is_some_and(|word| {
                        word.eq_ignore_ascii_case("STARTS") || word.eq_ignore_ascii_case("ENDS")
                    })
                {
                    continue;
                }
                out.push((idx, end));
                break;
            }
        }
    }
    out
}

fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut iter = input.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if in_string {
            if ch == '\'' {
                if iter.peek().is_some_and(|(_, next)| *next == '\'') {
                    iter.next();
                } else {
                    in_string = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(input[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    items.push(input[start..].trim());
    items
}

fn strip_projection_alias(input: &str) -> &str {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut iter = input.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        if in_string {
            if ch == '\'' {
                if iter.peek().is_some_and(|(_, next)| *next == '\'') {
                    iter.next();
                } else {
                    in_string = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                if keyword_end_at(input, idx, "AS").is_some() {
                    return &input[..idx];
                }
            }
            _ => {}
        }
    }
    input
}

fn document_scan_expr(collection: Expr) -> Expr {
    Expr::FunctionCall {
        name: "documents".into(),
        args: vec![collection, Expr::Literal(Literal::Integer(1000))],
    }
}

struct DocumentIndexPushdown {
    expr: Expr,
    keep_where: bool,
}

fn document_index_pushdown(
    alias: &str,
    collection: &Expr,
    expr: &Expr,
) -> Option<DocumentIndexPushdown> {
    if !document_collection_is_static(collection) {
        return None;
    }
    if let Expr::FunctionCall { name, args } = expr
        && name.eq_ignore_ascii_case("documentFullText")
        && args.len() == 2
        && let Some(path) = document_path_from_expr(alias, &args[0])
    {
        return Some(documents_full_text_expr(
            collection.clone(),
            path,
            args[1].clone(),
            false,
        ));
    }
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    match op {
        BinaryOp::Eq => document_path_from_expr(alias, left)
            .map(|path| documents_by_expr(collection.clone(), path, (**right).clone(), false))
            .or_else(|| {
                document_path_from_expr(alias, right).map(|path| {
                    documents_by_expr(collection.clone(), path, (**left).clone(), false)
                })
            }),
        BinaryOp::StartsWith => document_path_from_expr(alias, left)
            .map(|path| documents_prefix_expr(collection.clone(), path, (**right).clone(), false)),
        BinaryOp::Gt | BinaryOp::Gte | BinaryOp::Lt | BinaryOp::Lte => {
            document_range_pushdown(alias, collection, left, *op, right)
        }
        _ => None,
    }
}

fn documents_by_expr(
    collection: Expr,
    path: String,
    value: Expr,
    keep_where: bool,
) -> DocumentIndexPushdown {
    DocumentIndexPushdown {
        expr: Expr::FunctionCall {
            name: "documentsBy".into(),
            args: vec![
                collection,
                Expr::Literal(Literal::String(path)),
                value,
                Expr::Literal(Literal::Integer(1000)),
            ],
        },
        keep_where,
    }
}

fn documents_prefix_expr(
    collection: Expr,
    path: String,
    prefix: Expr,
    keep_where: bool,
) -> DocumentIndexPushdown {
    DocumentIndexPushdown {
        expr: Expr::FunctionCall {
            name: "documentsPrefix".into(),
            args: vec![
                collection,
                Expr::Literal(Literal::String(path)),
                prefix,
                Expr::Literal(Literal::Integer(1000)),
            ],
        },
        keep_where,
    }
}

fn document_range_pushdown(
    alias: &str,
    collection: &Expr,
    left: &Expr,
    op: BinaryOp,
    right: &Expr,
) -> Option<DocumentIndexPushdown> {
    if let Some(path) = document_path_from_expr(alias, left) {
        let (gte, lte) = match op {
            BinaryOp::Gt | BinaryOp::Gte => (right.clone(), Expr::Literal(Literal::Null)),
            BinaryOp::Lt | BinaryOp::Lte => (Expr::Literal(Literal::Null), right.clone()),
            _ => return None,
        };
        return Some(documents_range_expr(
            collection.clone(),
            path,
            gte,
            lte,
            true,
        ));
    }
    if let Some(path) = document_path_from_expr(alias, right) {
        let (gte, lte) = match op {
            BinaryOp::Gt | BinaryOp::Gte => (Expr::Literal(Literal::Null), left.clone()),
            BinaryOp::Lt | BinaryOp::Lte => (left.clone(), Expr::Literal(Literal::Null)),
            _ => return None,
        };
        return Some(documents_range_expr(
            collection.clone(),
            path,
            gte,
            lte,
            true,
        ));
    }
    None
}

fn documents_range_expr(
    collection: Expr,
    path: String,
    gte: Expr,
    lte: Expr,
    keep_where: bool,
) -> DocumentIndexPushdown {
    DocumentIndexPushdown {
        expr: Expr::FunctionCall {
            name: "documentsRange".into(),
            args: vec![
                collection,
                Expr::Literal(Literal::String(path)),
                gte,
                lte,
                Expr::Literal(Literal::Integer(1000)),
            ],
        },
        keep_where,
    }
}

fn documents_full_text_expr(
    collection: Expr,
    path: String,
    query: Expr,
    keep_where: bool,
) -> DocumentIndexPushdown {
    DocumentIndexPushdown {
        expr: Expr::FunctionCall {
            name: "documentsFullText".into(),
            args: vec![
                collection,
                Expr::Literal(Literal::String(path)),
                query,
                Expr::Literal(Literal::Integer(1000)),
            ],
        },
        keep_where,
    }
}

fn document_collection_is_static(collection: &Expr) -> bool {
    matches!(
        collection,
        Expr::Literal(Literal::String(_)) | Expr::Parameter(_)
    )
}

fn document_path_from_expr(alias: &str, expr: &Expr) -> Option<String> {
    let mut segments = Vec::new();
    let mut current = expr;
    loop {
        match current {
            Expr::Index { target, index } => {
                let Expr::Literal(Literal::String(segment)) = index.as_ref() else {
                    return None;
                };
                segments.push(segment.clone());
                current = target;
            }
            Expr::Property(PropertyAccess { variable, property }) if variable == alias => {
                if property != "document" || segments.is_empty() {
                    return None;
                }
                segments.reverse();
                return Some(segments.join("."));
            }
            _ => return None,
        }
    }
}

fn keyword_end_at(input: &str, idx: usize, keyword: &str) -> Option<usize> {
    let end = idx.checked_add(keyword.len())?;
    let slice = input.get(idx..end)?;
    if !slice.eq_ignore_ascii_case(keyword) {
        return None;
    }
    if input[..idx].chars().next_back().is_some_and(is_ident_char) {
        return None;
    }
    if input[end..].chars().next().is_some_and(is_ident_char) {
        return None;
    }
    Some(end)
}

fn previous_word(input: &str, idx: usize) -> Option<&str> {
    let prefix = input.get(..idx)?.trim_end();
    let end = prefix.len();
    let start = prefix
        .char_indices()
        .rev()
        .find_map(|(pos, ch)| (!is_ident_char(ch)).then_some(pos + ch.len_utf8()))
        .unwrap_or(0);
    (start < end).then(|| &prefix[start..end])
}

fn skip_ascii_ws(input: &str, mut idx: usize) -> usize {
    while input[idx..]
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_whitespace())
    {
        idx += input[idx..].chars().next().unwrap().len_utf8();
    }
    idx
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_match_return() {
        let q = Parser::parse_read("MATCH (n:Entity) RETURN n").unwrap();
        assert!(q.match_clause.is_some());
        let m = q.match_clause.unwrap();
        assert_eq!(m.patterns.len(), 1);
        assert_eq!(m.patterns[0].elements.len(), 1);
        if let PatternElement::Node(ref np) = m.patterns[0].elements[0] {
            assert_eq!(np.variable, Some("n".into()));
            assert_eq!(np.labels, vec!["Entity"]);
        } else {
            panic!("expected node pattern");
        }
    }

    #[test]
    fn parse_document_match_scan_sugar() {
        let q = Parser::parse_read(
            "MATCH DOCUMENT doc IN filings WHERE doc.document.ticker = 'NVDA' RETURN doc.key",
        )
        .unwrap();
        assert!(q.match_clause.is_none());
        assert_eq!(q.tail.len(), 1);
        let ReadClause::Unwind { expr, alias } = &q.tail[0] else {
            panic!("expected document MATCH to lower to UNWIND");
        };
        assert_eq!(alias, "doc");
        let Expr::FunctionCall { name, args } = expr else {
            panic!("expected documentsBy(...) function");
        };
        assert_eq!(name, "documentsBy");
        assert_eq!(args.len(), 4);
        assert!(matches!(
            &args[0],
            Expr::Literal(Literal::String(collection)) if collection == "filings"
        ));
        assert!(matches!(
            &args[1],
            Expr::Literal(Literal::String(path)) if path == "ticker"
        ));
    }

    #[test]
    fn parse_document_match_scan_sugar_uses_range_pushdown_with_filter() {
        let q = Parser::parse_read(
            "MATCH DOCUMENT doc IN filings WHERE doc.document.year >= 2024 RETURN doc.key",
        )
        .unwrap();
        assert!(q.match_clause.is_none());
        assert_eq!(q.tail.len(), 2);
        let ReadClause::Unwind { expr, alias } = &q.tail[0] else {
            panic!("expected document MATCH to lower to UNWIND");
        };
        assert_eq!(alias, "doc");
        let Expr::FunctionCall { name, args } = expr else {
            panic!("expected documents(...) function");
        };
        assert_eq!(name, "documentsRange");
        assert_eq!(args.len(), 5);
        assert!(matches!(
            &args[1],
            Expr::Literal(Literal::String(path)) if path == "year"
        ));
        assert!(matches!(&q.tail[1], ReadClause::Where(_)));
    }

    #[test]
    fn parse_document_match_scan_sugar_uses_prefix_pushdown() {
        let q = Parser::parse_read(
            "MATCH DOCUMENT doc IN filings WHERE doc.document.ticker STARTS WITH 'NV' RETURN doc.key",
        )
        .unwrap();
        assert!(q.match_clause.is_none());
        assert_eq!(q.tail.len(), 1);
        let ReadClause::Unwind { expr, alias } = &q.tail[0] else {
            panic!("expected document MATCH to lower to UNWIND");
        };
        assert_eq!(alias, "doc");
        let Expr::FunctionCall { name, args } = expr else {
            panic!("expected documentsPrefix(...) function");
        };
        assert_eq!(name, "documentsPrefix");
        assert_eq!(args.len(), 4);
        assert!(matches!(
            &args[1],
            Expr::Literal(Literal::String(path)) if path == "ticker"
        ));
    }

    #[test]
    fn parse_document_match_scan_sugar_uses_full_text_pushdown() {
        let q = Parser::parse_read(
            "MATCH DOCUMENT doc IN filings WHERE documentFullText(doc.document.body, 'revenue risk') RETURN doc.key",
        )
        .unwrap();
        assert!(q.match_clause.is_none());
        assert_eq!(q.tail.len(), 1);
        let ReadClause::Unwind { expr, alias } = &q.tail[0] else {
            panic!("expected document MATCH to lower to UNWIND");
        };
        assert_eq!(alias, "doc");
        let Expr::FunctionCall { name, args } = expr else {
            panic!("expected documentsFullText(...) function");
        };
        assert_eq!(name, "documentsFullText");
        assert_eq!(args.len(), 4);
        assert!(matches!(
            &args[1],
            Expr::Literal(Literal::String(path)) if path == "body"
        ));
    }

    #[test]
    fn parse_document_match_scan_sugar_does_not_push_contains() {
        let q = Parser::parse_read(
            "MATCH DOCUMENT doc IN filings WHERE doc.document.body CONTAINS 'venue' RETURN doc.key",
        )
        .unwrap();
        assert!(q.match_clause.is_none());
        assert_eq!(q.tail.len(), 2);
        let ReadClause::Unwind { expr, alias } = &q.tail[0] else {
            panic!("expected document MATCH to lower to UNWIND");
        };
        assert_eq!(alias, "doc");
        let Expr::FunctionCall { name, .. } = expr else {
            panic!("expected documents(...) function");
        };
        assert_eq!(name, "documents");
        assert!(matches!(&q.tail[1], ReadClause::Where(_)));
    }

    #[test]
    fn parse_match_with_relationship() {
        let q =
            Parser::parse_read("MATCH (a:Entity)-[:DISCLOSES]->(b:Metric) RETURN a, b").unwrap();
        let m = q.match_clause.unwrap();
        assert_eq!(m.patterns[0].elements.len(), 3); // node, rel, node
        if let PatternElement::Relationship(ref rp) = m.patterns[0].elements[1] {
            assert_eq!(rp.rel_types, vec!["DISCLOSES"]);
            assert_eq!(rp.direction, RelDirection::Outgoing);
        } else {
            panic!("expected relationship");
        }
    }

    #[test]
    fn parse_chained_bidirectional_relationship_after_node() {
        let q = Parser::parse_read("MATCH (a)-->(x)<-->(b) RETURN x").unwrap();
        let m = q.match_clause.unwrap();
        assert_eq!(m.patterns[0].elements.len(), 5);
        let PatternElement::Relationship(rel) = &m.patterns[0].elements[3] else {
            panic!("expected second relationship");
        };
        assert_eq!(rel.direction, RelDirection::Both);
    }

    #[test]
    fn parse_where_clause() {
        let q = Parser::parse_read(
            "MATCH (n:Entity) WHERE n.name = 'Apple' AND n.revenue > 100 RETURN n",
        )
        .unwrap();
        assert!(q.where_clause.is_some());
        if let Expr::BinaryOp { op, .. } = &q.where_clause.unwrap().expr {
            assert_eq!(*op, BinaryOp::And);
        } else {
            panic!("expected AND");
        }
    }

    #[test]
    fn parse_limit_and_skip() {
        let q = Parser::parse_read("MATCH (n) RETURN n SKIP 10 LIMIT 25").unwrap();
        assert_eq!(q.skip, Some(RowCount::Literal(10)));
        assert_eq!(q.limit, Some(RowCount::Literal(25)));
    }

    #[test]
    fn parse_variable_length_path() {
        let q = Parser::parse_read("MATCH (a:Entity)-[:DISCLOSES*1..3]->(b) RETURN b").unwrap();
        let m = q.match_clause.unwrap();
        if let PatternElement::Relationship(ref rp) = m.patterns[0].elements[1] {
            assert_eq!(rp.min_hops, Some(1));
            assert_eq!(rp.max_hops, Some(3));
        } else {
            panic!("expected relationship");
        }
    }

    #[test]
    fn parse_count_function() {
        let q = Parser::parse_read("MATCH (n:Entity) RETURN count(n)").unwrap();
        if let Expr::FunctionCall { name, args } = &q.return_clause.items[0].expr {
            assert_eq!(name, "count");
            assert_eq!(args.len(), 1);
        } else {
            panic!("expected function call");
        }
    }

    #[test]
    fn parse_in_binds_tighter_than_comparison() {
        let q = Parser::parse_read(
            "RETURN false = true IN [true, false] AS a, false = (true IN [true, false]) AS b, (false = true) IN [true, false] AS c",
        )
        .unwrap();
        assert_eq!(q.return_clause.items.len(), 3);

        match &q.return_clause.items[0].expr {
            Expr::BinaryOp { op, right, .. } => {
                assert_eq!(*op, BinaryOp::Eq);
                assert!(matches!(right.as_ref(), Expr::In { .. }));
            }
            other => panic!("expected comparison with IN on RHS, got {other:?}"),
        }

        assert!(matches!(q.return_clause.items[2].expr, Expr::In { .. }));
    }

    #[test]
    fn parse_and_binds_tighter_than_xor() {
        let q = Parser::parse_read("RETURN true XOR false AND false AS a").unwrap();
        match &q.return_clause.items[0].expr {
            Expr::BinaryOp {
                op: BinaryOp::Xor,
                right,
                ..
            } => {
                assert!(matches!(
                    right.as_ref(),
                    Expr::BinaryOp {
                        op: BinaryOp::And,
                        ..
                    }
                ));
            }
            other => panic!("expected XOR over AND, got {other:?}"),
        }
    }

    #[test]
    fn parse_list_comprehension() {
        let q = Parser::parse_read("RETURN [x IN [1, 2, 3] WHERE x > 1 | x + 1] AS xs").unwrap();
        match &q.return_clause.items[0].expr {
            Expr::ListComprehension {
                variable,
                predicate,
                projection,
                ..
            } => {
                assert_eq!(variable, "x");
                assert!(predicate.is_some());
                assert!(projection.is_some());
            }
            other => panic!("expected list comprehension, got {other:?}"),
        }
    }

    #[test]
    fn parse_pattern_predicate() {
        let q = Parser::parse_read("MATCH (n) WHERE (n)-[:REL]->() RETURN n").unwrap();
        match &q.where_clause.unwrap().expr {
            Expr::PatternPredicate(pattern) => {
                assert_eq!(pattern.elements.len(), 3);
                let PatternElement::Relationship(rel) = &pattern.elements[1] else {
                    panic!("expected relationship");
                };
                assert_eq!(rel.rel_types, vec!["REL"]);
            }
            other => panic!("expected pattern predicate, got {other:?}"),
        }
    }

    #[test]
    fn parse_pattern_comprehension() {
        let q = Parser::parse_read("MATCH (n) RETURN [p = (n)-[:REL]->() | p] AS paths").unwrap();
        match &q.return_clause.items[0].expr {
            Expr::PatternComprehension {
                variable,
                pattern,
                projection,
            } => {
                assert_eq!(variable.as_deref(), Some("p"));
                assert_eq!(pattern.path_variable.as_deref(), Some("p"));
                assert_eq!(pattern.elements.len(), 3);
                assert!(matches!(projection.as_ref(), Expr::Variable(name) if name == "p"));
            }
            other => panic!("expected pattern comprehension, got {other:?}"),
        }
    }

    #[test]
    fn parse_order_by_desc() {
        let q = Parser::parse_read("MATCH (n:Entity) RETURN n.name ORDER BY n.name DESC").unwrap();
        let ob = q.order_by.unwrap();
        assert!(ob.items[0].descending);
    }

    #[test]
    fn parse_starts_with_and_ends_with() {
        let starts =
            Parser::parse_read("MATCH (n:Entity) WHERE n.name STARTS WITH 'A' RETURN n").unwrap();
        assert!(starts.where_clause.is_some());

        let ends =
            Parser::parse_read("MATCH (n:Entity) WHERE n.name ENDS WITH 'Inc.' RETURN n").unwrap();
        assert!(ends.where_clause.is_some());
    }

    #[test]
    fn parse_property_filter_in_node() {
        let q = Parser::parse_read("MATCH (n:Entity {external_id: 'AAPL:Apple:ORG'}) RETURN n")
            .unwrap();
        let m = q.match_clause.unwrap();
        if let PatternElement::Node(ref np) = m.patterns[0].elements[0] {
            assert_eq!(np.properties.len(), 1);
            assert_eq!(np.properties[0].0, "external_id");
        } else {
            panic!("expected node");
        }
    }

    #[test]
    fn parse_create_single_node() {
        let stmt = Parser::parse("CREATE (n:Entity {name: 'Apple'})").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write statement");
        };
        assert!(wq.match_clause.is_none());
        assert_eq!(wq.mutations.len(), 1);
        let MutationClause::Create { patterns } = &wq.mutations[0] else {
            panic!("expected CREATE clause");
        };
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].elements.len(), 1);
        if let PatternElement::Node(np) = &patterns[0].elements[0] {
            assert_eq!(np.variable, Some("n".into()));
            assert_eq!(np.labels, vec!["Entity"]);
            assert_eq!(np.properties.len(), 1);
        } else {
            panic!("expected node pattern");
        }
    }

    #[test]
    fn parse_create_relationship() {
        let stmt = Parser::parse("CREATE (a:Entity {name: 'A'})-[:KNOWS]->(b:Entity {name: 'B'})")
            .unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Create { patterns } = &wq.mutations[0] else {
            panic!("expected CREATE");
        };
        assert_eq!(patterns[0].elements.len(), 3); // node, rel, node
    }

    #[test]
    fn parse_create_multiple_patterns() {
        let stmt = Parser::parse("CREATE (a:X), (b:Y), (c:Z)").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Create { patterns } = &wq.mutations[0] else {
            panic!("expected CREATE");
        };
        assert_eq!(patterns.len(), 3);
    }

    #[test]
    fn parse_match_delete() {
        let stmt = Parser::parse("MATCH (n:Entity) WHERE n.name = 'Apple' DELETE n").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        assert!(wq.match_clause.is_some());
        assert!(wq.where_clause.is_some());
        assert_eq!(wq.mutations.len(), 1);
        let MutationClause::Delete { targets, detach } = &wq.mutations[0] else {
            panic!("expected DELETE");
        };
        assert!(matches!(&targets[0], Expr::Variable(name) if name == "n"));
        assert!(!detach);
    }

    #[test]
    fn parse_match_detach_delete() {
        let stmt = Parser::parse("MATCH (n:Entity) DETACH DELETE n").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Delete { detach, .. } = &wq.mutations[0] else {
            panic!("expected DELETE");
        };
        assert!(detach);
    }

    #[test]
    fn parse_delete_multiple_targets() {
        let stmt = Parser::parse("MATCH (a), (b) DETACH DELETE a, b").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Delete { targets, .. } = &wq.mutations[0] else {
            panic!("expected DELETE");
        };
        assert!(matches!(&targets[0], Expr::Variable(name) if name == "a"));
        assert!(matches!(&targets[1], Expr::Variable(name) if name == "b"));
    }

    #[test]
    fn parse_match_set_property() {
        let stmt = Parser::parse("MATCH (n:Person) SET n.name = 'Bob'").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Set { items } = &wq.mutations[0] else {
            panic!("expected SET");
        };
        let SetItem::Property { target, .. } = &items[0] else {
            panic!("expected property SET, got label");
        };
        assert_eq!(target.variable, "n");
        assert_eq!(target.property, "name");
    }

    #[test]
    fn parse_match_remove_property() {
        let stmt = Parser::parse("MATCH (n:Person) REMOVE n.name").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Remove { items } = &wq.mutations[0] else {
            panic!("expected REMOVE");
        };
        let RemoveItem::Property(pa) = &items[0] else {
            panic!("expected property REMOVE, got label");
        };
        assert_eq!(pa.variable, "n");
        assert_eq!(pa.property, "name");
    }

    #[test]
    fn parse_set_paren_property_target() {
        // `SET (n).name = ...` — parenthesized variable as property target.
        let stmt = Parser::parse("MATCH (n) SET (n).name = 'x'").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Set { items } = &wq.mutations[0] else {
            panic!("expected SET");
        };
        let SetItem::Property { target, .. } = &items[0] else {
            panic!("expected property SET");
        };
        assert_eq!(target.variable, "n");
        assert_eq!(target.property, "name");
    }

    #[test]
    fn parse_set_labels() {
        let stmt = Parser::parse("MATCH (n) SET n:Foo:Bar").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Set { items } = &wq.mutations[0] else {
            panic!("expected SET");
        };
        let SetItem::Labels { variable, labels } = &items[0] else {
            panic!("expected label SET, got {:?}", items[0]);
        };
        assert_eq!(variable, "n");
        assert_eq!(labels, &vec!["Foo".to_string(), "Bar".to_string()]);
    }

    #[test]
    fn parse_remove_labels() {
        let stmt = Parser::parse("MATCH (n) REMOVE n:Foo:Bar").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Remove { items } = &wq.mutations[0] else {
            panic!("expected REMOVE");
        };
        let RemoveItem::Labels { variable, labels } = &items[0] else {
            panic!("expected label REMOVE");
        };
        assert_eq!(variable, "n");
        assert_eq!(labels, &vec!["Foo".to_string(), "Bar".to_string()]);
    }

    #[test]
    fn parse_merge_node() {
        let stmt = Parser::parse("MERGE (n:Person {name: 'Alice'})").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Merge { patterns, .. } = &wq.mutations[0] else {
            panic!("expected MERGE");
        };
        assert_eq!(patterns.len(), 1);
    }

    #[test]
    fn parse_merge_on_create_and_on_match_actions() {
        let stmt = Parser::parse(
            "MERGE (n:Person {name: 'Alice'}) ON CREATE SET n.created = true ON MATCH SET n.seen = true RETURN n.name",
        )
        .unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Merge {
            patterns,
            on_create,
            on_match,
        } = &wq.mutations[0]
        else {
            panic!("expected MERGE");
        };
        assert_eq!(patterns.len(), 1);
        assert_eq!(on_create.len(), 1);
        assert_eq!(on_match.len(), 1);
        assert!(wq.return_clause.is_some());
    }

    #[test]
    fn parse_relationship_delete_target() {
        let stmt = Parser::parse("MATCH ()-[r:KNOWS]->() DELETE r").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let m = wq.match_clause.unwrap();
        let PatternElement::Relationship(rp) = &m.patterns[0].elements[1] else {
            panic!("expected relationship");
        };
        assert_eq!(rp.variable.as_deref(), Some("r"));
    }

    #[test]
    fn parse_read_rejects_write_statement() {
        assert!(Parser::parse_read("CREATE (n:X)").is_err());
        assert!(Parser::parse_read("MATCH (n) DELETE n").is_err());
    }

    #[test]
    fn parse_rejects_trailing_unsupported_clause_instead_of_ignoring_it() {
        let err = Parser::parse("CREATE (:Entity {name: 'partial'}) CALL db.labels()")
            .expect_err("trailing CALL must not be ignored");
        assert!(
            err.to_string().contains("expected Eof") || err.to_string().contains("Unexpected"),
            "unexpected error: {err}"
        );
    }
}
