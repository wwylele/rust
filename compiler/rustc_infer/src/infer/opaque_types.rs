use crate::infer::{InferCtxt, InferOk};
use crate::traits;
use hir::def_id::{DefId, LocalDefId};
use hir::{HirId, OpaqueTyOrigin};
use rustc_data_structures::sync::Lrc;
use rustc_data_structures::vec_map::VecMap;
use rustc_hir as hir;
use rustc_middle::traits::{ObligationCause, ObligationCauseCode};
use rustc_middle::ty::fold::BottomUpFolder;
use rustc_middle::ty::subst::{GenericArgKind, Subst};
use rustc_middle::ty::{
    self, OpaqueHiddenType, OpaqueTypeKey, Ty, TyCtxt, TypeFoldable, TypeSuperFoldable, TypeVisitor,
};
use rustc_span::Span;

use std::ops::ControlFlow;

pub type OpaqueTypeMap<'tcx> = VecMap<OpaqueTypeKey<'tcx>, OpaqueTypeDecl<'tcx>>;

mod table;

pub use table::{OpaqueTypeStorage, OpaqueTypeTable};

use super::type_variable::{TypeVariableOrigin, TypeVariableOriginKind};
use super::InferResult;

/// Information about the opaque types whose values we
/// are inferring in this function (these are the `impl Trait` that
/// appear in the return type).
#[derive(Clone, Debug)]
pub struct OpaqueTypeDecl<'tcx> {
    /// The hidden types that have been inferred for this opaque type.
    /// There can be multiple, but they are all `lub`ed together at the end
    /// to obtain the canonical hidden type.
    pub hidden_type: OpaqueHiddenType<'tcx>,

    /// The origin of the opaque type.
    pub origin: hir::OpaqueTyOrigin,
}

impl<'a, 'tcx> InferCtxt<'a, 'tcx> {
    /// This is a backwards compatibility hack to prevent breaking changes from
    /// lazy TAIT around RPIT handling.
    pub fn replace_opaque_types_with_inference_vars<T: TypeFoldable<'tcx>>(
        &self,
        value: T,
        body_id: HirId,
        span: Span,
        code: ObligationCauseCode<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
    ) -> InferOk<'tcx, T> {
        if !value.has_opaque_types() {
            return InferOk { value, obligations: vec![] };
        }
        let mut obligations = vec![];
        let value = value.fold_with(&mut ty::fold::BottomUpFolder {
            tcx: self.tcx,
            lt_op: |lt| lt,
            ct_op: |ct| ct,
            ty_op: |ty| match *ty.kind() {
                // Closures can't create hidden types for opaque types of their parent, as they
                // do not have all the outlives information available. Also `type_of` looks for
                // hidden types in the owner (so the closure's parent), so it would not find these
                // definitions.
                ty::Opaque(def_id, _substs)
                    if matches!(
                        self.opaque_type_origin(def_id, span),
                        Some(OpaqueTyOrigin::FnReturn(..))
                    ) =>
                {
                    let span = if span.is_dummy() { self.tcx.def_span(def_id) } else { span };
                    let cause = ObligationCause::new(span, body_id, code.clone());
                    // FIXME(compiler-errors): We probably should add a new TypeVariableOriginKind
                    // for opaque types, and then use that kind to fix the spans for type errors
                    // that we see later on.
                    let ty_var = self.next_ty_var(TypeVariableOrigin {
                        kind: TypeVariableOriginKind::TypeInference,
                        span,
                    });
                    obligations.extend(
                        self.handle_opaque_type(ty, ty_var, true, &cause, param_env)
                            .unwrap()
                            .obligations,
                    );
                    ty_var
                }
                _ => ty,
            },
        });
        InferOk { value, obligations }
    }

    pub fn handle_opaque_type(
        &self,
        a: Ty<'tcx>,
        b: Ty<'tcx>,
        a_is_expected: bool,
        cause: &ObligationCause<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
    ) -> InferResult<'tcx, ()> {
        if a.references_error() || b.references_error() {
            return Ok(InferOk { value: (), obligations: vec![] });
        }
        let (a, b) = if a_is_expected { (a, b) } else { (b, a) };
        let process = |a: Ty<'tcx>, b: Ty<'tcx>| match *a.kind() {
            ty::Opaque(def_id, substs) => {
                let origin = if self.defining_use_anchor.is_some() {
                    // Check that this is `impl Trait` type is
                    // declared by `parent_def_id` -- i.e., one whose
                    // value we are inferring.  At present, this is
                    // always true during the first phase of
                    // type-check, but not always true later on during
                    // NLL. Once we support named opaque types more fully,
                    // this same scenario will be able to arise during all phases.
                    //
                    // Here is an example using type alias `impl Trait`
                    // that indicates the distinction we are checking for:
                    //
                    // ```rust
                    // mod a {
                    //   pub type Foo = impl Iterator;
                    //   pub fn make_foo() -> Foo { .. }
                    // }
                    //
                    // mod b {
                    //   fn foo() -> a::Foo { a::make_foo() }
                    // }
                    // ```
                    //
                    // Here, the return type of `foo` references an
                    // `Opaque` indeed, but not one whose value is
                    // presently being inferred. You can get into a
                    // similar situation with closure return types
                    // today:
                    //
                    // ```rust
                    // fn foo() -> impl Iterator { .. }
                    // fn bar() {
                    //     let x = || foo(); // returns the Opaque assoc with `foo`
                    // }
                    // ```
                    self.opaque_type_origin(def_id, cause.span)?
                } else {
                    self.opaque_ty_origin_unchecked(def_id, cause.span)
                };
                if let ty::Opaque(did2, _) = *b.kind() {
                    // We could accept this, but there are various ways to handle this situation, and we don't
                    // want to make a decision on it right now. Likely this case is so super rare anyway, that
                    // no one encounters it in practice.
                    // It does occur however in `fn fut() -> impl Future<Output = i32> { async { 42 } }`,
                    // where it is of no concern, so we only check for TAITs.
                    if let Some(OpaqueTyOrigin::TyAlias) = self.opaque_type_origin(did2, cause.span)
                    {
                        self.tcx
                                .sess
                                .struct_span_err(
                                    cause.span,
                                    "opaque type's hidden type cannot be another opaque type from the same scope",
                                )
                                .span_label(cause.span, "one of the two opaque types used here has to be outside its defining scope")
                                .span_note(
                                    self.tcx.def_span(def_id),
                                    "opaque type whose hidden type is being assigned",
                                )
                                .span_note(
                                    self.tcx.def_span(did2),
                                    "opaque type being used as hidden type",
                                )
                                .emit();
                    }
                }
                Some(self.register_hidden_type(
                    OpaqueTypeKey { def_id, substs },
                    cause.clone(),
                    param_env,
                    b,
                    origin,
                ))
            }
            _ => None,
        };
        if let Some(res) = process(a, b) {
            res
        } else if let Some(res) = process(b, a) {
            res
        } else {
            // Rerun equality check, but this time error out due to
            // different types.
            match self.at(cause, param_env).define_opaque_types(false).eq(a, b) {
                Ok(_) => span_bug!(
                    cause.span,
                    "opaque types are never equal to anything but themselves: {:#?}",
                    (a.kind(), b.kind())
                ),
                Err(e) => Err(e),
            }
        }
    }

    /// Given the map `opaque_types` containing the opaque
    /// `impl Trait` types whose underlying, hidden types are being
    /// inferred, this method adds constraints to the regions
    /// appearing in those underlying hidden types to ensure that they
    /// at least do not refer to random scopes within the current
    /// function. These constraints are not (quite) sufficient to
    /// guarantee that the regions are actually legal values; that
    /// final condition is imposed after region inference is done.
    ///
    /// # The Problem
    ///
    /// Let's work through an example to explain how it works. Assume
    /// the current function is as follows:
    ///
    /// ```text
    /// fn foo<'a, 'b>(..) -> (impl Bar<'a>, impl Bar<'b>)
    /// ```
    ///
    /// Here, we have two `impl Trait` types whose values are being
    /// inferred (the `impl Bar<'a>` and the `impl
    /// Bar<'b>`). Conceptually, this is sugar for a setup where we
    /// define underlying opaque types (`Foo1`, `Foo2`) and then, in
    /// the return type of `foo`, we *reference* those definitions:
    ///
    /// ```text
    /// type Foo1<'x> = impl Bar<'x>;
    /// type Foo2<'x> = impl Bar<'x>;
    /// fn foo<'a, 'b>(..) -> (Foo1<'a>, Foo2<'b>) { .. }
    ///                    //  ^^^^ ^^
    ///                    //  |    |
    ///                    //  |    substs
    ///                    //  def_id
    /// ```
    ///
    /// As indicating in the comments above, each of those references
    /// is (in the compiler) basically a substitution (`substs`)
    /// applied to the type of a suitable `def_id` (which identifies
    /// `Foo1` or `Foo2`).
    ///
    /// Now, at this point in compilation, what we have done is to
    /// replace each of the references (`Foo1<'a>`, `Foo2<'b>`) with
    /// fresh inference variables C1 and C2. We wish to use the values
    /// of these variables to infer the underlying types of `Foo1` and
    /// `Foo2`. That is, this gives rise to higher-order (pattern) unification
    /// constraints like:
    ///
    /// ```text
    /// for<'a> (Foo1<'a> = C1)
    /// for<'b> (Foo1<'b> = C2)
    /// ```
    ///
    /// For these equation to be satisfiable, the types `C1` and `C2`
    /// can only refer to a limited set of regions. For example, `C1`
    /// can only refer to `'static` and `'a`, and `C2` can only refer
    /// to `'static` and `'b`. The job of this function is to impose that
    /// constraint.
    ///
    /// Up to this point, C1 and C2 are basically just random type
    /// inference variables, and hence they may contain arbitrary
    /// regions. In fact, it is fairly likely that they do! Consider
    /// this possible definition of `foo`:
    ///
    /// ```text
    /// fn foo<'a, 'b>(x: &'a i32, y: &'b i32) -> (impl Bar<'a>, impl Bar<'b>) {
    ///         (&*x, &*y)
    ///     }
    /// ```
    ///
    /// Here, the values for the concrete types of the two impl
    /// traits will include inference variables:
    ///
    /// ```text
    /// &'0 i32
    /// &'1 i32
    /// ```
    ///
    /// Ordinarily, the subtyping rules would ensure that these are
    /// sufficiently large. But since `impl Bar<'a>` isn't a specific
    /// type per se, we don't get such constraints by default. This
    /// is where this function comes into play. It adds extra
    /// constraints to ensure that all the regions which appear in the
    /// inferred type are regions that could validly appear.
    ///
    /// This is actually a bit of a tricky constraint in general. We
    /// want to say that each variable (e.g., `'0`) can only take on
    /// values that were supplied as arguments to the opaque type
    /// (e.g., `'a` for `Foo1<'a>`) or `'static`, which is always in
    /// scope. We don't have a constraint quite of this kind in the current
    /// region checker.
    ///
    /// # The Solution
    ///
    /// We generally prefer to make `<=` constraints, since they
    /// integrate best into the region solver. To do that, we find the
    /// "minimum" of all the arguments that appear in the substs: that
    /// is, some region which is less than all the others. In the case
    /// of `Foo1<'a>`, that would be `'a` (it's the only choice, after
    /// all). Then we apply that as a least bound to the variables
    /// (e.g., `'a <= '0`).
    ///
    /// In some cases, there is no minimum. Consider this example:
    ///
    /// ```text
    /// fn baz<'a, 'b>() -> impl Trait<'a, 'b> { ... }
    /// ```
    ///
    /// Here we would report a more complex "in constraint", like `'r
    /// in ['a, 'b, 'static]` (where `'r` is some region appearing in
    /// the hidden type).
    ///
    /// # Constrain regions, not the hidden concrete type
    ///
    /// Note that generating constraints on each region `Rc` is *not*
    /// the same as generating an outlives constraint on `Tc` itself.
    /// For example, if we had a function like this:
    ///
    /// ```
    /// # #![feature(type_alias_impl_trait)]
    /// # fn main() {}
    /// # trait Foo<'a> {}
    /// # impl<'a, T> Foo<'a> for (&'a u32, T) {}
    /// fn foo<'a, T>(x: &'a u32, y: T) -> impl Foo<'a> {
    ///   (x, y)
    /// }
    ///
    /// // Equivalent to:
    /// # mod dummy { use super::*;
    /// type FooReturn<'a, T> = impl Foo<'a>;
    /// fn foo<'a, T>(x: &'a u32, y: T) -> FooReturn<'a, T> {
    ///   (x, y)
    /// }
    /// # }
    /// ```
    ///
    /// then the hidden type `Tc` would be `(&'0 u32, T)` (where `'0`
    /// is an inference variable). If we generated a constraint that
    /// `Tc: 'a`, then this would incorrectly require that `T: 'a` --
    /// but this is not necessary, because the opaque type we
    /// create will be allowed to reference `T`. So we only generate a
    /// constraint that `'0: 'a`.
    #[instrument(level = "debug", skip(self))]
    pub fn register_member_constraints(
        &self,
        param_env: ty::ParamEnv<'tcx>,
        opaque_type_key: OpaqueTypeKey<'tcx>,
        concrete_ty: Ty<'tcx>,
        span: Span,
    ) {
        let def_id = opaque_type_key.def_id;

        let tcx = self.tcx;

        let concrete_ty = self.resolve_vars_if_possible(concrete_ty);

        debug!(?concrete_ty);

        let first_own_region = match self.opaque_ty_origin_unchecked(def_id, span) {
            hir::OpaqueTyOrigin::FnReturn(..) | hir::OpaqueTyOrigin::AsyncFn(..) => {
                // We lower
                //
                // fn foo<'l0..'ln>() -> impl Trait<'l0..'lm>
                //
                // into
                //
                // type foo::<'p0..'pn>::Foo<'q0..'qm>
                // fn foo<l0..'ln>() -> foo::<'static..'static>::Foo<'l0..'lm>.
                //
                // For these types we only iterate over `'l0..lm` below.
                tcx.generics_of(def_id).parent_count
            }
            // These opaque type inherit all lifetime parameters from their
            // parent, so we have to check them all.
            hir::OpaqueTyOrigin::TyAlias => 0,
        };

        // For a case like `impl Foo<'a, 'b>`, we would generate a constraint
        // `'r in ['a, 'b, 'static]` for each region `'r` that appears in the
        // hidden type (i.e., it must be equal to `'a`, `'b`, or `'static`).
        //
        // `conflict1` and `conflict2` are the two region bounds that we
        // detected which were unrelated. They are used for diagnostics.

        // Create the set of choice regions: each region in the hidden
        // type can be equal to any of the region parameters of the
        // opaque type definition.
        let choice_regions: Lrc<Vec<ty::Region<'tcx>>> = Lrc::new(
            opaque_type_key.substs[first_own_region..]
                .iter()
                .filter_map(|arg| match arg.unpack() {
                    GenericArgKind::Lifetime(r) => Some(r),
                    GenericArgKind::Type(_) | GenericArgKind::Const(_) => None,
                })
                .chain(std::iter::once(self.tcx.lifetimes.re_static))
                .collect(),
        );

        concrete_ty.visit_with(&mut ConstrainOpaqueTypeRegionVisitor {
            op: |r| {
                self.member_constraint(
                    opaque_type_key.def_id,
                    span,
                    concrete_ty,
                    r,
                    &choice_regions,
                )
            },
        });
    }

    #[instrument(skip(self), level = "trace")]
    pub fn opaque_type_origin(&self, opaque_def_id: DefId, span: Span) -> Option<OpaqueTyOrigin> {
        let def_id = opaque_def_id.as_local()?;
        let opaque_hir_id = self.tcx.hir().local_def_id_to_hir_id(def_id);
        let parent_def_id = self.defining_use_anchor?;
        let item_kind = &self.tcx.hir().expect_item(def_id).kind;

        let hir::ItemKind::OpaqueTy(hir::OpaqueTy { origin, ..  }) = item_kind else {
            span_bug!(
                span,
                "weird opaque type: {:#?}, {:#?}",
                opaque_def_id,
                item_kind
            )
        };
        let in_definition_scope = match *origin {
            // Async `impl Trait`
            hir::OpaqueTyOrigin::AsyncFn(parent) => parent == parent_def_id,
            // Anonymous `impl Trait`
            hir::OpaqueTyOrigin::FnReturn(parent) => parent == parent_def_id,
            // Named `type Foo = impl Bar;`
            hir::OpaqueTyOrigin::TyAlias => {
                may_define_opaque_type(self.tcx, parent_def_id, opaque_hir_id)
            }
        };
        trace!(?origin);
        in_definition_scope.then_some(*origin)
    }

    #[instrument(skip(self), level = "trace")]
    fn opaque_ty_origin_unchecked(&self, opaque_def_id: DefId, span: Span) -> OpaqueTyOrigin {
        let def_id = opaque_def_id.as_local().unwrap();
        let origin = match self.tcx.hir().expect_item(def_id).kind {
            hir::ItemKind::OpaqueTy(hir::OpaqueTy { origin, .. }) => origin,
            ref itemkind => {
                span_bug!(span, "weird opaque type: {:?}, {:#?}", opaque_def_id, itemkind)
            }
        };
        trace!(?origin);
        origin
    }
}

// Visitor that requires that (almost) all regions in the type visited outlive
// `least_region`. We cannot use `push_outlives_components` because regions in
// closure signatures are not included in their outlives components. We need to
// ensure all regions outlive the given bound so that we don't end up with,
// say, `ReVar` appearing in a return type and causing ICEs when other
// functions end up with region constraints involving regions from other
// functions.
//
// We also cannot use `for_each_free_region` because for closures it includes
// the regions parameters from the enclosing item.
//
// We ignore any type parameters because impl trait values are assumed to
// capture all the in-scope type parameters.
struct ConstrainOpaqueTypeRegionVisitor<OP> {
    op: OP,
}

impl<'tcx, OP> TypeVisitor<'tcx> for ConstrainOpaqueTypeRegionVisitor<OP>
where
    OP: FnMut(ty::Region<'tcx>),
{
    fn visit_binder<T: TypeFoldable<'tcx>>(
        &mut self,
        t: &ty::Binder<'tcx, T>,
    ) -> ControlFlow<Self::BreakTy> {
        t.super_visit_with(self);
        ControlFlow::CONTINUE
    }

    fn visit_region(&mut self, r: ty::Region<'tcx>) -> ControlFlow<Self::BreakTy> {
        match *r {
            // ignore bound regions, keep visiting
            ty::ReLateBound(_, _) => ControlFlow::CONTINUE,
            _ => {
                (self.op)(r);
                ControlFlow::CONTINUE
            }
        }
    }

    fn visit_ty(&mut self, ty: Ty<'tcx>) -> ControlFlow<Self::BreakTy> {
        // We're only interested in types involving regions
        if !ty.flags().intersects(ty::TypeFlags::HAS_FREE_REGIONS) {
            return ControlFlow::CONTINUE;
        }

        match ty.kind() {
            ty::Closure(_, ref substs) => {
                // Skip lifetime parameters of the enclosing item(s)

                substs.as_closure().tupled_upvars_ty().visit_with(self);
                substs.as_closure().sig_as_fn_ptr_ty().visit_with(self);
            }

            ty::Generator(_, ref substs, _) => {
                // Skip lifetime parameters of the enclosing item(s)
                // Also skip the witness type, because that has no free regions.

                substs.as_generator().tupled_upvars_ty().visit_with(self);
                substs.as_generator().return_ty().visit_with(self);
                substs.as_generator().yield_ty().visit_with(self);
                substs.as_generator().resume_ty().visit_with(self);
            }
            _ => {
                ty.super_visit_with(self);
            }
        }

        ControlFlow::CONTINUE
    }
}

pub enum UseKind {
    DefiningUse,
    OpaqueUse,
}

impl UseKind {
    pub fn is_defining(self) -> bool {
        match self {
            UseKind::DefiningUse => true,
            UseKind::OpaqueUse => false,
        }
    }
}

impl<'a, 'tcx> InferCtxt<'a, 'tcx> {
    #[instrument(skip(self), level = "debug")]
    pub fn register_hidden_type(
        &self,
        opaque_type_key: OpaqueTypeKey<'tcx>,
        cause: ObligationCause<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        hidden_ty: Ty<'tcx>,
        origin: hir::OpaqueTyOrigin,
    ) -> InferResult<'tcx, ()> {
        let tcx = self.tcx;
        let OpaqueTypeKey { def_id, substs } = opaque_type_key;

        // Ideally, we'd get the span where *this specific `ty` came
        // from*, but right now we just use the span from the overall
        // value being folded. In simple cases like `-> impl Foo`,
        // these are the same span, but not in cases like `-> (impl
        // Foo, impl Bar)`.
        let span = cause.span;

        let mut obligations = vec![];
        let prev = self.inner.borrow_mut().opaque_types().register(
            OpaqueTypeKey { def_id, substs },
            OpaqueHiddenType { ty: hidden_ty, span },
            origin,
        );
        if let Some(prev) = prev {
            obligations = self.at(&cause, param_env).eq(prev, hidden_ty)?.obligations;
        }

        let item_bounds = tcx.bound_explicit_item_bounds(def_id);

        for predicate in item_bounds.transpose_iter().map(|e| e.map_bound(|(p, _)| *p)) {
            debug!(?predicate);
            let predicate = predicate.subst(tcx, substs);

            let predicate = predicate.fold_with(&mut BottomUpFolder {
                tcx,
                ty_op: |ty| match *ty.kind() {
                    // We can't normalize associated types from `rustc_infer`,
                    // but we can eagerly register inference variables for them.
                    ty::Projection(projection_ty) if !projection_ty.has_escaping_bound_vars() => {
                        self.infer_projection(
                            param_env,
                            projection_ty,
                            cause.clone(),
                            0,
                            &mut obligations,
                        )
                    }
                    // Replace all other mentions of the same opaque type with the hidden type,
                    // as the bounds must hold on the hidden type after all.
                    ty::Opaque(def_id2, substs2) if def_id == def_id2 && substs == substs2 => {
                        hidden_ty
                    }
                    _ => ty,
                },
                lt_op: |lt| lt,
                ct_op: |ct| ct,
            });

            if let ty::PredicateKind::Projection(projection) = predicate.kind().skip_binder() {
                if projection.term.references_error() {
                    // No point on adding these obligations since there's a type error involved.
                    return Ok(InferOk { value: (), obligations: vec![] });
                }
                trace!("{:#?}", projection.term);
            }
            // Require that the predicate holds for the concrete type.
            debug!(?predicate);
            obligations.push(traits::Obligation::new(cause.clone(), param_env, predicate));
        }
        Ok(InferOk { value: (), obligations })
    }
}

/// Returns `true` if `opaque_hir_id` is a sibling or a child of a sibling of `def_id`.
///
/// Example:
/// ```ignore UNSOLVED (is this a bug?)
/// # #![feature(type_alias_impl_trait)]
/// pub mod foo {
///     pub mod bar {
///         pub trait Bar { /* ... */ }
///         pub type Baz = impl Bar;
///
///         # impl Bar for () {}
///         fn f1() -> Baz { /* ... */ }
///     }
///     fn f2() -> bar::Baz { /* ... */ }
/// }
/// ```
///
/// Here, `def_id` is the `LocalDefId` of the defining use of the opaque type (e.g., `f1` or `f2`),
/// and `opaque_hir_id` is the `HirId` of the definition of the opaque type `Baz`.
/// For the above example, this function returns `true` for `f1` and `false` for `f2`.
fn may_define_opaque_type(tcx: TyCtxt<'_>, def_id: LocalDefId, opaque_hir_id: hir::HirId) -> bool {
    let mut hir_id = tcx.hir().local_def_id_to_hir_id(def_id);

    // Named opaque types can be defined by any siblings or children of siblings.
    let scope = tcx.hir().get_defining_scope(opaque_hir_id);
    // We walk up the node tree until we hit the root or the scope of the opaque type.
    while hir_id != scope && hir_id != hir::CRATE_HIR_ID {
        hir_id = tcx.hir().local_def_id_to_hir_id(tcx.hir().get_parent_item(hir_id));
    }
    // Syntactically, we are allowed to define the concrete type if:
    let res = hir_id == scope;
    trace!(
        "may_define_opaque_type(def={:?}, opaque_node={:?}) = {}",
        tcx.hir().find(hir_id),
        tcx.hir().get(opaque_hir_id),
        res
    );
    res
}
