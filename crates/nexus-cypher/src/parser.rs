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
        parser.parse_statement()
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
            return Ok(Statement::Write(WriteQuery {
                match_clause,
                where_clause,
                mutations,
                return_clause,
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
            Some(self.parse_integer()?)
        } else {
            None
        };

        let limit = if *self.peek() == Token::Limit {
            self.advance();
            Some(self.parse_integer()?)
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
            Some(self.parse_integer()?)
        } else {
            None
        };

        let limit = if *self.peek() == Token::Limit {
            self.advance();
            Some(self.parse_integer()?)
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
                    mutations.push(MutationClause::Merge { patterns });
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
                    let vars = self.parse_delete_targets()?;
                    mutations.push(MutationClause::Delete {
                        variables: vars,
                        detach: true,
                    });
                }
                Token::Delete => {
                    self.advance();
                    let vars = self.parse_delete_targets()?;
                    mutations.push(MutationClause::Delete {
                        variables: vars,
                        detach: false,
                    });
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

    fn parse_set_item(&mut self) -> CypherResult<SetItem> {
        let target = self.parse_property_access_target()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_or_expr()?;
        Ok(SetItem { target, value })
    }

    fn parse_remove_item(&mut self) -> CypherResult<RemoveItem> {
        Ok(RemoveItem::Property(self.parse_property_access_target()?))
    }

    fn parse_property_access_target(&mut self) -> CypherResult<PropertyAccess> {
        match self.parse_primary()? {
            Expr::Property(pa) => Ok(pa),
            other => Err(CypherError::Parse {
                position: self.pos,
                message: format!("expected property access, got {other:?}"),
            }),
        }
    }

    fn parse_delete_targets(&mut self) -> CypherResult<Vec<String>> {
        let mut vars = Vec::new();
        match self.advance() {
            Token::Ident(name) => vars.push(name),
            other => {
                return Err(CypherError::UnexpectedToken {
                    expected: "identifier".into(),
                    got: format!("{other:?}"),
                });
            }
        }
        while *self.peek() == Token::Comma {
            self.advance();
            match self.advance() {
                Token::Ident(name) => vars.push(name),
                other => {
                    return Err(CypherError::UnexpectedToken {
                        expected: "identifier".into(),
                        got: format!("{other:?}"),
                    });
                }
            }
        }
        Ok(vars)
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

    fn parse_pattern(&mut self) -> CypherResult<Pattern> {
        // Optional path binding: `p = (a)-[:R]->(b)`. Consumed and discarded
        // for now — the path variable isn't modelled in the AST yet.
        if let Token::Ident(_) = self.peek() {
            if let Some(Token::Eq) = self.tokens.get(self.pos + 1) {
                self.advance(); // path var
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
        Ok(Pattern { elements })
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
            if let Token::Ident(label) = self.advance() {
                labels.push(label);
            } else {
                return Err(CypherError::Parse {
                    position: self.pos,
                    message: "expected label name after ':'".into(),
                });
            }
        }

        let properties = if *self.peek() == Token::LBrace {
            self.parse_property_map()?
        } else {
            Vec::new()
        };

        self.expect(&Token::RParen)?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
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
                    if let Token::Ident(t) = self.advance() {
                        types.push(t);
                    }
                    while *self.peek() == Token::Pipe {
                        self.advance();
                        if *self.peek() == Token::Colon {
                            self.advance();
                        }
                        if let Token::Ident(t) = self.advance() {
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
            if *self.peek() == Token::Dash {
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
                let key = if let Token::Ident(k) = self.advance() {
                    k
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
        let mut left = self.parse_and_expr()?;
        while *self.peek() == Token::Or {
            self.advance();
            let right = self.parse_and_expr()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Or,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_xor_expr()?;
        while *self.peek() == Token::And {
            self.advance();
            let right = self.parse_xor_expr()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::And,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_xor_expr(&mut self) -> CypherResult<Expr> {
        let mut left = self.parse_comparison()?;
        loop {
            let is_xor = matches!(
                self.peek(),
                Token::Ident(s) if s.eq_ignore_ascii_case("XOR")
            );
            if !is_xor {
                break;
            }
            self.advance();
            let right = self.parse_comparison()?;
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

        let left = self.parse_is_null_postfix()?;

        // IN operator (right-hand side must be list-like expression)
        if *self.peek() == Token::In {
            self.advance();
            let list = self.parse_is_null_postfix()?;
            return Ok(Expr::In {
                expr: Box::new(left),
                list: Box::new(list),
            });
        }

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
                // Unreachable now — consumed by parse_is_null_postfix().
                self.advance();
                if *self.peek() == Token::Not {
                    self.advance();
                    self.expect(&Token::Null)?;
                    return Ok(Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        expr: Box::new(left),
                    });
                }
                self.expect(&Token::Null)?;
                return Ok(Expr::UnaryOp {
                    op: UnaryOp::IsNull,
                    expr: Box::new(left),
                });
            }
            _ => None,
        };

        if let Some(op) = op {
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
            let right = self.parse_is_null_postfix()?;
            Ok(Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
        } else {
            Ok(left)
        }
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

    /// `^` (right-associative, higher than * / %).
    fn parse_power(&mut self) -> CypherResult<Expr> {
        let base = self.parse_unary()?;
        if *self.peek() == Token::Caret {
            self.advance();
            let exp = self.parse_power()?;
            Ok(Expr::BinaryOp {
                left: Box::new(base),
                op: BinaryOp::Pow,
                right: Box::new(exp),
            })
        } else {
            Ok(base)
        }
    }

    /// Unary `-` and `+`.
    fn parse_unary(&mut self) -> CypherResult<Expr> {
        if *self.peek() == Token::Dash {
            self.advance();
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
                    let prop = self.expect_ident("property name")?;
                    // Fold into Property if the target was a simple Variable,
                    // otherwise emit as a function-style call `prop(target)`
                    // (the planner handles plain Property; dynamic access is
                    // preserved but not executed yet).
                    e = match e {
                        Expr::Variable(v) => Expr::Property(PropertyAccess {
                            variable: v,
                            property: prop,
                        }),
                        other => Expr::Property(PropertyAccess {
                            variable: format!("{other:?}"),
                            property: prop,
                        }),
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
                    // Skip over a balanced { } subquery body and record a
                    // placeholder — full semantics deferred.
                    self.skip_balanced_braces()?;
                    Ok(Expr::Exists(Box::new(Expr::Literal(Literal::Bool(true)))))
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
                    let prop = self.expect_ident("property name")?;
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
                    Ok(Expr::FunctionCall { name, args })
                } else if *self.peek() == Token::Colon {
                    // Label-test expression: `n:Label[:Label2...]`.
                    let mut labels = Vec::new();
                    while *self.peek() == Token::Colon {
                        self.advance();
                        labels.push(self.expect_ident("label")?);
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
        // Detect comprehension syntactically before the expression parser
        // can consume IN as a binary operator: `[ <ident> IN ... ]`.
        let is_comprehension = matches!(self.peek(), Token::Ident(_))
            && matches!(self.tokens.get(self.pos + 1), Some(Token::In));

        if is_comprehension {
            // Skip `<ident> IN`
            let _var = self.advance();
            self.advance(); // IN
            let _source = self.parse_or_expr()?;
            if *self.peek() == Token::Where {
                self.advance();
                let _pred = self.parse_or_expr()?;
            }
            if *self.peek() == Token::Pipe {
                self.advance();
                let _proj = self.parse_or_expr()?;
            }
            self.expect(&Token::RBracket)?;
            return Ok(Expr::List(Vec::new()));
        }

        let mut items = vec![self.parse_or_expr()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_or_expr()?);
        }
        self.expect(&Token::RBracket)?;
        Ok(Expr::List(items))
    }

    fn skip_balanced_braces(&mut self) -> CypherResult<()> {
        self.expect(&Token::LBrace)?;
        let mut depth = 1;
        while depth > 0 {
            match self.advance() {
                Token::LBrace => depth += 1,
                Token::RBrace => depth -= 1,
                Token::Eof => {
                    return Err(CypherError::Parse {
                        position: self.pos,
                        message: "unterminated '{'".into(),
                    });
                }
                _ => {}
            }
        }
        Ok(())
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
            if let Token::Ident(name) = self.advance() {
                Some(name)
            } else {
                return Err(CypherError::Parse {
                    position: self.pos,
                    message: "expected alias".into(),
                });
            }
        } else {
            None
        };
        Ok(ReturnItem { expr, alias })
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
        assert_eq!(q.skip, Some(10));
        assert_eq!(q.limit, Some(25));
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
        let MutationClause::Delete { variables, detach } = &wq.mutations[0] else {
            panic!("expected DELETE");
        };
        assert_eq!(variables, &vec!["n".to_string()]);
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
        let MutationClause::Delete { variables, .. } = &wq.mutations[0] else {
            panic!("expected DELETE");
        };
        assert_eq!(variables, &vec!["a".to_string(), "b".to_string()]);
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
        assert_eq!(items[0].target.variable, "n");
        assert_eq!(items[0].target.property, "name");
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
        let RemoveItem::Property(pa) = &items[0];
        assert_eq!(pa.variable, "n");
        assert_eq!(pa.property, "name");
    }

    #[test]
    fn parse_merge_node() {
        let stmt = Parser::parse("MERGE (n:Person {name: 'Alice'})").unwrap();
        let Statement::Write(wq) = stmt else {
            panic!("expected write");
        };
        let MutationClause::Merge { patterns } = &wq.mutations[0] else {
            panic!("expected MERGE");
        };
        assert_eq!(patterns.len(), 1);
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
}
