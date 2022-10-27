// Copyright 2021 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_datablocks::DataBlock;
use common_datavalues::prelude::*;
use common_exception::ErrorCode;
use common_exception::Result;
use common_functions::scalars::check_pattern_type;
use common_functions::scalars::FunctionContext;
use common_functions::scalars::FunctionFactory;
use common_functions::scalars::PatternType;
use common_fuse_meta::meta::StatisticsOfColumns;
use common_pipeline_transforms::processors::transforms::ExpressionExecutor;
use common_planner::PhysicalScalar;
use common_sql::evaluator::EvalNode;
use common_sql::evaluator::Evaluator;
use common_sql::evaluator::PhysicalScalarOp;

#[derive(Clone)]
pub struct RangeFilter {
    origin: DataSchemaRef,
    schema: DataSchemaRef,
    executor: EvalNode,
    stat_columns: StatColumns,
    func_ctx: FunctionContext,
}

impl RangeFilter {
    pub fn try_create(
        ctx: Arc<dyn TableContext>,
        exprs: &[PhysicalScalar],
        schema: DataSchemaRef,
    ) -> Result<Self> {
        debug_assert!(!exprs.is_empty());
        let mut stat_columns: StatColumns = Vec::new();
        let verifiable_expr = exprs
            .iter()
            .fold(None, |acc: Option<PhysicalScalar>, expr| {
                let verifiable_expr = build_verifiable_expr(expr, &schema, &mut stat_columns);
                match acc {
                    Some(acc) => Some(acc.and(&verifiable_expr).unwrap()),
                    None => Some(verifiable_expr),
                }
            })
            .unwrap();

        let executor = Evaluator::eval_physical_scalar(&verifiable_expr)?;
        Ok(Self {
            origin: schema,
            schema: input_schema,
            executor,
            stat_columns,
            func_ctx,
        })
    }

    pub fn try_eval_const(&self) -> Result<bool> {
        if !self.stat_columns.is_empty() {
            return Err(ErrorCode::LogicalError(
                "Constant folding requires the args are constant",
            ));
        }

        let input_fields = vec![DataField::new("_dummy", u8::to_data_type())];
        let input_schema = Arc::new(DataSchema::new(input_fields));
        let const_col = ConstColumn::new(Series::from_data(vec![1u8]), 1);
        let dummy_columns = vec![Arc::new(const_col) as ColumnRef];
        let data_block = DataBlock::create(input_schema, dummy_columns);

        let executed_data_block = self.executor.eval(&func_ctx, &data_block)?;

        match executed_data_block.vector.get(0) {
            DataValue::Null => Ok(false),
            other => other.as_bool(),
        }
    }

    #[tracing::instrument(level = "debug", name = "range_filter_eval", skip_all)]
    pub fn eval(&self, stats: &StatisticsOfColumns, row_count: u64) -> Result<bool> {
        let mut columns = Vec::with_capacity(self.stat_columns.len());
        for col in self.stat_columns.iter() {
            if col.stat_type == StatType::RowCount {
                columns.push(Series::from_data(vec![row_count]));
            } else {
                let val_opt = col.apply_stat_value(stats, self.origin.clone())?;
                if val_opt.is_none() {
                    return Ok(true);
                }
                columns.push(val_opt.unwrap());
            }
        }
        let data_block = DataBlock::create(self.schema.clone(), columns);
        let executed_data_block = self.executor.eval(&self.func_ctx, &data_block)?;

        match executed_data_block.vector.get(0) {
            DataValue::Null => Ok(false),
            other => other.as_bool(),
        }
    }
}

/// convert expr to Verifiable Expression
/// Rules: (section 5.2 of http://vldb.org/pvldb/vol14/p3083-edara.pdf)
pub fn build_verifiable_expr(
    expr: &PhysicalScalar,
    schema: &DataSchemaRef,
    stat_columns: &mut StatColumns,
) -> PhysicalScalar {
    let unhandled = PhysicalScalar::Constant {
        value: DataValue::Boolean(true),
        data_type: bool::to_data_type(),
    };

    // TODO(sundy)
    todo!()
    // VerifiableExprBuilder::try_create(exprs, op.to_lowercase().as_str(), schema, stat_columns)
        // .map_or(unhandled.clone(), |mut v| v.build().unwrap_or(unhandled))
}

fn inverse_operator(op: &str) -> Result<&str> {
    match op {
        "<" => Ok(">"),
        "<=" => Ok(">="),
        ">" => Ok("<"),
        ">=" => Ok("<="),
        "like" | "not like" | "ilike" | "not ilike" => Err(ErrorCode::UnknownException(format!(
            "cannot inverse the operator: {:?}",
            op
        ))),
        _ => Ok(op),
    }
}

/// Try to convert `not(is_not_null)` to `is_null`.
// TODO(sundy)
// fn try_convert_is_null(op: &str, args: Vec<PhysicalScalar>) -> (Vec<PhysicalScalar>, String) {
//     // `is null` will be converted to `not(is not null)` in the parser.
//     // we should convert it back to `is null` here.
//     if op == "not" && args.len() == 1 {
//         if let PhysicalScalar::ScalarFunction {
//             op: inner_op,
//             args: inner_args,
//         } = &args[0]
//         {
//             if inner_op == "is_not_null" {
//                 return (inner_args.clone(), String::from("is_null"));
//             }
//         }
//     }
//     (args, String::from(op))
// }

#[derive(Debug, Copy, Clone, PartialEq)]
enum StatType {
    Min,
    Max,
    Nulls,
    RowCount,
}

impl fmt::Display for StatType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StatType::Min => write!(f, "min"),
            StatType::Max => write!(f, "max"),
            StatType::Nulls => write!(f, "nulls"),
            StatType::RowCount => write!(f, "row_count"),
        }
    }
}

pub type StatColumns = Vec<StatColumn>;
pub type ColumnFields = HashMap<u32, DataField>;

#[derive(Debug, Clone)]
pub struct StatColumn {
    column_fields: ColumnFields,
    stat_type: StatType,
    stat_field: DataField,
    expr: PhysicalScalar,
}

impl StatColumn {
    fn create(
        column_fields: ColumnFields,
        stat_type: StatType,
        field: &DataField,
        expr: PhysicalScalar,
    ) -> Self {
        let column_new = format!("{}_{}", stat_type, field.name());
        let data_type = if matches!(stat_type, StatType::Nulls | StatType::RowCount) {
            u64::to_data_type()
        } else {
            field.data_type().clone()
        };
        let stat_field = DataField::new(column_new.as_str(), data_type);

        Self {
            column_fields,
            stat_type,
            stat_field,
            expr,
        }
    }

    fn apply_stat_value(
        &self,
        stats: &StatisticsOfColumns,
        schema: DataSchemaRef,
    ) -> Result<Option<ColumnRef>> {
        if self.stat_type == StatType::Nulls {
            // The len of column_fields is 1.
            let (k, _) = self.column_fields.iter().next().unwrap();
            let stat = stats.get(k).ok_or_else(|| {
                ErrorCode::UnknownException(format!(
                    "Unable to get the colStats by ColumnId: {}",
                    k
                ))
            })?;
            return Ok(Some(Series::from_data(vec![stat.null_count])));
        }

        let mut single_point = true;
        let mut variables = HashMap::with_capacity(self.column_fields.len());
        for (k, v) in &self.column_fields {
            let stat = stats.get(k).ok_or_else(|| {
                ErrorCode::UnknownException(format!(
                    "Unable to get the colStats by ColumnId: {}",
                    k
                ))
            })?;

            if single_point && stat.min != stat.max {
                single_point = false;
            }

            let min_col = v.data_type().create_constant_column(&stat.min, 1)?;
            let variable_left = Some(ColumnWithField::new(min_col, v.clone()));

            let max_col = v.data_type().create_constant_column(&stat.max, 1)?;
            let variable_right = Some(ColumnWithField::new(max_col, v.clone()));
            variables.insert(v.name().clone(), (variable_left, variable_right));
        }

        let monotonicity = ExpressionMonotonicityVisitor::check_expression(
            schema,
            &self.expr,
            variables,
            single_point,
        );
        if !monotonicity.is_monotonic {
            return Ok(None);
        }

        let column_with_field_opt = match self.stat_type {
            StatType::Min => {
                if monotonicity.is_positive {
                    monotonicity.left
                } else {
                    monotonicity.right
                }
            }
            StatType::Max => {
                if monotonicity.is_positive {
                    monotonicity.right
                } else {
                    monotonicity.left
                }
            }
            _ => unreachable!(),
        };

        Ok(column_with_field_opt.map(|v| v.column().slice(0, 1)))
    }
}

struct VerifiableExprBuilder<'a> {
    op: &'a str,
    args: Vec<PhysicalScalar>,
    fields: Vec<(DataField, ColumnFields)>,
    stat_columns: &'a mut StatColumns,
}

impl<'a> VerifiableExprBuilder<'a> {
    fn try_create(
        exprs: Vec<PhysicalScalar>,
        op: &'a str,
        schema: &'a DataSchemaRef,
        stat_columns: &'a mut StatColumns,
    ) -> Result<Self> {
        // TODO(sundy)
        todo!()
    }

    fn build(&mut self) -> Result<PhysicalScalar> {
        // TODO: support in/not in.
        match self.op {
            "is_null" => {
                // should_keep: col.null_count > 0
                let nulls_expr = self.nulls_column_expr(0)?;
                let scalar_expr = PhysicalScalar::Constant { value: DataValue::UInt64(0), data_type: u64::to_data_type() };
                nulls_expr.gt(&scalar_expr)
            }
            "is_not_null" => {
                // should_keep: col.null_count != col.row_count
                let nulls_expr = self.nulls_column_expr(0)?;
                let row_count_expr = self.row_count_column_expr(0)?;
                nulls_expr.not_eq(&row_count_expr)
            }
            "=" => {
                // left = right => min_left <= max_right and max_left >= min_right
                let left_min = self.min_column_expr(0)?;
                let left_max = self.max_column_expr(0)?;

                let right_min = if self.fields.len() == 1 {
                    self.args[1].clone()
                } else {
                    self.min_column_expr(1)?
                };
                let right_max = if self.fields.len() == 1 {
                    self.args[1].clone()
                } else {
                    self.max_column_expr(1)?
                };

                left_min
                    .lt_eq(&right_max)?
                    .and(&left_max.gt_eq(&right_min)?)
            }
            "<>" | "!=" => {
                let left_min = self.min_column_expr(0)?;
                let left_max = self.max_column_expr(0)?;
                left_min
                    .not_eq(&self.args[1])?
                    .or(&left_max.not_eq(&self.args[1])?)
            }
            ">" => {
                // left > right => max_left > min_right
                let left_max = self.max_column_expr(0)?;

                let right_min = if self.fields.len() == 1 {
                    self.args[1].clone()
                } else {
                    self.min_column_expr(1)?
                };

                left_max.gt(&right_min)
            }
            ">=" => {
                // left >= right => max_left >= min_right
                let left_max = self.max_column_expr(0)?;

                let right_min = if self.fields.len() == 1 {
                    self.args[1].clone()
                } else {
                    self.min_column_expr(1)?
                };

                left_max.gt_eq(&right_min)
            }
            "<" => {
                // left < right => min_left < max_right
                let left_min = self.min_column_expr(0)?;

                let right_max = if self.fields.len() == 1 {
                    self.args[1].clone()
                } else {
                    self.max_column_expr(1)?
                };

                left_min.lt(&right_max)
            }
            "<=" => {
                // left <= right => min_left <= max_right
                let left_min = self.min_column_expr(0)?;

                let right_max = if self.fields.len() == 1 {
                    self.args[1].clone()
                } else {
                    self.max_column_expr(1)?
                };

                left_min.lt_eq(&right_max)
            }
            "like" => {
                if let PhysicalScalar::Constant {
                    value: DataValue::String(v),
                    ..
                } = &self.args[1]
                {
                    // e.g. col like 'a%' => max_col >= 'a' and min_col < 'b'
                    let left = left_bound_for_like_pattern(v);
                    if !left.is_empty() {
                        let left_scalar = PhysicalScalar::Constant {
                            value: DataValue::String(left),
                            data_type: Vu8::to_data_type(),
                        };
                        let right = right_bound_for_like_pattern(left.clone());
                        let right_scalar = PhysicalScalar::Constant {
                            value: DataValue::String(right),
                            data_type: Vu8::to_data_type(),
                        };

                        let max_expr = self.max_column_expr(0)?;
                        if right.is_empty() {
                            return max_expr.gt_eq(&left_scalar);
                        } else {
                            let min_expr = self.min_column_expr(0)?;
                            return max_expr
                                .gt_eq(&left_scalar)?
                                .and(&min_expr.lt(&right_scalar)?);
                        }
                    }
                }
                Err(ErrorCode::UnknownException(
                    "Cannot build atom expression by the operator: like",
                ))
            }
            "not like" => {
                if let PhysicalScalar::Constant {
                    value: DataValue::String(v),
                    ..
                } = &self.args[1]
                {
                    // Only support such as 'abc' or 'ab%'.
                    match check_pattern_type(v, true) {
                        // e.g. col not like 'abc' => min_col != 'abc' or max_col != 'abc'
                        PatternType::OrdinalStr => {
                            let const_arg = left_bound_for_like_pattern(v);
                            let const_arg_scalar = PhysicalScalar::Constant {
                                value: DataValue::String(const_arg),
                                data_type: Vu8::to_data_type(),
                            };

                            let max_expr = self.max_column_expr(0)?;
                            let min_expr = self.min_column_expr(0)?;

                            return min_expr
                                .not_eq(&const_arg_scalar)?
                                .or(&max_expr.not_eq(&const_arg_scalar)?);
                        }
                        // e.g. col not like 'ab%' => min_col < 'ab' or max_col >= 'ac'
                        PatternType::EndOfPercent => {
                            let left = left_bound_for_like_pattern(v);
                            if !left.is_empty() {
                                let right = right_bound_for_like_pattern(left.clone());

                                let left_scalar = PhysicalScalar::Constant {
                                    value: DataValue::String(left),
                                    data_type: Vu8::to_data_type(),
                                };

                                let right_scalar = PhysicalScalar::Constant {
                                    value: DataValue::String(right),
                                    data_type: Vu8::to_data_type(),
                                };

                                let min_expr = self.min_column_expr(0)?;
                                if right.is_empty() {
                                    return min_expr.lt(&left_scalar);
                                } else {
                                    let max_expr = self.max_column_expr(0)?;
                                    return min_expr
                                        .lt(&left_scalar)?
                                        .or(&max_expr.gt_eq(&right_scalar)?);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(ErrorCode::UnknownException(
                    "Cannot build atom expression by the operator: not like",
                ))
            }
            other => Err(ErrorCode::UnknownException(format!(
                "Cannot build atom expression by the operator: {:?}",
                other
            ))),
        }
    }

    fn stat_column_expr(&mut self, stat_type: StatType, index: usize) -> Result<PhysicalScalar> {
        let (data_field, column_fields) = self.fields[index].clone();
        let stat_col = StatColumn::create(
            column_fields,
            stat_type,
            &data_field,
            self.args[index].clone(),
        );
        if !self
            .stat_columns
            .iter()
            .any(|c| c.stat_type == stat_type && c.stat_field.name() == data_field.name())
        {
            self.stat_columns.push(stat_col.clone());
        }

        Ok(PhysicalScalar::IndexedVariable {
            index,
            data_type: stat_col.stat_field.data_type().clone(),
            display_name: stat_col.stat_field.name().to_string(),
        })
    }

    fn min_column_expr(&mut self, index: usize) -> Result<PhysicalScalar> {
        self.stat_column_expr(StatType::Min, index)
    }

    fn max_column_expr(&mut self, index: usize) -> Result<PhysicalScalar> {
        self.stat_column_expr(StatType::Max, index)
    }

    fn nulls_column_expr(&mut self, index: usize) -> Result<PhysicalScalar> {
        self.stat_column_expr(StatType::Nulls, index)
    }

    fn row_count_column_expr(&mut self, index: usize) -> Result<PhysicalScalar> {
        self.stat_column_expr(StatType::RowCount, index)
    }
}

fn is_like_pattern_escape(c: u8) -> bool {
    c == b'%' || c == b'_' || c == b'\\'
}

pub fn left_bound_for_like_pattern(pattern: &[u8]) -> Vec<u8> {
    let mut index = 0;
    let len = pattern.len();
    let mut prefix: Vec<u8> = Vec::with_capacity(len);
    while index < len {
        match pattern[index] {
            b'%' | b'_' => break,
            b'\\' => {
                if index < len - 1 {
                    index += 1;
                    if !is_like_pattern_escape(pattern[index]) {
                        prefix.push(pattern[index - 1]);
                    }
                }
            }
            _ => {}
        }
        prefix.push(pattern[index]);
        index += 1;
    }
    prefix
}

pub fn right_bound_for_like_pattern(prefix: Vec<u8>) -> Vec<u8> {
    let mut res = prefix;
    while !res.is_empty() && *res.last().unwrap() == u8::MAX {
        res.pop();
    }

    if !res.is_empty() {
        if let Some(last) = res.last_mut() {
            *last += 1;
        }
    }

    res
}

fn get_maybe_monotonic(op: &str, args: &Vec<PhysicalScalar>) -> Result<bool> {
    let factory = FunctionFactory::instance();
    let function_features = factory.get_features(op)?;
    if !function_features.maybe_monotonic {
        return Ok(false);
    }

    for arg in args {
        if !check_maybe_monotonic(&arg)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn check_maybe_monotonic(expr: &PhysicalScalar) -> Result<bool> {
    match expr {
        PhysicalScalar::Constant { .. } => Ok(true),
        PhysicalScalar::IndexedVariable { .. } => Ok(true),
        PhysicalScalar::Function { name, args, .. } => get_maybe_monotonic(name, args),
        PhysicalScalar::Cast { input, .. } => check_maybe_monotonic(expr),
        _ => Ok(false),
    }
}

fn get_column_fields(schema: &DataSchemaRef, cols: HashSet<String>) -> Result<ColumnFields> {
    let mut column_fields = HashMap::with_capacity(cols.len());
    for col in &cols {
        let (index, field) = schema
            .column_with_name(col.as_str())
            .ok_or_else(|| ErrorCode::UnknownException("Unable to find the column name"))?;
        column_fields.insert(index as u32, field.clone());
    }
    Ok(column_fields)
}
