//! Code for projecting associated types out of trait references.

use super::elaborate_predicates;
use super::specialization_graph;
use super::translate_substs;
use super::util;
use super::MismatchedProjectionTypes;
use super::Obligation;
use super::ObligationCause;
use super::PredicateObligation;
use super::Selection;
use super::SelectionContext;
use super::SelectionError;
use super::{
    ImplSourceClosureData, ImplSourceDiscriminantKindData, ImplSourceFnPointerData,
    ImplSourceGeneratorData, ImplSourceUserDefinedData,
};
use super::{Normalized, NormalizedTy, ProjectionCacheEntry, ProjectionCacheKey};

use crate::infer::type_variable::{TypeVariableOrigin, TypeVariableOriginKind};
use crate::infer::{InferCtxt, InferOk, LateBoundRegionConversionTime};
use crate::traits::error_reporting::InferCtxtExt;
use rustc_data_structures::stack::ensure_sufficient_stack;
use rustc_errors::ErrorReported;
use rustc_hir::def_id::DefId;
use rustc_hir::lang_items::{FnOnceOutputLangItem, FnOnceTraitLangItem, GeneratorTraitLangItem};
use rustc_infer::infer::resolve::OpportunisticRegionResolver;
use rustc_middle::ty::fold::{TypeFoldable, TypeFolder};
use rustc_middle::ty::subst::Subst;
use rustc_middle::ty::util::IntTypeExt;
use rustc_middle::ty::{self, ToPolyTraitRef, ToPredicate, Ty, TyCtxt, WithConstness};
use rustc_span::symbol::sym;
use rustc_span::DUMMY_SP;

pub use rustc_middle::traits::Reveal;

pub type PolyProjectionObligation<'tcx> = Obligation<'tcx, ty::PolyProjectionPredicate<'tcx>>;

pub type ProjectionObligation<'tcx> = Obligation<'tcx, ty::ProjectionPredicate<'tcx>>;

pub type ProjectionTyObligation<'tcx> = Obligation<'tcx, ty::ProjectionTy<'tcx>>;

pub(super) struct InProgress;

/// When attempting to resolve `<T as TraitRef>::Name` ...
#[derive(Debug)]
pub enum ProjectionTyError<'tcx> {
    /// ...we found multiple sources of information and couldn't resolve the ambiguity.
    TooManyCandidates,

    /// ...an error occurred matching `T : TraitRef`
    TraitSelectionError(SelectionError<'tcx>),
}

#[derive(PartialEq, Eq, Debug)]
enum ProjectionTyCandidate<'tcx> {
    // from a where-clause in the env or object type
    ParamEnv(ty::PolyProjectionPredicate<'tcx>),

    // from the definition of `Trait` when you have something like <<A as Trait>::B as Trait2>::C
    TraitDef(ty::PolyProjectionPredicate<'tcx>),

    // from a "impl" (or a "pseudo-impl" returned by select)
    Select(Selection<'tcx>),
}

enum ProjectionTyCandidateSet<'tcx> {
    None,
    Single(ProjectionTyCandidate<'tcx>),
    Ambiguous,
    Error(SelectionError<'tcx>),
}

impl<'tcx> ProjectionTyCandidateSet<'tcx> {
    fn mark_ambiguous(&mut self) {
        *self = ProjectionTyCandidateSet::Ambiguous;
    }

    fn mark_error(&mut self, err: SelectionError<'tcx>) {
        *self = ProjectionTyCandidateSet::Error(err);
    }

    // Returns true if the push was successful, or false if the candidate
    // was discarded -- this could be because of ambiguity, or because
    // a higher-priority candidate is already there.
    fn push_candidate(&mut self, candidate: ProjectionTyCandidate<'tcx>) -> bool {
        use self::ProjectionTyCandidate::*;
        use self::ProjectionTyCandidateSet::*;

        // This wacky variable is just used to try and
        // make code readable and avoid confusing paths.
        // It is assigned a "value" of `()` only on those
        // paths in which we wish to convert `*self` to
        // ambiguous (and return false, because the candidate
        // was not used). On other paths, it is not assigned,
        // and hence if those paths *could* reach the code that
        // comes after the match, this fn would not compile.
        let convert_to_ambiguous;

        match self {
            None => {
                *self = Single(candidate);
                return true;
            }

            Single(current) => {
                // Duplicates can happen inside ParamEnv. In the case, we
                // perform a lazy deduplication.
                if current == &candidate {
                    return false;
                }

                // Prefer where-clauses. As in select, if there are multiple
                // candidates, we prefer where-clause candidates over impls.  This
                // may seem a bit surprising, since impls are the source of
                // "truth" in some sense, but in fact some of the impls that SEEM
                // applicable are not, because of nested obligations. Where
                // clauses are the safer choice. See the comment on
                // `select::SelectionCandidate` and #21974 for more details.
                match (current, candidate) {
                    (ParamEnv(..), ParamEnv(..)) => convert_to_ambiguous = (),
                    (ParamEnv(..), _) => return false,
                    (_, ParamEnv(..)) => unreachable!(),
                    (_, _) => convert_to_ambiguous = (),
                }
            }

            Ambiguous | Error(..) => {
                return false;
            }
        }

        // We only ever get here when we moved from a single candidate
        // to ambiguous.
        let () = convert_to_ambiguous;
        *self = Ambiguous;
        false
    }
}

/// Evaluates constraints of the form:
///
///     for<...> <T as Trait>::U == V
///
/// If successful, this may result in additional obligations. Also returns
/// the projection cache key used to track these additional obligations.
///
/// ## Returns
///
/// - `Err(_)`: the projection can be normalized, but is not equal to the
///   expected type.
/// - `Ok(Err(InProgress))`: this is called recursively while normalizing
///   the same projection.
/// - `Ok(Ok(None))`: The projection cannot be normalized due to ambiguity
///   (resolving some inference variables in the projection may fix this).
/// - `Ok(Ok(Some(obligations)))`: The projection bound holds subject to
///    the given obligations. If the projection cannot be normalized because
///    the required trait bound doesn't hold this returned with `obligations`
///    being a predicate that cannot be proven.
pub(super) fn poly_project_and_unify_type<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &PolyProjectionObligation<'tcx>,
) -> Result<
    Result<Option<Vec<PredicateObligation<'tcx>>>, InProgress>,
    MismatchedProjectionTypes<'tcx>,
> {
    debug!("poly_project_and_unify_type(obligation={:?})", obligation);

    let infcx = selcx.infcx();
    infcx.commit_if_ok(|_snapshot| {
        let (placeholder_predicate, _) =
            infcx.replace_bound_vars_with_placeholders(&obligation.predicate);

        let placeholder_obligation = obligation.with(placeholder_predicate);
        let result = project_and_unify_type(selcx, &placeholder_obligation)?;
        Ok(result)
    })
}

/// Evaluates constraints of the form:
///
///     <T as Trait>::U == V
///
/// If successful, this may result in additional obligations.
///
/// See [poly_project_and_unify_type] for an explanation of the return value.
fn project_and_unify_type<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionObligation<'tcx>,
) -> Result<
    Result<Option<Vec<PredicateObligation<'tcx>>>, InProgress>,
    MismatchedProjectionTypes<'tcx>,
> {
    debug!("project_and_unify_type(obligation={:?})", obligation);

    let mut obligations = vec![];
    let normalized_ty = match opt_normalize_projection_type(
        selcx,
        obligation.param_env,
        obligation.predicate.projection_ty,
        obligation.cause.clone(),
        obligation.recursion_depth,
        &mut obligations,
    ) {
        Ok(Some(n)) => n,
        Ok(None) => return Ok(Ok(None)),
        Err(InProgress) => return Ok(Err(InProgress)),
    };

    debug!(
        "project_and_unify_type: normalized_ty={:?} obligations={:?}",
        normalized_ty, obligations
    );

    let infcx = selcx.infcx();
    match infcx
        .at(&obligation.cause, obligation.param_env)
        .eq(normalized_ty, obligation.predicate.ty)
    {
        Ok(InferOk { obligations: inferred_obligations, value: () }) => {
            obligations.extend(inferred_obligations);
            Ok(Ok(Some(obligations)))
        }
        Err(err) => {
            debug!("project_and_unify_type: equating types encountered error {:?}", err);
            Err(MismatchedProjectionTypes { err })
        }
    }
}

/// Normalizes any associated type projections in `value`, replacing
/// them with a fully resolved type where possible. The return value
/// combines the normalized result and any additional obligations that
/// were incurred as result.
pub fn normalize<'a, 'b, 'tcx, T>(
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    cause: ObligationCause<'tcx>,
    value: &T,
) -> Normalized<'tcx, T>
where
    T: TypeFoldable<'tcx>,
{
    let mut obligations = Vec::new();
    let value = normalize_to(selcx, param_env, cause, value, &mut obligations);
    Normalized { value, obligations }
}

pub fn normalize_to<'a, 'b, 'tcx, T>(
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    cause: ObligationCause<'tcx>,
    value: &T,
    obligations: &mut Vec<PredicateObligation<'tcx>>,
) -> T
where
    T: TypeFoldable<'tcx>,
{
    normalize_with_depth_to(selcx, param_env, cause, 0, value, obligations)
}

/// As `normalize`, but with a custom depth.
pub fn normalize_with_depth<'a, 'b, 'tcx, T>(
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    cause: ObligationCause<'tcx>,
    depth: usize,
    value: &T,
) -> Normalized<'tcx, T>
where
    T: TypeFoldable<'tcx>,
{
    let mut obligations = Vec::new();
    let value = normalize_with_depth_to(selcx, param_env, cause, depth, value, &mut obligations);
    Normalized { value, obligations }
}

pub fn normalize_with_depth_to<'a, 'b, 'tcx, T>(
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    cause: ObligationCause<'tcx>,
    depth: usize,
    value: &T,
    obligations: &mut Vec<PredicateObligation<'tcx>>,
) -> T
where
    T: TypeFoldable<'tcx>,
{
    debug!("normalize_with_depth(depth={}, value={:?})", depth, value);
    let mut normalizer = AssocTypeNormalizer::new(selcx, param_env, cause, depth, obligations);
    let result = ensure_sufficient_stack(|| normalizer.fold(value));
    debug!(
        "normalize_with_depth: depth={} result={:?} with {} obligations",
        depth,
        result,
        normalizer.obligations.len()
    );
    debug!("normalize_with_depth: depth={} obligations={:?}", depth, normalizer.obligations);
    result
}

struct AssocTypeNormalizer<'a, 'b, 'tcx> {
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    cause: ObligationCause<'tcx>,
    obligations: &'a mut Vec<PredicateObligation<'tcx>>,
    depth: usize,
}

impl<'a, 'b, 'tcx> AssocTypeNormalizer<'a, 'b, 'tcx> {
    fn new(
        selcx: &'a mut SelectionContext<'b, 'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        cause: ObligationCause<'tcx>,
        depth: usize,
        obligations: &'a mut Vec<PredicateObligation<'tcx>>,
    ) -> AssocTypeNormalizer<'a, 'b, 'tcx> {
        AssocTypeNormalizer { selcx, param_env, cause, obligations, depth }
    }

    fn fold<T: TypeFoldable<'tcx>>(&mut self, value: &T) -> T {
        let value = self.selcx.infcx().resolve_vars_if_possible(value);

        if !value.has_projections() { value } else { value.fold_with(self) }
    }
}

impl<'a, 'b, 'tcx> TypeFolder<'tcx> for AssocTypeNormalizer<'a, 'b, 'tcx> {
    fn tcx<'c>(&'c self) -> TyCtxt<'tcx> {
        self.selcx.tcx()
    }

    fn fold_ty(&mut self, ty: Ty<'tcx>) -> Ty<'tcx> {
        if !ty.has_projections() {
            return ty;
        }
        // We don't want to normalize associated types that occur inside of region
        // binders, because they may contain bound regions, and we can't cope with that.
        //
        // Example:
        //
        //     for<'a> fn(<T as Foo<&'a>>::A)
        //
        // Instead of normalizing `<T as Foo<&'a>>::A` here, we'll
        // normalize it when we instantiate those bound regions (which
        // should occur eventually).

        let ty = ty.super_fold_with(self);
        match ty.kind {
            ty::Opaque(def_id, substs) => {
                // Only normalize `impl Trait` after type-checking, usually in codegen.
                match self.param_env.reveal() {
                    Reveal::UserFacing => ty,

                    Reveal::All => {
                        let recursion_limit = self.tcx().sess.recursion_limit();
                        if !recursion_limit.value_within_limit(self.depth) {
                            let obligation = Obligation::with_depth(
                                self.cause.clone(),
                                recursion_limit.0,
                                self.param_env,
                                ty,
                            );
                            self.selcx.infcx().report_overflow_error(&obligation, true);
                        }

                        let generic_ty = self.tcx().type_of(def_id);
                        let concrete_ty = generic_ty.subst(self.tcx(), substs);
                        self.depth += 1;
                        let folded_ty = self.fold_ty(concrete_ty);
                        self.depth -= 1;
                        folded_ty
                    }
                }
            }

            ty::Projection(ref data) if !data.has_escaping_bound_vars() => {
                // This is kind of hacky -- we need to be able to
                // handle normalization within binders because
                // otherwise we wind up a need to normalize when doing
                // trait matching (since you can have a trait
                // obligation like `for<'a> T::B: Fn(&'a i32)`), but
                // we can't normalize with bound regions in scope. So
                // far now we just ignore binders but only normalize
                // if all bound regions are gone (and then we still
                // have to renormalize whenever we instantiate a
                // binder). It would be better to normalize in a
                // binding-aware fashion.

                let normalized_ty = normalize_projection_type(
                    self.selcx,
                    self.param_env,
                    *data,
                    self.cause.clone(),
                    self.depth,
                    &mut self.obligations,
                );
                debug!(
                    "AssocTypeNormalizer: depth={} normalized {:?} to {:?}, \
                     now with {} obligations",
                    self.depth,
                    ty,
                    normalized_ty,
                    self.obligations.len()
                );
                normalized_ty
            }

            _ => ty,
        }
    }

    fn fold_const(&mut self, constant: &'tcx ty::Const<'tcx>) -> &'tcx ty::Const<'tcx> {
        if self.selcx.tcx().lazy_normalization() {
            constant
        } else {
            let constant = constant.super_fold_with(self);
            constant.eval(self.selcx.tcx(), self.param_env)
        }
    }
}

/// The guts of `normalize`: normalize a specific projection like `<T
/// as Trait>::Item`. The result is always a type (and possibly
/// additional obligations). If ambiguity arises, which implies that
/// there are unresolved type variables in the projection, we will
/// substitute a fresh type variable `$X` and generate a new
/// obligation `<T as Trait>::Item == $X` for later.
pub fn normalize_projection_type<'a, 'b, 'tcx>(
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    projection_ty: ty::ProjectionTy<'tcx>,
    cause: ObligationCause<'tcx>,
    depth: usize,
    obligations: &mut Vec<PredicateObligation<'tcx>>,
) -> Ty<'tcx> {
    opt_normalize_projection_type(
        selcx,
        param_env,
        projection_ty,
        cause.clone(),
        depth,
        obligations,
    )
    .ok()
    .flatten()
    .unwrap_or_else(move || {
        // if we bottom out in ambiguity, create a type variable
        // and a deferred predicate to resolve this when more type
        // information is available.

        let tcx = selcx.infcx().tcx;
        let def_id = projection_ty.item_def_id;
        let ty_var = selcx.infcx().next_ty_var(TypeVariableOrigin {
            kind: TypeVariableOriginKind::NormalizeProjectionType,
            span: tcx.def_span(def_id),
        });
        let projection = ty::Binder::dummy(ty::ProjectionPredicate { projection_ty, ty: ty_var });
        let obligation =
            Obligation::with_depth(cause, depth + 1, param_env, projection.to_predicate(tcx));
        obligations.push(obligation);
        ty_var
    })
}

/// The guts of `normalize`: normalize a specific projection like `<T
/// as Trait>::Item`. The result is always a type (and possibly
/// additional obligations). Returns `None` in the case of ambiguity,
/// which indicates that there are unbound type variables.
///
/// This function used to return `Option<NormalizedTy<'tcx>>`, which contains a
/// `Ty<'tcx>` and an obligations vector. But that obligation vector was very
/// often immediately appended to another obligations vector. So now this
/// function takes an obligations vector and appends to it directly, which is
/// slightly uglier but avoids the need for an extra short-lived allocation.
fn opt_normalize_projection_type<'a, 'b, 'tcx>(
    selcx: &'a mut SelectionContext<'b, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    projection_ty: ty::ProjectionTy<'tcx>,
    cause: ObligationCause<'tcx>,
    depth: usize,
    obligations: &mut Vec<PredicateObligation<'tcx>>,
) -> Result<Option<Ty<'tcx>>, InProgress> {
    let infcx = selcx.infcx();

    let projection_ty = infcx.resolve_vars_if_possible(&projection_ty);
    let cache_key = ProjectionCacheKey::new(projection_ty);

    debug!(
        "opt_normalize_projection_type(\
         projection_ty={:?}, \
         depth={})",
        projection_ty, depth
    );

    // FIXME(#20304) For now, I am caching here, which is good, but it
    // means we don't capture the type variables that are created in
    // the case of ambiguity. Which means we may create a large stream
    // of such variables. OTOH, if we move the caching up a level, we
    // would not benefit from caching when proving `T: Trait<U=Foo>`
    // bounds. It might be the case that we want two distinct caches,
    // or else another kind of cache entry.

    let cache_result = infcx.inner.borrow_mut().projection_cache().try_start(cache_key);
    match cache_result {
        Ok(()) => {}
        Err(ProjectionCacheEntry::Ambiguous) => {
            // If we found ambiguity the last time, that means we will continue
            // to do so until some type in the key changes (and we know it
            // hasn't, because we just fully resolved it).
            debug!(
                "opt_normalize_projection_type: \
                 found cache entry: ambiguous"
            );
            return Ok(None);
        }
        Err(ProjectionCacheEntry::InProgress) => {
            // If while normalized A::B, we are asked to normalize
            // A::B, just return A::B itself. This is a conservative
            // answer, in the sense that A::B *is* clearly equivalent
            // to A::B, though there may be a better value we can
            // find.

            // Under lazy normalization, this can arise when
            // bootstrapping.  That is, imagine an environment with a
            // where-clause like `A::B == u32`. Now, if we are asked
            // to normalize `A::B`, we will want to check the
            // where-clauses in scope. So we will try to unify `A::B`
            // with `A::B`, which can trigger a recursive
            // normalization.

            debug!(
                "opt_normalize_projection_type: \
                 found cache entry: in-progress"
            );

            return Err(InProgress);
        }
        Err(ProjectionCacheEntry::NormalizedTy(ty)) => {
            // This is the hottest path in this function.
            //
            // If we find the value in the cache, then return it along
            // with the obligations that went along with it. Note
            // that, when using a fulfillment context, these
            // obligations could in principle be ignored: they have
            // already been registered when the cache entry was
            // created (and hence the new ones will quickly be
            // discarded as duplicated). But when doing trait
            // evaluation this is not the case, and dropping the trait
            // evaluations can causes ICEs (e.g., #43132).
            debug!(
                "opt_normalize_projection_type: \
                 found normalized ty `{:?}`",
                ty
            );

            // Once we have inferred everything we need to know, we
            // can ignore the `obligations` from that point on.
            if infcx.unresolved_type_vars(&ty.value).is_none() {
                infcx.inner.borrow_mut().projection_cache().complete_normalized(cache_key, &ty);
            // No need to extend `obligations`.
            } else {
                obligations.extend(ty.obligations);
            }

            obligations.push(get_paranoid_cache_value_obligation(
                infcx,
                param_env,
                projection_ty,
                cause,
                depth,
            ));
            return Ok(Some(ty.value));
        }
        Err(ProjectionCacheEntry::Error) => {
            debug!(
                "opt_normalize_projection_type: \
                 found error"
            );
            let result = normalize_to_error(selcx, param_env, projection_ty, cause, depth);
            obligations.extend(result.obligations);
            return Ok(Some(result.value));
        }
    }

    let obligation = Obligation::with_depth(cause.clone(), depth, param_env, projection_ty);
    match project_type(selcx, &obligation) {
        Ok(ProjectedTy::Progress(Progress {
            ty: projected_ty,
            obligations: mut projected_obligations,
        })) => {
            // if projection succeeded, then what we get out of this
            // is also non-normalized (consider: it was derived from
            // an impl, where-clause etc) and hence we must
            // re-normalize it

            debug!(
                "opt_normalize_projection_type: \
                 projected_ty={:?} \
                 depth={} \
                 projected_obligations={:?}",
                projected_ty, depth, projected_obligations
            );

            let result = if projected_ty.has_projections() {
                let mut normalizer = AssocTypeNormalizer::new(
                    selcx,
                    param_env,
                    cause,
                    depth + 1,
                    &mut projected_obligations,
                );
                let normalized_ty = normalizer.fold(&projected_ty);

                debug!(
                    "opt_normalize_projection_type: \
                     normalized_ty={:?} depth={}",
                    normalized_ty, depth
                );

                Normalized { value: normalized_ty, obligations: projected_obligations }
            } else {
                Normalized { value: projected_ty, obligations: projected_obligations }
            };

            let cache_value = prune_cache_value_obligations(infcx, &result);
            infcx.inner.borrow_mut().projection_cache().insert_ty(cache_key, cache_value);
            obligations.extend(result.obligations);
            Ok(Some(result.value))
        }
        Ok(ProjectedTy::NoProgress(projected_ty)) => {
            debug!(
                "opt_normalize_projection_type: \
                 projected_ty={:?} no progress",
                projected_ty
            );
            let result = Normalized { value: projected_ty, obligations: vec![] };
            infcx.inner.borrow_mut().projection_cache().insert_ty(cache_key, result.clone());
            // No need to extend `obligations`.
            Ok(Some(result.value))
        }
        Err(ProjectionTyError::TooManyCandidates) => {
            debug!(
                "opt_normalize_projection_type: \
                 too many candidates"
            );
            infcx.inner.borrow_mut().projection_cache().ambiguous(cache_key);
            Ok(None)
        }
        Err(ProjectionTyError::TraitSelectionError(_)) => {
            debug!("opt_normalize_projection_type: ERROR");
            // if we got an error processing the `T as Trait` part,
            // just return `ty::err` but add the obligation `T :
            // Trait`, which when processed will cause the error to be
            // reported later

            infcx.inner.borrow_mut().projection_cache().error(cache_key);
            let result = normalize_to_error(selcx, param_env, projection_ty, cause, depth);
            obligations.extend(result.obligations);
            Ok(Some(result.value))
        }
    }
}

/// If there are unresolved type variables, then we need to include
/// any subobligations that bind them, at least until those type
/// variables are fully resolved.
fn prune_cache_value_obligations<'a, 'tcx>(
    infcx: &'a InferCtxt<'a, 'tcx>,
    result: &NormalizedTy<'tcx>,
) -> NormalizedTy<'tcx> {
    if infcx.unresolved_type_vars(&result.value).is_none() {
        return NormalizedTy { value: result.value, obligations: vec![] };
    }

    let mut obligations: Vec<_> = result
        .obligations
        .iter()
        .filter(|obligation| match obligation.predicate.kind() {
            // We found a `T: Foo<X = U>` predicate, let's check
            // if `U` references any unresolved type
            // variables. In principle, we only care if this
            // projection can help resolve any of the type
            // variables found in `result.value` -- but we just
            // check for any type variables here, for fear of
            // indirect obligations (e.g., we project to `?0`,
            // but we have `T: Foo<X = ?1>` and `?1: Bar<X =
            // ?0>`).
            ty::PredicateKind::Projection(ref data) => {
                infcx.unresolved_type_vars(&data.ty()).is_some()
            }

            // We are only interested in `T: Foo<X = U>` predicates, whre
            // `U` references one of `unresolved_type_vars`. =)
            _ => false,
        })
        .cloned()
        .collect();

    obligations.shrink_to_fit();

    NormalizedTy { value: result.value, obligations }
}

/// Whenever we give back a cache result for a projection like `<T as
/// Trait>::Item ==> X`, we *always* include the obligation to prove
/// that `T: Trait` (we may also include some other obligations). This
/// may or may not be necessary -- in principle, all the obligations
/// that must be proven to show that `T: Trait` were also returned
/// when the cache was first populated. But there are some vague concerns,
/// and so we take the precautionary measure of including `T: Trait` in
/// the result:
///
/// Concern #1. The current setup is fragile. Perhaps someone could
/// have failed to prove the concerns from when the cache was
/// populated, but also not have used a snapshot, in which case the
/// cache could remain populated even though `T: Trait` has not been
/// shown. In this case, the "other code" is at fault -- when you
/// project something, you are supposed to either have a snapshot or
/// else prove all the resulting obligations -- but it's still easy to
/// get wrong.
///
/// Concern #2. Even within the snapshot, if those original
/// obligations are not yet proven, then we are able to do projections
/// that may yet turn out to be wrong. This *may* lead to some sort
/// of trouble, though we don't have a concrete example of how that
/// can occur yet. But it seems risky at best.
fn get_paranoid_cache_value_obligation<'a, 'tcx>(
    infcx: &'a InferCtxt<'a, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    projection_ty: ty::ProjectionTy<'tcx>,
    cause: ObligationCause<'tcx>,
    depth: usize,
) -> PredicateObligation<'tcx> {
    let trait_ref = projection_ty.trait_ref(infcx.tcx).to_poly_trait_ref();
    Obligation {
        cause,
        recursion_depth: depth,
        param_env,
        predicate: trait_ref.without_const().to_predicate(infcx.tcx),
    }
}

/// If we are projecting `<T as Trait>::Item`, but `T: Trait` does not
/// hold. In various error cases, we cannot generate a valid
/// normalized projection. Therefore, we create an inference variable
/// return an associated obligation that, when fulfilled, will lead to
/// an error.
///
/// Note that we used to return `Error` here, but that was quite
/// dubious -- the premise was that an error would *eventually* be
/// reported, when the obligation was processed. But in general once
/// you see a `Error` you are supposed to be able to assume that an
/// error *has been* reported, so that you can take whatever heuristic
/// paths you want to take. To make things worse, it was possible for
/// cycles to arise, where you basically had a setup like `<MyType<$0>
/// as Trait>::Foo == $0`. Here, normalizing `<MyType<$0> as
/// Trait>::Foo> to `[type error]` would lead to an obligation of
/// `<MyType<[type error]> as Trait>::Foo`. We are supposed to report
/// an error for this obligation, but we legitimately should not,
/// because it contains `[type error]`. Yuck! (See issue #29857 for
/// one case where this arose.)
fn normalize_to_error<'a, 'tcx>(
    selcx: &mut SelectionContext<'a, 'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    projection_ty: ty::ProjectionTy<'tcx>,
    cause: ObligationCause<'tcx>,
    depth: usize,
) -> NormalizedTy<'tcx> {
    let trait_ref = projection_ty.trait_ref(selcx.tcx()).to_poly_trait_ref();
    let trait_obligation = Obligation {
        cause,
        recursion_depth: depth,
        param_env,
        predicate: trait_ref.without_const().to_predicate(selcx.tcx()),
    };
    let tcx = selcx.infcx().tcx;
    let def_id = projection_ty.item_def_id;
    let new_value = selcx.infcx().next_ty_var(TypeVariableOrigin {
        kind: TypeVariableOriginKind::NormalizeProjectionType,
        span: tcx.def_span(def_id),
    });
    Normalized { value: new_value, obligations: vec![trait_obligation] }
}

enum ProjectedTy<'tcx> {
    Progress(Progress<'tcx>),
    NoProgress(Ty<'tcx>),
}

struct Progress<'tcx> {
    ty: Ty<'tcx>,
    obligations: Vec<PredicateObligation<'tcx>>,
}

impl<'tcx> Progress<'tcx> {
    fn error(tcx: TyCtxt<'tcx>) -> Self {
        Progress { ty: tcx.ty_error(), obligations: vec![] }
    }

    fn with_addl_obligations(mut self, mut obligations: Vec<PredicateObligation<'tcx>>) -> Self {
        debug!(
            "with_addl_obligations: self.obligations.len={} obligations.len={}",
            self.obligations.len(),
            obligations.len()
        );

        debug!(
            "with_addl_obligations: self.obligations={:?} obligations={:?}",
            self.obligations, obligations
        );

        self.obligations.append(&mut obligations);
        self
    }
}

/// Computes the result of a projection type (if we can).
///
/// IMPORTANT:
/// - `obligation` must be fully normalized
fn project_type<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
) -> Result<ProjectedTy<'tcx>, ProjectionTyError<'tcx>> {
    debug!("project(obligation={:?})", obligation);

    if !selcx.tcx().sess.recursion_limit().value_within_limit(obligation.recursion_depth) {
        debug!("project: overflow!");
        return Err(ProjectionTyError::TraitSelectionError(SelectionError::Overflow));
    }

    let obligation_trait_ref = &obligation.predicate.trait_ref(selcx.tcx());

    debug!("project: obligation_trait_ref={:?}", obligation_trait_ref);

    if obligation_trait_ref.references_error() {
        return Ok(ProjectedTy::Progress(Progress::error(selcx.tcx())));
    }

    let mut candidates = ProjectionTyCandidateSet::None;

    // Make sure that the following procedures are kept in order. ParamEnv
    // needs to be first because it has highest priority, and Select checks
    // the return value of push_candidate which assumes it's ran at last.
    assemble_candidates_from_param_env(selcx, obligation, &obligation_trait_ref, &mut candidates);

    assemble_candidates_from_trait_def(selcx, obligation, &obligation_trait_ref, &mut candidates);

    assemble_candidates_from_impls(selcx, obligation, &obligation_trait_ref, &mut candidates);

    match candidates {
        ProjectionTyCandidateSet::Single(candidate) => Ok(ProjectedTy::Progress(
            confirm_candidate(selcx, obligation, &obligation_trait_ref, candidate),
        )),
        ProjectionTyCandidateSet::None => Ok(ProjectedTy::NoProgress(
            selcx
                .tcx()
                .mk_projection(obligation.predicate.item_def_id, obligation.predicate.substs),
        )),
        // Error occurred while trying to processing impls.
        ProjectionTyCandidateSet::Error(e) => Err(ProjectionTyError::TraitSelectionError(e)),
        // Inherent ambiguity that prevents us from even enumerating the
        // candidates.
        ProjectionTyCandidateSet::Ambiguous => Err(ProjectionTyError::TooManyCandidates),
    }
}

/// The first thing we have to do is scan through the parameter
/// environment to see whether there are any projection predicates
/// there that can answer this question.
fn assemble_candidates_from_param_env<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    obligation_trait_ref: &ty::TraitRef<'tcx>,
    candidate_set: &mut ProjectionTyCandidateSet<'tcx>,
) {
    debug!("assemble_candidates_from_param_env(..)");
    assemble_candidates_from_predicates(
        selcx,
        obligation,
        obligation_trait_ref,
        candidate_set,
        ProjectionTyCandidate::ParamEnv,
        obligation.param_env.caller_bounds().iter(),
    );
}

/// In the case of a nested projection like <<A as Foo>::FooT as Bar>::BarT, we may find
/// that the definition of `Foo` has some clues:
///
/// ```
/// trait Foo {
///     type FooT : Bar<BarT=i32>
/// }
/// ```
///
/// Here, for example, we could conclude that the result is `i32`.
fn assemble_candidates_from_trait_def<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    obligation_trait_ref: &ty::TraitRef<'tcx>,
    candidate_set: &mut ProjectionTyCandidateSet<'tcx>,
) {
    debug!("assemble_candidates_from_trait_def(..)");

    let tcx = selcx.tcx();
    // Check whether the self-type is itself a projection.
    // If so, extract what we know from the trait and try to come up with a good answer.
    let bounds = match obligation_trait_ref.self_ty().kind {
        ty::Projection(ref data) => {
            tcx.projection_predicates(data.item_def_id).subst(tcx, data.substs)
        }
        ty::Opaque(def_id, substs) => tcx.projection_predicates(def_id).subst(tcx, substs),
        ty::Infer(ty::TyVar(_)) => {
            // If the self-type is an inference variable, then it MAY wind up
            // being a projected type, so induce an ambiguity.
            candidate_set.mark_ambiguous();
            return;
        }
        _ => return,
    };

    assemble_candidates_from_predicates(
        selcx,
        obligation,
        obligation_trait_ref,
        candidate_set,
        ProjectionTyCandidate::TraitDef,
        bounds.iter(),
    )
}

fn assemble_candidates_from_predicates<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    obligation_trait_ref: &ty::TraitRef<'tcx>,
    candidate_set: &mut ProjectionTyCandidateSet<'tcx>,
    ctor: fn(ty::PolyProjectionPredicate<'tcx>) -> ProjectionTyCandidate<'tcx>,
    env_predicates: impl Iterator<Item = ty::Predicate<'tcx>>,
) {
    debug!("assemble_candidates_from_predicates(obligation={:?})", obligation);
    let infcx = selcx.infcx();
    for predicate in env_predicates {
        debug!("assemble_candidates_from_predicates: predicate={:?}", predicate);
        if let &ty::PredicateKind::Projection(data) = predicate.kind() {
            let same_def_id = data.projection_def_id() == obligation.predicate.item_def_id;

            let is_match = same_def_id
                && infcx.probe(|_| {
                    let data_poly_trait_ref = data.to_poly_trait_ref(infcx.tcx);
                    let obligation_poly_trait_ref = obligation_trait_ref.to_poly_trait_ref();
                    infcx
                        .at(&obligation.cause, obligation.param_env)
                        .sup(obligation_poly_trait_ref, data_poly_trait_ref)
                        .map(|InferOk { obligations: _, value: () }| {
                            // FIXME(#32730) -- do we need to take obligations
                            // into account in any way? At the moment, no.
                        })
                        .is_ok()
                });

            debug!(
                "assemble_candidates_from_predicates: candidate={:?} \
                 is_match={} same_def_id={}",
                data, is_match, same_def_id
            );

            if is_match {
                candidate_set.push_candidate(ctor(data));
            }
        }
    }
}

fn assemble_candidates_from_impls<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    obligation_trait_ref: &ty::TraitRef<'tcx>,
    candidate_set: &mut ProjectionTyCandidateSet<'tcx>,
) {
    // If we are resolving `<T as TraitRef<...>>::Item == Type`,
    // start out by selecting the predicate `T as TraitRef<...>`:
    let poly_trait_ref = obligation_trait_ref.to_poly_trait_ref();
    let trait_obligation = obligation.with(poly_trait_ref.to_poly_trait_predicate());
    let _ = selcx.infcx().commit_if_ok(|_| {
        let impl_source = match selcx.select(&trait_obligation) {
            Ok(Some(impl_source)) => impl_source,
            Ok(None) => {
                candidate_set.mark_ambiguous();
                return Err(());
            }
            Err(e) => {
                debug!("assemble_candidates_from_impls: selection error {:?}", e);
                candidate_set.mark_error(e);
                return Err(());
            }
        };

        let eligible = match &impl_source {
            super::ImplSourceClosure(_)
            | super::ImplSourceGenerator(_)
            | super::ImplSourceFnPointer(_)
            | super::ImplSourceObject(_)
            | super::ImplSourceTraitAlias(_) => {
                debug!("assemble_candidates_from_impls: impl_source={:?}", impl_source);
                true
            }
            super::ImplSourceUserDefined(impl_data) => {
                // We have to be careful when projecting out of an
                // impl because of specialization. If we are not in
                // codegen (i.e., projection mode is not "any"), and the
                // impl's type is declared as default, then we disable
                // projection (even if the trait ref is fully
                // monomorphic). In the case where trait ref is not
                // fully monomorphic (i.e., includes type parameters),
                // this is because those type parameters may
                // ultimately be bound to types from other crates that
                // may have specialized impls we can't see. In the
                // case where the trait ref IS fully monomorphic, this
                // is a policy decision that we made in the RFC in
                // order to preserve flexibility for the crate that
                // defined the specializable impl to specialize later
                // for existing types.
                //
                // In either case, we handle this by not adding a
                // candidate for an impl if it contains a `default`
                // type.
                //
                // NOTE: This should be kept in sync with the similar code in
                // `rustc_ty::instance::resolve_associated_item()`.
                let node_item =
                    assoc_ty_def(selcx, impl_data.impl_def_id, obligation.predicate.item_def_id)
                        .map_err(|ErrorReported| ())?;

                if node_item.is_final() {
                    // Non-specializable items are always projectable.
                    true
                } else {
                    // Only reveal a specializable default if we're past type-checking
                    // and the obligation is monomorphic, otherwise passes such as
                    // transmute checking and polymorphic MIR optimizations could
                    // get a result which isn't correct for all monomorphizations.
                    if obligation.param_env.reveal() == Reveal::All {
                        // NOTE(eddyb) inference variables can resolve to parameters, so
                        // assume `poly_trait_ref` isn't monomorphic, if it contains any.
                        let poly_trait_ref =
                            selcx.infcx().resolve_vars_if_possible(&poly_trait_ref);
                        !poly_trait_ref.still_further_specializable()
                    } else {
                        debug!(
                            "assemble_candidates_from_impls: not eligible due to default: \
                             assoc_ty={} predicate={}",
                            selcx.tcx().def_path_str(node_item.item.def_id),
                            obligation.predicate,
                        );
                        false
                    }
                }
            }
            super::ImplSourceDiscriminantKind(..) => {
                // While `DiscriminantKind` is automatically implemented for every type,
                // the concrete discriminant may not be known yet.
                //
                // Any type with multiple potential discriminant types is therefore not eligible.
                let self_ty = selcx.infcx().shallow_resolve(obligation.predicate.self_ty());

                match self_ty.kind {
                    ty::Bool
                    | ty::Char
                    | ty::Int(_)
                    | ty::Uint(_)
                    | ty::Float(_)
                    | ty::Adt(..)
                    | ty::Foreign(_)
                    | ty::Str
                    | ty::Array(..)
                    | ty::Slice(_)
                    | ty::RawPtr(..)
                    | ty::Ref(..)
                    | ty::FnDef(..)
                    | ty::FnPtr(..)
                    | ty::Dynamic(..)
                    | ty::Closure(..)
                    | ty::Generator(..)
                    | ty::GeneratorWitness(..)
                    | ty::Never
                    | ty::Tuple(..)
                    // Integers and floats always have `u8` as their discriminant.
                    | ty::Infer(ty::InferTy::IntVar(_) | ty::InferTy::FloatVar(..)) => true,

                    ty::Projection(..)
                    | ty::Opaque(..)
                    | ty::Param(..)
                    | ty::Bound(..)
                    | ty::Placeholder(..)
                    | ty::Infer(..)
                    | ty::Error(_) => false,
                }
            }
            super::ImplSourceParam(..) => {
                // This case tell us nothing about the value of an
                // associated type. Consider:
                //
                // ```
                // trait SomeTrait { type Foo; }
                // fn foo<T:SomeTrait>(...) { }
                // ```
                //
                // If the user writes `<T as SomeTrait>::Foo`, then the `T
                // : SomeTrait` binding does not help us decide what the
                // type `Foo` is (at least, not more specifically than
                // what we already knew).
                //
                // But wait, you say! What about an example like this:
                //
                // ```
                // fn bar<T:SomeTrait<Foo=usize>>(...) { ... }
                // ```
                //
                // Doesn't the `T : Sometrait<Foo=usize>` predicate help
                // resolve `T::Foo`? And of course it does, but in fact
                // that single predicate is desugared into two predicates
                // in the compiler: a trait predicate (`T : SomeTrait`) and a
                // projection. And the projection where clause is handled
                // in `assemble_candidates_from_param_env`.
                false
            }
            super::ImplSourceAutoImpl(..) | super::ImplSourceBuiltin(..) => {
                // These traits have no associated types.
                selcx.tcx().sess.delay_span_bug(
                    obligation.cause.span,
                    &format!("Cannot project an associated type from `{:?}`", impl_source),
                );
                return Err(());
            }
        };

        if eligible {
            if candidate_set.push_candidate(ProjectionTyCandidate::Select(impl_source)) {
                Ok(())
            } else {
                Err(())
            }
        } else {
            Err(())
        }
    });
}

fn confirm_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    obligation_trait_ref: &ty::TraitRef<'tcx>,
    candidate: ProjectionTyCandidate<'tcx>,
) -> Progress<'tcx> {
    debug!("confirm_candidate(candidate={:?}, obligation={:?})", candidate, obligation);

    let mut progress = match candidate {
        ProjectionTyCandidate::ParamEnv(poly_projection)
        | ProjectionTyCandidate::TraitDef(poly_projection) => {
            confirm_param_env_candidate(selcx, obligation, poly_projection)
        }

        ProjectionTyCandidate::Select(impl_source) => {
            confirm_select_candidate(selcx, obligation, obligation_trait_ref, impl_source)
        }
    };
    // When checking for cycle during evaluation, we compare predicates with
    // "syntactic" equality. Since normalization generally introduces a type
    // with new region variables, we need to resolve them to existing variables
    // when possible for this to work. See `auto-trait-projection-recursion.rs`
    // for a case where this matters.
    if progress.ty.has_infer_regions() {
        progress.ty = OpportunisticRegionResolver::new(selcx.infcx()).fold_ty(progress.ty);
    }
    progress
}

fn confirm_select_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    obligation_trait_ref: &ty::TraitRef<'tcx>,
    impl_source: Selection<'tcx>,
) -> Progress<'tcx> {
    match impl_source {
        super::ImplSourceUserDefined(data) => confirm_impl_candidate(selcx, obligation, data),
        super::ImplSourceGenerator(data) => confirm_generator_candidate(selcx, obligation, data),
        super::ImplSourceClosure(data) => confirm_closure_candidate(selcx, obligation, data),
        super::ImplSourceFnPointer(data) => confirm_fn_pointer_candidate(selcx, obligation, data),
        super::ImplSourceDiscriminantKind(data) => {
            confirm_discriminant_kind_candidate(selcx, obligation, data)
        }
        super::ImplSourceObject(_) => {
            confirm_object_candidate(selcx, obligation, obligation_trait_ref)
        }
        super::ImplSourceAutoImpl(..)
        | super::ImplSourceParam(..)
        | super::ImplSourceBuiltin(..)
        | super::ImplSourceTraitAlias(..) =>
        // we don't create Select candidates with this kind of resolution
        {
            span_bug!(
                obligation.cause.span,
                "Cannot project an associated type from `{:?}`",
                impl_source
            )
        }
    }
}

fn confirm_object_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    obligation_trait_ref: &ty::TraitRef<'tcx>,
) -> Progress<'tcx> {
    let self_ty = obligation_trait_ref.self_ty();
    let object_ty = selcx.infcx().shallow_resolve(self_ty);
    debug!("confirm_object_candidate(object_ty={:?})", object_ty);
    let data = match object_ty.kind {
        ty::Dynamic(ref data, ..) => data,
        _ => span_bug!(
            obligation.cause.span,
            "confirm_object_candidate called with non-object: {:?}",
            object_ty
        ),
    };
    let env_predicates = data
        .projection_bounds()
        .map(|p| p.with_self_ty(selcx.tcx(), object_ty).to_predicate(selcx.tcx()));
    let env_predicate = {
        let env_predicates = elaborate_predicates(selcx.tcx(), env_predicates);

        // select only those projections that are actually projecting an
        // item with the correct name
        let env_predicates = env_predicates.filter_map(|o| match o.predicate.kind() {
            &ty::PredicateKind::Projection(data)
                if data.projection_def_id() == obligation.predicate.item_def_id =>
            {
                Some(data)
            }
            _ => None,
        });

        // select those with a relevant trait-ref
        let mut env_predicates = env_predicates.filter(|data| {
            let data_poly_trait_ref = data.to_poly_trait_ref(selcx.tcx());
            let obligation_poly_trait_ref = obligation_trait_ref.to_poly_trait_ref();
            selcx.infcx().probe(|_| {
                selcx
                    .infcx()
                    .at(&obligation.cause, obligation.param_env)
                    .sup(obligation_poly_trait_ref, data_poly_trait_ref)
                    .is_ok()
            })
        });

        // select the first matching one; there really ought to be one or
        // else the object type is not WF, since an object type should
        // include all of its projections explicitly
        match env_predicates.next() {
            Some(env_predicate) => env_predicate,
            None => {
                debug!(
                    "confirm_object_candidate: no env-predicate \
                     found in object type `{:?}`; ill-formed",
                    object_ty
                );
                return Progress::error(selcx.tcx());
            }
        }
    };

    confirm_param_env_candidate(selcx, obligation, env_predicate)
}

fn confirm_generator_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    impl_source: ImplSourceGeneratorData<'tcx, PredicateObligation<'tcx>>,
) -> Progress<'tcx> {
    let gen_sig = impl_source.substs.as_generator().poly_sig();
    let Normalized { value: gen_sig, obligations } = normalize_with_depth(
        selcx,
        obligation.param_env,
        obligation.cause.clone(),
        obligation.recursion_depth + 1,
        &gen_sig,
    );

    debug!(
        "confirm_generator_candidate: obligation={:?},gen_sig={:?},obligations={:?}",
        obligation, gen_sig, obligations
    );

    let tcx = selcx.tcx();

    let gen_def_id = tcx.require_lang_item(GeneratorTraitLangItem, None);

    let predicate = super::util::generator_trait_ref_and_outputs(
        tcx,
        gen_def_id,
        obligation.predicate.self_ty(),
        gen_sig,
    )
    .map_bound(|(trait_ref, yield_ty, return_ty)| {
        let name = tcx.associated_item(obligation.predicate.item_def_id).ident.name;
        let ty = if name == sym::Return {
            return_ty
        } else if name == sym::Yield {
            yield_ty
        } else {
            bug!()
        };

        ty::ProjectionPredicate {
            projection_ty: ty::ProjectionTy {
                substs: trait_ref.substs,
                item_def_id: obligation.predicate.item_def_id,
            },
            ty,
        }
    });

    confirm_param_env_candidate(selcx, obligation, predicate)
        .with_addl_obligations(impl_source.nested)
        .with_addl_obligations(obligations)
}

fn confirm_discriminant_kind_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    _: ImplSourceDiscriminantKindData,
) -> Progress<'tcx> {
    let tcx = selcx.tcx();

    let self_ty = selcx.infcx().shallow_resolve(obligation.predicate.self_ty());
    let substs = tcx.mk_substs([self_ty.into()].iter());

    let assoc_items = tcx.associated_items(tcx.lang_items().discriminant_kind_trait().unwrap());
    // FIXME: emit an error if the trait definition is wrong
    let discriminant_def_id = assoc_items.in_definition_order().next().unwrap().def_id;

    let discriminant_ty = match self_ty.kind {
        // Use the discriminant type for enums.
        ty::Adt(adt, _) if adt.is_enum() => adt.repr.discr_type().to_ty(tcx),
        // Default to `i32` for generators.
        ty::Generator(..) => tcx.types.i32,
        // Use `u8` for all other types.
        _ => tcx.types.u8,
    };

    let predicate = ty::ProjectionPredicate {
        projection_ty: ty::ProjectionTy { substs, item_def_id: discriminant_def_id },
        ty: discriminant_ty,
    };

    confirm_param_env_candidate(selcx, obligation, ty::Binder::bind(predicate))
}

fn confirm_fn_pointer_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    fn_pointer_impl_source: ImplSourceFnPointerData<'tcx, PredicateObligation<'tcx>>,
) -> Progress<'tcx> {
    let fn_type = selcx.infcx().shallow_resolve(fn_pointer_impl_source.fn_ty);
    let sig = fn_type.fn_sig(selcx.tcx());
    let Normalized { value: sig, obligations } = normalize_with_depth(
        selcx,
        obligation.param_env,
        obligation.cause.clone(),
        obligation.recursion_depth + 1,
        &sig,
    );

    confirm_callable_candidate(selcx, obligation, sig, util::TupleArgumentsFlag::Yes)
        .with_addl_obligations(fn_pointer_impl_source.nested)
        .with_addl_obligations(obligations)
}

fn confirm_closure_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    impl_source: ImplSourceClosureData<'tcx, PredicateObligation<'tcx>>,
) -> Progress<'tcx> {
    let closure_sig = impl_source.substs.as_closure().sig();
    let Normalized { value: closure_sig, obligations } = normalize_with_depth(
        selcx,
        obligation.param_env,
        obligation.cause.clone(),
        obligation.recursion_depth + 1,
        &closure_sig,
    );

    debug!(
        "confirm_closure_candidate: obligation={:?},closure_sig={:?},obligations={:?}",
        obligation, closure_sig, obligations
    );

    confirm_callable_candidate(selcx, obligation, closure_sig, util::TupleArgumentsFlag::No)
        .with_addl_obligations(impl_source.nested)
        .with_addl_obligations(obligations)
}

fn confirm_callable_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    fn_sig: ty::PolyFnSig<'tcx>,
    flag: util::TupleArgumentsFlag,
) -> Progress<'tcx> {
    let tcx = selcx.tcx();

    debug!("confirm_callable_candidate({:?},{:?})", obligation, fn_sig);

    let fn_once_def_id = tcx.require_lang_item(FnOnceTraitLangItem, None);
    let fn_once_output_def_id = tcx.require_lang_item(FnOnceOutputLangItem, None);

    let predicate = super::util::closure_trait_ref_and_return_type(
        tcx,
        fn_once_def_id,
        obligation.predicate.self_ty(),
        fn_sig,
        flag,
    )
    .map_bound(|(trait_ref, ret_type)| ty::ProjectionPredicate {
        projection_ty: ty::ProjectionTy {
            substs: trait_ref.substs,
            item_def_id: fn_once_output_def_id,
        },
        ty: ret_type,
    });

    confirm_param_env_candidate(selcx, obligation, predicate)
}

fn confirm_param_env_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    poly_cache_entry: ty::PolyProjectionPredicate<'tcx>,
) -> Progress<'tcx> {
    let infcx = selcx.infcx();
    let cause = &obligation.cause;
    let param_env = obligation.param_env;

    let (cache_entry, _) = infcx.replace_bound_vars_with_fresh_vars(
        cause.span,
        LateBoundRegionConversionTime::HigherRankedType,
        &poly_cache_entry,
    );

    let cache_trait_ref = cache_entry.projection_ty.trait_ref(infcx.tcx);
    let obligation_trait_ref = obligation.predicate.trait_ref(infcx.tcx);
    match infcx.at(cause, param_env).eq(cache_trait_ref, obligation_trait_ref) {
        Ok(InferOk { value: _, obligations }) => Progress { ty: cache_entry.ty, obligations },
        Err(e) => {
            let msg = format!(
                "Failed to unify obligation `{:?}` with poly_projection `{:?}`: {:?}",
                obligation, poly_cache_entry, e,
            );
            debug!("confirm_param_env_candidate: {}", msg);
            let err = infcx.tcx.ty_error_with_message(obligation.cause.span, &msg);
            Progress { ty: err, obligations: vec![] }
        }
    }
}

fn confirm_impl_candidate<'cx, 'tcx>(
    selcx: &mut SelectionContext<'cx, 'tcx>,
    obligation: &ProjectionTyObligation<'tcx>,
    impl_impl_source: ImplSourceUserDefinedData<'tcx, PredicateObligation<'tcx>>,
) -> Progress<'tcx> {
    let tcx = selcx.tcx();

    let ImplSourceUserDefinedData { impl_def_id, substs, nested } = impl_impl_source;
    let assoc_item_id = obligation.predicate.item_def_id;
    let trait_def_id = tcx.trait_id_of_impl(impl_def_id).unwrap();

    let param_env = obligation.param_env;
    let assoc_ty = match assoc_ty_def(selcx, impl_def_id, assoc_item_id) {
        Ok(assoc_ty) => assoc_ty,
        Err(ErrorReported) => return Progress { ty: tcx.ty_error(), obligations: nested },
    };

    if !assoc_ty.item.defaultness.has_value() {
        // This means that the impl is missing a definition for the
        // associated type. This error will be reported by the type
        // checker method `check_impl_items_against_trait`, so here we
        // just return Error.
        debug!(
            "confirm_impl_candidate: no associated type {:?} for {:?}",
            assoc_ty.item.ident, obligation.predicate
        );
        return Progress { ty: tcx.ty_error(), obligations: nested };
    }
    // If we're trying to normalize `<Vec<u32> as X>::A<S>` using
    //`impl<T> X for Vec<T> { type A<Y> = Box<Y>; }`, then:
    //
    // * `obligation.predicate.substs` is `[Vec<u32>, S]`
    // * `substs` is `[u32]`
    // * `substs` ends up as `[u32, S]`
    let substs = obligation.predicate.substs.rebase_onto(tcx, trait_def_id, substs);
    let substs =
        translate_substs(selcx.infcx(), param_env, impl_def_id, substs, assoc_ty.defining_node);
    let ty = tcx.type_of(assoc_ty.item.def_id);
    if substs.len() != tcx.generics_of(assoc_ty.item.def_id).count() {
        let err = tcx.ty_error_with_message(
            DUMMY_SP,
            "impl item and trait item have different parameter counts",
        );
        Progress { ty: err, obligations: nested }
    } else {
        Progress { ty: ty.subst(tcx, substs), obligations: nested }
    }
}

/// Locate the definition of an associated type in the specialization hierarchy,
/// starting from the given impl.
///
/// Based on the "projection mode", this lookup may in fact only examine the
/// topmost impl. See the comments for `Reveal` for more details.
fn assoc_ty_def(
    selcx: &SelectionContext<'_, '_>,
    impl_def_id: DefId,
    assoc_ty_def_id: DefId,
) -> Result<specialization_graph::LeafDef, ErrorReported> {
    let tcx = selcx.tcx();
    let assoc_ty_name = tcx.associated_item(assoc_ty_def_id).ident;
    let trait_def_id = tcx.impl_trait_ref(impl_def_id).unwrap().def_id;
    let trait_def = tcx.trait_def(trait_def_id);

    // This function may be called while we are still building the
    // specialization graph that is queried below (via TraitDef::ancestors()),
    // so, in order to avoid unnecessary infinite recursion, we manually look
    // for the associated item at the given impl.
    // If there is no such item in that impl, this function will fail with a
    // cycle error if the specialization graph is currently being built.
    let impl_node = specialization_graph::Node::Impl(impl_def_id);
    for item in impl_node.items(tcx) {
        if matches!(item.kind, ty::AssocKind::Type)
            && tcx.hygienic_eq(item.ident, assoc_ty_name, trait_def_id)
        {
            return Ok(specialization_graph::LeafDef {
                item: *item,
                defining_node: impl_node,
                finalizing_node: if item.defaultness.is_default() { None } else { Some(impl_node) },
            });
        }
    }

    let ancestors = trait_def.ancestors(tcx, impl_def_id)?;
    if let Some(assoc_item) = ancestors.leaf_def(tcx, assoc_ty_name, ty::AssocKind::Type) {
        Ok(assoc_item)
    } else {
        // This is saying that neither the trait nor
        // the impl contain a definition for this
        // associated type.  Normally this situation
        // could only arise through a compiler bug --
        // if the user wrote a bad item name, it
        // should have failed in astconv.
        bug!("No associated type `{}` for {}", assoc_ty_name, tcx.def_path_str(impl_def_id))
    }
}

crate trait ProjectionCacheKeyExt<'tcx>: Sized {
    fn from_poly_projection_predicate(
        selcx: &mut SelectionContext<'cx, 'tcx>,
        predicate: ty::PolyProjectionPredicate<'tcx>,
    ) -> Option<Self>;
}

impl<'tcx> ProjectionCacheKeyExt<'tcx> for ProjectionCacheKey<'tcx> {
    fn from_poly_projection_predicate(
        selcx: &mut SelectionContext<'cx, 'tcx>,
        predicate: ty::PolyProjectionPredicate<'tcx>,
    ) -> Option<Self> {
        let infcx = selcx.infcx();
        // We don't do cross-snapshot caching of obligations with escaping regions,
        // so there's no cache key to use
        predicate.no_bound_vars().map(|predicate| {
            ProjectionCacheKey::new(
                // We don't attempt to match up with a specific type-variable state
                // from a specific call to `opt_normalize_projection_type` - if
                // there's no precise match, the original cache entry is "stranded"
                // anyway.
                infcx.resolve_vars_if_possible(&predicate.projection_ty),
            )
        })
    }
}
