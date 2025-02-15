use crate::infer::{InferCtxt, TyOrConstInferVar};
use rustc_data_structures::obligation_forest::ProcessResult;
use rustc_data_structures::obligation_forest::{DoCompleted, Error, ForestObligation};
use rustc_data_structures::obligation_forest::{ObligationForest, ObligationProcessor};
use rustc_errors::ErrorReported;
use rustc_infer::traits::{TraitEngine, TraitEngineExt as _};
use rustc_middle::mir::interpret::ErrorHandled;
use rustc_middle::ty::error::ExpectedFound;
use rustc_middle::ty::{self, Const, ToPolyTraitRef, Ty, TypeFoldable};
use std::marker::PhantomData;

use super::project;
use super::select::SelectionContext;
use super::wf;
use super::CodeAmbiguity;
use super::CodeProjectionError;
use super::CodeSelectionError;
use super::{ConstEvalFailure, Unimplemented};
use super::{FulfillmentError, FulfillmentErrorCode};
use super::{ObligationCause, PredicateObligation};

use crate::traits::error_reporting::InferCtxtExt as _;
use crate::traits::query::evaluate_obligation::InferCtxtExt as _;

impl<'tcx> ForestObligation for PendingPredicateObligation<'tcx> {
    /// Note that we include both the `ParamEnv` and the `Predicate`,
    /// as the `ParamEnv` can influence whether fulfillment succeeds
    /// or fails.
    type CacheKey = ty::ParamEnvAnd<'tcx, ty::Predicate<'tcx>>;

    fn as_cache_key(&self) -> Self::CacheKey {
        self.obligation.param_env.and(self.obligation.predicate)
    }
}

/// The fulfillment context is used to drive trait resolution. It
/// consists of a list of obligations that must be (eventually)
/// satisfied. The job is to track which are satisfied, which yielded
/// errors, and which are still pending. At any point, users can call
/// `select_where_possible`, and the fulfillment context will try to do
/// selection, retaining only those obligations that remain
/// ambiguous. This may be helpful in pushing type inference
/// along. Once all type inference constraints have been generated, the
/// method `select_all_or_error` can be used to report any remaining
/// ambiguous cases as errors.
pub struct FulfillmentContext<'tcx> {
    // A list of all obligations that have been registered with this
    // fulfillment context.
    predicates: ObligationForest<PendingPredicateObligation<'tcx>>,
    // Should this fulfillment context register type-lives-for-region
    // obligations on its parent infcx? In some cases, region
    // obligations are either already known to hold (normalization) or
    // hopefully verifed elsewhere (type-impls-bound), and therefore
    // should not be checked.
    //
    // Note that if we are normalizing a type that we already
    // know is well-formed, there should be no harm setting this
    // to true - all the region variables should be determinable
    // using the RFC 447 rules, which don't depend on
    // type-lives-for-region constraints, and because the type
    // is well-formed, the constraints should hold.
    register_region_obligations: bool,
    // Is it OK to register obligations into this infcx inside
    // an infcx snapshot?
    //
    // The "primary fulfillment" in many cases in typeck lives
    // outside of any snapshot, so any use of it inside a snapshot
    // will lead to trouble and therefore is checked against, but
    // other fulfillment contexts sometimes do live inside of
    // a snapshot (they don't *straddle* a snapshot, so there
    // is no trouble there).
    usable_in_snapshot: bool,
}

#[derive(Clone, Debug)]
pub struct PendingPredicateObligation<'tcx> {
    pub obligation: PredicateObligation<'tcx>,
    // This is far more often read than modified, meaning that we
    // should mostly optimize for reading speed, while modifying is not as relevant.
    //
    // For whatever reason using a boxed slice is slower than using a `Vec` here.
    pub stalled_on: Vec<TyOrConstInferVar<'tcx>>,
}

// `PendingPredicateObligation` is used a lot. Make sure it doesn't unintentionally get bigger.
#[cfg(target_arch = "x86_64")]
static_assert_size!(PendingPredicateObligation<'_>, 64);

impl<'a, 'tcx> FulfillmentContext<'tcx> {
    /// Creates a new fulfillment context.
    pub fn new() -> FulfillmentContext<'tcx> {
        FulfillmentContext {
            predicates: ObligationForest::new(),
            register_region_obligations: true,
            usable_in_snapshot: false,
        }
    }

    pub fn new_in_snapshot() -> FulfillmentContext<'tcx> {
        FulfillmentContext {
            predicates: ObligationForest::new(),
            register_region_obligations: true,
            usable_in_snapshot: true,
        }
    }

    pub fn new_ignoring_regions() -> FulfillmentContext<'tcx> {
        FulfillmentContext {
            predicates: ObligationForest::new(),
            register_region_obligations: false,
            usable_in_snapshot: false,
        }
    }

    /// Attempts to select obligations using `selcx`.
    fn select(
        &mut self,
        selcx: &mut SelectionContext<'a, 'tcx>,
    ) -> Result<(), Vec<FulfillmentError<'tcx>>> {
        debug!("select(obligation-forest-size={})", self.predicates.len());

        let mut errors = Vec::new();

        loop {
            debug!("select: starting another iteration");

            // Process pending obligations.
            let outcome = self.predicates.process_obligations(
                &mut FulfillProcessor {
                    selcx,
                    register_region_obligations: self.register_region_obligations,
                },
                DoCompleted::No,
            );
            debug!("select: outcome={:#?}", outcome);

            // FIXME: if we kept the original cache key, we could mark projection
            // obligations as complete for the projection cache here.

            errors.extend(outcome.errors.into_iter().map(to_fulfillment_error));

            // If nothing new was added, no need to keep looping.
            if outcome.stalled {
                break;
            }
        }

        debug!(
            "select({} predicates remaining, {} errors) done",
            self.predicates.len(),
            errors.len()
        );

        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }
}

impl<'tcx> TraitEngine<'tcx> for FulfillmentContext<'tcx> {
    /// "Normalize" a projection type `<SomeType as SomeTrait>::X` by
    /// creating a fresh type variable `$0` as well as a projection
    /// predicate `<SomeType as SomeTrait>::X == $0`. When the
    /// inference engine runs, it will attempt to find an impl of
    /// `SomeTrait` or a where-clause that lets us unify `$0` with
    /// something concrete. If this fails, we'll unify `$0` with
    /// `projection_ty` again.
    fn normalize_projection_type(
        &mut self,
        infcx: &InferCtxt<'_, 'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        projection_ty: ty::ProjectionTy<'tcx>,
        cause: ObligationCause<'tcx>,
    ) -> Ty<'tcx> {
        debug!("normalize_projection_type(projection_ty={:?})", projection_ty);

        debug_assert!(!projection_ty.has_escaping_bound_vars());

        // FIXME(#20304) -- cache

        let mut selcx = SelectionContext::new(infcx);
        let mut obligations = vec![];
        let normalized_ty = project::normalize_projection_type(
            &mut selcx,
            param_env,
            projection_ty,
            cause,
            0,
            &mut obligations,
        );
        self.register_predicate_obligations(infcx, obligations);

        debug!("normalize_projection_type: result={:?}", normalized_ty);

        normalized_ty
    }

    fn register_predicate_obligation(
        &mut self,
        infcx: &InferCtxt<'_, 'tcx>,
        obligation: PredicateObligation<'tcx>,
    ) {
        // this helps to reduce duplicate errors, as well as making
        // debug output much nicer to read and so on.
        let obligation = infcx.resolve_vars_if_possible(&obligation);

        debug!("register_predicate_obligation(obligation={:?})", obligation);

        assert!(!infcx.is_in_snapshot() || self.usable_in_snapshot);

        self.predicates
            .register_obligation(PendingPredicateObligation { obligation, stalled_on: vec![] });
    }

    fn select_all_or_error(
        &mut self,
        infcx: &InferCtxt<'_, 'tcx>,
    ) -> Result<(), Vec<FulfillmentError<'tcx>>> {
        self.select_where_possible(infcx)?;

        let errors: Vec<_> = self
            .predicates
            .to_errors(CodeAmbiguity)
            .into_iter()
            .map(to_fulfillment_error)
            .collect();
        if errors.is_empty() { Ok(()) } else { Err(errors) }
    }

    fn select_where_possible(
        &mut self,
        infcx: &InferCtxt<'_, 'tcx>,
    ) -> Result<(), Vec<FulfillmentError<'tcx>>> {
        let mut selcx = SelectionContext::new(infcx);
        self.select(&mut selcx)
    }

    fn pending_obligations(&self) -> Vec<PredicateObligation<'tcx>> {
        self.predicates.map_pending_obligations(|o| o.obligation.clone())
    }
}

struct FulfillProcessor<'a, 'b, 'tcx> {
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    register_region_obligations: bool,
}

fn mk_pending(os: Vec<PredicateObligation<'tcx>>) -> Vec<PendingPredicateObligation<'tcx>> {
    os.into_iter()
        .map(|o| PendingPredicateObligation { obligation: o, stalled_on: vec![] })
        .collect()
}

impl<'a, 'b, 'tcx> ObligationProcessor for FulfillProcessor<'a, 'b, 'tcx> {
    type Obligation = PendingPredicateObligation<'tcx>;
    type Error = FulfillmentErrorCode<'tcx>;

    /// Processes a predicate obligation and returns either:
    /// - `Changed(v)` if the predicate is true, presuming that `v` are also true
    /// - `Unchanged` if we don't have enough info to be sure
    /// - `Error(e)` if the predicate does not hold
    ///
    /// This is always inlined, despite its size, because it has a single
    /// callsite and it is called *very* frequently.
    #[inline(always)]
    fn process_obligation(
        &mut self,
        pending_obligation: &mut Self::Obligation,
    ) -> ProcessResult<Self::Obligation, Self::Error> {
        // If we were stalled on some unresolved variables, first check whether
        // any of them have been resolved; if not, don't bother doing more work
        // yet.
        let change = match pending_obligation.stalled_on.len() {
            // Match arms are in order of frequency, which matters because this
            // code is so hot. 1 and 0 dominate; 2+ is fairly rare.
            1 => {
                let infer_var = pending_obligation.stalled_on[0];
                self.selcx.infcx().ty_or_const_infer_var_changed(infer_var)
            }
            0 => {
                // In this case we haven't changed, but wish to make a change.
                true
            }
            _ => {
                // This `for` loop was once a call to `all()`, but this lower-level
                // form was a perf win. See #64545 for details.
                (|| {
                    for &infer_var in &pending_obligation.stalled_on {
                        if self.selcx.infcx().ty_or_const_infer_var_changed(infer_var) {
                            return true;
                        }
                    }
                    false
                })()
            }
        };

        if !change {
            debug!(
                "process_predicate: pending obligation {:?} still stalled on {:?}",
                self.selcx.infcx().resolve_vars_if_possible(&pending_obligation.obligation),
                pending_obligation.stalled_on
            );
            return ProcessResult::Unchanged;
        }

        // This part of the code is much colder.

        pending_obligation.stalled_on.truncate(0);

        let obligation = &mut pending_obligation.obligation;

        if obligation.predicate.has_infer_types_or_consts() {
            obligation.predicate =
                self.selcx.infcx().resolve_vars_if_possible(&obligation.predicate);
        }

        debug!("process_obligation: obligation = {:?} cause = {:?}", obligation, obligation.cause);

        let infcx = self.selcx.infcx();

        match obligation.predicate.kind() {
            ty::PredicateKind::Trait(ref data, _) => {
                let trait_obligation = obligation.with(*data);

                if obligation.predicate.is_global() {
                    // no type variables present, can use evaluation for better caching.
                    // FIXME: consider caching errors too.
                    if infcx.predicate_must_hold_considering_regions(&obligation) {
                        debug!(
                            "selecting trait `{:?}` at depth {} evaluated to holds",
                            data, obligation.recursion_depth
                        );
                        return ProcessResult::Changed(vec![]);
                    }
                }

                match self.selcx.select(&trait_obligation) {
                    Ok(Some(impl_source)) => {
                        debug!(
                            "selecting trait `{:?}` at depth {} yielded Ok(Some)",
                            data, obligation.recursion_depth
                        );
                        ProcessResult::Changed(mk_pending(impl_source.nested_obligations()))
                    }
                    Ok(None) => {
                        debug!(
                            "selecting trait `{:?}` at depth {} yielded Ok(None)",
                            data, obligation.recursion_depth
                        );

                        // This is a bit subtle: for the most part, the
                        // only reason we can fail to make progress on
                        // trait selection is because we don't have enough
                        // information about the types in the trait.
                        pending_obligation.stalled_on =
                            trait_ref_infer_vars(self.selcx, data.to_poly_trait_ref());

                        debug!(
                            "process_predicate: pending obligation {:?} now stalled on {:?}",
                            infcx.resolve_vars_if_possible(obligation),
                            pending_obligation.stalled_on
                        );

                        ProcessResult::Unchanged
                    }
                    Err(selection_err) => {
                        info!(
                            "selecting trait `{:?}` at depth {} yielded Err",
                            data, obligation.recursion_depth
                        );

                        ProcessResult::Error(CodeSelectionError(selection_err))
                    }
                }
            }

            &ty::PredicateKind::RegionOutlives(binder) => {
                match infcx.region_outlives_predicate(&obligation.cause, binder) {
                    Ok(()) => ProcessResult::Changed(vec![]),
                    Err(_) => ProcessResult::Error(CodeSelectionError(Unimplemented)),
                }
            }

            ty::PredicateKind::TypeOutlives(ref binder) => {
                // Check if there are higher-ranked vars.
                match binder.no_bound_vars() {
                    // If there are, inspect the underlying type further.
                    None => {
                        // Convert from `Binder<OutlivesPredicate<Ty, Region>>` to `Binder<Ty>`.
                        let binder = binder.map_bound_ref(|pred| pred.0);

                        // Check if the type has any bound vars.
                        match binder.no_bound_vars() {
                            // If so, this obligation is an error (for now). Eventually we should be
                            // able to support additional cases here, like `for<'a> &'a str: 'a`.
                            // NOTE: this is duplicate-implemented between here and fulfillment.
                            None => ProcessResult::Error(CodeSelectionError(Unimplemented)),
                            // Otherwise, we have something of the form
                            // `for<'a> T: 'a where 'a not in T`, which we can treat as
                            // `T: 'static`.
                            Some(t_a) => {
                                let r_static = self.selcx.tcx().lifetimes.re_static;
                                if self.register_region_obligations {
                                    self.selcx.infcx().register_region_obligation_with_cause(
                                        t_a,
                                        r_static,
                                        &obligation.cause,
                                    );
                                }
                                ProcessResult::Changed(vec![])
                            }
                        }
                    }
                    // If there aren't, register the obligation.
                    Some(ty::OutlivesPredicate(t_a, r_b)) => {
                        if self.register_region_obligations {
                            self.selcx.infcx().register_region_obligation_with_cause(
                                t_a,
                                r_b,
                                &obligation.cause,
                            );
                        }
                        ProcessResult::Changed(vec![])
                    }
                }
            }

            ty::PredicateKind::Projection(ref data) => {
                let project_obligation = obligation.with(*data);
                let tcx = self.selcx.tcx();
                match project::poly_project_and_unify_type(self.selcx, &project_obligation) {
                    Ok(Ok(Some(os))) => ProcessResult::Changed(mk_pending(os)),
                    Ok(Ok(None)) => {
                        pending_obligation.stalled_on = trait_ref_infer_vars(
                            self.selcx,
                            project_obligation.predicate.to_poly_trait_ref(tcx),
                        );
                        ProcessResult::Unchanged
                    }
                    // Let the caller handle the recursion
                    Ok(Err(project::InProgress)) => ProcessResult::Changed(mk_pending(vec![
                        pending_obligation.obligation.clone(),
                    ])),
                    Err(e) => ProcessResult::Error(CodeProjectionError(e)),
                }
            }

            &ty::PredicateKind::ObjectSafe(trait_def_id) => {
                if !self.selcx.tcx().is_object_safe(trait_def_id) {
                    ProcessResult::Error(CodeSelectionError(Unimplemented))
                } else {
                    ProcessResult::Changed(vec![])
                }
            }

            &ty::PredicateKind::ClosureKind(_, closure_substs, kind) => {
                match self.selcx.infcx().closure_kind(closure_substs) {
                    Some(closure_kind) => {
                        if closure_kind.extends(kind) {
                            ProcessResult::Changed(vec![])
                        } else {
                            ProcessResult::Error(CodeSelectionError(Unimplemented))
                        }
                    }
                    None => ProcessResult::Unchanged,
                }
            }

            &ty::PredicateKind::WellFormed(arg) => {
                match wf::obligations(
                    self.selcx.infcx(),
                    obligation.param_env,
                    obligation.cause.body_id,
                    arg,
                    obligation.cause.span,
                ) {
                    None => {
                        pending_obligation.stalled_on =
                            vec![TyOrConstInferVar::maybe_from_generic_arg(arg).unwrap()];
                        ProcessResult::Unchanged
                    }
                    Some(os) => ProcessResult::Changed(mk_pending(os)),
                }
            }

            &ty::PredicateKind::Subtype(subtype) => {
                match self.selcx.infcx().subtype_predicate(
                    &obligation.cause,
                    obligation.param_env,
                    subtype,
                ) {
                    None => {
                        // None means that both are unresolved.
                        pending_obligation.stalled_on = vec![
                            TyOrConstInferVar::maybe_from_ty(subtype.skip_binder().a).unwrap(),
                            TyOrConstInferVar::maybe_from_ty(subtype.skip_binder().b).unwrap(),
                        ];
                        ProcessResult::Unchanged
                    }
                    Some(Ok(ok)) => ProcessResult::Changed(mk_pending(ok.obligations)),
                    Some(Err(err)) => {
                        let expected_found = ExpectedFound::new(
                            subtype.skip_binder().a_is_expected,
                            subtype.skip_binder().a,
                            subtype.skip_binder().b,
                        );
                        ProcessResult::Error(FulfillmentErrorCode::CodeSubtypeError(
                            expected_found,
                            err,
                        ))
                    }
                }
            }

            &ty::PredicateKind::ConstEvaluatable(def_id, substs) => {
                match self.selcx.infcx().const_eval_resolve(
                    obligation.param_env,
                    def_id,
                    substs,
                    None,
                    Some(obligation.cause.span),
                ) {
                    Ok(_) => ProcessResult::Changed(vec![]),
                    Err(err) => ProcessResult::Error(CodeSelectionError(ConstEvalFailure(err))),
                }
            }

            ty::PredicateKind::ConstEquate(c1, c2) => {
                debug!("equating consts: c1={:?} c2={:?}", c1, c2);

                let stalled_on = &mut pending_obligation.stalled_on;

                let mut evaluate = |c: &'tcx Const<'tcx>| {
                    if let ty::ConstKind::Unevaluated(def_id, substs, promoted) = c.val {
                        match self.selcx.infcx().const_eval_resolve(
                            obligation.param_env,
                            def_id,
                            substs,
                            promoted,
                            Some(obligation.cause.span),
                        ) {
                            Ok(val) => Ok(Const::from_value(self.selcx.tcx(), val, c.ty)),
                            Err(ErrorHandled::TooGeneric) => {
                                stalled_on.append(
                                    &mut substs
                                        .types()
                                        .filter_map(|ty| TyOrConstInferVar::maybe_from_ty(ty))
                                        .collect(),
                                );
                                Err(ErrorHandled::TooGeneric)
                            }
                            Err(err) => Err(err),
                        }
                    } else {
                        Ok(c)
                    }
                };

                match (evaluate(c1), evaluate(c2)) {
                    (Ok(c1), Ok(c2)) => {
                        match self
                            .selcx
                            .infcx()
                            .at(&obligation.cause, obligation.param_env)
                            .eq(c1, c2)
                        {
                            Ok(_) => ProcessResult::Changed(vec![]),
                            Err(err) => {
                                ProcessResult::Error(FulfillmentErrorCode::CodeConstEquateError(
                                    ExpectedFound::new(true, c1, c2),
                                    err,
                                ))
                            }
                        }
                    }
                    (Err(ErrorHandled::Reported(ErrorReported)), _)
                    | (_, Err(ErrorHandled::Reported(ErrorReported))) => ProcessResult::Error(
                        CodeSelectionError(ConstEvalFailure(ErrorHandled::Reported(ErrorReported))),
                    ),
                    (Err(ErrorHandled::Linted), _) | (_, Err(ErrorHandled::Linted)) => span_bug!(
                        obligation.cause.span(self.selcx.tcx()),
                        "ConstEquate: const_eval_resolve returned an unexpected error"
                    ),
                    (Err(ErrorHandled::TooGeneric), _) | (_, Err(ErrorHandled::TooGeneric)) => {
                        ProcessResult::Unchanged
                    }
                }
            }
        }
    }

    fn process_backedge<'c, I>(
        &mut self,
        cycle: I,
        _marker: PhantomData<&'c PendingPredicateObligation<'tcx>>,
    ) where
        I: Clone + Iterator<Item = &'c PendingPredicateObligation<'tcx>>,
    {
        if self.selcx.coinductive_match(cycle.clone().map(|s| s.obligation.predicate)) {
            debug!("process_child_obligations: coinductive match");
        } else {
            let cycle: Vec<_> = cycle.map(|c| c.obligation.clone()).collect();
            self.selcx.infcx().report_overflow_error_cycle(&cycle);
        }
    }
}

/// Returns the set of inference variables contained in a trait ref.
fn trait_ref_infer_vars<'a, 'tcx>(
    selcx: &mut SelectionContext<'a, 'tcx>,
    trait_ref: ty::PolyTraitRef<'tcx>,
) -> Vec<TyOrConstInferVar<'tcx>> {
    selcx
        .infcx()
        .resolve_vars_if_possible(&trait_ref)
        .skip_binder() // ok b/c this check doesn't care about regions
        .substs
        .iter()
        // FIXME(eddyb) try using `skip_current_subtree` to skip everything that
        // doesn't contain inference variables, not just the outermost level.
        .filter(|arg| arg.has_infer_types_or_consts())
        .flat_map(|arg| arg.walk())
        .filter_map(TyOrConstInferVar::maybe_from_generic_arg)
        .collect()
}

fn to_fulfillment_error<'tcx>(
    error: Error<PendingPredicateObligation<'tcx>, FulfillmentErrorCode<'tcx>>,
) -> FulfillmentError<'tcx> {
    let obligation = error.backtrace.into_iter().next().unwrap().obligation;
    FulfillmentError::new(obligation, error.error)
}
