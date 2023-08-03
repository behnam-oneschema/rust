mod cache;

use self::cache::ProvisionalEntry;
use super::inspect::ProofTreeBuilder;
use super::SolverMode;
use cache::ProvisionalCache;
use rustc_data_structures::fx::FxHashSet;
use rustc_index::Idx;
use rustc_index::IndexVec;
use rustc_middle::dep_graph::DepKind;
use rustc_middle::traits::solve::inspect::CacheHit;
use rustc_middle::traits::solve::CacheData;
use rustc_middle::traits::solve::{CanonicalInput, Certainty, EvaluationCache, QueryResult};
use rustc_middle::ty::TyCtxt;
use rustc_session::Limit;
use std::{collections::hash_map::Entry, mem};

rustc_index::newtype_index! {
    pub struct StackDepth {}
}

#[derive(Debug)]
struct StackEntry<'tcx> {
    input: CanonicalInput<'tcx>,
    available_depth: Limit,
    // The maximum depth reached by this stack entry, only up-to date
    // for the top of the stack and lazily updated for the rest.
    reached_depth: StackDepth,
    encountered_overflow: bool,
    has_been_used: bool,

    /// We put only the root goal of a coinductive cycle into the global cache.
    ///
    /// If we were to use that result when later trying to prove another cycle
    /// participant, we can end up with unstable query results.
    ///
    /// See tests/ui/new-solver/coinduction/incompleteness-unstable-result.rs for
    /// an example of where this is needed.
    cycle_participants: FxHashSet<CanonicalInput<'tcx>>,
}

pub(super) struct SearchGraph<'tcx> {
    mode: SolverMode,
    local_overflow_limit: usize,
    /// The stack of goals currently being computed.
    ///
    /// An element is *deeper* in the stack if its index is *lower*.
    stack: IndexVec<StackDepth, StackEntry<'tcx>>,
    provisional_cache: ProvisionalCache<'tcx>,
}

impl<'tcx> SearchGraph<'tcx> {
    pub(super) fn new(tcx: TyCtxt<'tcx>, mode: SolverMode) -> SearchGraph<'tcx> {
        Self {
            mode,
            local_overflow_limit: tcx.recursion_limit().0.ilog2() as usize,
            stack: Default::default(),
            provisional_cache: ProvisionalCache::empty(),
        }
    }

    pub(super) fn solver_mode(&self) -> SolverMode {
        self.mode
    }

    pub(super) fn local_overflow_limit(&self) -> usize {
        self.local_overflow_limit
    }

    /// Update the stack and reached depths on cache hits.
    #[instrument(level = "debug", skip(self))]
    fn on_cache_hit(&mut self, additional_depth: usize, encountered_overflow: bool) {
        let reached_depth = self.stack.next_index().plus(additional_depth);
        if let Some(last) = self.stack.raw.last_mut() {
            last.reached_depth = last.reached_depth.max(reached_depth);
            last.encountered_overflow |= encountered_overflow;
        }
    }

    /// Pops the highest goal from the stack, lazily updating the
    /// the next goal in the stack.
    ///
    /// Directly popping from the stack instead of using this method
    /// would cause us to not track overflow and recursion depth correctly.
    fn pop_stack(&mut self) -> StackEntry<'tcx> {
        let elem = self.stack.pop().unwrap();
        if let Some(last) = self.stack.raw.last_mut() {
            last.reached_depth = last.reached_depth.max(elem.reached_depth);
            last.encountered_overflow |= elem.encountered_overflow;
        }
        elem
    }

    /// The trait solver behavior is different for coherence
    /// so we use a separate cache. Alternatively we could use
    /// a single cache and share it between coherence and ordinary
    /// trait solving.
    pub(super) fn global_cache(&self, tcx: TyCtxt<'tcx>) -> &'tcx EvaluationCache<'tcx> {
        match self.mode {
            SolverMode::Normal => &tcx.new_solver_evaluation_cache,
            SolverMode::Coherence => &tcx.new_solver_coherence_evaluation_cache,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.stack.is_empty() && self.provisional_cache.is_empty()
    }

    /// Whether we're currently in a cycle. This should only be used
    /// for debug assertions.
    pub(super) fn in_cycle(&self) -> bool {
        if let Some(stack_depth) = self.stack.last_index() {
            // Either the current goal on the stack is the root of a cycle...
            if self.stack[stack_depth].has_been_used {
                return true;
            }

            // ...or it depends on a goal with a lower depth.
            let current_goal = self.stack[stack_depth].input;
            let entry_index = self.provisional_cache.lookup_table[&current_goal];
            self.provisional_cache.entries[entry_index].depth != stack_depth
        } else {
            false
        }
    }

    /// Fetches whether the current goal encountered overflow.
    ///
    /// This should only be used for the check in `evaluate_goal`.
    pub(super) fn encountered_overflow(&self) -> bool {
        if let Some(last) = self.stack.raw.last() { last.encountered_overflow } else { false }
    }

    /// Resets `encountered_overflow` of the current goal.
    ///
    /// This should only be used for the check in `evaluate_goal`.
    pub(super) fn reset_encountered_overflow(&mut self, encountered_overflow: bool) {
        if encountered_overflow {
            self.stack.raw.last_mut().unwrap().encountered_overflow = true;
        }
    }

    /// Returns the remaining depth allowed for nested goals.
    ///
    /// This is generally simply one less than the current depth.
    /// However, if we encountered overflow, we significantly reduce
    /// the remaining depth of all nested goals to prevent hangs
    /// in case there is exponential blowup.
    fn allowed_depth_for_nested(
        tcx: TyCtxt<'tcx>,
        stack: &IndexVec<StackDepth, StackEntry<'tcx>>,
    ) -> Option<Limit> {
        if let Some(last) = stack.raw.last() {
            if last.available_depth.0 == 0 {
                return None;
            }

            Some(if last.encountered_overflow {
                Limit(last.available_depth.0 / 4)
            } else {
                Limit(last.available_depth.0 - 1)
            })
        } else {
            Some(tcx.recursion_limit())
        }
    }

    /// Tries putting the new goal on the stack, returning an error if it is already cached.
    ///
    /// This correctly updates the provisional cache if there is a cycle.
    #[instrument(level = "debug", skip(self, tcx, inspect), ret)]
    fn try_push_stack(
        &mut self,
        tcx: TyCtxt<'tcx>,
        input: CanonicalInput<'tcx>,
        available_depth: Limit,
        inspect: &mut ProofTreeBuilder<'tcx>,
    ) -> Result<(), QueryResult<'tcx>> {
        // Look at the provisional cache to check for cycles.
        let cache = &mut self.provisional_cache;
        match cache.lookup_table.entry(input) {
            // No entry, simply push this goal on the stack.
            Entry::Vacant(v) => {
                let depth = self.stack.next_index();
                let entry = StackEntry {
                    input,
                    available_depth,
                    reached_depth: depth,
                    encountered_overflow: false,
                    has_been_used: false,
                    cycle_participants: Default::default(),
                };
                assert_eq!(self.stack.push(entry), depth);
                let response = Self::response_no_constraints(tcx, input, Certainty::Yes);
                let entry_index = cache.entries.push(ProvisionalEntry { response, depth, input });
                v.insert(entry_index);
                Ok(())
            }
            // We have a nested goal which relies on a goal `root` deeper in the stack.
            //
            // We first store that we may have to rerun `evaluate_goal` for `root` in case the
            // provisional response is not equal to the final response. We also update the depth
            // of all goals which recursively depend on our current goal to depend on `root`
            // instead.
            //
            // Finally we can return either the provisional response for that goal if we have a
            // coinductive cycle or an ambiguous result if the cycle is inductive.
            Entry::Occupied(entry_index) => {
                inspect.cache_hit(CacheHit::Provisional);

                let entry_index = *entry_index.get();

                let stack_depth = cache.depth(entry_index);
                debug!("encountered cycle with depth {stack_depth:?}");

                cache.add_dependency_of_leaf_on(entry_index);
                let mut iter = self.stack.iter_mut();
                let root = iter.nth(stack_depth.as_usize()).unwrap();
                for e in iter {
                    root.cycle_participants.insert(e.input);
                }

                // NOTE: The goals on the stack aren't the only goals involved in this cycle.
                // We can also depend on goals which aren't part of the stack but coinductively
                // depend on the stack themselves. We already checked whether all the goals
                // between these goals and their root on the stack. This means that as long as
                // each goal in a cycle is checked for coinductivity by itself, simply checking
                // the stack is enough.
                if self.stack.raw[stack_depth.index()..]
                    .iter()
                    .all(|g| g.input.value.goal.predicate.is_coinductive(tcx))
                {
                    // If we're in a coinductive cycle, we have to retry proving the current goal
                    // until we reach a fixpoint.
                    self.stack[stack_depth].has_been_used = true;
                    Err(cache.provisional_result(entry_index))
                } else {
                    Err(Self::response_no_constraints(tcx, input, Certainty::OVERFLOW))
                }
            }
        }
    }

    /// We cannot simply store the result of [super::EvalCtxt::compute_goal] as we have to deal with
    /// coinductive cycles.
    ///
    /// When we encounter a coinductive cycle, we have to prove the final result of that cycle
    /// while we are still computing that result. Because of this we continuously recompute the
    /// cycle until the result of the previous iteration is equal to the final result, at which
    /// point we are done.
    ///
    /// This function returns `true` if we were able to finalize the goal and `false` if it has
    /// updated the provisional cache and we have to recompute the current goal.
    ///
    /// FIXME: Refer to the rustc-dev-guide entry once it exists.
    #[instrument(level = "debug", skip(self, actual_input), ret)]
    fn try_finalize_goal(
        &mut self,
        actual_input: CanonicalInput<'tcx>,
        response: QueryResult<'tcx>,
    ) -> Result<StackEntry<'tcx>, ()> {
        let stack_entry = self.pop_stack();
        assert_eq!(stack_entry.input, actual_input);

        let cache = &mut self.provisional_cache;
        let provisional_entry_index = *cache.lookup_table.get(&stack_entry.input).unwrap();
        let provisional_entry = &mut cache.entries[provisional_entry_index];
        // We eagerly update the response in the cache here. If we have to reevaluate
        // this goal we use the new response when hitting a cycle, and we definitely
        // want to access the final response whenever we look at the cache.
        let prev_response = mem::replace(&mut provisional_entry.response, response);

        // Was the current goal the root of a cycle and was the provisional response
        // different from the final one.
        if stack_entry.has_been_used && prev_response != response {
            // If so, remove all entries whose result depends on this goal
            // from the provisional cache...
            //
            // That's not completely correct, as a nested goal can also
            // depend on a goal which is lower in the stack so it doesn't
            // actually depend on the current goal. This should be fairly
            // rare and is hopefully not relevant for performance.
            #[allow(rustc::potential_query_instability)]
            cache.lookup_table.retain(|_key, index| *index <= provisional_entry_index);
            cache.entries.truncate(provisional_entry_index.index() + 1);

            // ...and finally push our goal back on the stack and reevaluate it.
            self.stack.push(StackEntry { has_been_used: false, ..stack_entry });
            Err(())
        } else {
            Ok(stack_entry)
        }
    }

    pub(super) fn with_new_goal(
        &mut self,
        tcx: TyCtxt<'tcx>,
        input: CanonicalInput<'tcx>,
        inspect: &mut ProofTreeBuilder<'tcx>,
        mut loop_body: impl FnMut(&mut Self, &mut ProofTreeBuilder<'tcx>) -> QueryResult<'tcx>,
    ) -> QueryResult<'tcx> {
        let Some(available_depth) = Self::allowed_depth_for_nested(tcx, &self.stack) else {
            if let Some(last) = self.stack.raw.last_mut() {
                last.encountered_overflow = true;
            }
            return Self::response_no_constraints(tcx, input, Certainty::OVERFLOW);
        };

        if inspect.use_global_cache() {
            if let Some(CacheData { result, reached_depth, encountered_overflow }) =
                self.global_cache(tcx).get(
                    tcx,
                    input,
                    |cycle_participants| {
                        self.stack.iter().any(|entry| cycle_participants.contains(&entry.input))
                    },
                    available_depth,
                )
            {
                self.on_cache_hit(reached_depth, encountered_overflow);
                return result;
            }
        }

        match self.try_push_stack(tcx, input, available_depth, inspect) {
            Ok(()) => {}
            // Our goal is already on the stack, eager return.
            Err(response) => return response,
        }

        // This is for global caching, so we properly track query dependencies.
        // Everything that affects the `Result` should be performed within this
        // `with_anon_task` closure.
        let ((final_entry, result), dep_node) =
            tcx.dep_graph.with_anon_task(tcx, DepKind::TraitSelect, || {
                // We run our goal in a loop to handle coinductive cycles. If we fail to reach a
                // fipoint we return overflow.
                for _ in 0..self.local_overflow_limit() {
                    let result = loop_body(self, inspect);
                    if let Ok(final_entry) = self.try_finalize_goal(input, result) {
                        return (final_entry, result);
                    }
                }

                debug!("canonical cycle overflow");
                let current_entry = self.pop_stack();
                let result = Self::response_no_constraints(tcx, input, Certainty::OVERFLOW);
                (current_entry, result)
            });

        let cache = &mut self.provisional_cache;
        let provisional_entry_index = *cache.lookup_table.get(&input).unwrap();
        let provisional_entry = &mut cache.entries[provisional_entry_index];
        let depth = provisional_entry.depth;

        // We're now done with this goal. In case this goal is involved in a cycle
        // do not remove it from the provisional cache and do not add it to the global
        // cache.
        //
        // It is not possible for any nested goal to depend on something deeper on the
        // stack, as this would have also updated the depth of the current goal.
        if depth == self.stack.next_index() {
            for (i, entry) in cache.entries.drain_enumerated(provisional_entry_index.index()..) {
                let actual_index = cache.lookup_table.remove(&entry.input);
                debug_assert_eq!(Some(i), actual_index);
                debug_assert!(entry.depth == depth);
            }

            // When encountering a cycle, both inductive and coinductive, we only
            // move the root into the global cache. We also store all other cycle
            // participants involved.
            //
            // We disable the global cache entry of the root goal if a cycle
            // participant is on the stack. This is necessary to prevent unstable
            // results. See the comment of `StackEntry::cycle_participants` for
            // more details.
            let reached_depth = final_entry.reached_depth.as_usize() - self.stack.len();
            self.global_cache(tcx).insert(
                input,
                reached_depth,
                final_entry.encountered_overflow,
                final_entry.cycle_participants,
                dep_node,
                result,
            )
        }

        result
    }

    fn response_no_constraints(
        tcx: TyCtxt<'tcx>,
        goal: CanonicalInput<'tcx>,
        certainty: Certainty,
    ) -> QueryResult<'tcx> {
        Ok(super::response_no_constraints_raw(tcx, goal.max_universe, goal.variables, certainty))
    }
}
