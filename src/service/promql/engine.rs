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

use std::{collections::HashSet, str::FromStr, sync::Arc, time::Duration};

use arrow::array::Array;
use async_recursion::async_recursion;
use config::{
    TIMESTAMP_COL_NAME,
    meta::promql::{EXEMPLARS_LABEL, HASH_LABEL, HashLabelValue, NAME_LABEL, VALUE_LABEL},
    utils::json,
};
use datafusion::{
    arrow::{
        array::{Float64Array, Int64Array, StringArray, UInt64Array},
        datatypes::{DataType, Schema},
    },
    error::{DataFusionError, Result},
    functions_aggregate::min_max::max,
    prelude::{DataFrame, SessionContext, col, lit},
};
use futures::{TryStreamExt, future::try_join_all};
use hashbrown::HashMap;
use promql_parser::{
    label::MatchOp,
    parser::{
        AggregateExpr, BinModifier, BinaryExpr, Call, Expr as PromExpr, Function, FunctionArgs,
        LabelModifier, MatrixSelector, NumberLiteral, Offset, ParenExpr, StringLiteral, UnaryExpr,
        VectorMatchCardinality, VectorSelector, token,
    },
};
use rayon::iter::{IntoParallelRefMutIterator, ParallelIterator};

use super::{
    PromqlContext,
    utils::{apply_label_selector, apply_matchers},
};
use crate::service::promql::{
    DEFAULT_MAX_SERIES_PER_QUERY, aggregations, binaries, functions, micros, value::*,
};

pub struct Engine {
    ctx: Arc<PromqlContext>,
    /// The time boundaries for the evaluation.
    time: i64,
    /// Filters to include certain columns
    col_filters: Option<HashSet<String>>,
    result_type: Option<String>,
    trace_id: String,
}

impl Engine {
    pub fn new(trace_id: &str, ctx: Arc<PromqlContext>, time: i64) -> Self {
        Self {
            ctx,
            time,
            col_filters: Some(HashSet::new()),
            result_type: None,
            trace_id: trace_id.to_string(),
        }
    }

    pub async fn exec(&mut self, prom_expr: &PromExpr) -> Result<(Value, Option<String>)> {
        self.extract_columns_from_prom_expr(prom_expr)?;
        let value = self.exec_expr(prom_expr).await?;
        Ok((value, self.result_type.clone()))
    }

    /// Recursively filters the necessary columns needed for before executing the given PromQL
    /// expression.
    pub fn extract_columns_from_prom_expr(&mut self, prom_expr: &PromExpr) -> Result<()> {
        match prom_expr {
            PromExpr::Aggregate(AggregateExpr {
                op,
                expr,
                param,
                modifier,
            }) => {
                self.extract_columns_from_prom_expr(expr)?;
                if let Some(expr) = param {
                    self.extract_columns_from_prom_expr(expr)?;
                }
                self.extract_columns_from_modifier(modifier, op);
                Ok(())
            }
            PromExpr::Unary(UnaryExpr { expr }) => self.extract_columns_from_prom_expr(expr),
            PromExpr::Binary(BinaryExpr {
                op,
                lhs,
                rhs,
                modifier,
            }) => {
                self.extract_columns_from_prom_expr(lhs)?;
                self.extract_columns_from_prom_expr(rhs)?;
                if let Some(BinModifier {
                    card,
                    matching,
                    return_bool: _,
                }) = modifier
                {
                    self.extract_columns_from_modifier(matching, op);
                    // group_left or group_right -> no column selection
                    match card {
                        VectorMatchCardinality::ManyToOne(_)
                        | VectorMatchCardinality::OneToMany(_) => {
                            self.col_filters = None;
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            PromExpr::Paren(ParenExpr { expr }) => self.extract_columns_from_prom_expr(expr),
            PromExpr::Subquery(expr) => self.extract_columns_from_prom_expr(&expr.expr),
            PromExpr::Call(Call { func: _, args }) => {
                _ = args
                    .args
                    .iter()
                    .map(|expr| self.extract_columns_from_prom_expr(expr))
                    .collect::<Vec<_>>();
                Ok(())
            }
            PromExpr::Extension(expr) => Err(DataFusionError::NotImplemented(format!(
                "Unsupported Extension: {expr:?}",
            ))),
            _ => Ok(()),
        }
    }

    /// Help function to extract columns from [LabelModifier].
    /// Aggregation function topk & bottomk are special cases where
    /// modifier is applied to grouped result -> not columns filtered.
    /// For promql:
    ///     sum(irate(zo_incoming_requests{namespace="ziox"}[5m])) by (exported_endpoint)
    /// we need to extract the columns `exported_endpoint` from the modifier. Because
    /// the result will be grouped by `exported_endpoint`, and don't consider other labesl.
    fn extract_columns_from_modifier(
        &mut self,
        modifier: &Option<LabelModifier>,
        op: &token::TokenType,
    ) {
        if let Some(label_modifier) = modifier {
            match op.id() {
                // topk and bottomk query all columns when with modifiers
                token::T_TOPK | token::T_BOTTOMK => self.col_filters = None,
                _ => {
                    if let (Some(col_filters), LabelModifier::Include(labels)) =
                        (&mut self.col_filters, label_modifier)
                    {
                        col_filters.extend(labels.labels.iter().cloned());
                    }
                }
            }
        }
    }

    #[async_recursion]
    pub async fn exec_expr(&mut self, prom_expr: &PromExpr) -> Result<Value> {
        Ok(match &prom_expr {
            PromExpr::Aggregate(AggregateExpr {
                op,
                expr,
                param,
                modifier,
            }) => self.aggregate_exprs(op, expr, param, modifier).await?,

            PromExpr::Unary(UnaryExpr { expr }) => {
                let val = self.exec_expr(expr).await?;
                match val {
                    Value::Vector(v) => {
                        let out = v
                            .into_iter()
                            .map(|mut instant| InstantValue {
                                labels: std::mem::take(&mut instant.labels),
                                sample: Sample {
                                    timestamp: instant.sample.timestamp,
                                    value: -instant.sample.value,
                                },
                            })
                            .collect();
                        Value::Vector(out)
                    }
                    Value::Float(f) => {
                        let v = InstantValue {
                            labels: Labels::default(),
                            sample: Sample {
                                timestamp: self.time,
                                value: -f,
                            },
                        };
                        Value::Vector(vec![v])
                    }
                    _ => {
                        return Err(DataFusionError::NotImplemented(format!(
                            "Unsupported Unary: {expr:?}"
                        )));
                    }
                }
            }
            PromExpr::Binary(expr) => {
                let lhs = self.exec_expr(&expr.lhs).await?;
                let rhs = self.exec_expr(&expr.rhs).await?;
                let token = expr.op.id();
                let return_bool = expr.return_bool();
                let op = expr.op.is_comparison_operator();

                // This is a very special case, as we treat the float also a
                // `Value::Vector(vec![element])` therefore, better convert it
                // back to its representation.
                let rhs = match rhs {
                    Value::Vector(v) if v.len() == 1 => Value::Float(v[0].sample.value),
                    _ => rhs,
                };
                match (lhs, rhs) {
                    (Value::Float(left), Value::Float(right)) => {
                        let value = binaries::scalar_binary_operations(
                            token,
                            left,
                            right,
                            return_bool,
                            op,
                        )?;
                        Value::Float(value)
                    }
                    (Value::Vector(left), Value::Vector(right)) => {
                        binaries::vector_bin_op(expr, left, right)?
                    }
                    (Value::Vector(left), Value::Float(right)) => {
                        binaries::vector_scalar_bin_op(expr, left, right, false).await?
                    }
                    (Value::Float(left), Value::Vector(right)) => {
                        binaries::vector_scalar_bin_op(expr, right, left, true).await?
                    }
                    (Value::None, Value::None) => Value::None,
                    _ => {
                        log::debug!(
                            "[trace_id: {}] [PromExpr::Binary] either lhs or rhs vector is found to be empty",
                            self.trace_id
                        );
                        Value::Vector(vec![])
                    }
                }
            }
            PromExpr::Paren(ParenExpr { expr }) => self.exec_expr(expr).await?,
            PromExpr::Subquery(expr) => {
                let val = self.exec_expr(&expr.expr).await?;
                let time_window = Some(TimeWindow::new(self.time, expr.range));
                let matrix = match val {
                    Value::Vector(v) => v
                        .iter()
                        .map(|v| RangeValue {
                            labels: v.labels.to_owned(),
                            samples: vec![v.sample.clone()],
                            exemplars: None,
                            time_window: time_window.clone(),
                        })
                        .collect(),
                    Value::Instant(v) => {
                        vec![RangeValue {
                            labels: v.labels.to_owned(),
                            samples: vec![v.sample.clone()],
                            exemplars: None,
                            time_window,
                        }]
                    }
                    Value::Range(v) => vec![v],
                    Value::Matrix(vs) => vs,
                    Value::Sample(s) => vec![RangeValue {
                        labels: Labels::default(),
                        samples: vec![s],
                        exemplars: None,
                        time_window,
                    }],
                    Value::Float(val) => vec![RangeValue {
                        labels: Labels::default(),
                        samples: vec![Sample::new(self.time, val)],
                        exemplars: None,
                        time_window,
                    }],
                    v => {
                        return Err(DataFusionError::NotImplemented(format!(
                            "Unsupported subquery, the return value should have been a matrix but got {:?}",
                            v.get_type()
                        )));
                    }
                };

                Value::Matrix(matrix)
            }
            PromExpr::NumberLiteral(NumberLiteral { val }) => Value::Float(*val),
            PromExpr::StringLiteral(StringLiteral { val }) => Value::String(val.clone()),
            PromExpr::VectorSelector(v) => {
                let data = self.eval_vector_selector(v).await?;
                if data.is_empty() {
                    Value::None
                } else {
                    Value::Vector(data)
                }
            }
            PromExpr::MatrixSelector(MatrixSelector { vs, range }) => {
                let data = self.eval_matrix_selector(vs, *range).await?;
                if data.is_empty() {
                    Value::None
                } else {
                    Value::Matrix(data)
                }
            }
            PromExpr::Call(Call { func, args }) => self.call_expr(func, args).await?,
            PromExpr::Extension(expr) => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported Extension: {expr:?}"
                )));
            }
        })
    }

    /// Instant vector selector --- select a single sample at each evaluation
    /// timestamp.
    ///
    /// See <https://promlabs.com/blog/2020/07/02/selecting-data-in-promql/#confusion-alert-instantrange-selectors-vs-instantrange-queries>
    async fn eval_vector_selector(
        &mut self,
        selector: &VectorSelector,
    ) -> Result<Vec<InstantValue>> {
        if self.result_type.is_none() {
            self.result_type = Some("vector".to_string());
        }

        let mut selector = selector.clone();
        if selector.name.is_none() {
            let name = selector
                .matchers
                .find_matchers(NAME_LABEL)
                .first()
                .unwrap()
                .value
                .clone();

            selector.name = Some(name);
        }

        let data_cache_key = &selector.to_string();

        let cache_exists = {
            self.ctx
                .data_cache
                .read()
                .await
                .contains_key(data_cache_key)
        };
        if !cache_exists {
            self.selector_load_data(&selector, None).await?;
        }
        let metrics_cache = self.ctx.data_cache.read().await;
        let metrics_cache = match metrics_cache.get(data_cache_key) {
            Some(v) => match v.get_ref_matrix_values() {
                Some(v) => v,
                None => return Ok(vec![]),
            },
            None => return Ok(vec![]),
        };

        // Evaluation timestamp.
        let eval_ts = self.time;
        let start = eval_ts - self.ctx.lookback_delta;

        let mut offset_modifier: i64 = 0;
        if let Some(offset) = selector.offset {
            match offset {
                Offset::Pos(off) => {
                    offset_modifier = micros(off);
                }
                Offset::Neg(off) => {
                    offset_modifier = -micros(off);
                }
            }
        }

        let mut values = vec![];
        for metric in metrics_cache {
            let end_index = metric
                .samples
                .partition_point(|v| v.timestamp + offset_modifier <= eval_ts);
            let match_sample = if end_index > 0 {
                metric.samples.get(end_index - 1)
            } else if !metric.samples.is_empty() {
                metric.samples.first()
            } else {
                None
            };
            if let Some(sample) = match_sample
                && sample.timestamp + offset_modifier <= eval_ts
                && sample.timestamp + offset_modifier > start
            {
                let last_value = sample.value;
                values.push(
                    // See https://promlabs.com/blog/2020/06/18/the-anatomy-of-a-promql-query/#instant-queries
                    InstantValue {
                        labels: metric.labels.clone(),
                        sample: Sample::new(eval_ts, last_value),
                    },
                );
            }
        }
        Ok(values)
    }

    /// Range vector selector --- select a whole time range at each evaluation
    /// timestamp.
    ///
    /// See <https://promlabs.com/blog/2020/07/02/selecting-data-in-promql/#confusion-alert-instantrange-selectors-vs-instantrange-queries>
    ///
    /// MatrixSelector is a special case of VectorSelector that returns a matrix
    /// of samples.
    async fn eval_matrix_selector(
        &mut self,
        selector: &VectorSelector,
        range: Duration,
    ) -> Result<Vec<RangeValue>> {
        if self.result_type.is_none() {
            self.result_type = Some("matrix".to_string());
        }

        let mut selector = selector.clone();
        if selector.name.is_none() {
            let name = selector
                .matchers
                .find_matchers(NAME_LABEL)
                .first()
                .unwrap()
                .value
                .clone();

            selector.name = Some(name);
        }

        let data_cache_key = &selector.to_string();
        let cache_exists = {
            self.ctx
                .data_cache
                .read()
                .await
                .contains_key(data_cache_key)
        };
        if !cache_exists {
            self.selector_load_data(&selector, Some(range)).await?;
        }
        let metrics_cache = self.ctx.data_cache.read().await;
        let metrics_cache = match metrics_cache.get(data_cache_key) {
            Some(v) => match v.get_ref_matrix_values() {
                Some(v) => v,
                None => return Ok(vec![]),
            },
            None => return Ok(vec![]),
        };

        // Evaluation timestamp --- end of the time window.
        let eval_ts = self.time;
        // Start of the time window.
        let start = eval_ts - micros(range); // e.g. [5m]
        let mut offset_modifier = 0;
        if let Some(offset) = selector.offset {
            match offset {
                Offset::Pos(offset) => {
                    offset_modifier = micros(offset);
                }
                Offset::Neg(offset) => {
                    offset_modifier = -micros(offset);
                }
            }
        };

        let mut values = Vec::with_capacity(metrics_cache.len());
        for metric in metrics_cache {
            // use binary search to find the start and end index
            let start_index = metric
                .samples
                .partition_point(|v| v.timestamp + offset_modifier < start);
            let end_index = metric
                .samples
                .partition_point(|v| v.timestamp + offset_modifier <= eval_ts);
            let samples = metric.samples[start_index..end_index]
                .iter()
                .map(|v| Sample {
                    timestamp: v.timestamp + offset_modifier,
                    value: v.value,
                })
                .collect::<Vec<_>>();
            let exemplars = if self.ctx.query_exemplars {
                metric.exemplars.clone()
            } else {
                None
            };
            values.push(RangeValue {
                labels: metric.labels.clone(),
                samples,
                exemplars,
                time_window: Some(TimeWindow::new(eval_ts, range)),
            });
        }

        Ok(values)
    }

    #[tracing::instrument(name = "promql:engine:load_data", skip_all)]
    async fn selector_load_data(
        &mut self,
        selector: &VectorSelector,
        range: Option<Duration>,
    ) -> Result<()> {
        let data_cache_key = selector.to_string();
        let mut data_loaded = self.ctx.data_loading.lock().await;
        if data_loaded.contains(&data_cache_key) {
            return Ok(()); // data is already loading
        }

        let metrics = match self.selector_load_data_inner(selector, range).await {
            Ok(v) => v,
            Err(e) => {
                log::error!(
                    "[trace_id: {}] [PromQL] Failed to load data for stream: {data_cache_key}, error: {e:?}",
                    self.trace_id
                );
                data_loaded.insert(data_cache_key);
                return Err(e);
            }
        };

        // no data, return immediately
        if metrics.is_empty() {
            self.ctx
                .data_cache
                .write()
                .await
                .insert(data_cache_key.clone(), Value::None);
            data_loaded.insert(data_cache_key);
            return Ok(());
        }

        // cache data
        let mut metric_values = metrics.into_values().collect::<Vec<_>>();
        metric_values.par_iter_mut().for_each(|metric| {
            metric.samples.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            if self.ctx.query_exemplars && metric.exemplars.is_some() {
                metric
                    .exemplars
                    .as_mut()
                    .unwrap()
                    .sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            }
        });
        let values = if metric_values.is_empty() {
            Value::None
        } else {
            Value::Matrix(metric_values)
        };
        self.ctx
            .data_cache
            .write()
            .await
            .insert(data_cache_key.clone(), values);
        data_loaded.insert(data_cache_key);
        Ok(())
    }

    #[tracing::instrument(name = "promql:engine:load_data", skip_all)]
    async fn selector_load_data_inner(
        &self,
        selector: &VectorSelector,
        range: Option<Duration>,
    ) -> Result<HashMap<HashLabelValue, RangeValue>> {
        let start_time = std::time::Instant::now();
        // https://promlabs.com/blog/2020/07/02/selecting-data-in-promql/#lookback-delta
        let mut start = self.ctx.start - range.map_or(self.ctx.lookback_delta, micros);
        let mut end = self.ctx.end; // 30 minutes + 5m = 35m

        if let Some(offset) = selector.offset.clone() {
            match offset {
                Offset::Pos(offset) => {
                    start -= micros(offset);
                    end -= micros(offset);
                }
                Offset::Neg(offset) => {
                    start += micros(offset);
                    end += micros(offset);
                }
            }
        }

        // 1. Group by metrics (sets of label name-value pairs)
        let table_name = selector.name.as_ref().unwrap();
        log::info!(
            "[trace_id: {}] loading data for stream: {}, range: [{},{}), filter: {:?}",
            self.trace_id,
            table_name,
            start,
            end,
            selector.to_string(),
        );

        let mut filters = selector
            .matchers
            .matchers
            .iter()
            .filter_map(|mat| {
                if mat.op == MatchOp::Equal {
                    Some((mat.name.to_string(), vec![mat.value.to_string()]))
                } else {
                    None
                }
            })
            .collect::<Vec<(_, _)>>();
        let ctxs = self
            .ctx
            .table_provider
            .create_context(
                &self.ctx.org_id,
                table_name,
                (start, end),
                selector.matchers.clone(),
                self.col_filters.clone(),
                &mut filters,
            )
            .await?;

        let mut tasks = Vec::new();
        for (ctx, schema, scan_stats) in ctxs {
            let selector = selector.clone();
            let col_filters = &self.col_filters;
            let query_exemplars = self.ctx.query_exemplars;
            let trace_id = self.trace_id.to_string();
            let task = tokio::time::timeout(Duration::from_secs(self.ctx.timeout), async move {
                selector_load_data_from_datafusion(
                    &trace_id,
                    ctx,
                    schema,
                    selector,
                    start,
                    end,
                    col_filters,
                    query_exemplars,
                )
                .await
            });
            tasks.push(task);
            // update stats
            let mut ctx_scan_stats = self.ctx.scan_stats.write().await;
            ctx_scan_stats.add(&scan_stats);
        }
        let task_results = try_join_all(tasks)
            .await
            .map_err(|e| DataFusionError::Plan(format!("task error: {e}")))?;

        let mut metrics: HashMap<HashLabelValue, RangeValue> = HashMap::default();
        let task_results_len = task_results.len();
        for task_result in task_results {
            if task_results_len == 1 {
                // only one ctx, no need to merge, just set it to metrics
                metrics = task_result?;
                break;
            }
            for (key, value) in task_result? {
                if let Some(metric) = metrics.get_mut(&key) {
                    metric.extend(value);
                } else {
                    metrics.insert(key, value);
                }
            }
        }

        log::info!(
            "[trace_id: {}] load data done for stream: {}, took: {} ms",
            self.trace_id,
            table_name,
            start_time.elapsed().as_millis()
        );

        Ok(metrics)
    }

    async fn aggregate_exprs(
        &mut self,
        op: &token::TokenType,
        expr: &PromExpr,
        param: &Option<Box<PromExpr>>,
        modifier: &Option<LabelModifier>,
    ) -> Result<Value> {
        let sample_time = self.time;
        let input = self.exec_expr(expr).await?;

        Ok(match op.id() {
            token::T_SUM => aggregations::sum(sample_time, modifier, input)?,
            token::T_AVG => aggregations::avg(sample_time, modifier, input)?,
            token::T_COUNT => aggregations::count(sample_time, modifier, input)?,
            token::T_MIN => aggregations::min(sample_time, modifier, input)?,
            token::T_MAX => aggregations::max(sample_time, modifier, input)?,
            token::T_GROUP => aggregations::group(sample_time, modifier, input)?,
            token::T_STDDEV => aggregations::stddev(sample_time, modifier, input)?,
            token::T_STDVAR => aggregations::stdvar(sample_time, modifier, input)?,
            token::T_TOPK => {
                aggregations::topk(self, param.clone().unwrap(), modifier, input).await?
            }
            token::T_BOTTOMK => {
                aggregations::bottomk(self, param.clone().unwrap(), modifier, input).await?
            }
            token::T_COUNT_VALUES => {
                aggregations::count_values(
                    self,
                    sample_time,
                    param.clone().unwrap(),
                    modifier,
                    input,
                )
                .await?
            }
            token::T_QUANTILE => {
                aggregations::quantile(self, sample_time, param.clone().unwrap(), input).await?
            }
            _ => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported Aggregate: {op:?}"
                )));
            }
        })
    }

    async fn call_expr_first_arg(&mut self, args: &FunctionArgs) -> Result<Value> {
        self.exec_expr(args.args.first().expect("Missing arg 0"))
            .await
    }

    async fn call_expr_second_arg(&mut self, args: &FunctionArgs) -> Result<Value> {
        self.exec_expr(args.args.get(1).expect("Missing arg 1"))
            .await
    }

    async fn call_expr_third_arg(&mut self, args: &FunctionArgs) -> Result<Value> {
        self.exec_expr(args.args.get(2).expect("Missing arg 2"))
            .await
    }

    async fn call_expr_fourth_arg(&mut self, args: &FunctionArgs) -> Result<Value> {
        self.exec_expr(args.args.get(3).expect("Missing arg 3"))
            .await
    }

    async fn call_expr_fifth_arg(&mut self, args: &FunctionArgs) -> Result<Value> {
        self.exec_expr(args.args.get(4).expect("Missing arg 4"))
            .await
    }

    fn ensure_two_args(&self, args: &FunctionArgs, err: &str) -> Result<()> {
        if args.len() != 2 {
            return Err(DataFusionError::NotImplemented(err.into()));
        }
        Ok(())
    }

    fn ensure_three_args(&self, args: &FunctionArgs, err: &str) -> Result<()> {
        if args.len() != 3 {
            return Err(DataFusionError::NotImplemented(err.into()));
        }
        Ok(())
    }

    fn ensure_ge_three_args(&self, args: &FunctionArgs, err: &str) -> Result<()> {
        if args.len() < 3 {
            return Err(DataFusionError::NotImplemented(err.into()));
        }
        Ok(())
    }

    fn ensure_five_args(&self, args: &FunctionArgs, err: &str) -> Result<()> {
        if args.len() != 5 {
            return Err(DataFusionError::NotImplemented(err.into()));
        }
        Ok(())
    }

    fn parse_f64_else_err<T: Into<String>>(&self, value: &Value, err: T) -> Result<f64> {
        match value {
            Value::Float(f) => Ok(*f),
            _ => Err(DataFusionError::NotImplemented(err.into())),
        }
    }
    async fn call_expr(&mut self, func: &Function, args: &FunctionArgs) -> Result<Value> {
        use crate::service::promql::functions::Func;

        let func_name = Func::from_str(func.name).map_err(|_| {
            DataFusionError::NotImplemented(format!("Unsupported function: {}", func.name))
        })?;

        // There are a few functions which need no arguments for e.g. time()
        let functions_without_args: HashSet<&str> = HashSet::from_iter(vec![
            "day_of_month",
            "day_of_week",
            "day_of_year",
            "days_in_month",
            "hour",
            "minute",
            "month",
            "time",
            "year",
        ]);
        let input = match functions_without_args.contains(func.name) {
            true => match args.len() {
                0 => {
                    // Found no arg to pass to, lets use a `vector(time())` as the arg.
                    // https://prometheus.io/docs/prometheus/latest/querying/functions/#functions
                    let default_now_vector = vec![InstantValue {
                        labels: Labels::default(),
                        sample: Sample::new(self.time, self.time as f64),
                    }];
                    Value::Vector(default_now_vector)
                }
                1 => self.call_expr_first_arg(args).await?,

                _ => {
                    return Err(DataFusionError::NotImplemented(
                        "Invalid args passed to the function".into(),
                    ));
                }
            },
            false => {
                let last_arg = args
                    .last()
                    .expect("BUG: promql-parser should have validated function arguments");
                self.exec_expr(&last_arg).await?
            }
        };

        Ok(match func_name {
            Func::Abs => functions::abs(input)?,
            Func::Absent => functions::absent(input, self.time)?,
            Func::AbsentOverTime => functions::absent_over_time(input)?,
            Func::AvgOverTime => functions::avg_over_time(input)?,
            Func::Ceil => functions::ceil(input)?,
            Func::Changes => functions::changes(input)?,
            Func::Clamp => {
                let err =
                    "Invalid args, expected \"clamp(v instant-vector, min scalar, max scalar)\"";
                self.ensure_three_args(args, err)?;

                let input = self.call_expr_first_arg(args).await?;
                let min = self.call_expr_second_arg(args).await?;
                let max = self.call_expr_third_arg(args).await?;

                let (min_f, max_f) = match (min, max) {
                    (Value::Float(min), Value::Float(max)) => {
                        if min > max {
                            return Ok(Value::Vector(vec![]));
                        }
                        (min, max)
                    }
                    _ => {
                        return Err(DataFusionError::NotImplemented(err.into()));
                    }
                };
                functions::clamp(input, min_f, max_f)?
            }
            Func::ClampMax => {
                let err = "Invalid args, expected \"clamp(v instant-vector, max scalar)\"";
                self.ensure_two_args(args, err)?;

                let input = self.call_expr_first_arg(args).await?;
                let max = self.call_expr_second_arg(args).await?;
                let max_f = match max {
                    Value::Float(max) => max,
                    _ => {
                        return Err(DataFusionError::NotImplemented(err.into()));
                    }
                };
                functions::clamp(input, f64::MIN, max_f)?
            }
            Func::ClampMin => {
                let err = "Invalid args, expected \"clamp(v instant-vector, min scalar)\"";
                self.ensure_two_args(args, err)?;

                let input = self.call_expr_first_arg(args).await?;
                let min = self.call_expr_second_arg(args).await?;
                let min_f = match min {
                    Value::Float(min) => min,
                    _ => {
                        return Err(DataFusionError::NotImplemented(err.into()));
                    }
                };
                functions::clamp(input, min_f, f64::MAX)?
            }
            Func::CountOverTime => functions::count_over_time(input)?,
            Func::DayOfMonth => functions::day_of_month(input)?,
            Func::DayOfWeek => functions::day_of_week(input)?,
            Func::DayOfYear => functions::day_of_year(input)?,
            Func::DaysInMonth => functions::days_in_month(input)?,
            Func::Delta => functions::delta(input)?,
            Func::Deriv => functions::deriv(input)?,
            Func::Exp => functions::exp(input)?,
            Func::Floor => functions::floor(input)?,
            Func::HistogramCount => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported Function: {func_name:?}"
                )));
            }
            Func::HistogramFraction => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported Function: {func_name:?}"
                )));
            }
            Func::HistogramQuantile => {
                let args = &args.args;
                if args.len() != 2 {
                    return Err(DataFusionError::Plan(format!(
                        "{}: expected 2 arguments, got {}",
                        func.name,
                        args.len()
                    )));
                }
                let phi = {
                    match *args[0] {
                        PromExpr::NumberLiteral(ref num) => num.val,
                        _ => {
                            return Err(DataFusionError::Plan(format!(
                                "{}: the first argument must be a number",
                                func.name
                            )));
                        }
                    }
                };
                let sample_time = self.time;
                functions::histogram_quantile(sample_time, phi, input)?
            }
            Func::HistogramSum => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported Function: {func_name:?}"
                )));
            }
            Func::HoltWinters => {
                let err =
                    "Invalid args, expected \"holt_winters(v range-vector, sf scalar, tf scalar)\"";
                self.ensure_three_args(args, err)?;

                let input = self.call_expr_first_arg(args).await?;
                let sf = self.call_expr_second_arg(args).await?;
                let tf = self.call_expr_third_arg(args).await?;

                let scaling_factor = self.parse_f64_else_err(&sf, err)?;
                let trend_factor = self.parse_f64_else_err(&tf, err)?;

                functions::holt_winters(input, scaling_factor, trend_factor)?
            }
            Func::Hour => functions::hour(input)?,
            Func::Idelta => functions::idelta(input)?,
            Func::Increase => functions::increase(input)?,
            Func::Irate => functions::irate(input)?,
            Func::LabelJoin => {
                let err = "Invalid args, expected \"label_join(v instant-vector, dst string, sep string, src_1 string, src_2 string, ...)\"";
                self.ensure_ge_three_args(args, err)?;

                let input = self.call_expr_first_arg(args).await?;
                let dst_label = self.call_expr_second_arg(args).await?.get_string().ok_or(
                    DataFusionError::NotImplemented("Invalid destination label found".into()),
                )?;
                let separator = self.call_expr_third_arg(args).await?.get_string().ok_or(
                    DataFusionError::NotImplemented("Invalid separator label found".into()),
                )?;

                let mut source_labels = vec![];
                for each_src in args.args[3..].iter() {
                    if let Value::String(label) = self.exec_expr(each_src).await.unwrap() {
                        source_labels.push(label);
                    };
                }
                if source_labels.is_empty() {
                    return Err(DataFusionError::NotImplemented(
                        "source labels can not be empty or invalid".into(),
                    ));
                }
                functions::label_join(input, &dst_label, &separator, source_labels)?
            }
            Func::LabelReplace => {
                let err = "Invalid args, expected \"label_replace(v instant-vector, dst_label string, replacement string, src_label string, regex string)\"";

                self.ensure_five_args(args, err)?;
                let input = self.call_expr_first_arg(args).await?;

                let dst_label = self.call_expr_second_arg(args).await?.get_string().ok_or(
                    DataFusionError::NotImplemented("Invalid destination label found".into()),
                )?;
                let replacement = self.call_expr_third_arg(args).await?.get_string().ok_or(
                    DataFusionError::NotImplemented("Invalid replacement string found".into()),
                )?;

                let src_label = self.call_expr_fourth_arg(args).await?.get_string().ok_or(
                    DataFusionError::NotImplemented("Invalid source label string found".into()),
                )?;

                let regex = self.call_expr_fifth_arg(args).await?.get_string().ok_or(
                    DataFusionError::NotImplemented("Invalid regex string found".into()),
                )?;

                functions::label_replace(input, &dst_label, &replacement, &src_label, &regex)?
            }
            Func::LastOverTime => functions::last_over_time(input)?,
            Func::Ln => functions::ln(input)?,
            Func::Log10 => functions::log10(input)?,
            Func::Log2 => functions::log2(input)?,
            Func::MaxOverTime => functions::max_over_time(input)?,
            Func::MinOverTime => functions::min_over_time(input)?,
            Func::Minute => functions::minute(input)?,
            Func::Month => functions::month(input)?,
            Func::PredictLinear => {
                let err = "Invalid args, expected \"predict_linear(v range-vector, t scalar)\"";

                self.ensure_two_args(args, err)?;
                let input = self.call_expr_first_arg(args).await?;

                let prediction_steps = self.call_expr_second_arg(args).await?.get_float().ok_or(
                    DataFusionError::NotImplemented(
                        "Invalid prediction_steps, f64 expected".into(),
                    ),
                )?;
                functions::predict_linear(input, prediction_steps)?
            }
            Func::QuantileOverTime => {
                let err = "Invalid args, expected \"quantile_over_time(scalar, range-vector)\"";

                self.ensure_two_args(args, err)?;
                let phi_quantile = match self.call_expr_first_arg(args).await {
                    Ok(Value::Float(v)) => v,
                    _ => {
                        return Err(DataFusionError::Plan(
                            "[quantile] param must be a NumberLiteral".into(),
                        ));
                    }
                };
                let input = self.call_expr_second_arg(args).await?;
                functions::quantile_over_time(self.time, phi_quantile, input)?
            }
            Func::Rate => functions::rate(input)?,
            Func::Resets => functions::resets(input)?,
            Func::Round => functions::round(input)?,
            Func::Scalar => match input {
                Value::Float(_) => input,
                _ => {
                    return Err(DataFusionError::NotImplemented(format!(
                        "Invalid scalar value: {input:?}"
                    )));
                }
            },
            Func::Sgn => functions::sgn(input)?,
            Func::Sort => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported Function: {func_name:?}"
                )));
            }
            Func::SortDesc => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Unsupported Function: {func_name:?}"
                )));
            }
            Func::Sqrt => functions::sqrt(input)?,
            Func::StddevOverTime => functions::stddev_over_time(input)?,
            Func::StdvarOverTime => functions::stdvar_over_time(input)?,
            Func::SumOverTime => functions::sum_over_time(input)?,
            Func::Time => Value::Float((self.time / 1_000_000) as f64),
            Func::Timestamp => match input {
                Value::Vector(instant_value) => {
                    let out: Vec<InstantValue> = instant_value
                        .into_iter()
                        .map(|mut instant| InstantValue {
                            labels: std::mem::take(&mut instant.labels),
                            sample: Sample {
                                timestamp: instant.sample.timestamp,
                                value: (instant.sample.timestamp / 1000 / 1000) as f64,
                            },
                        })
                        .collect();
                    Value::Vector(out)
                }
                _ => {
                    return Err(DataFusionError::NotImplemented(format!(
                        "Unexpected input to timestamp function: {input:?}"
                    )));
                }
            },
            Func::Vector => functions::vector(input, self.time)?,
            Func::Year => functions::year(input)?,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn selector_load_data_from_datafusion(
    trace_id: &str,
    ctx: SessionContext,
    schema: Arc<Schema>,
    selector: VectorSelector,
    start: i64,
    end: i64,
    label_selector: &Option<HashSet<String>>,
    query_exemplars: bool,
) -> Result<HashMap<HashLabelValue, RangeValue>> {
    let cfg = config::get_config();
    let table_name = selector.name.as_ref().unwrap();
    let mut df_group = match ctx.table(table_name).await {
        Ok(v) => v.filter(
            col(TIMESTAMP_COL_NAME)
                .gt(lit(start))
                .and(col(TIMESTAMP_COL_NAME).lt_eq(lit(end))),
        )?,
        Err(_) => {
            return Ok(HashMap::default());
        }
    };

    df_group = apply_matchers(df_group, &schema, &selector.matchers)?;

    match apply_label_selector(df_group, &schema, label_selector) {
        Some(dataframe) => df_group = dataframe,
        None => return Ok(HashMap::default()),
    }

    // check if exemplars field is exists
    if query_exemplars {
        let schema: Schema = df_group.schema().into();
        if schema.field_with_name(EXEMPLARS_LABEL).is_err() {
            return Ok(HashMap::default());
        }
    }

    let label_cols = df_group
        .schema()
        .fields()
        .iter()
        .filter_map(|field| {
            let name = field.name();
            if name == TIMESTAMP_COL_NAME
                || name == VALUE_LABEL
                || name == EXEMPLARS_LABEL
                || name == NAME_LABEL
            {
                None
            } else {
                Some(col(name))
            }
        })
        .collect::<Vec<_>>();

    let max_series = if cfg.limit.metrics_max_series_per_query > 0 {
        cfg.limit.metrics_max_series_per_query
    } else {
        DEFAULT_MAX_SERIES_PER_QUERY
    };

    // get hash & timestamp
    let start_time = std::time::Instant::now();
    let sub_batch = df_group
        .clone()
        .aggregate(
            vec![col(HASH_LABEL)],
            vec![max(col(TIMESTAMP_COL_NAME)).alias(TIMESTAMP_COL_NAME)],
        )?
        .sort(vec![col(HASH_LABEL).sort(true, true)])?
        .limit(0, Some(max_series))?
        .collect()
        .await?;

    let hash_field_type = schema.field_with_name(HASH_LABEL).unwrap().data_type();
    let (mut timestamp_values, hash_value_set): (Vec<_>, HashSet<HashLabelValue>) =
        if hash_field_type == &DataType::UInt64 {
            sub_batch
                .iter()
                .flat_map(|batch| {
                    let ts = batch
                        .column_by_name(TIMESTAMP_COL_NAME)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap();
                    let hash = batch
                        .column_by_name(HASH_LABEL)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<UInt64Array>()
                        .unwrap();
                    ts.iter()
                        .zip(hash.iter())
                        .map(|(t, h)| (t.unwrap_or_default(), h.unwrap_or(0).into()))
                })
                .unzip()
        } else {
            sub_batch
                .iter()
                .flat_map(|batch| {
                    let ts = batch
                        .column_by_name(TIMESTAMP_COL_NAME)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap();
                    let hash = batch
                        .column_by_name(HASH_LABEL)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap();
                    ts.iter()
                        .zip(hash.iter())
                        .map(|(t, h)| (t.unwrap_or_default(), h.unwrap_or("").into()))
                })
                .unzip()
        };
    timestamp_values.sort();
    timestamp_values.dedup();
    let timestamp_values = timestamp_values.into_iter().map(lit).collect::<Vec<_>>();

    log::info!(
        "[trace_id: {trace_id}] load hashing took: {:?}",
        start_time.elapsed()
    );

    // get series
    let series = df_group
        .clone()
        .filter(col(TIMESTAMP_COL_NAME).in_list(timestamp_values, false))?
        .select(label_cols)?
        .collect()
        .await?;

    let mut metrics: HashMap<HashLabelValue, RangeValue> =
        HashMap::with_capacity(hash_value_set.len());
    for batch in series {
        let columns = batch.columns();
        let schema = batch.schema();
        let fields = schema.fields();
        let mut cols = fields
            .iter()
            .zip(columns)
            .filter_map(|(field, col)| {
                if field.name() == HASH_LABEL {
                    None
                } else {
                    col.as_any()
                        .downcast_ref::<StringArray>()
                        .map(|col| (field.name(), col))
                }
            })
            .collect::<Vec<(_, _)>>();
        cols.sort_by(|a, b| a.0.cmp(b.0));
        let mut labels = Vec::with_capacity(columns.len());
        if hash_field_type == &DataType::UInt64 {
            let hash_values = batch
                .column_by_name(HASH_LABEL)
                .unwrap()
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                let hash = hash_values.value(i).into();
                if !hash_value_set.contains(&hash) {
                    continue;
                }
                if metrics.contains_key(&hash) {
                    continue;
                }
                labels.clear(); // reset and reuse the same vector
                for (name, value) in cols.iter() {
                    if value.is_null(i) {
                        continue;
                    }
                    labels.push(Arc::new(Label {
                        name: name.to_string(),
                        value: value.value(i).to_string(),
                    }));
                }
                metrics.insert(hash, RangeValue::new(labels.clone(), Vec::new()));
            }
        } else {
            let hash_values = batch
                .column_by_name(HASH_LABEL)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for i in 0..batch.num_rows() {
                let hash = hash_values.value(i).into();
                if !hash_value_set.contains(&hash) {
                    continue;
                }
                if metrics.contains_key(&hash) {
                    continue;
                }
                labels.clear(); // reset and reuse the same vector
                for (name, value) in cols.iter() {
                    if value.is_null(i) {
                        continue;
                    }
                    labels.push(Arc::new(Label {
                        name: name.to_string(),
                        value: value.value(i).to_string(),
                    }));
                }
                metrics.insert(hash, RangeValue::new(labels.clone(), Vec::new()));
            }
        }
    }

    log::info!(
        "[trace_id: {trace_id}] load series took: {:?}",
        start_time.elapsed()
    );

    // get values
    if query_exemplars {
        load_exemplars_from_datafusion(trace_id, hash_field_type, &mut metrics, df_group).await?;
    } else {
        load_samples_from_datafusion(trace_id, hash_field_type, &mut metrics, df_group).await?;
    }

    log::info!(
        "[trace_id: {trace_id}] load data took: {:?}",
        start_time.elapsed()
    );

    Ok(metrics)
}

async fn load_samples_from_datafusion(
    trace_id: &str,
    hash_field_type: &DataType,
    metrics: &mut HashMap<HashLabelValue, RangeValue>,
    df: DataFrame,
) -> Result<()> {
    let start_time = std::time::Instant::now();
    let streams = df
        .select_columns(&[TIMESTAMP_COL_NAME, HASH_LABEL, VALUE_LABEL])?
        .execute_stream_partitioned()
        .await?;

    log::info!(
        "[trace_id: {trace_id}] load samples from datafusion took: {:?}",
        start_time.elapsed()
    );

    let mut tasks = Vec::new();
    for mut stream in streams {
        let hash_field_type = hash_field_type.clone();
        let mut series = metrics.clone();
        let task: tokio::task::JoinHandle<Result<HashMap<HashLabelValue, RangeValue>>> =
            tokio::task::spawn(async move {
                loop {
                    match stream.try_next().await {
                        Ok(Some(batch)) => {
                            let time_values = batch
                                .column_by_name(TIMESTAMP_COL_NAME)
                                .unwrap()
                                .as_any()
                                .downcast_ref::<Int64Array>()
                                .unwrap();
                            let value_values = batch
                                .column_by_name(VALUE_LABEL)
                                .unwrap()
                                .as_any()
                                .downcast_ref::<Float64Array>()
                                .unwrap();
                            if hash_field_type == DataType::UInt64 {
                                let hash_values = batch
                                    .column_by_name(HASH_LABEL)
                                    .unwrap()
                                    .as_any()
                                    .downcast_ref::<UInt64Array>()
                                    .unwrap();
                                for i in 0..batch.num_rows() {
                                    let hash: HashLabelValue = hash_values.value(i).into();
                                    if let Some(range_val) = series.get_mut(&hash) {
                                        range_val.samples.push(Sample::new(
                                            time_values.value(i),
                                            value_values.value(i),
                                        ));
                                    }
                                }
                            } else {
                                let hash_values = batch
                                    .column_by_name(HASH_LABEL)
                                    .unwrap()
                                    .as_any()
                                    .downcast_ref::<StringArray>()
                                    .unwrap();
                                for i in 0..batch.num_rows() {
                                    let hash: HashLabelValue = hash_values.value(i).into();
                                    if let Some(range_val) = series.get_mut(&hash) {
                                        range_val.samples.push(Sample::new(
                                            time_values.value(i),
                                            value_values.value(i),
                                        ));
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log::error!("load samples from datafusion execute stream Error: {e}");
                            return Err(e);
                        }
                    }
                }
                Ok(series)
            });
        tasks.push(task);
    }

    // collect results
    for task in tasks {
        let m = task
            .await
            .map_err(|e| DataFusionError::Execution(e.to_string()))??;
        for (hash, value) in m {
            if let Some(range_val) = metrics.get_mut(&hash) {
                range_val.samples.extend(value.samples);
            }
        }
    }

    log::info!(
        "[trace_id: {trace_id}] group batches took: {:?}",
        start_time.elapsed()
    );

    Ok(())
}

async fn load_exemplars_from_datafusion(
    trace_id: &str,
    hash_field_type: &DataType,
    metrics: &mut HashMap<HashLabelValue, RangeValue>,
    df: DataFrame,
) -> Result<()> {
    let start_time = std::time::Instant::now();
    let streams = df
        .filter(col(EXEMPLARS_LABEL).is_not_null())?
        .select_columns(&[HASH_LABEL, EXEMPLARS_LABEL])?
        .execute_stream_partitioned()
        .await?;

    log::info!(
        "[trace_id: {trace_id}] load exemplars from datafusion took: {:?}",
        start_time.elapsed()
    );

    let mut tasks = Vec::new();
    for mut stream in streams {
        let hash_field_type = hash_field_type.clone();
        let mut series = metrics.clone();
        let task: tokio::task::JoinHandle<Result<HashMap<HashLabelValue, RangeValue>>> =
            tokio::task::spawn(async move {
                loop {
                    match stream.try_next().await {
                        Ok(Some(batch)) => {
                            let exemplars_values = batch
                                .column_by_name(EXEMPLARS_LABEL)
                                .unwrap()
                                .as_any()
                                .downcast_ref::<StringArray>()
                                .unwrap();
                            if hash_field_type == DataType::UInt64 {
                                let hash_values = batch
                                    .column_by_name(HASH_LABEL)
                                    .unwrap()
                                    .as_any()
                                    .downcast_ref::<UInt64Array>()
                                    .unwrap();
                                for i in 0..batch.num_rows() {
                                    let hash: HashLabelValue = hash_values.value(i).into();
                                    let exemplar = exemplars_values.value(i);
                                    if let Some(range_val) = series.get_mut(&hash)
                                        && let Ok(exemplars) =
                                            json::from_str::<Vec<json::Value>>(exemplar)
                                    {
                                        for exemplar in exemplars {
                                            if let Some(exemplar) = exemplar.as_object() {
                                                if range_val.exemplars.is_none() {
                                                    range_val.exemplars = Some(vec![]);
                                                }
                                                range_val
                                                    .exemplars
                                                    .as_mut()
                                                    .unwrap()
                                                    .push(Arc::new(Exemplar::from(exemplar)));
                                            }
                                        }
                                    }
                                }
                            } else {
                                let hash_values = batch
                                    .column_by_name(HASH_LABEL)
                                    .unwrap()
                                    .as_any()
                                    .downcast_ref::<StringArray>()
                                    .unwrap();
                                for i in 0..batch.num_rows() {
                                    let hash: HashLabelValue = hash_values.value(i).into();
                                    let exemplar = exemplars_values.value(i);
                                    if let Some(range_val) = series.get_mut(&hash)
                                        && let Ok(exemplars) =
                                            json::from_str::<Vec<json::Value>>(exemplar)
                                    {
                                        for exemplar in exemplars {
                                            if let Some(exemplar) = exemplar.as_object() {
                                                if range_val.exemplars.is_none() {
                                                    range_val.exemplars = Some(vec![]);
                                                }
                                                range_val
                                                    .exemplars
                                                    .as_mut()
                                                    .unwrap()
                                                    .push(Arc::new(Exemplar::from(exemplar)));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log::error!("load exemplars from datafusion execute stream Error: {e}");
                            return Err(e);
                        }
                    }
                }
                Ok(series)
            });
        tasks.push(task);
    }

    // collect results
    for task in tasks {
        let m = task
            .await
            .map_err(|e| DataFusionError::Execution(e.to_string()))??;
        for (hash, value) in m {
            let Some(exemplars) = value.exemplars else {
                continue;
            };
            if let Some(range_val) = metrics.get_mut(&hash) {
                if range_val.exemplars.is_none() {
                    range_val.exemplars = Some(vec![]);
                }
                range_val.exemplars.as_mut().unwrap().extend(exemplars);
            }
        }
    }

    log::info!(
        "[trace_id: {trace_id}] group batches took: {:?}",
        start_time.elapsed()
    );

    Ok(())
}
