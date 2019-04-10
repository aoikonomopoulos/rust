//! ### Inferring borrow kinds for upvars
//!
//! Whenever there is a closure expression, we need to determine how each
//! upvar is used. We do this by initially assigning each upvar an
//! immutable "borrow kind" (see `ty::BorrowKind` for details) and then
//! "escalating" the kind as needed. The borrow kind proceeds according to
//! the following lattice:
//!
//!     ty::ImmBorrow -> ty::UniqueImmBorrow -> ty::MutBorrow
//!
//! So, for example, if we see an assignment `x = 5` to an upvar `x`, we
//! will promote its borrow kind to mutable borrow. If we see an `&mut x`
//! we'll do the same. Naturally, this applies not just to the upvar, but
//! to everything owned by `x`, so the result is the same for something
//! like `x.f = 5` and so on (presuming `x` is not a borrowed pointer to a
//! struct). These adjustments are performed in
//! `adjust_upvar_borrow_kind()` (you can trace backwards through the code
//! from there).
//!
//! The fact that we are inferring borrow kinds as we go results in a
//! semi-hacky interaction with mem-categorization. In particular,
//! mem-categorization will query the current borrow kind as it
//! categorizes, and we'll return the *current* value, but this may get
//! adjusted later. Therefore, in this module, we generally ignore the
//! borrow kind (and derived mutabilities) that are returned from
//! mem-categorization, since they may be inaccurate. (Another option
//! would be to use a unification scheme, where instead of returning a
//! concrete borrow kind like `ty::ImmBorrow`, we return a
//! `ty::InferBorrow(upvar_id)` or something like that, but this would
//! then mean that all later passes would have to check for these figments
//! and report an error, and it just seems like more mess in the end.)

use super::FnCtxt;

use errors::DiagnosticBuilder;
use crate::middle::expr_use_visitor as euv;
use crate::middle::mem_categorization as mc;
use crate::middle::mem_categorization::Categorization;
use rustc::hir;
use rustc::hir::def_id::DefId;
use rustc::hir::def_id::LocalDefId;
use rustc::hir::intravisit::{self, NestedVisitorMap, Visitor};
use rustc::infer::UpvarRegion;
use rustc::ty::{self, Ty, TyCtxt, UpvarSubsts};
use syntax::ast;
use syntax_pos::Span;
use rustc_data_structures::fx::FxHashSet;

impl<'a, 'gcx, 'tcx> FnCtxt<'a, 'gcx, 'tcx> {
    pub fn closure_analyze(&self, body: &'gcx hir::Body) {
        InferBorrowKindVisitor { fcx: self }.visit_body(body);

        // it's our job to process these.
        assert!(self.deferred_call_resolutions.borrow().is_empty());

        let tcx = self.tcx;
        let item_id = tcx.hir().body_owner(body.id());
        let item_def_id = tcx.hir().local_def_id(item_id);

        if !tcx.has_attr(item_def_id, "rustc_dump_closure_captures") {
            return
        }

        let mut errors_buffer = Vec::new();
        for (upvar_id, path_map) in self.tables.borrow().upvar_captures.iter() {
            let upvar_node_id = tcx.hir().hir_to_node_id(upvar_id.var_path.hir_id);
            let upvar = tcx.hir().node_to_string(upvar_node_id);
            let closure_node_id = tcx.hir().local_def_id_to_node_id(upvar_id.closure_expr_id);
            let span = tcx.hir().span(closure_node_id);
            for (path, capture) in path_map {
                let mut err = tcx.sess.diagnostic().span_note_diag(
                    span,
                    "closure capture path"
                );
                err.note(&format!("Upvar {} {:?}: {}", upvar, path, capture));
                err.buffer(&mut errors_buffer);
            }
        }
        if !errors_buffer.is_empty() {
            errors_buffer.sort_by_key(|diag| diag.span.primary_span());
            for diag in errors_buffer.drain(..) {
                DiagnosticBuilder::new_diagnostic(tcx.sess.diagnostic(), diag).emit();
            }
        }
    }
}

struct InferBorrowKindVisitor<'a, 'gcx: 'a + 'tcx, 'tcx: 'a> {
    fcx: &'a FnCtxt<'a, 'gcx, 'tcx>,
}

impl<'a, 'gcx, 'tcx> Visitor<'gcx> for InferBorrowKindVisitor<'a, 'gcx, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'gcx> {
        NestedVisitorMap::None
    }

    fn visit_expr(&mut self, expr: &'gcx hir::Expr) {
        if let hir::ExprKind::Closure(cc, _, body_id, _, _) = expr.node {
            let body = self.fcx.tcx.hir().body(body_id);
            self.visit_body(body);
            self.fcx
                .analyze_closure(expr.hir_id, expr.span, body, cc);
        }

        intravisit::walk_expr(self, expr);
    }
}

impl<'a, 'gcx, 'tcx> FnCtxt<'a, 'gcx, 'tcx> {
    fn analyze_closure(
        &self,
        closure_hir_id: hir::HirId,
        span: Span,
        body: &hir::Body,
        capture_clause: hir::CaptureClause,
    ) {
        /*!
         * Analysis starting point.
         */

        debug!(
            "analyze_closure(id={:?}, body.id={:?})",
            closure_hir_id,
            body.id()
        );

        // Extract the type of the closure.
        let ty = self.node_ty(closure_hir_id);
        let (closure_def_id, substs) = match ty.sty {
            ty::Closure(def_id, substs) => (def_id, UpvarSubsts::Closure(substs)),
            ty::Generator(def_id, substs, _) => (def_id, UpvarSubsts::Generator(substs)),
            ty::Error => {
                // #51714: skip analysis when we have already encountered type errors
                return;
            }
            _ => {
                span_bug!(
                    span,
                    "type of closure expr {:?} is not a closure {:?}",
                    closure_hir_id,
                    ty
                );
            }
        };

        let infer_kind = if let UpvarSubsts::Closure(closure_substs) = substs {
            if self.closure_kind(closure_def_id, closure_substs).is_none() {
                Some(closure_substs)
            } else {
                None
            }
        } else {
            None
        };

        self.tcx.with_freevars(closure_hir_id, |freevars| {
            let mut freevar_list: Vec<ty::UpvarId> = Vec::with_capacity(freevars.len());
            for freevar in freevars {
                let upvar_id = ty::UpvarId {
                    var_path: ty::UpvarPath {
                        hir_id: self.tcx.hir().node_to_hir_id(freevar.var_id()),
                    },
                    closure_expr_id: LocalDefId::from_def_id(closure_def_id),
                };
                debug!("seed upvar_id {:?}", upvar_id);
                // Adding the upvar Id to the list of Upvars, which will be added
                // to the map for the closure at the end of the for loop.
                freevar_list.push(upvar_id.clone());

                let capture_kind = match capture_clause {
                    hir::CaptureByValue => ty::UpvarCapture::ByValue,
                    hir::CaptureByRef => {
                        let origin = UpvarRegion(upvar_id.clone(), span);
                        let freevar_region = self.next_region_var(origin);
                        let upvar_borrow = ty::UpvarBorrow {
                            kind: ty::ImmBorrow,
                            region: freevar_region,
                        };
                        ty::UpvarCapture::ByRef(upvar_borrow)
                    }
                };

                self.tables
                    .borrow_mut()
                    .upvar_capture_map
                    .insert(upvar_id, capture_kind);
            }
            // Add the vector of freevars to the map keyed with the closure id.
            // This gives us an easier access to them without having to call
            // with_freevars again..
            if !freevar_list.is_empty() {
                self.tables
                    .borrow_mut()
                    .upvar_list
                    .insert(closure_def_id, freevar_list);
            }
        });

        let body_owner_def_id = self.tcx.hir().body_owner_def_id(body.id());
        let region_scope_tree = &self.tcx.region_scope_tree(body_owner_def_id);
        let mut delegate = InferBorrowKind {
            fcx: self,
            closure_def_id: closure_def_id,
            current_closure_kind: ty::ClosureKind::LATTICE_BOTTOM,
            current_origin: None,
            adjust_upvar_captures: ty::UpvarCaptureMap::default(),
            span: span,
            capture_clause: capture_clause,
            upvar_captures: ty::UpvarMap::default(),
        };
        let parts_omitted = {
            let tables = self.tables.borrow();
            let mut euv = euv::ExprUseVisitor::with_infer(
                &mut delegate,
                &self.infcx,
                self.param_env,
                region_scope_tree,
                &tables,
            );
            euv.consume_body(body);
            euv.parts_omitted()
        };

        if let Some(closure_substs) = infer_kind {
            // Unify the (as yet unbound) type variable in the closure
            // substs with the kind we inferred.
            let inferred_kind = delegate.current_closure_kind;
            let closure_kind_ty = closure_substs.closure_kind_ty(closure_def_id, self.tcx);
            self.demand_eqtype(span, inferred_kind.to_ty(self.tcx), closure_kind_ty);

            // If we have an origin, store it.
            if let Some(origin) = delegate.current_origin {
                self.tables
                    .borrow_mut()
                    .closure_kind_origins_mut()
                    .insert(closure_hir_id, origin);
            }
        }
        // Ensure that we have captured /a/ path for every upvar; this
        // is not sufficient to guarantee we haven't missed a path;
        // but, if we've completely missed an upvar, we'll know.
        if !parts_omitted &&
            !(self.tables
              .borrow()
              .upvar_list
              .get(&closure_def_id)
              .unwrap_or(&vec![])
              .iter()
              .map(|upvar_id| upvar_id.var_path)
              .collect::<FxHashSet<ty::UpvarPath>>()).is_superset(
                &delegate.upvar_captures
                    .keys()
                    .map(|upvar_id| upvar_id.var_path)
                    .collect::<FxHashSet<ty::UpvarPath>>()) {
                let empty = vec![];
                let tables = self.tables.borrow();
                let keys1: FxHashSet<ty::UpvarPath> = tables
                    .upvar_list
                    .get(&closure_def_id)
                    .unwrap_or(&empty)
                    .iter()
                    .map(|upvar_id| upvar_id.var_path)
                    .collect();
                let keys2: FxHashSet<ty::UpvarPath> = delegate.upvar_captures
                    .keys()
                    .map(|upvar_id| upvar_id.var_path)
                    .collect();
                bug!("{:#?} is not a superset of {:#?}",
                     keys1,
                     keys2,
                );
            }

        self.tables
            .borrow_mut()
            .upvar_capture_map
            .extend(delegate.adjust_upvar_captures);

        self.tables
            .borrow_mut()
            .upvar_captures
            .extend(delegate.upvar_captures);
        // Now that we've analyzed the closure, we know how each
        // variable is borrowed, and we know what traits the closure
        // implements (Fn vs FnMut etc). We now have some updates to do
        // with that information.
        //
        // Note that no closure type C may have an upvar of type C
        // (though it may reference itself via a trait object). This
        // results from the desugaring of closures to a struct like
        // `Foo<..., UV0...UVn>`. If one of those upvars referenced
        // C, then the type would have infinite size (and the
        // inference algorithm will reject it).

        // Equate the type variables for the upvars with the actual types.
        let final_upvar_tys = self.final_upvar_tys(closure_hir_id);
        debug!(
            "analyze_closure: id={:?} substs={:?} final_upvar_tys={:?}",
            closure_hir_id, substs, final_upvar_tys
        );
        for (upvar_ty, final_upvar_ty) in substs
            .upvar_tys(closure_def_id, self.tcx)
            .zip(final_upvar_tys)
        {
            self.demand_suptype(span, upvar_ty, final_upvar_ty);
        }

        // If we are also inferred the closure kind here,
        // process any deferred resolutions.
        let deferred_call_resolutions = self.remove_deferred_call_resolutions(closure_def_id);
        for deferred_call_resolution in deferred_call_resolutions {
            deferred_call_resolution.resolve(self);
        }
    }

    // Returns a list of `ClosureUpvar`s for each upvar.
    fn final_upvar_tys(&self, closure_id: hir::HirId) -> Vec<Ty<'tcx>> {
        // Presently an unboxed closure type cannot "escape" out of a
        // function, so we will only encounter ones that originated in the
        // local crate or were inlined into it along with some function.
        // This may change if abstract return types of some sort are
        // implemented.
        let tcx = self.tcx;
        let closure_def_index = tcx.hir().local_def_id_from_hir_id(closure_id);

        tcx.with_freevars(closure_id, |freevars| {
            freevars
                .iter()
                .map(|freevar| {
                    let var_node_id = freevar.var_id();
                    let var_hir_id = tcx.hir().node_to_hir_id(var_node_id);
                    let freevar_ty = self.node_ty(var_hir_id);
                    let upvar_id = ty::UpvarId {
                        var_path: ty::UpvarPath { hir_id: var_hir_id },
                        closure_expr_id: LocalDefId::from_def_id(closure_def_index),
                    };
                    let capture = self.tables.borrow().upvar_capture(&upvar_id);

                    debug!(
                        "var_id={:?} freevar_ty={:?} capture={:?}",
                        var_node_id, freevar_ty, capture
                    );

                    match capture {
                        ty::UpvarCapture::ByValue => freevar_ty,
                        ty::UpvarCapture::ByRef(borrow) => tcx.mk_ref(
                            borrow.region,
                            ty::TypeAndMut {
                                ty: freevar_ty,
                                mutbl: borrow.kind.to_mutbl_lossy(),
                            },
                        ),
                    }
                })
                .collect()
        })
    }
}

struct InferBorrowKind<'a, 'gcx: 'a + 'tcx, 'tcx: 'a> {
    fcx: &'a FnCtxt<'a, 'gcx, 'tcx>,

    // The def-id of the closure whose kind and upvar accesses are being inferred.
    closure_def_id: DefId,

    // The kind that we have inferred that the current closure
    // requires. Note that we *always* infer a minimal kind, even if
    // we don't always *use* that in the final result (i.e., sometimes
    // we've taken the closure kind from the expectations instead, and
    // for generators we don't even implement the closure traits
    // really).
    current_closure_kind: ty::ClosureKind,

    // If we modified `current_closure_kind`, this field contains a `Some()` with the
    // variable access that caused us to do so.
    current_origin: Option<(Span, ast::Name)>,

    // For each upvar that we access, we track the minimal kind of
    // access we need (ref, ref mut, move, etc).
    adjust_upvar_captures: ty::UpvarCaptureMap<'tcx>,

    span: Span,
    capture_clause: hir::CaptureClause,
    upvar_captures: ty::UpvarMap<'tcx>,
}

impl<'a, 'gcx, 'tcx> InferBorrowKind<'a, 'gcx, 'tcx> {
    // This ensures that a given (upvar, path) begins life with an
    // appropriate UpvarCapture (which might then be strengthened).
    fn initialize_capture_path(&mut self, cmt: &mc::cmt_<'tcx>) {
        match self.capture_path_by_cmt(cmt) {
            (Some (upvar_id), path) => {
                // If there's no UpvarCapturePathMap for this upvar_id,
                // create one
                let paths_for_upvar = self
                    .upvar_captures
                    .entry(upvar_id.clone())
                    .or_insert_with(ty::UpvarCapturePathMap::default);
                let fcx = self.fcx;
                let span = self.span;
                let capture_clause = self.capture_clause;
                // If it's the first time we're observing this path for
                // this upvar, seed it with the weakest possible state.
                paths_for_upvar.entry(path).or_insert_with(|| {
                    match capture_clause {
                        hir::CaptureByValue => ty::UpvarCapture::ByValue,
                        hir::CaptureByRef => {
                            let origin = UpvarRegion(upvar_id, span);
                            let freevar_region = fcx.next_region_var(origin);
                            let upvar_borrow = ty::UpvarBorrow {
                                kind: ty::ImmBorrow,
                                region: freevar_region,
                            };
                            ty::UpvarCapture::ByRef(upvar_borrow)
                        }
                    }
                });
            }
            _ => {}
        }
    }

    fn capture_path_by_cmt_inner(&self, mut acc: Vec<ty::CapturePathComponent>,
                                 cmt: &mc::cmt_<'tcx>)
                                 -> (Option<ty::UpvarId>, Vec<ty::CapturePathComponent>) {
        use crate::middle::mem_categorization as mc;
        use crate::middle::mem_categorization::{
            Categorization::*,
            InteriorKind::*,
            FieldIndex,
        };
        use ty::CapturePathComponent as CPC;
        match &cmt.cat {
            Interior(cmt, InteriorField(FieldIndex(_, name))) => {
                debug!("capture_path_by_cmt: push Interior");
                acc.push(CPC::Field(*name));
                self.capture_path_by_cmt_inner(acc, &cmt)
            },
            Deref(cmt, _) => {
                debug!("capture_path_by_cmt: in Deref; note {:?}", cmt.note);
                match &cmt.note {
                    mc::NoteClosureEnv(upvar_id) |
                    mc::NoteUpvarRef(upvar_id) => {
                        debug!("capture_path_by_cmt: got path for {:?}", upvar_id);
                        (Some(upvar_id.clone()), acc)
                    }
                    mc::NoteNone => {
                        debug!("capture_path_by_cmt: push Vanilla Deref");
                        acc.push(CPC::Deref);
                        self.capture_path_by_cmt_inner(acc, &cmt)
                    }
                    mc::NoteIndex => {
                        // FIXME
                        // Say we have something like `x.y[z].w`. We've
                        // already seen the `w` and are now at `y[z]`; We
                        // can't really see past the indexing, so we need
                        // to throw away both anything we've accumulated so
                        // far and the current Deref.
                        debug!("capture_path_by_cmt: NoteIndex, dropping {:?}", acc);
                        self.capture_path_by_cmt_inner(vec![], &cmt)
                    }
                }
            },
            Upvar(mc::Upvar {id: upvar_id, ..}) => {
                debug!("capture_path_by_cmt: Upvar {:?}", upvar_id);
                (Some(upvar_id.clone()), acc)
            },
            Interior(cmt, InteriorElement(..)) => {
                // FIXME: can't look past that; flush the accumulator
                debug!("capture_path_by_cmt: InteriorElement");
                self.capture_path_by_cmt_inner(vec![], &cmt)
            },
            Rvalue(_, cmt) => {
                debug!("capture_path_by_cmt: Rvalue {:#?}", cmt);
                match cmt {
                    Some (cmt) => self.capture_path_by_cmt_inner(vec![], &cmt),
                    None => {
                        debug!("no cmt for Rvalue"); // FIXME
                        (None, vec![])
                    }
                }
            }
            Downcast (cmt, variant) => {
                debug!("capture_path_by_cmt: Downcast");
                acc.push(CPC::Downcast(*variant));
                self.capture_path_by_cmt_inner(acc, &cmt)
            }
            ThreadLocal (..) | StaticItem | Local(..) => {
                // FIXME: this cannot possibly resolve to an Upvar?
                // But we do see them in practice
                debug!("capture_path_by_cmt: other FIXME {:#?}", cmt.cat);
                (None, vec![])
            }
        }
    }
    fn capture_path_by_cmt(&self, cmt: &mc::cmt_<'tcx>) -> (Option<ty::UpvarId>, ty::CapturePath) {
        let (upvar, path) = self.capture_path_by_cmt_inner(vec![], cmt);
        (upvar, ty::CapturePath(path.into_iter().rev().collect()))
    }

    fn adjust_upvar_borrow_kind_for_consume(
        &mut self,
        cmt: &mc::cmt_<'tcx>,
        mode: euv::ConsumeMode,
    ) {
        debug!(
            "adjust_upvar_borrow_kind_for_consume(cmt={:?}, mode={:?})",
            cmt, mode
        );

        // we only care about moves
        match mode {
            euv::Copy => {
                return;
            }
            euv::Move(_) => {}
        }

        let tcx = self.fcx.tcx;

        // watch out for a move of the deref of a borrowed pointer;
        // for that to be legal, the upvar would have to be borrowed
        // by value instead
        let guarantor = cmt.guarantor();
        debug!(
            "adjust_upvar_borrow_kind_for_consume: guarantor={:?}",
            guarantor
        );
        debug!(
            "adjust_upvar_borrow_kind_for_consume: guarantor.cat={:?}",
            guarantor.cat
        );
        if let Categorization::Deref(_, mc::BorrowedPtr(..)) = guarantor.cat {
            debug!(
                "adjust_upvar_borrow_kind_for_consume: found deref with note {:?}",
                cmt.note
            );
            match guarantor.note {
                mc::NoteUpvarRef(upvar_id) => {
                    debug!(
                        "adjust_upvar_borrow_kind_for_consume: \
                         setting upvar_id={:?} to by value",
                        upvar_id
                    );

                    // to move out of an upvar, this must be a FnOnce closure
                    self.adjust_closure_kind(
                        upvar_id.closure_expr_id,
                        ty::ClosureKind::FnOnce,
                        guarantor.span,
                        var_name(tcx, upvar_id.var_path.hir_id),
                    );

                    self.adjust_upvar_captures
                        .insert(upvar_id.clone(), ty::UpvarCapture::ByValue);
                    match self.capture_path_by_cmt(&cmt) {
                        (Some(cmt_upvar_id), path) => {
                            assert!(cmt_upvar_id == upvar_id);
                            self.upvar_captures.get_mut(&upvar_id)
                                .unwrap()
                                .insert(path, ty::UpvarCapture::ByValue);
                        }
                        (None, _) => bug!("No upvar found")
                    }
                }
                mc::NoteClosureEnv(upvar_id) => {
                    // we get just a closureenv ref if this is a
                    // `move` closure, or if the upvar has already
                    // been inferred to by-value. In any case, we
                    // must still adjust the kind of the closure
                    // to be a FnOnce closure to permit moves out
                    // of the environment.
                    self.adjust_closure_kind(
                        upvar_id.closure_expr_id,
                        ty::ClosureKind::FnOnce,
                        guarantor.span,
                        var_name(tcx, upvar_id.var_path.hir_id),
                    );
                }
                mc::NoteIndex | mc::NoteNone => {}
            }
        }
    }

    /// Indicates that `cmt` is being directly mutated (e.g., assigned
    /// to). If cmt contains any by-ref upvars, this implies that
    /// those upvars must be borrowed using an `&mut` borrow.
    fn adjust_upvar_borrow_kind_for_mut(&mut self, cmt: &mc::cmt_<'tcx>) {
        debug!("adjust_upvar_borrow_kind_for_mut(cmt={:?})", cmt);

        match cmt.cat.clone() {
            Categorization::Deref(base, mc::Unique)
            | Categorization::Interior(base, _)
            | Categorization::Downcast(base, _) => {
                // Interior or owned data is mutable if base is
                // mutable, so iterate to the base.
                self.adjust_upvar_borrow_kind_for_mut(&base);
            }

            Categorization::Deref(base, mc::BorrowedPtr(..)) => {
                if !self.try_adjust_upvar_deref(cmt, ty::MutBorrow) {
                    // assignment to deref of an `&mut`
                    // borrowed pointer implies that the
                    // pointer itself must be unique, but not
                    // necessarily *mutable*
                    self.adjust_upvar_borrow_kind_for_unique(&base);
                }
            }

            Categorization::Deref(_, mc::UnsafePtr(..))
            | Categorization::StaticItem
            | Categorization::ThreadLocal(..)
            | Categorization::Rvalue(..)
            | Categorization::Local(_)
            | Categorization::Upvar(..) => {
                return;
            }
        }
    }

    fn adjust_upvar_borrow_kind_for_unique(&mut self, cmt: &mc::cmt_<'tcx>) {
        debug!("adjust_upvar_borrow_kind_for_unique(cmt={:?})", cmt);

        match cmt.cat.clone() {
            Categorization::Deref(base, mc::Unique)
            | Categorization::Interior(base, _)
            | Categorization::Downcast(base, _) => {
                // Interior or owned data is unique if base is
                // unique.
                self.adjust_upvar_borrow_kind_for_unique(&base);
            }

            Categorization::Deref(base, mc::BorrowedPtr(..)) => {
                if !self.try_adjust_upvar_deref(cmt, ty::UniqueImmBorrow) {
                    // for a borrowed pointer to be unique, its
                    // base must be unique
                    self.adjust_upvar_borrow_kind_for_unique(&base);
                }
            }

            Categorization::Deref(_, mc::UnsafePtr(..))
            | Categorization::StaticItem
            | Categorization::ThreadLocal(..)
            | Categorization::Rvalue(..)
            | Categorization::Local(_)
            | Categorization::Upvar(..) => {}
        }
    }

    fn try_adjust_upvar_deref(
        &mut self,
        cmt: &mc::cmt_<'tcx>,
        borrow_kind: ty::BorrowKind,
    ) -> bool {
        assert!(match borrow_kind {
            ty::MutBorrow => true,
            ty::UniqueImmBorrow => true,

            // imm borrows never require adjusting any kinds, so we don't wind up here
            ty::ImmBorrow => false,
        });

        let tcx = self.fcx.tcx;

        match &cmt.note {
            mc::NoteUpvarRef(upvar_id) => {
                // if this is an implicit deref of an
                // upvar, then we need to modify the
                // borrow_kind of the upvar to make sure it
                // is inferred to mutable if necessary
                self.adjust_upvar_borrow_kind(upvar_id, borrow_kind, cmt);

                // also need to be in an FnMut closure since this is not an ImmBorrow
                self.adjust_closure_kind(
                    upvar_id.closure_expr_id,
                    ty::ClosureKind::FnMut,
                    cmt.span,
                    var_name(tcx, upvar_id.var_path.hir_id),
                );

                true
            }
            mc::NoteClosureEnv(upvar_id) => {
                // this kind of deref occurs in a `move` closure, or
                // for a by-value upvar; in either case, to mutate an
                // upvar, we need to be an FnMut closure
                self.adjust_closure_kind(
                    upvar_id.closure_expr_id,
                    ty::ClosureKind::FnMut,
                    cmt.span,
                    var_name(tcx, upvar_id.var_path.hir_id),
                );

                true
            }
            mc::NoteIndex | mc::NoteNone => false,
        }
    }

    /// We infer the borrow_kind with which to borrow upvars in a stack closure.
    /// The borrow_kind basically follows a lattice of `imm < unique-imm < mut`,
    /// moving from left to right as needed (but never right to left).
    /// Here the argument `mutbl` is the borrow_kind that is required by
    /// some particular use.
    fn adjust_upvar_borrow_kind(&mut self, upvar_id: &ty::UpvarId, kind: ty::BorrowKind,
                                cmt: &mc::cmt_<'tcx>) {
        let upvar_capture = self
            .adjust_upvar_captures
            .get(upvar_id)
            .cloned()
            .unwrap_or_else(|| self.fcx.tables.borrow().upvar_capture(upvar_id));
        debug!(
            "adjust_upvar_borrow_kind(upvar_id={:?}, upvar_capture={:?}, kind={:?})",
            upvar_id, upvar_capture, kind
        );

        match upvar_capture {
            ty::UpvarCapture::ByValue => {
                // Upvar is already by-value, the strongest criteria.
            }
            ty::UpvarCapture::ByRef(mut upvar_borrow) => {
                match (upvar_borrow.kind, kind) {
                    // Take RHS:
                    (ty::ImmBorrow, ty::UniqueImmBorrow)
                    | (ty::ImmBorrow, ty::MutBorrow)
                    | (ty::UniqueImmBorrow, ty::MutBorrow) => {
                        upvar_borrow.kind = kind;
                        self.adjust_upvar_captures
                            .insert(upvar_id.clone(), ty::UpvarCapture::ByRef(upvar_borrow));
                    }
                    // Take LHS:
                    (ty::ImmBorrow, ty::ImmBorrow)
                    | (ty::UniqueImmBorrow, ty::ImmBorrow)
                    | (ty::UniqueImmBorrow, ty::UniqueImmBorrow)
                    | (ty::MutBorrow, _) => {}
                }
            }
        }
        let capture = match self.capture_path_by_cmt(&cmt) {
            (Some(cmt_upvar_id), path) => {
                assert!(cmt_upvar_id == *upvar_id);
                let paths_for_upvar = self.
                    upvar_captures.get_mut(&upvar_id).unwrap();
                let fcx = self.fcx;
                let span = self.span;
                let capture_clause = self.capture_clause;
                paths_for_upvar.entry(path).or_insert_with(|| {
                    match capture_clause {
                        hir::CaptureByValue => ty::UpvarCapture::ByValue,
                        hir::CaptureByRef => {
                            let origin = UpvarRegion(upvar_id.clone(), span);
                            let freevar_region = fcx.next_region_var(origin);
                            let upvar_borrow = ty::UpvarBorrow {
                                kind: ty::ImmBorrow,
                                region: freevar_region,
                            };
                            ty::UpvarCapture::ByRef(upvar_borrow)
                        }
                    }
                })
            }
            (None, _) => {
                bug!("No Upvar")
            }
        };
        match capture {
            ty::UpvarCapture::ByValue => {
                // Upvar is already by-value, the strongest criteria.
            }
            ty::UpvarCapture::ByRef(mut upvar_borrow) => {
                match (upvar_borrow.kind, kind) {
                    // Take RHS:
                    (ty::ImmBorrow, ty::UniqueImmBorrow)
                    | (ty::ImmBorrow, ty::MutBorrow)
                    | (ty::UniqueImmBorrow, ty::MutBorrow) => {
                        upvar_borrow.kind = kind;
                        *capture = ty::UpvarCapture::ByRef(upvar_borrow);
                    }
                    // Take LHS:
                    (ty::ImmBorrow, ty::ImmBorrow)
                    | (ty::UniqueImmBorrow, ty::ImmBorrow)
                    | (ty::UniqueImmBorrow, ty::UniqueImmBorrow)
                    | (ty::MutBorrow, _) => {}
                }
            }
        }

    }

    fn adjust_closure_kind(
        &mut self,
        closure_id: LocalDefId,
        new_kind: ty::ClosureKind,
        upvar_span: Span,
        var_name: ast::Name,
    ) {
        debug!(
            "adjust_closure_kind(closure_id={:?}, new_kind={:?}, upvar_span={:?}, var_name={})",
            closure_id, new_kind, upvar_span, var_name
        );

        // Is this the closure whose kind is currently being inferred?
        if closure_id.to_def_id() != self.closure_def_id {
            debug!("adjust_closure_kind: not current closure");
            return;
        }

        // closures start out as `Fn`.
        let existing_kind = self.current_closure_kind;

        debug!(
            "adjust_closure_kind: closure_id={:?}, existing_kind={:?}, new_kind={:?}",
            closure_id, existing_kind, new_kind
        );

        match (existing_kind, new_kind) {
            (ty::ClosureKind::Fn, ty::ClosureKind::Fn)
            | (ty::ClosureKind::FnMut, ty::ClosureKind::Fn)
            | (ty::ClosureKind::FnMut, ty::ClosureKind::FnMut)
            | (ty::ClosureKind::FnOnce, _) => {
                // no change needed
            }

            (ty::ClosureKind::Fn, ty::ClosureKind::FnMut)
            | (ty::ClosureKind::Fn, ty::ClosureKind::FnOnce)
            | (ty::ClosureKind::FnMut, ty::ClosureKind::FnOnce) => {
                // new kind is stronger than the old kind
                self.current_closure_kind = new_kind;
                self.current_origin = Some((upvar_span, var_name));
            }
        }
    }
}

impl<'a, 'gcx, 'tcx> euv::Delegate<'tcx> for InferBorrowKind<'a, 'gcx, 'tcx> {
    fn consume(
        &mut self,
        _consume_id: hir::HirId,
        _consume_span: Span,
        cmt: &mc::cmt_<'tcx>,
        mode: euv::ConsumeMode,
    ) {
        debug!("consume(cmt={:?},mode={:?})", cmt, mode);
        self.initialize_capture_path(cmt);
        self.adjust_upvar_borrow_kind_for_consume(cmt, mode);
    }

    fn matched_pat(
        &mut self,
        _matched_pat: &hir::Pat,
        _cmt: &mc::cmt_<'tcx>,
        _mode: euv::MatchMode,
    ) {
    }

    fn consume_pat(
        &mut self,
        _consume_pat: &hir::Pat,
        cmt: &mc::cmt_<'tcx>,
        mode: euv::ConsumeMode,
    ) {
        debug!("consume_pat(cmt={:?},mode={:?})", cmt, mode);
        self.initialize_capture_path(cmt);
        self.adjust_upvar_borrow_kind_for_consume(cmt, mode);
    }

    fn borrow(
        &mut self,
        borrow_id: hir::HirId,
        _borrow_span: Span,
        cmt: &mc::cmt_<'tcx>,
        _loan_region: ty::Region<'tcx>,
        bk: ty::BorrowKind,
        _loan_cause: euv::LoanCause,
    ) {
        debug!(
            "borrow(borrow_id={}, cmt={:?}, bk={:?})",
            borrow_id, cmt, bk
        );

        self.initialize_capture_path(cmt); // FIXME: completely untested
        match bk {
            ty::ImmBorrow => {}
            ty::UniqueImmBorrow => {
                self.adjust_upvar_borrow_kind_for_unique(cmt);
            }
            ty::MutBorrow => {
                self.adjust_upvar_borrow_kind_for_mut(cmt);
            }
        }
    }

    fn decl_without_init(&mut self, _id: hir::HirId, _span: Span) {}

    fn mutate(
        &mut self,
        _assignment_id: hir::HirId,
        _assignment_span: Span,
        assignee_cmt: &mc::cmt_<'tcx>,
        _mode: euv::MutateMode,
    ) {
        debug!("mutate(assignee_cmt={:?})", assignee_cmt);

        self.initialize_capture_path(assignee_cmt); // FIXME: completely untested
        self.adjust_upvar_borrow_kind_for_mut(assignee_cmt);
    }
}

fn var_name(tcx: TyCtxt<'_, '_, '_>, var_hir_id: hir::HirId) -> ast::Name {
    tcx.hir().name_by_hir_id(var_hir_id)
}
