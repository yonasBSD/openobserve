// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use chrono::DateTime;
use datafusion::sql::{TableReference, parser::DFParser, resolve::resolve_table_references};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlparser::{
    ast::{
        AccessExpr, BinaryOperator, Expr as SqlExpr, Function, FunctionArg, FunctionArgExpr,
        FunctionArguments, GroupByExpr, Offset as SqlOffset, OrderByExpr, OrderByKind, Query,
        Select, SelectItem, SetExpr, Statement, Subscript, TableFactor, TableWithJoins, Value,
        ValueWithSpan,
    },
    dialect::PostgreSqlDialect,
    parser::Parser,
};
use utoipa::ToSchema;

use super::stream::StreamType;
use crate::{TIMESTAMP_COL_NAME, get_config};

pub const MAX_LIMIT: i64 = 100000;
pub const MAX_OFFSET: i64 = 100000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ToSchema, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderBy {
    #[default]
    Desc,
    Asc,
}

/// get stream name from a sql
pub fn resolve_stream_names(sql: &str) -> Result<Vec<String>, anyhow::Error> {
    let dialect = &PostgreSqlDialect {};
    let statement = DFParser::parse_sql_with_dialect(sql, dialect)?
        .pop_back()
        .ok_or(anyhow::anyhow!("Failed to parse sql"))?;
    let (table_refs, _) = resolve_table_references(&statement, true)?;
    let mut tables = Vec::new();
    for table in table_refs {
        tables.push(table.table().to_string());
    }
    Ok(tables)
}

pub fn resolve_stream_names_with_type(sql: &str) -> Result<Vec<TableReference>, anyhow::Error> {
    let dialect = &PostgreSqlDialect {};
    let statement = DFParser::parse_sql_with_dialect(sql, dialect)?
        .pop_back()
        .ok_or(anyhow::anyhow!("Failed to parse sql"))?;
    let (table_refs, _) = resolve_table_references(&statement, true)?;
    let mut tables = Vec::new();
    for table in table_refs {
        tables.push(table);
    }
    Ok(tables)
}

pub trait TableReferenceExt {
    fn stream_type(&self) -> String;
    fn stream_name(&self) -> String;
    fn has_stream_type(&self) -> bool;
    fn get_stream_type(&self, stream_type: StreamType) -> StreamType;
}

impl TableReferenceExt for TableReference {
    fn stream_type(&self) -> String {
        self.schema().unwrap_or("").to_string()
    }

    fn stream_name(&self) -> String {
        self.table().to_string()
    }

    fn has_stream_type(&self) -> bool {
        self.schema().is_some()
    }

    fn get_stream_type(&self, stream_type: StreamType) -> StreamType {
        if self.has_stream_type() {
            StreamType::from(self.stream_type().as_str())
        } else {
            stream_type
        }
    }
}

/// parsed sql
#[derive(Clone, Debug, Serialize)]
pub struct Sql {
    pub fields: Vec<String>,              // projection, select, fields
    pub selection: Option<SqlExpr>,       // where
    pub source: String,                   // table
    pub order_by: Vec<(String, OrderBy)>, // desc
    pub group_by: Vec<String>,            // field
    pub having: bool,
    pub offset: i64,
    pub limit: i64,
    pub time_range: Option<(i64, i64)>,
    pub quick_text: Vec<(String, String, SqlOperator)>, // use text line quick filter
    pub field_alias: Vec<(String, String)>,             // alias for select field
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum SqlOperator {
    And,
    Or,
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    Like,
    Nop,
}

#[derive(Clone, Debug, Serialize)]
pub enum SqlValue {
    String(String),
    Number(i64),
    Float(f64),
}

pub struct Projection<'a>(pub &'a Vec<SelectItem>);
pub struct Quicktext<'a>(pub &'a Option<SqlExpr>);
pub struct Timerange<'a>(pub &'a Option<SqlExpr>);
pub struct Source<'a>(pub &'a [TableWithJoins]);
pub struct Order<'a>(pub &'a OrderByExpr);
pub struct Group<'a>(pub &'a SqlExpr);
pub struct Offset<'a>(pub &'a SqlOffset);
pub struct Limit<'a>(pub &'a SqlExpr);
pub struct Where<'a>(pub &'a Option<SqlExpr>);

impl Sql {
    #[deprecated(since = "0.14.5", note = "use service::search::Sql::new instead")]
    pub fn new(sql: &str) -> Result<Sql, anyhow::Error> {
        if sql.is_empty() {
            return Err(anyhow::anyhow!("SQL is empty"));
        }
        let dialect = sqlparser::dialect::GenericDialect {};
        let statement = Parser::parse_sql(&dialect, sql);
        if statement.is_err() {
            return Err(anyhow::anyhow!(statement.err().unwrap()));
        }
        let statement = statement.unwrap();
        if statement.is_empty() {
            return Err(anyhow::anyhow!("SQL is empty"));
        }
        let statement = &statement[0];
        let sql: Result<Sql, anyhow::Error> = statement.try_into();
        if sql.is_err() {
            return Err(sql.err().unwrap());
        }

        Ok(sql.unwrap())
    }
}

impl TryFrom<&Statement> for Sql {
    type Error = anyhow::Error;

    fn try_from(sql: &Statement) -> Result<Self, Self::Error> {
        match sql {
            // just take case of: query (select ... from ... where ...)
            Statement::Query(q) => {
                let offset = q.offset.as_ref();
                let limit = q.limit.as_ref();
                let orders = &q.order_by;
                let Select {
                    from: table_with_joins,
                    selection,
                    projection,
                    group_by: groups,
                    having,
                    ..
                } = match &q.body.as_ref() {
                    SetExpr::Select(statement) => statement.as_ref(),
                    _ => {
                        return Err(anyhow::anyhow!(
                            "We only support Select Query at the moment"
                        ));
                    }
                };

                let source = Source(table_with_joins).try_into()?;

                let mut order_by = Vec::new();
                if let Some(orders) = orders
                    && let OrderByKind::Expressions(exprs) = &orders.kind
                {
                    for expr in exprs.iter() {
                        order_by.push(Order(expr).try_into()?);
                    }
                }

                // TODO: support Group by all
                // https://docs.snowflake.com/en/sql-reference/constructs/group-by#label-group-by-all-columns
                let mut group_by = Vec::new();
                if let GroupByExpr::Expressions(exprs, _) = groups {
                    for expr in exprs {
                        group_by.push(Group(expr).try_into()?);
                    }
                }

                let offset = offset.map_or(0, |v| Offset(v).into());
                let limit = limit.map_or(0, |v| Limit(v).into());

                let mut fields: Vec<String> = Projection(projection).try_into()?;
                let selection = selection.as_ref().cloned();
                let field_alias: Vec<(String, String)> = Projection(projection).try_into()?;
                let time_range: Option<(i64, i64)> = Timerange(&selection).try_into()?;
                let quick_text: Vec<(String, String, SqlOperator)> =
                    Quicktext(&selection).try_into()?;
                let where_fields: Vec<String> = Where(&selection).try_into()?;

                fields.extend(where_fields);
                fields.sort();
                fields.dedup();

                Ok(Sql {
                    fields,
                    selection,
                    source,
                    order_by,
                    group_by,
                    having: having.is_some(),
                    offset,
                    limit,
                    time_range,
                    quick_text,
                    field_alias,
                })
            }
            _ => Err(anyhow::anyhow!("We only support Query at the moment")),
        }
    }
}

impl std::fmt::Display for SqlValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlValue::String(s) => write!(f, "{s}"),
            SqlValue::Number(n) => write!(f, "{n}"),
            SqlValue::Float(fl) => write!(f, "{fl}"),
        }
    }
}

impl From<Offset<'_>> for i64 {
    fn from(offset: Offset) -> Self {
        match offset.0 {
            SqlOffset {
                value:
                    SqlExpr::Value(ValueWithSpan {
                        value: Value::Number(v, _b),
                        ..
                    }),
                ..
            } => {
                let mut v: i64 = v.parse().unwrap_or(0);
                if v > MAX_OFFSET {
                    v = MAX_OFFSET;
                }
                v
            }
            _ => 0,
        }
    }
}

impl<'a> From<Limit<'a>> for i64 {
    fn from(l: Limit<'a>) -> Self {
        match l.0 {
            SqlExpr::Value(ValueWithSpan {
                value: Value::Number(v, _b),
                ..
            }) => {
                let mut v: i64 = v.parse().unwrap_or(0);
                if v > MAX_LIMIT {
                    v = MAX_LIMIT;
                }
                v
            }
            _ => 0,
        }
    }
}

impl<'a> TryFrom<Source<'a>> for String {
    type Error = anyhow::Error;

    fn try_from(source: Source<'a>) -> Result<Self, Self::Error> {
        if source.0.len() != 1 {
            return Err(anyhow::anyhow!(
                "We only support single data source at the moment"
            ));
        }

        let table = &source.0[0];
        if !table.joins.is_empty() {
            return Err(anyhow::anyhow!(
                "We do not support joint data source at the moment"
            ));
        }

        match &table.relation {
            TableFactor::Table { name, .. } => {
                Ok(trim_quotes(name.0.first().unwrap().to_string().as_str()))
            }
            _ => Err(anyhow::anyhow!("We only support single table")),
        }
    }
}

impl TryFrom<Order<'_>> for (String, OrderBy) {
    type Error = anyhow::Error;

    fn try_from(order: Order) -> Result<Self, Self::Error> {
        match &order.0.expr {
            SqlExpr::Identifier(id) => Ok((
                id.value.to_string(),
                if order.0.options.asc.unwrap_or_default() {
                    OrderBy::Asc
                } else {
                    OrderBy::Desc
                },
            )),
            expr => Err(anyhow::anyhow!(
                "We only support identifier for order by, got {expr}"
            )),
        }
    }
}

impl TryFrom<Group<'_>> for String {
    type Error = anyhow::Error;

    fn try_from(g: Group) -> Result<Self, Self::Error> {
        match &g.0 {
            SqlExpr::Identifier(id) => Ok(id.value.to_string()),
            expr => Err(anyhow::anyhow!(
                "We only support identifier for group by, got {expr}"
            )),
        }
    }
}

impl<'a> TryFrom<Projection<'a>> for Vec<String> {
    type Error = anyhow::Error;

    fn try_from(projection: Projection<'a>) -> Result<Self, Self::Error> {
        let mut fields = Vec::new();
        for item in projection.0 {
            let field = match item {
                SelectItem::UnnamedExpr(expr) => get_field_name_from_expr(expr)?,
                SelectItem::ExprWithAlias { expr, alias: _ } => get_field_name_from_expr(expr)?,
                _ => None,
            };
            if let Some(field) = field {
                let field = field
                    .into_iter()
                    .map(|v| v.trim_matches(|v| v == '\'' || v == '"').to_string());
                fields.extend(field);
            }
        }
        Ok(fields)
    }
}

impl<'a> TryFrom<Projection<'a>> for Vec<(String, String)> {
    type Error = anyhow::Error;

    fn try_from(projection: Projection<'a>) -> Result<Self, Self::Error> {
        let mut fields = Vec::new();
        for item in projection.0 {
            if let SelectItem::ExprWithAlias { expr, alias } = item {
                fields.push((expr.to_string(), alias.to_string().replace('"', "")))
            }
        }
        Ok(fields)
    }
}

impl<'a> TryFrom<Timerange<'a>> for Option<(i64, i64)> {
    type Error = anyhow::Error;

    fn try_from(selection: Timerange<'a>) -> Result<Self, Self::Error> {
        let mut fields = Vec::new();
        if let Some(expr) = selection.0 {
            parse_expr_for_field(expr, &SqlOperator::And, TIMESTAMP_COL_NAME, &mut fields)?
        }

        let mut time_min = Vec::new();
        for (_field, value, op, _operator) in fields.iter() {
            match op {
                SqlOperator::Gt => match parse_timestamp(value) {
                    Ok(v) => time_min.push(v.unwrap_or_default()),
                    Err(e) => return Err(e),
                },
                SqlOperator::Gte => match parse_timestamp(value) {
                    Ok(v) => time_min.push(v.unwrap_or_default()),
                    Err(e) => return Err(e),
                },
                _ => {}
            }
        }

        let mut time_max = Vec::new();
        for (_field, value, op, _operator) in fields.iter() {
            match op {
                SqlOperator::Lt => match parse_timestamp(value) {
                    Ok(v) => time_max.push(v.unwrap_or_default()),
                    Err(e) => return Err(e),
                },
                SqlOperator::Lte => match parse_timestamp(value) {
                    Ok(v) => time_max.push(v.unwrap_or_default()),
                    Err(e) => return Err(e),
                },
                _ => {}
            }
        }

        let time_min = {
            if !time_min.is_empty() {
                time_min.iter().min().unwrap().to_owned()
            } else {
                0
            }
        };
        let time_max = {
            if !time_max.is_empty() {
                time_max.iter().max().unwrap().to_owned()
            } else {
                0
            }
        };
        Ok(Some((time_min, time_max)))
    }
}

impl<'a> TryFrom<Quicktext<'a>> for Vec<(String, String, SqlOperator)> {
    type Error = anyhow::Error;

    fn try_from(selection: Quicktext<'a>) -> Result<Self, Self::Error> {
        let mut fields = Vec::new();
        if let Some(expr) = selection.0 {
            parse_expr_for_field(expr, &SqlOperator::And, "*", &mut fields)?
        }
        let fields = fields
            .iter()
            .filter_map(|(field, value, op, operator)| {
                if op == &SqlOperator::Eq || op == &SqlOperator::Like {
                    Some((
                        field.to_string(),
                        value.to_owned().to_string(),
                        operator.to_owned(),
                    ))
                } else {
                    None
                }
            })
            .collect();

        Ok(fields)
    }
}

impl<'a> TryFrom<Where<'a>> for Vec<String> {
    type Error = anyhow::Error;

    fn try_from(selection: Where<'a>) -> Result<Self, Self::Error> {
        let mut fields = Vec::new();
        if let Some(expr) = selection.0 {
            fields.extend(get_field_name_from_expr(expr)?.unwrap_or_default())
        }
        Ok(fields)
    }
}

fn parse_timestamp(s: &SqlValue) -> Result<Option<i64>, anyhow::Error> {
    match s {
        SqlValue::String(s) => {
            let s = s.to_lowercase();
            let mut s = s.as_str();
            if s.starts_with("to_timestamp") {
                if s.starts_with("to_timestamp_seconds(") {
                    s = s.strip_prefix("to_timestamp_seconds(").unwrap();
                } else if s.starts_with("to_timestamp_micros(") {
                    s = s.strip_prefix("to_timestamp_micros(").unwrap();
                } else if s.starts_with("to_timestamp_millis(") {
                    s = s.strip_prefix("to_timestamp_millis(").unwrap();
                } else if s.starts_with("to_timestamp(") {
                    s = s.strip_prefix("to_timestamp(").unwrap();
                } else {
                    return Err(anyhow::anyhow!(
                        "Only support timestamp functions [to_timestamp|to_timestamp_millis|to_timestamp_micros|to_timestamp_seconds]"
                    ));
                }
                s = s.strip_suffix(')').unwrap();
                s = s.trim_matches(|v| v == '\'' || v == '"');
            }
            let v = DateTime::parse_from_rfc3339(s)?;
            Ok(Some(v.timestamp_micros()))
        }
        SqlValue::Number(n) => {
            if *n == 0 {
                Ok(None)
            } else if *n > (1e18 as i64) {
                Ok(Some(*n / 1000))
            } else if *n > (1e15 as i64) {
                Ok(Some(*n))
            } else if *n > (1e12 as i64) {
                Ok(Some(*n * 1000))
            } else if *n > (1e9 as i64) {
                Ok(Some(*n * 1000 * 1000))
            } else {
                Err(anyhow::anyhow!("Invalid timestamp: {}", n))
            }
        }
        SqlValue::Float(f) => Err(anyhow::anyhow!("Invalid timestamp: {}", f)),
    }
}

fn parse_expr_for_field(
    expr: &SqlExpr,
    expr_op: &SqlOperator,
    field: &str,
    fields: &mut Vec<(String, SqlValue, SqlOperator, SqlOperator)>,
) -> Result<(), anyhow::Error> {
    // println!("! parse_expr -> {:?}", expr);
    match expr {
        SqlExpr::Nested(e) => parse_expr_for_field(e, expr_op, field, fields)?,
        SqlExpr::BinaryOp { left, op, right } => {
            let next_op: SqlOperator = op.try_into()?;
            if let SqlExpr::Identifier(ident) = &**left {
                let eq = parse_expr_check_field_name(&ident.value, field);
                if ident.value == field || (eq && next_op == SqlOperator::Eq) {
                    let val = get_value_from_expr(right);
                    if matches!(right.as_ref(), SqlExpr::Subquery(_)) {
                        return Ok(());
                    }
                    if val.is_none() {
                        return Err(anyhow::anyhow!(
                            "SqlExpr::Identifier: We only support Identifier at the moment"
                        ));
                    }
                    fields.push((ident.value.to_string(), val.unwrap(), next_op, *expr_op));
                }
            } else {
                parse_expr_for_field(left, &next_op, field, fields)?;
                parse_expr_for_field(right, expr_op, field, fields)?;
            }
        }
        SqlExpr::Like {
            negated,
            expr,
            pattern,
            escape_char,
            any: _,
        } => {
            parse_expr_like(negated, expr, pattern, escape_char, expr_op, field, fields).unwrap();
        }
        SqlExpr::InList {
            expr,
            list,
            negated,
        } => {
            parse_expr_in_list(expr, list, negated, expr_op, field, fields).unwrap();
        }
        SqlExpr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let ret = parse_expr_between(expr, negated, low, high, field, fields);
            if ret.is_err() {
                return Err(anyhow::anyhow!("{:?}", ret.err()));
            }
        }
        SqlExpr::Function(f) => {
            let ret = parse_expr_function(f, field, fields);
            if ret.is_err() {
                return Err(anyhow::anyhow!("{:?}", ret.err()));
            }
        }
        SqlExpr::IsNull(expr) => {
            if let SqlExpr::Identifier(ident) = expr.as_ref()
                && parse_expr_check_field_name(&ident.value, field)
            {
                fields.push((
                    ident.value.to_string(),
                    SqlValue::String("".to_string()),
                    SqlOperator::Eq,
                    *expr_op,
                ));
            }
        }
        SqlExpr::IsNotNull(expr) => {
            if let SqlExpr::Identifier(ident) = expr.as_ref()
                && parse_expr_check_field_name(&ident.value, field)
            {
                fields.push((
                    ident.value.to_string(),
                    SqlValue::String("".to_string()),
                    SqlOperator::Eq,
                    *expr_op,
                ));
            }
        }
        _ => {}
    }

    Ok(())
}

fn parse_expr_check_field_name(s: &str, field: &str) -> bool {
    if s == field {
        return true;
    }
    let cfg = get_config();
    if field == "*" && s != cfg.common.column_all.as_str() && s != TIMESTAMP_COL_NAME {
        return true;
    }

    // check function, like: to_timestamp_micros("field")
    let re = Regex::new(&format!(r#"(?i)\(['"]?{field}['"]?\)"#)).unwrap();
    re.is_match(s)
}

fn parse_expr_like(
    _negated: &bool,
    expr: &SqlExpr,
    pattern: &SqlExpr,
    _escape_char: &Option<String>,
    next_op: &SqlOperator,
    field: &str,
    fields: &mut Vec<(String, SqlValue, SqlOperator, SqlOperator)>,
) -> Result<(), anyhow::Error> {
    if let SqlExpr::Identifier(ident) = expr
        && parse_expr_check_field_name(&ident.value, field)
    {
        let val = get_value_from_expr(pattern);
        if val.is_none() {
            return Err(anyhow::anyhow!(
                "SqlExpr::Like: We only support Identifier at the moment"
            ));
        }
        fields.push((
            ident.value.to_string(),
            val.unwrap(),
            SqlOperator::Like,
            *next_op,
        ));
    }
    Ok(())
}

fn parse_expr_in_list(
    expr: &SqlExpr,
    list: &[SqlExpr],
    negated: &bool,
    next_op: &SqlOperator,
    field: &str,
    fields: &mut Vec<(String, SqlValue, SqlOperator, SqlOperator)>,
) -> Result<(), anyhow::Error> {
    if *negated {
        return Ok(());
    }
    if list.is_empty() {
        return Ok(());
    }
    let field_name = get_value_from_expr(expr).unwrap().to_string();
    if !parse_expr_check_field_name(&field_name, field) {
        return Ok(());
    }
    let exprs_len = list.len();
    for (i, item) in list.iter().enumerate() {
        let op = if i + 1 == exprs_len {
            *next_op
        } else {
            SqlOperator::Or
        };
        if let Some(val) = get_value_from_expr(item) {
            fields.push((field_name.to_string(), val, SqlOperator::Eq, op));
        }
    }
    Ok(())
}

fn parse_expr_between(
    expr: &SqlExpr,
    negated: &bool,
    low: &SqlExpr,
    high: &SqlExpr,
    field: &str,
    fields: &mut Vec<(String, SqlValue, SqlOperator, SqlOperator)>,
) -> Result<(), anyhow::Error> {
    if *negated {
        return Ok(());
    }
    let f_name = get_value_from_expr(expr).unwrap().to_string();
    if parse_expr_check_field_name(&f_name, field) {
        let min = get_value_from_expr(low).unwrap();
        let max = get_value_from_expr(high).unwrap();
        fields.push((field.to_string(), min, SqlOperator::Gte, SqlOperator::And));
        fields.push((field.to_string(), max, SqlOperator::Lt, SqlOperator::And));
    }
    Ok(())
}

fn parse_expr_function(
    f: &Function,
    field: &str,
    fields: &mut Vec<(String, SqlValue, SqlOperator, SqlOperator)>,
) -> Result<(), anyhow::Error> {
    let f_name = f.name.to_string().to_lowercase();
    if ![
        "strpos",
        "contains",
        "match",
        "time_range",
        "to_timestamp",
        "to_timestamp_millis",
        "to_timestamp_micros",
        "to_timestamp_seconds",
    ]
    .contains(&f_name.as_str())
    {
        return Ok(());
    }

    // Hack time_range
    if f_name == "time_range" {
        return parse_expr_fun_time_range(f, field, fields);
    }

    let args = match &f.args {
        FunctionArguments::None => return Ok(()),
        FunctionArguments::Subquery(_) => {
            log::error!("We do not support subquery at the moment");
            return Ok(());
        }
        FunctionArguments::List(args) => &args.args,
    };
    if args.len() < 2 {
        return Ok(());
    }

    let nop = SqlOperator::And;
    let next_op = SqlOperator::And;
    let field_name = args.first().unwrap().to_string();
    let field_name = field_name.trim_matches(|c: char| c == '\'' || c == '"');
    if parse_expr_check_field_name(field_name, field) {
        match args.get(1).unwrap() {
            FunctionArg::Named {
                name: _name,
                arg,
                operator: _operator,
            } => match arg {
                FunctionArgExpr::Expr(expr) => {
                    let val = get_value_from_expr(expr);
                    if val.is_none() {
                        return Err(anyhow::anyhow!(
                            "SqlExpr::Function<Named>: We only support Identifier at the moment"
                        ));
                    }
                    fields.push((field.to_string(), val.unwrap(), nop, next_op));
                }
                _ => return Err(anyhow::anyhow!("We only support String at the moment")),
            },
            FunctionArg::Unnamed(arg) => match arg {
                FunctionArgExpr::Expr(expr) => {
                    let val = get_value_from_expr(expr);
                    if val.is_none() {
                        return Err(anyhow::anyhow!(
                            "SqlExpr::Function<Unnamed>: We only support Identifier at the moment"
                        ));
                    }
                    fields.push((field.to_string(), val.unwrap(), nop, next_op));
                }
                _ => return Err(anyhow::anyhow!("We only support String at the moment")),
            },
            _ => {}
        }
    }

    Ok(())
}

fn parse_expr_fun_time_range(
    f: &Function,
    field: &str,
    fields: &mut Vec<(String, SqlValue, SqlOperator, SqlOperator)>,
) -> Result<(), anyhow::Error> {
    let args = match &f.args {
        FunctionArguments::None => return Ok(()),
        FunctionArguments::Subquery(_) => return Ok(()),
        FunctionArguments::List(args) => &args.args,
    };
    if args.len() != 3 {
        return Err(anyhow::anyhow!(
            "SqlExpr::Function: time_range function must have 3 arguments"
        ));
    }

    let next_op = SqlOperator::And;
    let field_name = args.first().unwrap().to_string();
    let field_name = field_name.trim_matches(|c: char| c == '\'' || c == '"');
    if parse_expr_check_field_name(field_name, field) {
        let mut vals = Vec::new();
        for arg in args.iter() {
            let val = match arg {
                FunctionArg::Named {
                    name: _name,
                    arg,
                    operator: _operator,
                } => match arg {
                    FunctionArgExpr::Expr(expr) => {
                        let val = get_value_from_expr(expr);
                        if val.is_none() {
                            return Err(anyhow::anyhow!(
                                "SqlExpr::Function<Named>: We only support Identifier at the moment"
                            ));
                        }
                        val.unwrap()
                    }
                    _ => return Err(anyhow::anyhow!("We only support String at the moment")),
                },
                FunctionArg::Unnamed(arg) => match arg {
                    FunctionArgExpr::Expr(expr) => {
                        let val = get_value_from_expr(expr);
                        if val.is_none() {
                            return Err(anyhow::anyhow!(
                                "SqlExpr::Function<Unnamed>: We only support Identifier at the moment"
                            ));
                        }
                        val.unwrap()
                    }
                    _ => return Err(anyhow::anyhow!("We only support String at the moment")),
                },
                _ => unreachable!(),
            };
            vals.push(val);
        }

        fields.push((
            field_name.to_string(),
            vals.get(1).unwrap().to_owned(),
            SqlOperator::Gte,
            next_op,
        ));
        fields.push((
            field_name.to_string(),
            vals.get(2).unwrap().to_owned(),
            SqlOperator::Lt,
            next_op,
        ));
    }

    Ok(())
}

fn get_value_from_expr(expr: &SqlExpr) -> Option<SqlValue> {
    match expr {
        SqlExpr::Identifier(ident) => Some(SqlValue::String(ident.value.to_string())),
        SqlExpr::Value(value) => match &value.value {
            Value::SingleQuotedString(s) => Some(SqlValue::String(s.to_string())),
            Value::DoubleQuotedString(s) => Some(SqlValue::String(s.to_string())),
            Value::Number(s, _) => {
                if let Ok(num) = s.parse::<i64>() {
                    Some(SqlValue::Number(num))
                } else {
                    // Not integer, try float
                    s.parse::<f64>().ok().map(SqlValue::Float)
                }
            }
            _ => None,
        },
        SqlExpr::Function(f) => Some(SqlValue::String(f.to_string())),
        _ => None,
    }
}

fn get_field_name_from_expr(expr: &SqlExpr) -> Result<Option<Vec<String>>, anyhow::Error> {
    match expr {
        SqlExpr::Identifier(ident) => Ok(Some(vec![ident.value.to_string()])),
        SqlExpr::BinaryOp { left, op: _, right } => {
            let mut fields = Vec::new();
            if let Some(v) = get_field_name_from_expr(left)? {
                fields.extend(v);
            }
            if let Some(v) = get_field_name_from_expr(right)? {
                fields.extend(v);
            }
            Ok((!fields.is_empty()).then_some(fields))
        }
        SqlExpr::Function(f) => {
            let args = match &f.args {
                FunctionArguments::None => return Ok(None),
                FunctionArguments::Subquery(_) => return Ok(None),
                FunctionArguments::List(args) => &args.args,
            };
            let mut fields = Vec::with_capacity(args.len());
            for arg in args.iter() {
                match arg {
                    FunctionArg::Named {
                        name: _name,
                        arg: FunctionArgExpr::Expr(expr),
                        operator: _operator,
                    } => {
                        if let Some(v) = get_field_name_from_expr(expr)? {
                            fields.extend(v);
                        }
                    }
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                        if let Some(v) = get_field_name_from_expr(expr)? {
                            fields.extend(v);
                        }
                    }
                    _ => {}
                }
            }
            Ok((!fields.is_empty()).then_some(fields))
        }
        SqlExpr::Nested(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsFalse(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsNotFalse(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsTrue(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsNotTrue(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsNull(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsNotNull(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsUnknown(expr) => get_field_name_from_expr(expr),
        SqlExpr::IsNotUnknown(expr) => get_field_name_from_expr(expr),
        SqlExpr::InList { expr, list, .. } => {
            let mut fields = Vec::new();
            if let Some(v) = get_field_name_from_expr(expr)? {
                fields.extend(v);
            }
            for expr in list.iter() {
                if let Some(v) = get_field_name_from_expr(expr)? {
                    fields.extend(v);
                }
            }
            Ok((!fields.is_empty()).then_some(fields))
        }
        SqlExpr::Between { expr, .. } => get_field_name_from_expr(expr),
        SqlExpr::Like { expr, pattern, .. } | SqlExpr::ILike { expr, pattern, .. } => {
            let mut fields = Vec::new();
            if let Some(expr) = get_field_name_from_expr(expr)? {
                fields.extend(expr);
            }
            if let Some(pattern) = get_field_name_from_expr(pattern)? {
                fields.extend(pattern);
            }
            Ok((!fields.is_empty()).then_some(fields))
        }
        SqlExpr::Cast { expr, .. } => get_field_name_from_expr(expr),
        SqlExpr::Case {
            operand: _,
            conditions,
            else_result,
        } => {
            let mut fields = Vec::new();
            for expr in conditions.iter() {
                if let Some(v) = get_field_name_from_expr(&expr.condition)? {
                    fields.extend(v);
                }
                if let Some(v) = get_field_name_from_expr(&expr.result)? {
                    fields.extend(v);
                }
            }
            if let Some(expr) = else_result.as_ref()
                && let Some(v) = get_field_name_from_expr(expr)?
            {
                fields.extend(v);
            }
            Ok((!fields.is_empty()).then_some(fields))
        }
        SqlExpr::AtTimeZone { timestamp, .. } => get_field_name_from_expr(timestamp),
        SqlExpr::Extract { expr, .. } => get_field_name_from_expr(expr),
        SqlExpr::CompoundFieldAccess { root, access_chain } => {
            let mut fields = Vec::new();
            if let Some(v) = get_field_name_from_expr(root)? {
                fields.extend(v);
            }
            for access in access_chain.iter() {
                match access {
                    AccessExpr::Dot(expr) => {
                        if let Some(v) = get_field_name_from_expr(expr)? {
                            fields.extend(v);
                        }
                    }
                    AccessExpr::Subscript(expr) => match expr {
                        Subscript::Index { index } => {
                            if let Some(v) = get_field_name_from_expr(index)? {
                                fields.extend(v);
                            }
                        }
                        Subscript::Slice {
                            lower_bound,
                            upper_bound,
                            stride,
                        } => {
                            let mut func = |expr: &Option<SqlExpr>| -> Result<(), anyhow::Error> {
                                if let Some(expr) = expr
                                    && let Some(v) = get_field_name_from_expr(expr)?
                                {
                                    fields.extend(v);
                                }
                                Ok(())
                            };
                            func(lower_bound)?;
                            func(upper_bound)?;
                            func(stride)?;
                        }
                    },
                }
            }
            Ok((!fields.is_empty()).then_some(fields))
        }
        SqlExpr::Subquery(subquery) => get_field_name_from_query(subquery),
        SqlExpr::InSubquery { expr, subquery, .. } => {
            let mut fields = Vec::new();
            if let Some(v) = get_field_name_from_expr(expr)? {
                fields.extend(v);
            }
            if let Some(v) = get_field_name_from_query(subquery)? {
                fields.extend(v);
            }
            Ok((!fields.is_empty()).then_some(fields))
        }
        _ => Ok(None),
    }
}

fn get_field_name_from_query(query: &Query) -> Result<Option<Vec<String>>, anyhow::Error> {
    let Select {
        from: _table_with_joins,
        selection,
        projection,
        group_by: _groups,
        having: _,
        ..
    } = match &query.body.as_ref() {
        SetExpr::Select(statement) => statement.as_ref(),
        _ => return Ok(None),
    };

    let mut fields: Vec<String> = Projection(projection).try_into()?;
    let selection = selection.as_ref().cloned();
    let where_fields: Vec<String> = Where(&selection).try_into()?;

    fields.extend(where_fields);
    fields.sort();
    fields.dedup();

    Ok(Some(fields))
}

impl TryFrom<&BinaryOperator> for SqlOperator {
    type Error = anyhow::Error;
    fn try_from(value: &BinaryOperator) -> Result<Self, Self::Error> {
        match value {
            BinaryOperator::And => Ok(SqlOperator::And),
            BinaryOperator::Or => Ok(SqlOperator::Or),
            BinaryOperator::Eq => Ok(SqlOperator::Eq),
            BinaryOperator::NotEq => Ok(SqlOperator::Neq),
            BinaryOperator::Gt => Ok(SqlOperator::Gt),
            BinaryOperator::GtEq => Ok(SqlOperator::Gte),
            BinaryOperator::Lt => Ok(SqlOperator::Lt),
            BinaryOperator::LtEq => Ok(SqlOperator::Lte),
            _ => Err(anyhow::anyhow!(
                "We only support BinaryOperator at the moment"
            )),
        }
    }
}

fn trim_quotes(s: &str) -> String {
    let s = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    s.strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(s)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sql_works() {
        let table = "index.1.2022";
        let sql = format!(
            "select a, b, c from \"{}\" where a=1 and b=1 or c=1 order by c desc limit 5 offset 10",
            table
        );
        let dialect = sqlparser::dialect::GenericDialect {};
        let statement = &Parser::parse_sql(&dialect, sql.as_ref()).unwrap()[0];
        let sql: Sql = statement.try_into().unwrap();
        assert_eq!(sql.source, table);
        assert_eq!(sql.limit, 5);
        assert_eq!(sql.offset, 10);
        assert_eq!(sql.order_by, vec![("c".into(), OrderBy::Desc)]);
        assert_eq!(sql.fields, vec!["a", "b", "c"]);
    }

    #[test]
    #[allow(deprecated)]
    fn test_sql_new() {
        let table = "index.1.2022";
        let sql = format!(
            "select a, b, c from \"{}\" where a=1 and b=1 or c=1 order by c desc limit 5 offset 10",
            table
        );
        let local_sql: Sql = Sql::new(sql.as_str()).unwrap();
        assert_eq!(local_sql.source, table);
        assert_eq!(local_sql.limit, 5);
        assert_eq!(local_sql.offset, 10);
        assert_eq!(local_sql.order_by, vec![("c".into(), OrderBy::Desc)]);
        assert_eq!(local_sql.fields, vec!["a", "b", "c"]);
    }

    #[test]
    #[allow(deprecated)]
    fn test_sql_parse() {
        let sqls = [
            ("select * from table1", true),
            ("select * from table1 where a=1", true),
            ("select * from table1 where a='b'", true),
            ("select * from table1 where a='b' limit 10 offset 10", true),
            ("select * from table1 where a='b' group by abc", true),
            (
                "select * from table1 where a='b' group by abc having count(*) > 19",
                true,
            ),
            ("select * from table1, table2 where a='b'", false),
            (
                "select * from table1 left join table2 on table1.a=table2.b where a='b'",
                false,
            ),
            (
                "select * from table1 union select * from table2 where a='b'",
                false,
            ),
        ];
        for (sql, ok) in sqls {
            let ret = Sql::new(sql);
            assert_eq!(ret.is_ok(), ok);
        }
    }

    #[test]
    fn test_sql_parse_timestamp() {
        let val = 1666093521151350;
        let ts_val = SqlValue::Number(val);
        let ts = parse_timestamp(&ts_val).unwrap().unwrap();
        let ts_str_val = SqlValue::String("to_timestamp1()".to_string());
        let ts_str = parse_timestamp(&ts_str_val);
        assert_eq!(ts, val);
        assert!(ts_str.is_err());
    }

    #[test]
    #[allow(deprecated)]
    fn test_sql_parse_timerange() {
        let samples = [
            ("select * from tbl where ts in (1, 2, 3)", (0,0)),
            ("select * from tbl where _timestamp >= 1666093521151350", (1666093521151350,0)),
            ("select * from tbl where _timestamp >= 1666093521151350 AND _timestamp < 1666093521151351", (1666093521151350,1666093521151351)),
            ("select * from tbl where a=1 AND _timestamp>=1666093521151350 AND _timestamp < 1666093521151351", (1666093521151350,1666093521151351)),
            ("select * from tbl where a=1 AND b = 2 AND _timestamp>=1666093521151350 AND _timestamp < 1666093521151351", (1666093521151350,1666093521151351)),
            (r#"select * from tbl where "a"=1 AND b = 2 AND (_timestamp>=1666093521151350 AND _timestamp < 1666093521151351)"#, (1666093521151350,1666093521151351)),
            ("select * from tbl where b = 2 AND (_timestamp>=1666093521151350 AND _timestamp < 1666093521151351)", (1666093521151350,1666093521151351)),
            ("select * from tbl where b = 2 AND _timestamp>=1666093521151350 AND _timestamp < 1666093521151351", (1666093521151350,1666093521151351)),
            ("select * from tbl where (_timestamp>=1666093521151350 AND _timestamp < 1666093521151351)", (1666093521151350,1666093521151351)),
            ("select * from tbl where _timestamp>=1666093521151350 AND _timestamp < 1666093521151351", (1666093521151350,1666093521151351)),
            ("select * from tbl where a=1 AND b = 2 AND (_timestamp BETWEEN 1666093521151350 AND 1666093521151351)", (1666093521151350,1666093521151351)),
            ("select * from tbl where b = 2 AND (_timestamp BETWEEN 1666093521151350 AND 1666093521151351)", (1666093521151350,1666093521151351)),
            ("select * from tbl where (_timestamp BETWEEN 1666093521151350 AND 1666093521151351)", (1666093521151350,1666093521151351)),
            ("select * from tbl where _timestamp BETWEEN 1666093521151350 AND 1666093521151351", (1666093521151350,1666093521151351)),
            (r#"select * from tbl where time_range("_timestamp", '2022-10-19T15:19:24.587Z','2022-10-19T15:34:24.587Z')"#,(1666192764587000,1666193664587000))].to_vec();

        for (sql, (expected_t1, expected_t2)) in samples {
            let (actual_t1, actual_t2) = Sql::new(sql).unwrap().time_range.unwrap();
            assert_eq!(actual_t1, expected_t1);
            if expected_t2 != 0 {
                assert_eq!(actual_t2, expected_t2);
            }
        }
    }

    #[test]
    #[allow(deprecated)]
    fn test_sql_parse_fields() {
        let samples = [
            ("select * FROM tbl", vec![]),
            ("select a, b, c FROM tbl", vec!["a", "b", "c"]),
            ("select a, avg(b) FROM tbl where c=1", vec!["a", "b", "c"]),
            ("select a, a + b FROM tbl where c=1", vec!["a", "b", "c"]),
            ("select a, b + 1 FROM tbl where c=1", vec!["a", "b", "c"]),
            (
                "select a, (a + b) as d FROM tbl where c=1",
                vec!["a", "b", "c"],
            ),
            ("select a, COALESCE(b, c) FROM tbl", vec!["a", "b", "c"]),
            ("select a, COALESCE(b, 'c') FROM tbl", vec!["a", "b"]),
            ("select a, COALESCE(b, \"c\") FROM tbl", vec!["a", "b", "c"]),
            ("select a, b + 1 FROM tbl where c>1", vec!["a", "b", "c"]),
            (
                "select a, b FROM tbl  where (a >= 3 AND a < 10) AND (c='abc' AND d='abcd' AND str_match(b, 'Error')) order by a desc LIMIT 250",
                vec!["a", "b", "c", "d"],
            ),
            (
                "select _timestamp, message FROM tbl  where (_timestamp >= 17139560000000 AND _timestamp < 171395076000000) AND (pid='2fs93s' AND stream_id='asdf834sdf2' AND str_match(message, 'Error')) order by _timestamp desc LIMIT 250",
                vec!["_timestamp", "message", "pid", "stream_id"],
            ),
            ("SELECT a FROM tbl WHERE b IS FALSE", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IS NOT FALSE", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IS TRUE", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IS NOT TRUE", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IS NULL", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IS NOT NULL", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IS UNKNOWN", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IS NOT UNKNOWN", vec!["a", "b"]),
            ("SELECT a FROM tbl WHERE b IN (1, 2, 3)", vec!["a", "b"]),
            (
                "SELECT a FROM tbl WHERE b BETWEEN 10 AND 20",
                vec!["a", "b"],
            ),
            ("SELECT a FROM tbl WHERE b LIKE '%pattern%'", vec!["a", "b"]),
            (
                "SELECT a FROM tbl WHERE b ILIKE '%pattern%'",
                vec!["a", "b"],
            ),
            ("SELECT CAST(a AS INTEGER) FROM tbl", vec!["a"]),
            ("SELECT TRY_CAST(a AS INTEGER) FROM tbl", vec!["a"]),
            ("SELECT a AT TIME ZONE 'UTC' FROM tbl", vec!["a"]),
            ("SELECT EXTRACT(YEAR FROM a) FROM tbl", vec!["a"]),
            ("SELECT map['key'] from tbl", vec!["map"]),
            (
                "SELECT a FROM tbl WHERE c IS NOT NULL AND (b IS FALSE OR d > 3)",
                vec!["a", "b", "c", "d"],
            ),
            (
                "select _timestamp, message FROM tbl where  (pid='2fs93s' or stream_id='asdf834sdf2') AND str_match(new_message, 'Error')",
                vec!["_timestamp", "message", "new_message", "pid", "stream_id"],
            ),
            (
                "SELECT COUNT(CASE WHEN k8s_namespace_name IS NULL THEN 0 ELSE 1 END) AS null_count FROM default1",
                vec!["k8s_namespace_name"],
            ),
            (
                "SELECT COUNT(CASE WHEN k8s_namespace_name IS NULL THEN 0 ELSE 1 END) AS null_count FROM default1 WHERE a=1",
                vec!["a", "k8s_namespace_name"],
            ),
        ];
        for (sql, fields) in samples {
            let actual = Sql::new(sql).unwrap().fields;
            assert_eq!(actual, fields);
        }
    }

    #[test]
    fn test_resolve_stream_names_with_type() {
        let sql = "select * from \"log\".default";
        let names = resolve_stream_names_with_type(sql).unwrap();
        println!("{:?}", names);
    }

    #[test]
    fn test_resolve_stream_names_error() {
        let sql = "";
        let names = resolve_stream_names_with_type(sql);
        assert!(names.is_err());
        assert!(
            names
                .err()
                .unwrap()
                .to_string()
                .contains("Failed to parse sql")
        );
        let names = resolve_stream_names(sql);
        assert!(names.is_err());
        assert!(
            names
                .err()
                .unwrap()
                .to_string()
                .contains("Failed to parse sql")
        );
    }
}
