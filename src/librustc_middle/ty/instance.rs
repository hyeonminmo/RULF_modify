use crate::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use crate::ty::print::{FmtPrinter, Printer};
use crate::ty::{self, SubstsRef, Ty, TyCtxt, TypeFoldable};
use rustc_errors::ErrorReported;
use rustc_hir::def::Namespace;
use rustc_hir::def_id::{CrateNum, DefId};
use rustc_hir::lang_items::{DropInPlaceFnLangItem, FnOnceTraitLangItem};
use rustc_macros::HashStable;

use std::fmt;

/// A monomorphized `InstanceDef`.
///
/// Monomorphization happens on-the-fly and no monomorphized MIR is ever created. Instead, this type
/// simply couples a potentially generic `InstanceDef` with some substs, and codegen and const eval
/// will do all required substitution as they run.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, RustcEncodable, RustcDecodable)]
#[derive(HashStable, Lift)]
pub struct Instance<'tcx> {
    pub def: InstanceDef<'tcx>,
    pub substs: SubstsRef<'tcx>,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, RustcEncodable, RustcDecodable, HashStable)]
pub enum InstanceDef<'tcx> {
    /// A user-defined callable item.
    ///
    /// This includes:
    /// - `fn` items
    /// - closures
    /// - generators
    Item(DefId),

    /// An intrinsic `fn` item (with `"rust-intrinsic"` or `"platform-intrinsic"` ABI).
    ///
    /// Alongside `Virtual`, this is the only `InstanceDef` that does not have its own callable MIR.
    /// Instead, codegen and const eval "magically" evaluate calls to intrinsics purely in the
    /// caller.
    Intrinsic(DefId),

    /// `<T as Trait>::method` where `method` receives unsizeable `self: Self` (part of the
    /// `unsized_locals` feature).
    ///
    /// The generated shim will take `Self` via `*mut Self` - conceptually this is `&owned Self` -
    /// and dereference the argument to call the original function.
    VtableShim(DefId),

    /// `fn()` pointer where the function itself cannot be turned into a pointer.
    ///
    /// One example is `<dyn Trait as Trait>::fn`, where the shim contains
    /// a virtual call, which codegen supports only via a direct call to the
    /// `<dyn Trait as Trait>::fn` instance (an `InstanceDef::Virtual`).
    ///
    /// Another example is functions annotated with `#[track_caller]`, which
    /// must have their implicit caller location argument populated for a call.
    /// Because this is a required part of the function's ABI but can't be tracked
    /// as a property of the function pointer, we use a single "caller location"
    /// (the definition of the function itself).
    ReifyShim(DefId),

    /// `<fn() as FnTrait>::call_*` (generated `FnTrait` implementation for `fn()` pointers).
    ///
    /// `DefId` is `FnTrait::call_*`.
    ///
    /// NB: the (`fn` pointer) type must currently be monomorphic to avoid double substitution
    /// problems with the MIR shim bodies. `Instance::resolve` enforces this.
    // FIXME(#69925) support polymorphic MIR shim bodies properly instead.
    FnPtrShim(DefId, Ty<'tcx>),

    /// Dynamic dispatch to `<dyn Trait as Trait>::fn`.
    ///
    /// This `InstanceDef` does not have callable MIR. Calls to `Virtual` instances must be
    /// codegen'd as virtual calls through the vtable.
    ///
    /// If this is reified to a `fn` pointer, a `ReifyShim` is used (see `ReifyShim` above for more
    /// details on that).
    Virtual(DefId, usize),

    /// `<[FnMut closure] as FnOnce>::call_once`.
    ///
    /// The `DefId` is the ID of the `call_once` method in `FnOnce`.
    ClosureOnceShim { call_once: DefId },

    /// `core::ptr::drop_in_place::<T>`.
    ///
    /// The `DefId` is for `core::ptr::drop_in_place`.
    /// The `Option<Ty<'tcx>>` is either `Some(T)`, or `None` for empty drop
    /// glue.
    ///
    /// NB: the type must currently be monomorphic to avoid double substitution
    /// problems with the MIR shim bodies. `Instance::resolve` enforces this.
    // FIXME(#69925) support polymorphic MIR shim bodies properly instead.
    DropGlue(DefId, Option<Ty<'tcx>>),

    /// Compiler-generated `<T as Clone>::clone` implementation.
    ///
    /// For all types that automatically implement `Copy`, a trivial `Clone` impl is provided too.
    /// Additionally, arrays, tuples, and closures get a `Clone` shim even if they aren't `Copy`.
    ///
    /// The `DefId` is for `Clone::clone`, the `Ty` is the type `T` with the builtin `Clone` impl.
    ///
    /// NB: the type must currently be monomorphic to avoid double substitution
    /// problems with the MIR shim bodies. `Instance::resolve` enforces this.
    // FIXME(#69925) support polymorphic MIR shim bodies properly instead.
    CloneShim(DefId, Ty<'tcx>),
}

impl<'tcx> Instance<'tcx> {
    /// Returns the `Ty` corresponding to this `Instance`,
    /// with generic substitutions applied and lifetimes erased.
    ///
    /// This method can only be called when the 'substs' for this Instance
    /// are fully monomorphic (no `ty::Param`'s are present).
    /// This is usually the case (e.g. during codegen).
    /// However, during constant evaluation, we may want
    /// to try to resolve a `Instance` using generic parameters
    /// (e.g. when we are attempting to to do const-propagation).
    /// In this case, `Instance.ty_env` should be used to provide
    /// the `ParamEnv` for our generic context.
    pub fn monomorphic_ty(&self, tcx: TyCtxt<'tcx>) -> Ty<'tcx> {
        let ty = tcx.type_of(self.def.def_id());
        // There shouldn't be any params - if there are, then
        // Instance.ty_env should have been used to provide the proper
        // ParamEnv
        if self.substs.has_param_types_or_consts() {
            bug!("Instance.ty called for type {:?} with params in substs: {:?}", ty, self.substs);
        }
        tcx.subst_and_normalize_erasing_regions(self.substs, ty::ParamEnv::reveal_all(), &ty)
    }

    /// Like `Instance.ty`, but allows a `ParamEnv` to be specified for use during
    /// normalization. This method is only really useful during constant evaluation,
    /// where we are dealing with potentially generic types.
    pub fn ty_env(&self, tcx: TyCtxt<'tcx>, param_env: ty::ParamEnv<'tcx>) -> Ty<'tcx> {
        let ty = tcx.type_of(self.def.def_id());
        tcx.subst_and_normalize_erasing_regions(self.substs, param_env, &ty)
    }

    /// Finds a crate that contains a monomorphization of this instance that
    /// can be linked to from the local crate. A return value of `None` means
    /// no upstream crate provides such an exported monomorphization.
    ///
    /// This method already takes into account the global `-Zshare-generics`
    /// setting, always returning `None` if `share-generics` is off.
    pub fn upstream_monomorphization(&self, tcx: TyCtxt<'tcx>) -> Option<CrateNum> {
        // If we are not in share generics mode, we don't link to upstream
        // monomorphizations but always instantiate our own internal versions
        // instead.
        if !tcx.sess.opts.share_generics() {
            return None;
        }

        // If this is an item that is defined in the local crate, no upstream
        // crate can know about it/provide a monomorphization.
        if self.def_id().is_local() {
            return None;
        }

        // If this a non-generic instance, it cannot be a shared monomorphization.
        self.substs.non_erasable_generics().next()?;

        match self.def {
            InstanceDef::Item(def_id) => tcx
                .upstream_monomorphizations_for(def_id)
                .and_then(|monos| monos.get(&self.substs).cloned()),
            InstanceDef::DropGlue(_, Some(_)) => tcx.upstream_drop_glue_for(self.substs),
            _ => None,
        }
    }
}

impl<'tcx> InstanceDef<'tcx> {
    #[inline]
    pub fn def_id(&self) -> DefId {
        match *self {
            InstanceDef::Item(def_id)
            | InstanceDef::VtableShim(def_id)
            | InstanceDef::ReifyShim(def_id)
            | InstanceDef::FnPtrShim(def_id, _)
            | InstanceDef::Virtual(def_id, _)
            | InstanceDef::Intrinsic(def_id)
            | InstanceDef::ClosureOnceShim { call_once: def_id }
            | InstanceDef::DropGlue(def_id, _)
            | InstanceDef::CloneShim(def_id, _) => def_id,
        }
    }

    #[inline]
    pub fn attrs(&self, tcx: TyCtxt<'tcx>) -> ty::Attributes<'tcx> {
        tcx.get_attrs(self.def_id())
    }

    /// Returns `true` if the LLVM version of this instance is unconditionally
    /// marked with `inline`. This implies that a copy of this instance is
    /// generated in every codegen unit.
    /// Note that this is only a hint. See the documentation for
    /// `generates_cgu_internal_copy` for more information.
    pub fn requires_inline(&self, tcx: TyCtxt<'tcx>) -> bool {
        use rustc_hir::definitions::DefPathData;
        let def_id = match *self {
            ty::InstanceDef::Item(def_id) => def_id,
            ty::InstanceDef::DropGlue(_, Some(_)) => return false,
            _ => return true,
        };
        match tcx.def_key(def_id).disambiguated_data.data {
            DefPathData::Ctor | DefPathData::ClosureExpr => true,
            _ => false,
        }
    }

    /// Returns `true` if the machine code for this instance is instantiated in
    /// each codegen unit that references it.
    /// Note that this is only a hint! The compiler can globally decide to *not*
    /// do this in order to speed up compilation. CGU-internal copies are
    /// only exist to enable inlining. If inlining is not performed (e.g. at
    /// `-Copt-level=0`) then the time for generating them is wasted and it's
    /// better to create a single copy with external linkage.
    pub fn generates_cgu_internal_copy(&self, tcx: TyCtxt<'tcx>) -> bool {
        if self.requires_inline(tcx) {
            return true;
        }
        if let ty::InstanceDef::DropGlue(.., Some(ty)) = *self {
            // Drop glue generally wants to be instantiated at every codegen
            // unit, but without an #[inline] hint. We should make this
            // available to normal end-users.
            if tcx.sess.opts.incremental.is_none() {
                return true;
            }
            // When compiling with incremental, we can generate a *lot* of
            // codegen units. Including drop glue into all of them has a
            // considerable compile time cost.
            //
            // We include enums without destructors to allow, say, optimizing
            // drops of `Option::None` before LTO. We also respect the intent of
            // `#[inline]` on `Drop::drop` implementations.
            return ty.ty_adt_def().map_or(true, |adt_def| {
                adt_def.destructor(tcx).map_or(adt_def.is_enum(), |dtor| {
                    tcx.codegen_fn_attrs(dtor.did).requests_inline()
                })
            });
        }
        tcx.codegen_fn_attrs(self.def_id()).requests_inline()
    }

    pub fn requires_caller_location(&self, tcx: TyCtxt<'_>) -> bool {
        match *self {
            InstanceDef::Item(def_id) => {
                tcx.codegen_fn_attrs(def_id).flags.contains(CodegenFnAttrFlags::TRACK_CALLER)
            }
            _ => false,
        }
    }
}

impl<'tcx> fmt::Display for Instance<'tcx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ty::tls::with(|tcx| {
            let substs = tcx.lift(&self.substs).expect("could not lift for printing");
            FmtPrinter::new(tcx, &mut *f, Namespace::ValueNS)
                .print_def_path(self.def_id(), substs)?;
            Ok(())
        })?;

        match self.def {
            InstanceDef::Item(_) => Ok(()),
            InstanceDef::VtableShim(_) => write!(f, " - shim(vtable)"),
            InstanceDef::ReifyShim(_) => write!(f, " - shim(reify)"),
            InstanceDef::Intrinsic(_) => write!(f, " - intrinsic"),
            InstanceDef::Virtual(_, num) => write!(f, " - virtual#{}", num),
            InstanceDef::FnPtrShim(_, ty) => write!(f, " - shim({:?})", ty),
            InstanceDef::ClosureOnceShim { .. } => write!(f, " - shim"),
            InstanceDef::DropGlue(_, ty) => write!(f, " - shim({:?})", ty),
            InstanceDef::CloneShim(_, ty) => write!(f, " - shim({:?})", ty),
        }
    }
}

impl<'tcx> Instance<'tcx> {
    pub fn new(def_id: DefId, substs: SubstsRef<'tcx>) -> Instance<'tcx> {
        assert!(
            !substs.has_escaping_bound_vars(),
            "substs of instance {:?} not normalized for codegen: {:?}",
            def_id,
            substs
        );
        Instance { def: InstanceDef::Item(def_id), substs }
    }

    pub fn mono(tcx: TyCtxt<'tcx>, def_id: DefId) -> Instance<'tcx> {
        Instance::new(def_id, tcx.empty_substs_for_def_id(def_id))
    }

    #[inline]
    pub fn def_id(&self) -> DefId {
        self.def.def_id()
    }

    /// Resolves a `(def_id, substs)` pair to an (optional) instance -- most commonly,
    /// this is used to find the precise code that will run for a trait method invocation,
    /// if known.
    ///
    /// Returns `Ok(None)` if we cannot resolve `Instance` to a specific instance.
    /// For example, in a context like this,
    ///
    /// ```
    /// fn foo<T: Debug>(t: T) { ... }
    /// ```
    ///
    /// trying to resolve `Debug::fmt` applied to `T` will yield `Ok(None)`, because we do not
    /// know what code ought to run. (Note that this setting is also affected by the
    /// `RevealMode` in the parameter environment.)
    ///
    /// Presuming that coherence and type-check have succeeded, if this method is invoked
    /// in a monomorphic context (i.e., like during codegen), then it is guaranteed to return
    /// `Ok(Some(instance))`.
    ///
    /// Returns `Err(ErrorReported)` when the `Instance` resolution process
    /// couldn't complete due to errors elsewhere - this is distinct
    /// from `Ok(None)` to avoid misleading diagnostics when an error
    /// has already been/will be emitted, for the original cause
    pub fn resolve(
        tcx: TyCtxt<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        def_id: DefId,
        substs: SubstsRef<'tcx>,
    ) -> Result<Option<Instance<'tcx>>, ErrorReported> {
        // All regions in the result of this query are erased, so it's
        // fine to erase all of the input regions.

        // HACK(eddyb) erase regions in `substs` first, so that `param_env.and(...)`
        // below is more likely to ignore the bounds in scope (e.g. if the only
        // generic parameters mentioned by `substs` were lifetime ones).
        let substs = tcx.erase_regions(&substs);

        // FIXME(eddyb) should this always use `param_env.with_reveal_all()`?
        tcx.resolve_instance(tcx.erase_regions(&param_env.and((def_id, substs))))
    }

    pub fn resolve_for_fn_ptr(
        tcx: TyCtxt<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        def_id: DefId,
        substs: SubstsRef<'tcx>,
    ) -> Option<Instance<'tcx>> {
        debug!("resolve(def_id={:?}, substs={:?})", def_id, substs);
        Instance::resolve(tcx, param_env, def_id, substs).ok().flatten().map(|mut resolved| {
            match resolved.def {
                InstanceDef::Item(def_id) if resolved.def.requires_caller_location(tcx) => {
                    debug!(" => fn pointer created for function with #[track_caller]");
                    resolved.def = InstanceDef::ReifyShim(def_id);
                }
                InstanceDef::Virtual(def_id, _) => {
                    debug!(" => fn pointer created for virtual call");
                    resolved.def = InstanceDef::ReifyShim(def_id);
                }
                _ => {}
            }

            resolved
        })
    }

    pub fn resolve_for_vtable(
        tcx: TyCtxt<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        def_id: DefId,
        substs: SubstsRef<'tcx>,
    ) -> Option<Instance<'tcx>> {
        debug!("resolve(def_id={:?}, substs={:?})", def_id, substs);
        let fn_sig = tcx.fn_sig(def_id);
        let is_vtable_shim = !fn_sig.inputs().skip_binder().is_empty()
            && fn_sig.input(0).skip_binder().is_param(0)
            && tcx.generics_of(def_id).has_self;
        if is_vtable_shim {
            debug!(" => associated item with unsizeable self: Self");
            Some(Instance { def: InstanceDef::VtableShim(def_id), substs })
        } else {
            Instance::resolve_for_fn_ptr(tcx, param_env, def_id, substs)
        }
    }

    pub fn resolve_closure(
        tcx: TyCtxt<'tcx>,
        def_id: DefId,
        substs: ty::SubstsRef<'tcx>,
        requested_kind: ty::ClosureKind,
    ) -> Instance<'tcx> {
        let actual_kind = substs.as_closure().kind();

        match needs_fn_once_adapter_shim(actual_kind, requested_kind) {
            Ok(true) => Instance::fn_once_adapter_instance(tcx, def_id, substs),
            _ => Instance::new(def_id, substs),
        }
    }

    pub fn resolve_drop_in_place(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> ty::Instance<'tcx> {
        let def_id = tcx.require_lang_item(DropInPlaceFnLangItem, None);
        let substs = tcx.intern_substs(&[ty.into()]);
        Instance::resolve(tcx, ty::ParamEnv::reveal_all(), def_id, substs).unwrap().unwrap()
    }

    pub fn fn_once_adapter_instance(
        tcx: TyCtxt<'tcx>,
        closure_did: DefId,
        substs: ty::SubstsRef<'tcx>,
    ) -> Instance<'tcx> {
        debug!("fn_once_adapter_shim({:?}, {:?})", closure_did, substs);
        let fn_once = tcx.require_lang_item(FnOnceTraitLangItem, None);
        let call_once = tcx
            .associated_items(fn_once)
            .in_definition_order()
            .find(|it| it.kind == ty::AssocKind::Fn)
            .unwrap()
            .def_id;
        let def = ty::InstanceDef::ClosureOnceShim { call_once };

        let self_ty = tcx.mk_closure(closure_did, substs);

        let sig = substs.as_closure().sig();
        let sig = tcx.normalize_erasing_late_bound_regions(ty::ParamEnv::reveal_all(), &sig);
        assert_eq!(sig.inputs().len(), 1);
        let substs = tcx.mk_substs_trait(self_ty, &[sig.inputs()[0].into()]);

        debug!("fn_once_adapter_shim: self_ty={:?} sig={:?}", self_ty, sig);
        Instance { def, substs }
    }

    /// FIXME(#69925) Depending on the kind of `InstanceDef`, the MIR body associated with an
    /// instance is expressed in terms of the generic parameters of `self.def_id()`, and in other
    /// cases the MIR body is expressed in terms of the types found in the substitution array.
    /// In the former case, we want to substitute those generic types and replace them with the
    /// values from the substs when monomorphizing the function body. But in the latter case, we
    /// don't want to do that substitution, since it has already been done effectively.
    ///
    /// This function returns `Some(substs)` in the former case and None otherwise -- i.e., if
    /// this function returns `None`, then the MIR body does not require substitution during
    /// monomorphization.
    pub fn substs_for_mir_body(&self) -> Option<SubstsRef<'tcx>> {
        match self.def {
            InstanceDef::CloneShim(..)
            | InstanceDef::DropGlue(_, Some(_)) => None,
            InstanceDef::ClosureOnceShim { .. }
            | InstanceDef::DropGlue(..)
            // FIXME(#69925): `FnPtrShim` should be in the other branch.
            | InstanceDef::FnPtrShim(..)
            | InstanceDef::Item(_)
            | InstanceDef::Intrinsic(..)
            | InstanceDef::ReifyShim(..)
            | InstanceDef::Virtual(..)
            | InstanceDef::VtableShim(..) => Some(self.substs),
        }
    }
}

fn needs_fn_once_adapter_shim(
    actual_closure_kind: ty::ClosureKind,
    trait_closure_kind: ty::ClosureKind,
) -> Result<bool, ()> {
    match (actual_closure_kind, trait_closure_kind) {
        (ty::ClosureKind::Fn, ty::ClosureKind::Fn)
        | (ty::ClosureKind::FnMut, ty::ClosureKind::FnMut)
        | (ty::ClosureKind::FnOnce, ty::ClosureKind::FnOnce) => {
            // No adapter needed.
            Ok(false)
        }
        (ty::ClosureKind::Fn, ty::ClosureKind::FnMut) => {
            // The closure fn `llfn` is a `fn(&self, ...)`.  We want a
            // `fn(&mut self, ...)`. In fact, at codegen time, these are
            // basically the same thing, so we can just return llfn.
            Ok(false)
        }
        (ty::ClosureKind::Fn | ty::ClosureKind::FnMut, ty::ClosureKind::FnOnce) => {
            // The closure fn `llfn` is a `fn(&self, ...)` or `fn(&mut
            // self, ...)`.  We want a `fn(self, ...)`. We can produce
            // this by doing something like:
            //
            //     fn call_once(self, ...) { call_mut(&self, ...) }
            //     fn call_once(mut self, ...) { call_mut(&mut self, ...) }
            //
            // These are both the same at codegen time.
            Ok(true)
        }
        (ty::ClosureKind::FnMut | ty::ClosureKind::FnOnce, _) => Err(()),
    }
}
