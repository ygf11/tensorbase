// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Collection of utility functions that are leveraged by the query optimizer rules

use std::{collections::HashSet, sync::Arc};

use arrow::datatypes::Schema;

use super::optimizer::OptimizerRule;
use crate::execution::context::ExecutionProps;
use crate::logical_plan::{
    build_join_schema, Column, DFSchemaRef, Expr, LogicalPlan, LogicalPlanBuilder,
    Operator, Partitioning, PlanType, Recursion, StringifiedPlan, ToDFSchema,
};
use crate::prelude::lit;
use crate::scalar::ScalarValue;
use crate::{
    error::{DataFusionError, Result},
    logical_plan::ExpressionVisitor,
};

const CASE_EXPR_MARKER: &str = "__DATAFUSION_CASE_EXPR__";
const CASE_ELSE_MARKER: &str = "__DATAFUSION_CASE_ELSE__";
const WINDOW_PARTITION_MARKER: &str = "__DATAFUSION_WINDOW_PARTITION__";
const WINDOW_SORT_MARKER: &str = "__DATAFUSION_WINDOW_SORT__";

/// Recursively walk a list of expression trees, collecting the unique set of columns
/// referenced in the expression
pub fn exprlist_to_columns(expr: &[Expr], accum: &mut HashSet<Column>) -> Result<()> {
    for e in expr {
        expr_to_columns(e, accum)?;
    }
    Ok(())
}

/// Recursively walk an expression tree, collecting the unique set of column names
/// referenced in the expression
struct ColumnNameVisitor<'a> {
    accum: &'a mut HashSet<Column>,
}

impl ExpressionVisitor for ColumnNameVisitor<'_> {
    fn pre_visit(self, expr: &Expr) -> Result<Recursion<Self>> {
        match expr {
            Expr::Column(qc) => {
                self.accum.insert(qc.clone());
            }
            Expr::ScalarVariable(var_names) => {
                self.accum.insert(Column::from_name(var_names.join(".")));
            }
            Expr::Alias(_, _) => {}
            Expr::Literal(_) => {}
            Expr::BinaryExpr { .. } => {}
            Expr::Not(_) => {}
            Expr::IsNotNull(_) => {}
            Expr::IsNull(_) => {}
            Expr::Negative(_) => {}
            Expr::Between { .. } => {}
            Expr::Case { .. } => {}
            Expr::Cast { .. } => {}
            Expr::TryCast { .. } => {}
            Expr::Sort { .. } => {}
            Expr::ScalarFunction { .. } => {}
            Expr::ScalarUDF { .. } => {}
            Expr::WindowFunction { .. } => {}
            Expr::AggregateFunction { .. } => {}
            Expr::AggregateUDF { .. } => {}
            Expr::InList { .. } => {}
            Expr::Wildcard => {}
        }
        Ok(Recursion::Continue(self))
    }
}

/// Recursively walk an expression tree, collecting the unique set of columns
/// referenced in the expression
pub fn expr_to_columns(expr: &Expr, accum: &mut HashSet<Column>) -> Result<()> {
    expr.accept(ColumnNameVisitor { accum })?;
    Ok(())
}

/// Create a `LogicalPlan::Explain` node by running `optimizer` on the
/// input plan and capturing the resulting plan string
pub fn optimize_explain(
    optimizer: &impl OptimizerRule,
    verbose: bool,
    plan: &LogicalPlan,
    stringified_plans: &[StringifiedPlan],
    schema: &Schema,
    execution_props: &ExecutionProps,
) -> Result<LogicalPlan> {
    // These are the fields of LogicalPlan::Explain It might be nice
    // to transform that enum Variant into its own struct and avoid
    // passing the fields individually
    let plan = Arc::new(optimizer.optimize(plan, execution_props)?);
    let mut stringified_plans = stringified_plans.to_vec();
    let optimizer_name = optimizer.name().into();
    stringified_plans.push(StringifiedPlan::new(
        PlanType::OptimizedLogicalPlan { optimizer_name },
        format!("{:#?}", plan),
    ));
    Ok(LogicalPlan::Explain {
        verbose,
        plan,
        stringified_plans,
        schema: schema.clone().to_dfschema_ref()?,
    })
}

/// Convenience rule for writing optimizers: recursively invoke
/// optimize on plan's children and then return a node of the same
/// type. Useful for optimizer rules which want to leave the type
/// of plan unchanged but still apply to the children.
/// This also handles the case when the `plan` is a [`LogicalPlan::Explain`].
pub fn optimize_children(
    optimizer: &impl OptimizerRule,
    plan: &LogicalPlan,
    execution_props: &ExecutionProps,
) -> Result<LogicalPlan> {
    if let LogicalPlan::Explain {
        verbose,
        plan,
        stringified_plans,
        schema,
    } = plan
    {
        return optimize_explain(
            optimizer,
            *verbose,
            &*plan,
            stringified_plans,
            &schema.as_ref().to_owned().into(),
            execution_props,
        );
    }

    let new_exprs = plan.expressions();
    let new_inputs = plan
        .inputs()
        .into_iter()
        .map(|plan| optimizer.optimize(plan, execution_props))
        .collect::<Result<Vec<_>>>()?;

    from_plan(plan, &new_exprs, &new_inputs)
}

/// Returns a new logical plan based on the original one with inputs and expressions replaced
pub fn from_plan(
    plan: &LogicalPlan,
    expr: &[Expr],
    inputs: &[LogicalPlan],
) -> Result<LogicalPlan> {
    match plan {
        LogicalPlan::Projection { schema, .. } => Ok(LogicalPlan::Projection {
            expr: expr.to_vec(),
            input: Arc::new(inputs[0].clone()),
            schema: schema.clone(),
        }),
        LogicalPlan::Filter { .. } => Ok(LogicalPlan::Filter {
            predicate: expr[0].clone(),
            input: Arc::new(inputs[0].clone()),
        }),
        LogicalPlan::Repartition {
            partitioning_scheme,
            ..
        } => match partitioning_scheme {
            Partitioning::RoundRobinBatch(n) => Ok(LogicalPlan::Repartition {
                partitioning_scheme: Partitioning::RoundRobinBatch(*n),
                input: Arc::new(inputs[0].clone()),
            }),
            Partitioning::Hash(_, n) => Ok(LogicalPlan::Repartition {
                partitioning_scheme: Partitioning::Hash(expr.to_owned(), *n),
                input: Arc::new(inputs[0].clone()),
            }),
        },
        LogicalPlan::Window {
            window_expr,
            schema,
            ..
        } => Ok(LogicalPlan::Window {
            input: Arc::new(inputs[0].clone()),
            window_expr: expr[0..window_expr.len()].to_vec(),
            schema: schema.clone(),
        }),
        LogicalPlan::Aggregate {
            group_expr, schema, ..
        } => Ok(LogicalPlan::Aggregate {
            group_expr: expr[0..group_expr.len()].to_vec(),
            aggr_expr: expr[group_expr.len()..].to_vec(),
            input: Arc::new(inputs[0].clone()),
            schema: schema.clone(),
        }),
        LogicalPlan::Sort { .. } => Ok(LogicalPlan::Sort {
            expr: expr.to_vec(),
            input: Arc::new(inputs[0].clone()),
        }),
        LogicalPlan::Join {
            join_type,
            join_constraint,
            on,
            ..
        } => {
            let schema =
                build_join_schema(inputs[0].schema(), inputs[1].schema(), join_type)?;
            Ok(LogicalPlan::Join {
                left: Arc::new(inputs[0].clone()),
                right: Arc::new(inputs[1].clone()),
                join_type: *join_type,
                join_constraint: *join_constraint,
                on: on.clone(),
                schema: DFSchemaRef::new(schema),
            })
        }
        LogicalPlan::CrossJoin { .. } => {
            let left = inputs[0].clone();
            let right = &inputs[1];
            LogicalPlanBuilder::from(left).cross_join(right)?.build()
        }
        LogicalPlan::Limit { n, .. } => Ok(LogicalPlan::Limit {
            n: *n,
            input: Arc::new(inputs[0].clone()),
        }),
        LogicalPlan::Extension { node } => Ok(LogicalPlan::Extension {
            node: node.from_template(expr, inputs),
        }),
        LogicalPlan::Union { schema, alias, .. } => Ok(LogicalPlan::Union {
            inputs: inputs.to_vec(),
            schema: schema.clone(),
            alias: alias.clone(),
        }),
        LogicalPlan::EmptyRelation { .. }
        | LogicalPlan::TableScan { .. }
        | LogicalPlan::CreateExternalTable { .. }
        | LogicalPlan::Explain { .. } => Ok(plan.clone()),
    }
}

/// Returns all direct children `Expression`s of `expr`.
/// E.g. if the expression is "(a + 1) + 1", it returns ["a + 1", "1"] (as Expr objects)
pub fn expr_sub_expressions(expr: &Expr) -> Result<Vec<Expr>> {
    match expr {
        Expr::BinaryExpr { left, right, .. } => {
            Ok(vec![left.as_ref().to_owned(), right.as_ref().to_owned()])
        }
        Expr::IsNull(e) => Ok(vec![e.as_ref().to_owned()]),
        Expr::IsNotNull(e) => Ok(vec![e.as_ref().to_owned()]),
        Expr::ScalarFunction { args, .. } => Ok(args.clone()),
        Expr::ScalarUDF { args, .. } => Ok(args.clone()),
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            let mut expr_list: Vec<Expr> = vec![];
            expr_list.extend(args.clone());
            expr_list.push(lit(WINDOW_PARTITION_MARKER));
            expr_list.extend(partition_by.clone());
            expr_list.push(lit(WINDOW_SORT_MARKER));
            expr_list.extend(order_by.clone());
            Ok(expr_list)
        }
        Expr::AggregateFunction { args, .. } => Ok(args.clone()),
        Expr::AggregateUDF { args, .. } => Ok(args.clone()),
        Expr::Case {
            expr,
            when_then_expr,
            else_expr,
            ..
        } => {
            let mut expr_list: Vec<Expr> = vec![];
            if let Some(e) = expr {
                expr_list.push(lit(CASE_EXPR_MARKER));
                expr_list.push(e.as_ref().to_owned());
            }
            for (w, t) in when_then_expr {
                expr_list.push(w.as_ref().to_owned());
                expr_list.push(t.as_ref().to_owned());
            }
            if let Some(e) = else_expr {
                expr_list.push(lit(CASE_ELSE_MARKER));
                expr_list.push(e.as_ref().to_owned());
            }
            Ok(expr_list)
        }
        Expr::Cast { expr, .. } => Ok(vec![expr.as_ref().to_owned()]),
        Expr::TryCast { expr, .. } => Ok(vec![expr.as_ref().to_owned()]),
        Expr::Column(_) => Ok(vec![]),
        Expr::Alias(expr, ..) => Ok(vec![expr.as_ref().to_owned()]),
        Expr::Literal(_) => Ok(vec![]),
        Expr::ScalarVariable(_) => Ok(vec![]),
        Expr::Not(expr) => Ok(vec![expr.as_ref().to_owned()]),
        Expr::Negative(expr) => Ok(vec![expr.as_ref().to_owned()]),
        Expr::Sort { expr, .. } => Ok(vec![expr.as_ref().to_owned()]),
        Expr::Between {
            expr, low, high, ..
        } => Ok(vec![
            expr.as_ref().to_owned(),
            low.as_ref().to_owned(),
            high.as_ref().to_owned(),
        ]),
        Expr::InList { expr, list, .. } => {
            let mut expr_list: Vec<Expr> = vec![expr.as_ref().to_owned()];
            for list_expr in list {
                expr_list.push(list_expr.to_owned());
            }
            Ok(expr_list)
        }
        Expr::Wildcard { .. } => Err(DataFusionError::Internal(
            "Wildcard expressions are not valid in a logical query plan".to_owned(),
        )),
    }
}

/// returns a new expression where the expressions in `expr` are replaced by the ones in
/// `expressions`.
/// This is used in conjunction with ``expr_expressions`` to re-write expressions.
pub fn rewrite_expression(expr: &Expr, expressions: &[Expr]) -> Result<Expr> {
    match expr {
        Expr::BinaryExpr { op, .. } => Ok(Expr::BinaryExpr {
            left: Box::new(expressions[0].clone()),
            op: *op,
            right: Box::new(expressions[1].clone()),
        }),
        Expr::IsNull(_) => Ok(Expr::IsNull(Box::new(expressions[0].clone()))),
        Expr::IsNotNull(_) => Ok(Expr::IsNotNull(Box::new(expressions[0].clone()))),
        Expr::ScalarFunction { fun, .. } => Ok(Expr::ScalarFunction {
            fun: fun.clone(),
            args: expressions.to_vec(),
        }),
        Expr::ScalarUDF { fun, .. } => Ok(Expr::ScalarUDF {
            fun: fun.clone(),
            args: expressions.to_vec(),
        }),
        Expr::WindowFunction {
            fun, window_frame, ..
        } => {
            let partition_index = expressions
                .iter()
                .position(|expr| {
                    matches!(expr, Expr::Literal(ScalarValue::Utf8(Some(str)))
            if str == WINDOW_PARTITION_MARKER)
                })
                .ok_or_else(|| {
                    DataFusionError::Internal(
                        "Ill-formed window function expressions: unexpected marker"
                            .to_owned(),
                    )
                })?;

            let sort_index = expressions
                .iter()
                .position(|expr| {
                    matches!(expr, Expr::Literal(ScalarValue::Utf8(Some(str)))
            if str == WINDOW_SORT_MARKER)
                })
                .ok_or_else(|| {
                    DataFusionError::Internal(
                        "Ill-formed window function expressions".to_owned(),
                    )
                })?;

            if partition_index >= sort_index {
                Err(DataFusionError::Internal(
                    "Ill-formed window function expressions: partition index too large"
                        .to_owned(),
                ))
            } else {
                Ok(Expr::WindowFunction {
                    fun: fun.clone(),
                    args: expressions[..partition_index].to_vec(),
                    partition_by: expressions[partition_index + 1..sort_index].to_vec(),
                    order_by: expressions[sort_index + 1..].to_vec(),
                    window_frame: *window_frame,
                })
            }
        }
        Expr::AggregateFunction { fun, distinct, .. } => Ok(Expr::AggregateFunction {
            fun: fun.clone(),
            args: expressions.to_vec(),
            distinct: *distinct,
        }),
        Expr::AggregateUDF { fun, .. } => Ok(Expr::AggregateUDF {
            fun: fun.clone(),
            args: expressions.to_vec(),
        }),
        Expr::Case { .. } => {
            let mut base_expr: Option<Box<Expr>> = None;
            let mut when_then: Vec<(Box<Expr>, Box<Expr>)> = vec![];
            let mut else_expr: Option<Box<Expr>> = None;
            let mut i = 0;

            while i < expressions.len() {
                match &expressions[i] {
                    Expr::Literal(ScalarValue::Utf8(Some(str)))
                        if str == CASE_EXPR_MARKER =>
                    {
                        base_expr = Some(Box::new(expressions[i + 1].clone()));
                        i += 2;
                    }
                    Expr::Literal(ScalarValue::Utf8(Some(str)))
                        if str == CASE_ELSE_MARKER =>
                    {
                        else_expr = Some(Box::new(expressions[i + 1].clone()));
                        i += 2;
                    }
                    _ => {
                        when_then.push((
                            Box::new(expressions[i].clone()),
                            Box::new(expressions[i + 1].clone()),
                        ));
                        i += 2;
                    }
                }
            }

            Ok(Expr::Case {
                expr: base_expr,
                when_then_expr: when_then,
                else_expr,
            })
        }
        Expr::Cast { data_type, .. } => Ok(Expr::Cast {
            expr: Box::new(expressions[0].clone()),
            data_type: data_type.clone(),
        }),
        Expr::TryCast { data_type, .. } => Ok(Expr::TryCast {
            expr: Box::new(expressions[0].clone()),
            data_type: data_type.clone(),
        }),
        Expr::Alias(_, alias) => {
            Ok(Expr::Alias(Box::new(expressions[0].clone()), alias.clone()))
        }
        Expr::Not(_) => Ok(Expr::Not(Box::new(expressions[0].clone()))),
        Expr::Negative(_) => Ok(Expr::Negative(Box::new(expressions[0].clone()))),
        Expr::Column(_) => Ok(expr.clone()),
        Expr::Literal(_) => Ok(expr.clone()),
        Expr::ScalarVariable(_) => Ok(expr.clone()),
        Expr::Sort {
            asc, nulls_first, ..
        } => Ok(Expr::Sort {
            expr: Box::new(expressions[0].clone()),
            asc: *asc,
            nulls_first: *nulls_first,
        }),
        Expr::Between { negated, .. } => {
            let expr = Expr::BinaryExpr {
                left: Box::new(Expr::BinaryExpr {
                    left: Box::new(expressions[0].clone()),
                    op: Operator::GtEq,
                    right: Box::new(expressions[1].clone()),
                }),
                op: Operator::And,
                right: Box::new(Expr::BinaryExpr {
                    left: Box::new(expressions[0].clone()),
                    op: Operator::LtEq,
                    right: Box::new(expressions[2].clone()),
                }),
            };

            if *negated {
                Ok(Expr::Not(Box::new(expr)))
            } else {
                Ok(expr)
            }
        }
        Expr::InList { .. } => Ok(expr.clone()),
        Expr::Wildcard { .. } => Err(DataFusionError::Internal(
            "Wildcard expressions are not valid in a logical query plan".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical_plan::{col, LogicalPlanBuilder};
    use arrow::datatypes::DataType;
    use std::collections::HashSet;

    #[test]
    fn test_collect_expr() -> Result<()> {
        let mut accum: HashSet<Column> = HashSet::new();
        expr_to_columns(
            &Expr::Cast {
                expr: Box::new(col("a")),
                data_type: DataType::Float64,
            },
            &mut accum,
        )?;
        expr_to_columns(
            &Expr::Cast {
                expr: Box::new(col("a")),
                data_type: DataType::Float64,
            },
            &mut accum,
        )?;
        assert_eq!(1, accum.len());
        assert!(accum.contains(&Column::from_name("a")));
        Ok(())
    }

    struct TestOptimizer {}

    impl OptimizerRule for TestOptimizer {
        fn optimize(
            &self,
            plan: &LogicalPlan,
            _: &ExecutionProps,
        ) -> Result<LogicalPlan> {
            Ok(plan.clone())
        }

        fn name(&self) -> &str {
            "test_optimizer"
        }
    }

    #[test]
    fn test_optimize_explain() -> Result<()> {
        let optimizer = TestOptimizer {};

        let empty_plan = LogicalPlanBuilder::empty(false).build()?;
        let schema = LogicalPlan::explain_schema();

        let optimized_explain = optimize_explain(
            &optimizer,
            true,
            &empty_plan,
            &[StringifiedPlan::new(PlanType::LogicalPlan, "...")],
            schema.as_ref(),
            &ExecutionProps::new(),
        )?;

        match &optimized_explain {
            LogicalPlan::Explain {
                verbose,
                stringified_plans,
                ..
            } => {
                assert!(*verbose);

                let expected_stringified_plans = vec![
                    StringifiedPlan::new(PlanType::LogicalPlan, "..."),
                    StringifiedPlan::new(
                        PlanType::OptimizedLogicalPlan {
                            optimizer_name: "test_optimizer".into(),
                        },
                        "EmptyRelation",
                    ),
                ];
                assert_eq!(*stringified_plans, expected_stringified_plans);
            }
            _ => panic!("Expected explain plan but got {:?}", optimized_explain),
        }

        Ok(())
    }
}
