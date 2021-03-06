// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#![allow(clippy::op_ref)]
use differential_dataflow::lattice::Lattice;
use dogsdogsdogs::altneu::AltNeu;
use std::collections::HashSet;
use timely::dataflow::Scope;

use dataflow_types::DataflowError;
use expr::{JoinInputMapper, RelationExpr, ScalarExpr};
use repr::{Datum, Row, RowArena, Timestamp};

use super::context::{ArrangementFlavor, Context};
use crate::operator::CollectionExt;

impl<G> Context<G, RelationExpr, Row, Timestamp>
where
    G: Scope<Timestamp = Timestamp>,
{
    /// Renders `RelationExpr:Join` using dogs^3 delta query dataflows.
    pub fn render_delta_join<F>(
        &mut self,
        relation_expr: &RelationExpr,
        predicates: &[ScalarExpr],
        scope: &mut G,
        worker_index: usize,
        subtract: F,
    ) -> (Collection<G, Row>, Collection<G, DataflowError>)
    where
        F: Fn(&G::Timestamp) -> G::Timestamp + Clone + 'static,
    {
        if let RelationExpr::Join {
            inputs,
            equivalences,
            demand: _,
            implementation: expr::JoinImplementation::DeltaQuery(orders),
        } = relation_expr
        {
            for input in inputs.iter() {
                self.ensure_rendered(input, scope, worker_index);
            }

            // Collects error streams for the ambient scope.
            let mut scope_errs = Vec::new();

            // Deduplicate the error streams of multiply used arrangements.
            let mut local_err_dedup = HashSet::new();
            let mut trace_err_dedup = HashSet::new();

            // We'll need a new scope, to hold `AltNeu` wrappers, and we'll want
            // to import all traces as alt and neu variants (unless we do a more
            // careful analysis).
            let results =
                scope
                    .clone()
                    .scoped::<AltNeu<G::Timestamp>, _, _>("delta query", |inner| {
                        // Our plan is to iterate through each input relation, and attempt
                        // to find a plan that maximally uses existing keys (better: uses
                        // existing arrangements, to which we have access).
                        let mut delta_queries = Vec::new();

                        let input_mapper = JoinInputMapper::new(inputs);
                        // Collects error streams for the inner scope. Concats before leaving.
                        let mut inner_errs = Vec::with_capacity(inputs.len());
                        for relation in 0..inputs.len() {

                            // We maintain a private copy of `equivalences`, which we will digest
                            // as we produce the join.
                            let mut equivalences = equivalences.clone();
                            for equivalence in equivalences.iter_mut() {
                                equivalence.sort();
                                equivalence.dedup();
                            }

                            // This collection determines changes that result from updates inbound
                            // from `inputs[relation]` and reflects all strictly prior updates and
                            // concurrent updates from relations prior to `relation`.
                            let delta_query = inner.clone().region(|region| {

                                // Collects error streams for the region scope. Concats before leaving.
                                let mut region_errs = Vec::with_capacity(inputs.len());

                                // Ensure this input is rendered, and extract its update stream.
                                let (update_stream, errs) = self
                                    .collection(&inputs[relation])
                                    .expect("Failed to render update stream");
                                let update_stream = update_stream.enter(inner).enter(region);
                                scope_errs.push(errs);

                                // We track the sources of each column in our update stream.
                                let mut source_columns = input_mapper.global_columns(relation)
                                    .collect::<Vec<_>>();

                                let mut predicates = predicates.to_vec();
                                let (mut update_stream, errs) = build_filter(
                                    update_stream,
                                    &source_columns,
                                    &mut predicates,
                                    &mut equivalences,
                                );
                                if let Some(errs) = errs {
                                    region_errs.push(errs);
                                }

                                // We track the input relations as they are
                                // added to the join so we can figure out
                                // which expressions have been bound.
                                let mut bound_inputs = vec![relation];
                                // We use the order specified by the implementation.
                                let order = &orders[relation];

                                // Repeatedly update `update_stream` to reflect joins with more and more
                                // other relations, in the specified order.
                                for (other, next_key) in order.iter() {

                                    let next_key_rebased = next_key.iter().map(
                                        |k| input_mapper.map_expr_to_global(k.clone(), *other)
                                    ).collect::<Vec<_>>();

                                    // Keys for the incoming updates are determined by locating
                                    // the elements of `next_keys` among the existing `columns`.
                                    let prev_key = next_key_rebased
                                        .iter()
                                        .map(|expr| {
                                            let mut bound_expr = input_mapper
                                                .find_bound_expr(expr, &bound_inputs, &equivalences)
                                                .expect("Expression in join plan is not bound at time of use");

                                            bound_expr.visit_mut(&mut |e| if let ScalarExpr::Column(c) = e {
                                                *c = source_columns.iter().position(|x| x == c).expect("Did not find bound column in source_columns");
                                            });
                                            bound_expr
                                        })
                                        .collect::<Vec<_>>();

                                    // We should extract each element of `next_keys` from `equivalences`,
                                    // as each *should* now be a redundant constraint. We do this so that
                                    // the demand analysis does not require these columns be produced.
                                    for equivalence in equivalences.iter_mut() {
                                        equivalence.retain(|expr| !next_key_rebased.contains(expr));
                                    }
                                    equivalences.retain(|e| e.len() > 1);

                                    // TODO: Investigate demanded columns as in DifferentialLinear join.

                                    // We require different logic based on the flavor of arrangement.
                                    // We may need to cache each of these if we want to re-use the same wrapped
                                    // arrangement, rather than re-wrap each time we use a thing.
                                    let subtract = subtract.clone();
                                    let (oks, errs) = match self
                                        .arrangement(&inputs[*other], &next_key[..])
                                        .unwrap_or_else(|| {
                                            panic!(
                                                "Arrangement alarmingly absent!: {}, {:?}",
                                                inputs[*other].pretty(),
                                                &next_key[..]
                                            )
                                        }) {
                                        ArrangementFlavor::Local(oks, errs) => {
                                            if local_err_dedup.insert((&inputs[*other], &next_key[..])) {
                                                scope_errs.push(errs.as_collection(|k, _v| k.clone()));
                                            }
                                            if other > &relation {
                                                let oks = oks
                                                    .enter_at(
                                                        inner,
                                                        |_, _, t| AltNeu::alt(t.clone()),
                                                        move |t| subtract(&t.time),
                                                    )
                                                    .enter(region);
                                                build_lookup(update_stream, oks, prev_key)
                                            } else {
                                                let oks = oks
                                                    .enter_at(
                                                        inner,
                                                        |_, _, t| AltNeu::neu(t.clone()),
                                                        move |t| subtract(&t.time),
                                                    )
                                                    .enter(region);
                                                build_lookup(update_stream, oks, prev_key)
                                            }
                                        }
                                        ArrangementFlavor::Trace(_gid, oks, errs) => {
                                            if trace_err_dedup.insert((&inputs[*other], &next_key[..])) {
                                                scope_errs.push(errs.as_collection(|k, _v| k.clone()));
                                            }
                                            if other > &relation {
                                                let oks = oks
                                                    .enter_at(
                                                        inner,
                                                        |_, _, t| AltNeu::alt(t.clone()),
                                                        move |t| subtract(&t.time),
                                                    )
                                                    .enter(region);
                                                build_lookup(update_stream, oks, prev_key)
                                            } else {
                                                let oks = oks
                                                    .enter_at(
                                                        inner,
                                                        |_, _, t| AltNeu::neu(t.clone()),
                                                        move |t| subtract(&t.time),
                                                    )
                                                    .enter(region);
                                                build_lookup(update_stream, oks, prev_key)
                                            }
                                        }
                                    };
                                    update_stream = oks;
                                    region_errs.push(errs);

                                    // Update our map of the sources of each column in the update stream.
                                    source_columns
                                        .extend(input_mapper.global_columns(*other));

                                    let (oks, errs) = build_filter(
                                        update_stream,
                                        &source_columns,
                                        &mut predicates,
                                        &mut equivalences,
                                    );
                                    update_stream = oks;
                                    if let Some(errs) = errs {
                                        region_errs.push(errs);
                                    }

                                    bound_inputs.push(*other);
                                }

                                // We must now de-permute the results to return to the common order.
                                // TODO: Non-demanded columns would need default values here.
                                let permutation = (0 .. source_columns.len()).map(|c| {
                                    source_columns.iter().position(|x| &c == x).expect("Did not find required column in output")
                                }).collect::<Vec<_>>();
                                update_stream = update_stream.map({
                                    let mut row_packer = repr::RowPacker::new();
                                    move |row| {
                                        let datums = row.unpack();
                                        row_packer.pack(permutation.iter().map(|c| datums[*c]))
                                }});

                                inner_errs.push(differential_dataflow::collection::concatenate(region, region_errs).leave());
                                update_stream.leave()
                            });

                            delta_queries.push(delta_query);
                        }

                        scope_errs.push(differential_dataflow::collection::concatenate(inner, inner_errs).leave());

                        // Concatenate the results of each delta query as the accumulated results.
                        (
                            differential_dataflow::collection::concatenate(inner, delta_queries)
                                .leave(),
                            differential_dataflow::collection::concatenate(scope, scope_errs),
                        )
                    });
            results
        } else {
            panic!("delta_join invoke on non-delta join");
        }
    }
}

use differential_dataflow::operators::arrange::Arranged;
use differential_dataflow::trace::BatchReader;
use differential_dataflow::trace::Cursor;
use differential_dataflow::trace::TraceReader;
use differential_dataflow::Collection;

/// Constructs a `lookup_map` from supplied arguments.
///
/// This method exists to factor common logic from four code paths that are generic over the type of trace.
fn build_lookup<G, Tr>(
    updates: Collection<G, Row>,
    trace: Arranged<G, Tr>,
    prev_key: Vec<ScalarExpr>,
) -> (Collection<G, Row>, Collection<G, DataflowError>)
where
    G: Scope,
    G::Timestamp: Lattice,
    Tr: TraceReader<Time = G::Timestamp, Key = Row, Val = Row, R = isize> + Clone + 'static,
    Tr::Batch: BatchReader<Tr::Key, Tr::Val, Tr::Time, Tr::R>,
    Tr::Cursor: Cursor<Tr::Key, Tr::Val, Tr::Time, Tr::R>,
{
    let (updates, errs) = updates.map_fallible(move |row| {
        let datums = row.unpack();
        let temp_storage = RowArena::new();
        let row_key = Row::try_pack(prev_key.iter().map(|e| e.eval(&datums, &temp_storage)))?;
        Ok((row, row_key))
    });

    let oks = dogsdogsdogs::operators::lookup_map(
        &updates,
        trace,
        move |(_row, row_key), key| {
            // Prefix key selector must populate `key` with key from prefix `row`.
            *key = row_key.clone();
        },
        move |(prev_row, _prev_row_key), diff1, next_row, diff2| {
            // Output selector must produce (d_out, r_out) for each match.
            // TODO: We can improve this.
            let prev_datums = prev_row.unpack();
            let next_datums = next_row.unpack();
            // Append columns on to accumulated columns.
            (
                // TODO: This is a Fn closure and so cannot re-use a RowPacker.
                Row::pack(prev_datums.into_iter().chain(next_datums)),
                diff1 * diff2,
            )
        },
        // Three default values, for decoding keys into.
        Row::pack::<_, Datum>(None),
        Row::pack::<_, Datum>(None),
        Row::pack::<_, Datum>(None),
    );

    (oks, errs)
}

/// Filters updates on some columns by predicates that are ready to go.
///
/// Both the `predicates` and `equivalences` arguments will have all applied
/// predicates removed. Importantly, `equivalences` equates expressions with
/// the `Datum::eq` method, not `BinaryFunc::eq` which does not equate `Null`.
pub fn build_filter<G>(
    updates: Collection<G, Row>,
    source_columns: &[usize],
    predicates: &mut Vec<ScalarExpr>,
    equivalences: &mut Vec<Vec<ScalarExpr>>,
) -> (Collection<G, Row>, Option<Collection<G, DataflowError>>)
where
    G: Scope,
    G::Timestamp: Lattice,
{
    let mut ready_to_go = Vec::new();

    // Extract predicates fully supported by available columns.
    predicates.retain(|p| {
        if p.support().into_iter().all(|c| source_columns.contains(&c)) {
            ready_to_go.push(p.clone());
            false
        } else {
            true
        }
    });
    // Extract equivalences fully supported by available columns.
    // This only happens if at least *two* expressions are fully supported.
    // Importantly, we should *not* use `BinaryFunc::Eq` to compare these
    // terms, as this would cause `Datum::Null` to not match.
    let mut ready_equivalences = Vec::new();
    for equivalence in equivalences.iter_mut() {
        if let Some(pos) = equivalence
            .iter()
            .position(|e| e.support().into_iter().all(|c| source_columns.contains(&c)))
        {
            let mut should_equate = Vec::new();
            let mut cursor = pos + 1;
            while cursor < equivalence.len() {
                if equivalence[cursor]
                    .support()
                    .into_iter()
                    .all(|c| source_columns.contains(&c))
                {
                    // Remove expression and equate with the first bound expression.
                    should_equate.push(equivalence.remove(cursor));
                } else {
                    cursor += 1;
                }
            }
            if !should_equate.is_empty() {
                should_equate.push(equivalence[pos].clone());
                ready_equivalences.push(should_equate);
            }
        }
    }
    equivalences.retain(|e| e.len() > 1);

    // Rewrite column references to their locations under `source_columns`.
    for expr in ready_to_go.iter_mut() {
        expr.visit_mut(&mut |e| {
            if let ScalarExpr::Column(c) = e {
                *c = source_columns
                    .iter()
                    .position(|x| x == c)
                    .expect("Column not found in source_columns");
            }
        })
    }
    for exprs in ready_equivalences.iter_mut() {
        for expr in exprs.iter_mut() {
            expr.visit_mut(&mut |e| {
                if let ScalarExpr::Column(c) = e {
                    *c = source_columns
                        .iter()
                        .position(|x| x == c)
                        .expect("Column not found in source_columns");
                }
            });
        }
    }

    // Apply a filter if either list of constraints is non-empty.
    if ready_to_go.is_empty() && ready_equivalences.is_empty() {
        (updates, None)
    } else {
        let (ok_collection, err_collection) = updates.filter_fallible(move |input_row| {
            let temp_storage = repr::RowArena::new();
            let datums = input_row.unpack();
            for p in &ready_to_go {
                if p.eval(&datums, &temp_storage)? != Datum::True {
                    return Ok(false);
                }
            }
            for exprs in &ready_equivalences {
                // Each list of expressions should be equal to the same value.
                let val = exprs[0].eval(&datums, &temp_storage)?;
                for expr in exprs[1..].iter() {
                    if expr.eval(&datums, &temp_storage)? != val {
                        return Ok(false);
                    }
                }
            }
            Ok::<_, DataflowError>(true)
        });
        (ok_collection, Some(err_collection))
    }
}
