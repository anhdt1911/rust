use smallvec::smallvec;

use crate::infer::outlives::components::{push_outlives_components, Component};
use crate::traits::{self, Obligation, PredicateObligation};
use rustc_data_structures::fx::{FxHashSet, FxIndexSet};
use rustc_middle::ty::{self, ToPredicate, TyCtxt};
use rustc_span::symbol::Ident;
use rustc_span::Span;

pub fn anonymize_predicate<'tcx>(
    tcx: TyCtxt<'tcx>,
    pred: ty::Predicate<'tcx>,
) -> ty::Predicate<'tcx> {
    let new = tcx.anonymize_bound_vars(pred.kind());
    tcx.reuse_or_mk_predicate(pred, new)
}

pub struct PredicateSet<'tcx> {
    tcx: TyCtxt<'tcx>,
    set: FxHashSet<ty::Predicate<'tcx>>,
}

impl<'tcx> PredicateSet<'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>) -> Self {
        Self { tcx, set: Default::default() }
    }

    pub fn insert(&mut self, pred: ty::Predicate<'tcx>) -> bool {
        // We have to be careful here because we want
        //
        //    for<'a> Foo<&'a i32>
        //
        // and
        //
        //    for<'b> Foo<&'b i32>
        //
        // to be considered equivalent. So normalize all late-bound
        // regions before we throw things into the underlying set.
        self.set.insert(anonymize_predicate(self.tcx, pred))
    }
}

impl<'tcx> Extend<ty::Predicate<'tcx>> for PredicateSet<'tcx> {
    fn extend<I: IntoIterator<Item = ty::Predicate<'tcx>>>(&mut self, iter: I) {
        for pred in iter {
            self.insert(pred);
        }
    }

    fn extend_one(&mut self, pred: ty::Predicate<'tcx>) {
        self.insert(pred);
    }

    fn extend_reserve(&mut self, additional: usize) {
        Extend::<ty::Predicate<'tcx>>::extend_reserve(&mut self.set, additional);
    }
}

///////////////////////////////////////////////////////////////////////////
// `Elaboration` iterator
///////////////////////////////////////////////////////////////////////////

/// "Elaboration" is the process of identifying all the predicates that
/// are implied by a source predicate. Currently, this basically means
/// walking the "supertraits" and other similar assumptions. For example,
/// if we know that `T: Ord`, the elaborator would deduce that `T: PartialOrd`
/// holds as well. Similarly, if we have `trait Foo: 'static`, and we know that
/// `T: Foo`, then we know that `T: 'static`.
pub struct Elaborator<'tcx, O> {
    stack: Vec<O>,
    visited: PredicateSet<'tcx>,
}

/// Describes how to elaborate an obligation into a sub-obligation.
///
/// For [`Obligation`], a sub-obligation is combined with the current obligation's
/// param-env and cause code. For [`ty::Predicate`], none of this is needed, since
/// there is no param-env or cause code to copy over.
pub trait Elaboratable<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx>;

    // Makes a new `Self` but with a different predicate.
    fn child(&self, predicate: ty::Predicate<'tcx>) -> Self;

    // Makes a new `Self` but with a different predicate and a different cause
    // code (if `Self` has one).
    fn child_with_derived_cause(
        &self,
        predicate: ty::Predicate<'tcx>,
        span: Span,
        parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        index: usize,
    ) -> Self;
}

impl<'tcx> Elaboratable<'tcx> for PredicateObligation<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.predicate
    }

    fn child(&self, predicate: ty::Predicate<'tcx>) -> Self {
        Obligation {
            cause: self.cause.clone(),
            param_env: self.param_env,
            recursion_depth: 0,
            predicate,
        }
    }

    fn child_with_derived_cause(
        &self,
        predicate: ty::Predicate<'tcx>,
        span: Span,
        parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        index: usize,
    ) -> Self {
        let cause = self.cause.clone().derived_cause(parent_trait_pred, |derived| {
            traits::ImplDerivedObligation(Box::new(traits::ImplDerivedObligationCause {
                derived,
                impl_or_alias_def_id: parent_trait_pred.def_id(),
                impl_def_predicate_index: Some(index),
                span,
            }))
        });
        Obligation { cause, param_env: self.param_env, recursion_depth: 0, predicate }
    }
}

impl<'tcx> Elaboratable<'tcx> for ty::Predicate<'tcx> {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        *self
    }

    fn child(&self, predicate: ty::Predicate<'tcx>) -> Self {
        predicate
    }

    fn child_with_derived_cause(
        &self,
        predicate: ty::Predicate<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        _index: usize,
    ) -> Self {
        predicate
    }
}

impl<'tcx> Elaboratable<'tcx> for (ty::Predicate<'tcx>, Span) {
    fn predicate(&self) -> ty::Predicate<'tcx> {
        self.0
    }

    fn child(&self, predicate: ty::Predicate<'tcx>) -> Self {
        (predicate, self.1)
    }

    fn child_with_derived_cause(
        &self,
        predicate: ty::Predicate<'tcx>,
        _span: Span,
        _parent_trait_pred: ty::PolyTraitPredicate<'tcx>,
        _index: usize,
    ) -> Self {
        (predicate, self.1)
    }
}

pub fn elaborate<'tcx, O: Elaboratable<'tcx>>(
    tcx: TyCtxt<'tcx>,
    obligations: impl IntoIterator<Item = O>,
) -> Elaborator<'tcx, O> {
    let mut elaborator = Elaborator { stack: Vec::new(), visited: PredicateSet::new(tcx) };
    elaborator.extend_deduped(obligations);
    elaborator
}

impl<'tcx, O: Elaboratable<'tcx>> Elaborator<'tcx, O> {
    fn extend_deduped(&mut self, obligations: impl IntoIterator<Item = O>) {
        // Only keep those bounds that we haven't already seen.
        // This is necessary to prevent infinite recursion in some
        // cases. One common case is when people define
        // `trait Sized: Sized { }` rather than `trait Sized { }`.
        // let visited = &mut self.visited;
        self.stack.extend(obligations.into_iter().filter(|o| self.visited.insert(o.predicate())));
    }

    fn elaborate(&mut self, elaboratable: &O) {
        let tcx = self.visited.tcx;

        let bound_predicate = elaboratable.predicate().kind();
        match bound_predicate.skip_binder() {
            ty::PredicateKind::Clause(ty::Clause::Trait(data)) => {
                // Get predicates declared on the trait.
                let predicates = tcx.super_predicates_of(data.def_id());

                let obligations =
                    predicates.predicates.iter().enumerate().map(|(index, &(mut pred, span))| {
                        // when parent predicate is non-const, elaborate it to non-const predicates.
                        if data.constness == ty::BoundConstness::NotConst {
                            pred = pred.without_const(tcx);
                        }
                        elaboratable.child_with_derived_cause(
                            pred.subst_supertrait(tcx, &bound_predicate.rebind(data.trait_ref)),
                            span,
                            bound_predicate.rebind(data),
                            index,
                        )
                    });
                debug!(?data, ?obligations, "super_predicates");
                self.extend_deduped(obligations);
            }
            ty::PredicateKind::WellFormed(..) => {
                // Currently, we do not elaborate WF predicates,
                // although we easily could.
            }
            ty::PredicateKind::ObjectSafe(..) => {
                // Currently, we do not elaborate object-safe
                // predicates.
            }
            ty::PredicateKind::Subtype(..) => {
                // Currently, we do not "elaborate" predicates like `X <: Y`,
                // though conceivably we might.
            }
            ty::PredicateKind::Coerce(..) => {
                // Currently, we do not "elaborate" predicates like `X -> Y`,
                // though conceivably we might.
            }
            ty::PredicateKind::Clause(ty::Clause::Projection(..)) => {
                // Nothing to elaborate in a projection predicate.
            }
            ty::PredicateKind::ClosureKind(..) => {
                // Nothing to elaborate when waiting for a closure's kind to be inferred.
            }
            ty::PredicateKind::ConstEvaluatable(..) => {
                // Currently, we do not elaborate const-evaluatable
                // predicates.
            }
            ty::PredicateKind::ConstEquate(..) => {
                // Currently, we do not elaborate const-equate
                // predicates.
            }
            ty::PredicateKind::Clause(ty::Clause::RegionOutlives(..)) => {
                // Nothing to elaborate from `'a: 'b`.
            }
            ty::PredicateKind::Clause(ty::Clause::TypeOutlives(ty::OutlivesPredicate(
                ty_max,
                r_min,
            ))) => {
                // We know that `T: 'a` for some type `T`. We can
                // often elaborate this. For example, if we know that
                // `[U]: 'a`, that implies that `U: 'a`. Similarly, if
                // we know `&'a U: 'b`, then we know that `'a: 'b` and
                // `U: 'b`.
                //
                // We can basically ignore bound regions here. So for
                // example `for<'c> Foo<'a,'c>: 'b` can be elaborated to
                // `'a: 'b`.

                // Ignore `for<'a> T: 'a` -- we might in the future
                // consider this as evidence that `T: 'static`, but
                // I'm a bit wary of such constructions and so for now
                // I want to be conservative. --nmatsakis
                if r_min.is_late_bound() {
                    return;
                }

                let mut components = smallvec![];
                push_outlives_components(tcx, ty_max, &mut components);
                self.extend_deduped(
                    components
                        .into_iter()
                        .filter_map(|component| match component {
                            Component::Region(r) => {
                                if r.is_late_bound() {
                                    None
                                } else {
                                    Some(ty::PredicateKind::Clause(ty::Clause::RegionOutlives(
                                        ty::OutlivesPredicate(r, r_min),
                                    )))
                                }
                            }

                            Component::Param(p) => {
                                let ty = tcx.mk_ty_param(p.index, p.name);
                                Some(ty::PredicateKind::Clause(ty::Clause::TypeOutlives(
                                    ty::OutlivesPredicate(ty, r_min),
                                )))
                            }

                            Component::UnresolvedInferenceVariable(_) => None,

                            Component::Alias(alias_ty) => {
                                // We might end up here if we have `Foo<<Bar as Baz>::Assoc>: 'a`.
                                // With this, we can deduce that `<Bar as Baz>::Assoc: 'a`.
                                Some(ty::PredicateKind::Clause(ty::Clause::TypeOutlives(
                                    ty::OutlivesPredicate(alias_ty.to_ty(tcx), r_min),
                                )))
                            }

                            Component::EscapingAlias(_) => {
                                // We might be able to do more here, but we don't
                                // want to deal with escaping vars right now.
                                None
                            }
                        })
                        .map(|predicate_kind| {
                            bound_predicate.rebind(predicate_kind).to_predicate(tcx)
                        })
                        .map(|predicate| elaboratable.child(predicate)),
                );
            }
            ty::PredicateKind::TypeWellFormedFromEnv(..) => {
                // Nothing to elaborate
            }
            ty::PredicateKind::Ambiguous => {}
            ty::PredicateKind::AliasRelate(..) => {
                // No
            }
            ty::PredicateKind::Clause(ty::Clause::ConstArgHasType(..)) => {
                // Nothing to elaborate
            }
        }
    }
}

impl<'tcx, O: Elaboratable<'tcx>> Iterator for Elaborator<'tcx, O> {
    type Item = O;

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.stack.len(), None)
    }

    fn next(&mut self) -> Option<Self::Item> {
        // Extract next item from top-most stack frame, if any.
        if let Some(obligation) = self.stack.pop() {
            self.elaborate(&obligation);
            Some(obligation)
        } else {
            None
        }
    }
}

///////////////////////////////////////////////////////////////////////////
// Supertrait iterator
///////////////////////////////////////////////////////////////////////////

pub fn supertraits<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_ref: ty::PolyTraitRef<'tcx>,
) -> impl Iterator<Item = ty::PolyTraitRef<'tcx>> {
    let pred: ty::Predicate<'tcx> = trait_ref.to_predicate(tcx);
    FilterToTraits::new(elaborate(tcx, [pred]))
}

pub fn transitive_bounds<'tcx>(
    tcx: TyCtxt<'tcx>,
    trait_refs: impl Iterator<Item = ty::PolyTraitRef<'tcx>>,
) -> impl Iterator<Item = ty::PolyTraitRef<'tcx>> {
    FilterToTraits::new(elaborate(
        tcx,
        trait_refs.map(|trait_ref| -> ty::Predicate<'tcx> { trait_ref.to_predicate(tcx) }),
    ))
}

/// A specialized variant of `elaborate` that only elaborates trait references that may
/// define the given associated type `assoc_name`. It uses the
/// `super_predicates_that_define_assoc_type` query to avoid enumerating super-predicates that
/// aren't related to `assoc_item`. This is used when resolving types like `Self::Item` or
/// `T::Item` and helps to avoid cycle errors (see e.g. #35237).
pub fn transitive_bounds_that_define_assoc_type<'tcx>(
    tcx: TyCtxt<'tcx>,
    bounds: impl Iterator<Item = ty::PolyTraitRef<'tcx>>,
    assoc_name: Ident,
) -> impl Iterator<Item = ty::PolyTraitRef<'tcx>> {
    let mut stack: Vec<_> = bounds.collect();
    let mut visited = FxIndexSet::default();

    std::iter::from_fn(move || {
        while let Some(trait_ref) = stack.pop() {
            let anon_trait_ref = tcx.anonymize_bound_vars(trait_ref);
            if visited.insert(anon_trait_ref) {
                let super_predicates = tcx.super_predicates_that_define_assoc_type((
                    trait_ref.def_id(),
                    Some(assoc_name),
                ));
                for (super_predicate, _) in super_predicates.predicates {
                    let subst_predicate = super_predicate.subst_supertrait(tcx, &trait_ref);
                    if let Some(binder) = subst_predicate.to_opt_poly_trait_pred() {
                        stack.push(binder.map_bound(|t| t.trait_ref));
                    }
                }

                return Some(trait_ref);
            }
        }

        return None;
    })
}

///////////////////////////////////////////////////////////////////////////
// Other
///////////////////////////////////////////////////////////////////////////

/// A filter around an iterator of predicates that makes it yield up
/// just trait references.
pub struct FilterToTraits<I> {
    base_iterator: I,
}

impl<I> FilterToTraits<I> {
    fn new(base: I) -> FilterToTraits<I> {
        FilterToTraits { base_iterator: base }
    }
}

impl<'tcx, I: Iterator<Item = ty::Predicate<'tcx>>> Iterator for FilterToTraits<I> {
    type Item = ty::PolyTraitRef<'tcx>;

    fn next(&mut self) -> Option<ty::PolyTraitRef<'tcx>> {
        while let Some(pred) = self.base_iterator.next() {
            if let Some(data) = pred.to_opt_poly_trait_pred() {
                return Some(data.map_bound(|t| t.trait_ref));
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (_, upper) = self.base_iterator.size_hint();
        (0, upper)
    }
}
