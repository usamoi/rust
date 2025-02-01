use std::ops::ControlFlow;

use rustc_infer::infer::InferCtxt;
use rustc_infer::traits::solve::{CandidateSource, GoalSource, MaybeCause};
use rustc_infer::traits::{
    self, MismatchedProjectionTypes, Obligation, ObligationCause, ObligationCauseCode,
    PredicateObligation, SelectionError,
};
use rustc_middle::ty::error::{ExpectedFound, TypeError};
use rustc_middle::ty::{self, TyCtxt};
use rustc_middle::{bug, span_bug};
use rustc_next_trait_solver::solve::{GenerateProofTree, SolverDelegateEvalExt as _};
use rustc_type_ir::solve::{Goal, NoSolution};
use tracing::{instrument, trace};

use crate::solve::Certainty;
use crate::solve::delegate::SolverDelegate;
use crate::solve::inspect::{self, ProofTreeInferCtxtExt, ProofTreeVisitor};
use crate::traits::{FulfillmentError, FulfillmentErrorCode, wf};

pub(super) fn fulfillment_error_for_no_solution<'tcx>(
    infcx: &InferCtxt<'tcx>,
    root_obligation: PredicateObligation<'tcx>,
) -> FulfillmentError<'tcx> {
    let obligation = find_best_leaf_obligation(infcx, &root_obligation, false);

    let code = match obligation.predicate.kind().skip_binder() {
        ty::PredicateKind::Clause(ty::ClauseKind::Projection(_)) => {
            FulfillmentErrorCode::Project(
                // FIXME: This could be a `Sorts` if the term is a type
                MismatchedProjectionTypes { err: TypeError::Mismatch },
            )
        }
        ty::PredicateKind::Clause(ty::ClauseKind::ConstArgHasType(ct, expected_ty)) => {
            let ct_ty = match ct.kind() {
                ty::ConstKind::Unevaluated(uv) => {
                    infcx.tcx.type_of(uv.def).instantiate(infcx.tcx, uv.args)
                }
                ty::ConstKind::Param(param_ct) => param_ct.find_ty_from_env(obligation.param_env),
                ty::ConstKind::Value(cv) => cv.ty,
                kind => span_bug!(
                    obligation.cause.span,
                    "ConstArgHasWrongType failed but we don't know how to compute type for {kind:?}"
                ),
            };
            FulfillmentErrorCode::Select(SelectionError::ConstArgHasWrongType {
                ct,
                ct_ty,
                expected_ty,
            })
        }
        ty::PredicateKind::NormalizesTo(..) => {
            FulfillmentErrorCode::Project(MismatchedProjectionTypes { err: TypeError::Mismatch })
        }
        ty::PredicateKind::AliasRelate(_, _, _) => {
            FulfillmentErrorCode::Project(MismatchedProjectionTypes { err: TypeError::Mismatch })
        }
        ty::PredicateKind::Subtype(pred) => {
            let (a, b) = infcx.enter_forall_and_leak_universe(
                obligation.predicate.kind().rebind((pred.a, pred.b)),
            );
            let expected_found = ExpectedFound::new(a, b);
            FulfillmentErrorCode::Subtype(expected_found, TypeError::Sorts(expected_found))
        }
        ty::PredicateKind::Coerce(pred) => {
            let (a, b) = infcx.enter_forall_and_leak_universe(
                obligation.predicate.kind().rebind((pred.a, pred.b)),
            );
            let expected_found = ExpectedFound::new(b, a);
            FulfillmentErrorCode::Subtype(expected_found, TypeError::Sorts(expected_found))
        }
        ty::PredicateKind::Clause(_)
        | ty::PredicateKind::DynCompatible(_)
        | ty::PredicateKind::Ambiguous => {
            FulfillmentErrorCode::Select(SelectionError::Unimplemented)
        }
        ty::PredicateKind::ConstEquate(..) => {
            bug!("unexpected goal: {obligation:?}")
        }
    };

    FulfillmentError { obligation, code, root_obligation }
}

pub(super) fn fulfillment_error_for_stalled<'tcx>(
    infcx: &InferCtxt<'tcx>,
    root_obligation: PredicateObligation<'tcx>,
) -> FulfillmentError<'tcx> {
    let (code, refine_obligation) = infcx.probe(|_| {
        match <&SolverDelegate<'tcx>>::from(infcx)
            .evaluate_root_goal(root_obligation.clone().into(), GenerateProofTree::No)
            .0
        {
            Ok((_, Certainty::Maybe(MaybeCause::Ambiguity))) => {
                (FulfillmentErrorCode::Ambiguity { overflow: None }, true)
            }
            Ok((_, Certainty::Maybe(MaybeCause::Overflow { suggest_increasing_limit }))) => (
                FulfillmentErrorCode::Ambiguity { overflow: Some(suggest_increasing_limit) },
                // Don't look into overflows because we treat overflows weirdly anyways.
                // We discard the inference constraints from overflowing goals, so
                // recomputing the goal again during `find_best_leaf_obligation` may apply
                // inference guidance that makes other goals go from ambig -> pass, for example.
                //
                // FIXME: We should probably just look into overflows here.
                false,
            ),
            Ok((_, Certainty::Yes)) => {
                bug!("did not expect successful goal when collecting ambiguity errors")
            }
            Err(_) => {
                bug!("did not expect selection error when collecting ambiguity errors")
            }
        }
    });

    FulfillmentError {
        obligation: if refine_obligation {
            find_best_leaf_obligation(infcx, &root_obligation, true)
        } else {
            root_obligation.clone()
        },
        code,
        root_obligation,
    }
}

pub(super) fn fulfillment_error_for_overflow<'tcx>(
    infcx: &InferCtxt<'tcx>,
    root_obligation: PredicateObligation<'tcx>,
) -> FulfillmentError<'tcx> {
    FulfillmentError {
        obligation: find_best_leaf_obligation(infcx, &root_obligation, true),
        code: FulfillmentErrorCode::Ambiguity { overflow: Some(true) },
        root_obligation,
    }
}

fn find_best_leaf_obligation<'tcx>(
    infcx: &InferCtxt<'tcx>,
    obligation: &PredicateObligation<'tcx>,
    consider_ambiguities: bool,
) -> PredicateObligation<'tcx> {
    let obligation = infcx.resolve_vars_if_possible(obligation.clone());
    // FIXME: we use a probe here as the `BestObligation` visitor does not
    // check whether it uses candidates which get shadowed by where-bounds.
    //
    // We should probably fix the visitor to not do so instead, as this also
    // means the leaf obligation may be incorrect.
    infcx
        .fudge_inference_if_ok(|| {
            infcx
                .visit_proof_tree(obligation.clone().into(), &mut BestObligation {
                    obligation: obligation.clone(),
                    consider_ambiguities,
                })
                .break_value()
                .ok_or(())
        })
        .unwrap_or(obligation)
}

struct BestObligation<'tcx> {
    obligation: PredicateObligation<'tcx>,
    consider_ambiguities: bool,
}

impl<'tcx> BestObligation<'tcx> {
    fn with_derived_obligation(
        &mut self,
        derived_obligation: PredicateObligation<'tcx>,
        and_then: impl FnOnce(&mut Self) -> <Self as ProofTreeVisitor<'tcx>>::Result,
    ) -> <Self as ProofTreeVisitor<'tcx>>::Result {
        let old_obligation = std::mem::replace(&mut self.obligation, derived_obligation);
        let res = and_then(self);
        self.obligation = old_obligation;
        res
    }

    /// Filter out the candidates that aren't interesting to visit for the
    /// purposes of reporting errors. For ambiguities, we only consider
    /// candidates that may hold. For errors, we only consider candidates that
    /// *don't* hold and which have impl-where clauses that also don't hold.
    fn non_trivial_candidates<'a>(
        &self,
        goal: &'a inspect::InspectGoal<'a, 'tcx>,
    ) -> Vec<inspect::InspectCandidate<'a, 'tcx>> {
        let mut candidates = goal.candidates();
        match self.consider_ambiguities {
            true => {
                // If we have an ambiguous obligation, we must consider *all* candidates
                // that hold, or else we may guide inference causing other goals to go
                // from ambig -> pass/fail.
                candidates.retain(|candidate| candidate.result().is_ok());
            }
            false => {
                // If we have >1 candidate, one may still be due to "boring" reasons, like
                // an alias-relate that failed to hold when deeply evaluated. We really
                // don't care about reasons like this.
                if candidates.len() > 1 {
                    candidates.retain(|candidate| {
                        goal.infcx().probe(|_| {
                            candidate.instantiate_nested_goals(self.span()).iter().any(
                                |nested_goal| {
                                    matches!(
                                        nested_goal.source(),
                                        GoalSource::ImplWhereBound
                                            | GoalSource::AliasBoundConstCondition
                                            | GoalSource::InstantiateHigherRanked
                                            | GoalSource::AliasWellFormed
                                    ) && match (self.consider_ambiguities, nested_goal.result()) {
                                        (true, Ok(Certainty::Maybe(MaybeCause::Ambiguity)))
                                        | (false, Err(_)) => true,
                                        _ => false,
                                    }
                                },
                            )
                        })
                    });
                }

                // Prefer a non-rigid candidate if there is one.
                if candidates.len() > 1 {
                    candidates.retain(|candidate| {
                        !matches!(candidate.kind(), inspect::ProbeKind::RigidAlias { .. })
                    });
                }
            }
        }

        candidates
    }

    /// HACK: We walk the nested obligations for a well-formed arg manually,
    /// since there's nontrivial logic in `wf.rs` to set up an obligation cause.
    /// Ideally we'd be able to track this better.
    fn visit_well_formed_goal(
        &mut self,
        candidate: &inspect::InspectCandidate<'_, 'tcx>,
        arg: ty::GenericArg<'tcx>,
    ) -> ControlFlow<PredicateObligation<'tcx>> {
        let infcx = candidate.goal().infcx();
        let param_env = candidate.goal().goal().param_env;
        let body_id = self.obligation.cause.body_id;

        for obligation in wf::unnormalized_obligations(infcx, param_env, arg, self.span(), body_id)
            .into_iter()
            .flatten()
        {
            let nested_goal = candidate.instantiate_proof_tree_for_nested_goal(
                GoalSource::Misc,
                Goal::new(infcx.tcx, obligation.param_env, obligation.predicate),
                self.span(),
            );
            // Skip nested goals that aren't the *reason* for our goal's failure.
            match (self.consider_ambiguities, nested_goal.result()) {
                (true, Ok(Certainty::Maybe(MaybeCause::Ambiguity))) | (false, Err(_)) => {}
                _ => continue,
            }

            self.with_derived_obligation(obligation, |this| nested_goal.visit_with(this))?;
        }

        ControlFlow::Break(self.obligation.clone())
    }
}

impl<'tcx> ProofTreeVisitor<'tcx> for BestObligation<'tcx> {
    type Result = ControlFlow<PredicateObligation<'tcx>>;

    fn span(&self) -> rustc_span::Span {
        self.obligation.cause.span
    }

    #[instrument(level = "trace", skip(self, goal), fields(goal = ?goal.goal()))]
    fn visit_goal(&mut self, goal: &inspect::InspectGoal<'_, 'tcx>) -> Self::Result {
        let candidates = self.non_trivial_candidates(goal);
        trace!(candidates = ?candidates.iter().map(|c| c.kind()).collect::<Vec<_>>());

        let [candidate] = candidates.as_slice() else {
            return ControlFlow::Break(self.obligation.clone());
        };

        // Don't walk into impls that have `do_not_recommend`.
        if let inspect::ProbeKind::TraitCandidate {
            source: CandidateSource::Impl(impl_def_id),
            result: _,
        } = candidate.kind()
            && goal.infcx().tcx.do_not_recommend_impl(impl_def_id)
        {
            return ControlFlow::Break(self.obligation.clone());
        }

        let tcx = goal.infcx().tcx;
        // FIXME: Also, what about considering >1 layer up the stack? May be necessary
        // for normalizes-to.
        let pred_kind = goal.goal().predicate.kind();
        let child_mode = match pred_kind.skip_binder() {
            ty::PredicateKind::Clause(ty::ClauseKind::Trait(pred)) => {
                ChildMode::Trait(pred_kind.rebind(pred))
            }
            ty::PredicateKind::Clause(ty::ClauseKind::HostEffect(pred)) => {
                ChildMode::Host(pred_kind.rebind(pred))
            }
            ty::PredicateKind::NormalizesTo(normalizes_to)
                if matches!(
                    normalizes_to.alias.kind(tcx),
                    ty::AliasTermKind::ProjectionTy | ty::AliasTermKind::ProjectionConst
                ) =>
            {
                ChildMode::Trait(pred_kind.rebind(ty::TraitPredicate {
                    trait_ref: normalizes_to.alias.trait_ref(tcx),
                    polarity: ty::PredicatePolarity::Positive,
                }))
            }
            ty::PredicateKind::Clause(ty::ClauseKind::WellFormed(arg)) => {
                return self.visit_well_formed_goal(candidate, arg);
            }
            _ => ChildMode::PassThrough,
        };

        let nested_goals = candidate.instantiate_nested_goals(self.span());

        // If the candidate requires some `T: FnPtr` bound which does not hold should not be treated as
        // an actual candidate, instead we should treat them as if the impl was never considered to
        // have potentially applied. As if `impl<A, R> Trait for for<..> fn(..A) -> R` was written
        // instead of `impl<T: FnPtr> Trait for T`.
        //
        // We do this as a separate loop so that we do not choose to tell the user about some nested
        // goal before we encounter a `T: FnPtr` nested goal.
        for nested_goal in &nested_goals {
            if let Some(fn_ptr_trait) = tcx.lang_items().fn_ptr_trait()
                && let Some(poly_trait_pred) = nested_goal.goal().predicate.as_trait_clause()
                && poly_trait_pred.def_id() == fn_ptr_trait
                && let Err(NoSolution) = nested_goal.result()
            {
                return ControlFlow::Break(self.obligation.clone());
            }
        }

        let mut impl_where_bound_count = 0;
        for nested_goal in nested_goals {
            trace!(nested_goal = ?(nested_goal.goal(), nested_goal.source(), nested_goal.result()));

            let make_obligation = |cause| Obligation {
                cause,
                param_env: nested_goal.goal().param_env,
                predicate: nested_goal.goal().predicate,
                recursion_depth: self.obligation.recursion_depth + 1,
            };

            let obligation;
            match (child_mode, nested_goal.source()) {
                (ChildMode::Trait(_) | ChildMode::Host(_), GoalSource::Misc) => {
                    continue;
                }
                (ChildMode::Trait(parent_trait_pred), GoalSource::ImplWhereBound) => {
                    obligation = make_obligation(derive_cause(
                        tcx,
                        candidate.kind(),
                        self.obligation.cause.clone(),
                        impl_where_bound_count,
                        parent_trait_pred,
                    ));
                    impl_where_bound_count += 1;
                }
                (
                    ChildMode::Host(parent_host_pred),
                    GoalSource::ImplWhereBound | GoalSource::AliasBoundConstCondition,
                ) => {
                    obligation = make_obligation(derive_host_cause(
                        tcx,
                        candidate.kind(),
                        self.obligation.cause.clone(),
                        impl_where_bound_count,
                        parent_host_pred,
                    ));
                    impl_where_bound_count += 1;
                }
                // Skip over a higher-ranked predicate.
                (_, GoalSource::InstantiateHigherRanked) => {
                    obligation = self.obligation.clone();
                }
                (ChildMode::PassThrough, _)
                | (_, GoalSource::AliasWellFormed | GoalSource::AliasBoundConstCondition) => {
                    obligation = make_obligation(self.obligation.cause.clone());
                }
            }

            // Skip nested goals that aren't the *reason* for our goal's failure.
            match (self.consider_ambiguities, nested_goal.result()) {
                (true, Ok(Certainty::Maybe(MaybeCause::Ambiguity))) | (false, Err(_)) => {}
                _ => continue,
            }

            self.with_derived_obligation(obligation, |this| nested_goal.visit_with(this))?;
        }

        // alias-relate may fail because the lhs or rhs can't be normalized,
        // and therefore is treated as rigid.
        if let Some(ty::PredicateKind::AliasRelate(lhs, rhs, _)) = pred_kind.no_bound_vars() {
            if let Some(obligation) = goal
                .infcx()
                .visit_proof_tree_at_depth(
                    goal.goal().with(goal.infcx().tcx, ty::ClauseKind::WellFormed(lhs.into())),
                    goal.depth() + 1,
                    self,
                )
                .break_value()
            {
                return ControlFlow::Break(obligation);
            } else if let Some(obligation) = goal
                .infcx()
                .visit_proof_tree_at_depth(
                    goal.goal().with(goal.infcx().tcx, ty::ClauseKind::WellFormed(rhs.into())),
                    goal.depth() + 1,
                    self,
                )
                .break_value()
            {
                return ControlFlow::Break(obligation);
            }
        }

        ControlFlow::Break(self.obligation.clone())
    }
}

#[derive(Debug, Copy, Clone)]
enum ChildMode<'tcx> {
    // Try to derive an `ObligationCause::{ImplDerived,BuiltinDerived}`,
    // and skip all `GoalSource::Misc`, which represent useless obligations
    // such as alias-eq which may not hold.
    Trait(ty::PolyTraitPredicate<'tcx>),
    // Try to derive an `ObligationCause::{ImplDerived,BuiltinDerived}`,
    // and skip all `GoalSource::Misc`, which represent useless obligations
    // such as alias-eq which may not hold.
    Host(ty::Binder<'tcx, ty::HostEffectPredicate<'tcx>>),
    // Skip trying to derive an `ObligationCause` from this obligation, and
    // report *all* sub-obligations as if they came directly from the parent
    // obligation.
    PassThrough,
}

fn derive_cause<'tcx>(
    tcx: TyCtxt<'tcx>,
    candidate_kind: inspect::ProbeKind<TyCtxt<'tcx>>,
    mut cause: ObligationCause<'tcx>,
    idx: usize,
    parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
) -> ObligationCause<'tcx> {
    match candidate_kind {
        inspect::ProbeKind::TraitCandidate {
            source: CandidateSource::Impl(impl_def_id),
            result: _,
        } => {
            if let Some((_, span)) =
                tcx.predicates_of(impl_def_id).instantiate_identity(tcx).iter().nth(idx)
            {
                cause = cause.derived_cause(parent_trait_pred, |derived| {
                    ObligationCauseCode::ImplDerived(Box::new(traits::ImplDerivedCause {
                        derived,
                        impl_or_alias_def_id: impl_def_id,
                        impl_def_predicate_index: Some(idx),
                        span,
                    }))
                })
            }
        }
        inspect::ProbeKind::TraitCandidate {
            source: CandidateSource::BuiltinImpl(..),
            result: _,
        } => {
            cause = cause.derived_cause(parent_trait_pred, ObligationCauseCode::BuiltinDerived);
        }
        _ => {}
    };
    cause
}

fn derive_host_cause<'tcx>(
    tcx: TyCtxt<'tcx>,
    candidate_kind: inspect::ProbeKind<TyCtxt<'tcx>>,
    mut cause: ObligationCause<'tcx>,
    idx: usize,
    parent_host_pred: ty::Binder<'tcx, ty::HostEffectPredicate<'tcx>>,
) -> ObligationCause<'tcx> {
    match candidate_kind {
        inspect::ProbeKind::TraitCandidate {
            source: CandidateSource::Impl(impl_def_id),
            result: _,
        } => {
            if let Some((_, span)) = tcx
                .predicates_of(impl_def_id)
                .instantiate_identity(tcx)
                .into_iter()
                .chain(tcx.const_conditions(impl_def_id).instantiate_identity(tcx).into_iter().map(
                    |(trait_ref, span)| {
                        (
                            trait_ref.to_host_effect_clause(
                                tcx,
                                parent_host_pred.skip_binder().constness,
                            ),
                            span,
                        )
                    },
                ))
                .nth(idx)
            {
                cause =
                    cause.derived_host_cause(parent_host_pred, |derived| {
                        ObligationCauseCode::ImplDerivedHost(Box::new(
                            traits::ImplDerivedHostCause { derived, impl_def_id, span },
                        ))
                    })
            }
        }
        inspect::ProbeKind::TraitCandidate {
            source: CandidateSource::BuiltinImpl(..),
            result: _,
        } => {
            cause =
                cause.derived_host_cause(parent_host_pred, ObligationCauseCode::BuiltinDerivedHost);
        }
        _ => {}
    };
    cause
}
