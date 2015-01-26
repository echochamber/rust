// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Type resolution: the phase that finds all the types in the AST with
// unresolved type variables and replaces "ty_var" types with their
// substitutions.
use self::ResolveReason::*;

use astconv::AstConv;
use check::FnCtxt;
use middle::pat_util;
use middle::ty::{self, Ty, MethodCall, MethodCallee};
use middle::ty_fold::{TypeFolder,TypeFoldable};
use middle::infer;
use write_substs_to_tcx;
use write_ty_to_tcx;
use util::ppaux::Repr;

use std::cell::Cell;

use syntax::ast;
use syntax::codemap::{DUMMY_SP, Span};
use syntax::print::pprust::pat_to_string;
use syntax::visit;
use syntax::visit::Visitor;

///////////////////////////////////////////////////////////////////////////
// Entry point functions

pub fn resolve_type_vars_in_expr(fcx: &FnCtxt, e: &ast::Expr) {
    assert_eq!(fcx.writeback_errors.get(), false);
    let mut wbcx = WritebackCx::new(fcx);
    wbcx.visit_expr(e);
    wbcx.visit_upvar_borrow_map();
    wbcx.visit_closures();
    wbcx.visit_object_cast_map();
}

pub fn resolve_type_vars_in_fn(fcx: &FnCtxt,
                               decl: &ast::FnDecl,
                               blk: &ast::Block) {
    assert_eq!(fcx.writeback_errors.get(), false);
    let mut wbcx = WritebackCx::new(fcx);
    wbcx.visit_block(blk);
    for arg in decl.inputs.iter() {
        wbcx.visit_node_id(ResolvingPattern(arg.pat.span), arg.id);
        wbcx.visit_pat(&*arg.pat);

        // Privacy needs the type for the whole pattern, not just each binding
        if !pat_util::pat_is_binding(&fcx.tcx().def_map, &*arg.pat) {
            wbcx.visit_node_id(ResolvingPattern(arg.pat.span),
                               arg.pat.id);
        }
    }
    wbcx.visit_upvar_borrow_map();
    wbcx.visit_closures();
    wbcx.visit_object_cast_map();
}

///////////////////////////////////////////////////////////////////////////
// The Writerback context. This visitor walks the AST, checking the
// fn-specific tables to find references to types or regions. It
// resolves those regions to remove inference variables and writes the
// final result back into the master tables in the tcx. Here and
// there, it applies a few ad-hoc checks that were not convenient to
// do elsewhere.

struct WritebackCx<'cx, 'tcx: 'cx> {
    fcx: &'cx FnCtxt<'cx, 'tcx>,
}

impl<'cx, 'tcx> WritebackCx<'cx, 'tcx> {
    fn new(fcx: &'cx FnCtxt<'cx, 'tcx>) -> WritebackCx<'cx, 'tcx> {
        WritebackCx { fcx: fcx }
    }

    fn tcx(&self) -> &'cx ty::ctxt<'tcx> {
        self.fcx.tcx()
    }
}

///////////////////////////////////////////////////////////////////////////
// Impl of Visitor for Resolver
//
// This is the master code which walks the AST. It delegates most of
// the heavy lifting to the generic visit and resolve functions
// below. In general, a function is made into a `visitor` if it must
// traffic in node-ids or update tables in the type context etc.

impl<'cx, 'tcx, 'v> Visitor<'v> for WritebackCx<'cx, 'tcx> {
    fn visit_item(&mut self, _: &ast::Item) {
        // Ignore items
    }

    fn visit_stmt(&mut self, s: &ast::Stmt) {
        if self.fcx.writeback_errors.get() {
            return;
        }

        self.visit_node_id(ResolvingExpr(s.span), ty::stmt_node_id(s));
        visit::walk_stmt(self, s);
    }

    fn visit_expr(&mut self, e: &ast::Expr) {
        if self.fcx.writeback_errors.get() {
            return;
        }

        self.visit_node_id(ResolvingExpr(e.span), e.id);
        self.visit_method_map_entry(ResolvingExpr(e.span),
                                    MethodCall::expr(e.id));

        match e.node {
            ast::ExprClosure(_, _, ref decl, _) => {
                for input in decl.inputs.iter() {
                    let _ = self.visit_node_id(ResolvingExpr(e.span),
                                               input.id);
                }
            }
            _ => {}
        }

        visit::walk_expr(self, e);
    }

    fn visit_block(&mut self, b: &ast::Block) {
        if self.fcx.writeback_errors.get() {
            return;
        }

        self.visit_node_id(ResolvingExpr(b.span), b.id);
        visit::walk_block(self, b);
    }

    fn visit_pat(&mut self, p: &ast::Pat) {
        if self.fcx.writeback_errors.get() {
            return;
        }

        self.visit_node_id(ResolvingPattern(p.span), p.id);

        debug!("Type for pattern binding {} (id {}) resolved to {}",
               pat_to_string(p),
               p.id,
               ty::node_id_to_type(self.tcx(), p.id).repr(self.tcx()));

        visit::walk_pat(self, p);
    }

    fn visit_local(&mut self, l: &ast::Local) {
        if self.fcx.writeback_errors.get() {
            return;
        }

        let var_ty = self.fcx.local_ty(l.span, l.id);
        let var_ty = self.resolve(&var_ty, ResolvingLocal(l.span));
        write_ty_to_tcx(self.tcx(), l.id, var_ty);
        visit::walk_local(self, l);
    }

    fn visit_ty(&mut self, t: &ast::Ty) {
        match t.node {
            ast::TyFixedLengthVec(ref ty, ref count_expr) => {
                self.visit_ty(&**ty);
                write_ty_to_tcx(self.tcx(), count_expr.id, self.tcx().types.uint);
            }
            _ => visit::walk_ty(self, t)
        }
    }
}

impl<'cx, 'tcx> WritebackCx<'cx, 'tcx> {
    fn visit_upvar_borrow_map(&self) {
        if self.fcx.writeback_errors.get() {
            return;
        }

        for (upvar_id, upvar_borrow) in self.fcx.inh.upvar_borrow_map.borrow().iter() {
            let r = upvar_borrow.region;
            let r = self.resolve(&r, ResolvingUpvar(*upvar_id));
            let new_upvar_borrow = ty::UpvarBorrow { kind: upvar_borrow.kind,
                                                     region: r };
            debug!("Upvar borrow for {} resolved to {}",
                   upvar_id.repr(self.tcx()),
                   new_upvar_borrow.repr(self.tcx()));
            self.fcx.tcx().upvar_borrow_map.borrow_mut().insert(
                *upvar_id, new_upvar_borrow);
        }
    }

    fn visit_closures(&self) {
        if self.fcx.writeback_errors.get() {
            return
        }

        for (def_id, closure) in self.fcx.inh.closures.borrow().iter() {
            let closure_ty = self.resolve(&closure.closure_type,
                                          ResolvingClosure(*def_id));
            let closure = ty::Closure {
                closure_type: closure_ty,
                kind: closure.kind,
            };
            self.fcx.tcx().closures.borrow_mut().insert(*def_id, closure);
        }
    }

    fn visit_object_cast_map(&self) {
        if self.fcx.writeback_errors.get() {
            return
        }

        for (&node_id, trait_ref) in self.fcx
                                            .inh
                                            .object_cast_map
                                            .borrow()
                                            .iter()
        {
            let span = ty::expr_span(self.tcx(), node_id);
            let reason = ResolvingExpr(span);
            let closure_ty = self.resolve(trait_ref, reason);
            self.tcx()
                .object_cast_map
                .borrow_mut()
                .insert(node_id, closure_ty);
        }
    }

    fn visit_node_id(&self, reason: ResolveReason, id: ast::NodeId) {
        // Resolve any borrowings for the node with id `id`
        self.visit_adjustments(reason, id);

        // Resolve the type of the node with id `id`
        let n_ty = self.fcx.node_ty(id);
        let n_ty = self.resolve(&n_ty, reason);
        write_ty_to_tcx(self.tcx(), id, n_ty);
        debug!("Node {} has type {}", id, n_ty.repr(self.tcx()));

        // Resolve any substitutions
        self.fcx.opt_node_ty_substs(id, |item_substs| {
            write_substs_to_tcx(self.tcx(), id,
                                self.resolve(item_substs, reason));
        });
    }

    fn visit_adjustments(&self, reason: ResolveReason, id: ast::NodeId) {
        match self.fcx.inh.adjustments.borrow_mut().remove(&id) {
            None => {
                debug!("No adjustments for node {}", id);
            }

            Some(adjustment) => {
                let adj_object = ty::adjust_is_object(&adjustment);
                let resolved_adjustment = match adjustment {
                    ty::AdjustReifyFnPointer(def_id) => {
                        ty::AdjustReifyFnPointer(def_id)
                    }

                    ty::AdjustDerefRef(adj) => {
                        for autoderef in 0..adj.autoderefs {
                            let method_call = MethodCall::autoderef(id, autoderef);
                            self.visit_method_map_entry(reason, method_call);
                        }

                        if adj_object {
                            let method_call = MethodCall::autoobject(id);
                            self.visit_method_map_entry(reason, method_call);
                        }

                        ty::AdjustDerefRef(ty::AutoDerefRef {
                            autoderefs: adj.autoderefs,
                            autoref: self.resolve(&adj.autoref, reason),
                        })
                    }
                };
                debug!("Adjustments for node {}: {:?}", id, resolved_adjustment);
                self.tcx().adjustments.borrow_mut().insert(
                    id, resolved_adjustment);
            }
        }
    }

    fn visit_method_map_entry(&self,
                              reason: ResolveReason,
                              method_call: MethodCall) {
        // Resolve any method map entry
        match self.fcx.inh.method_map.borrow_mut().remove(&method_call) {
            Some(method) => {
                debug!("writeback::resolve_method_map_entry(call={:?}, entry={})",
                       method_call,
                       method.repr(self.tcx()));
                let new_method = MethodCallee {
                    origin: self.resolve(&method.origin, reason),
                    ty: self.resolve(&method.ty, reason),
                    substs: self.resolve(&method.substs, reason),
                };

                self.tcx().method_map.borrow_mut().insert(
                    method_call,
                    new_method);
            }
            None => {}
        }
    }

    fn resolve<T:TypeFoldable<'tcx>>(&self, t: &T, reason: ResolveReason) -> T {
        t.fold_with(&mut Resolver::new(self.fcx, reason))
    }
}

///////////////////////////////////////////////////////////////////////////
// Resolution reason.

#[derive(Copy)]
enum ResolveReason {
    ResolvingExpr(Span),
    ResolvingLocal(Span),
    ResolvingPattern(Span),
    ResolvingUpvar(ty::UpvarId),
    ResolvingClosure(ast::DefId),
}

impl ResolveReason {
    fn span(&self, tcx: &ty::ctxt) -> Span {
        match *self {
            ResolvingExpr(s) => s,
            ResolvingLocal(s) => s,
            ResolvingPattern(s) => s,
            ResolvingUpvar(upvar_id) => {
                ty::expr_span(tcx, upvar_id.closure_expr_id)
            }
            ResolvingClosure(did) => {
                if did.krate == ast::LOCAL_CRATE {
                    ty::expr_span(tcx, did.node)
                } else {
                    DUMMY_SP
                }
            }
        }
    }
}

///////////////////////////////////////////////////////////////////////////
// The Resolver. This is the type folding engine that detects
// unresolved types and so forth.

struct Resolver<'cx, 'tcx: 'cx> {
    tcx: &'cx ty::ctxt<'tcx>,
    infcx: &'cx infer::InferCtxt<'cx, 'tcx>,
    writeback_errors: &'cx Cell<bool>,
    reason: ResolveReason,
}

impl<'cx, 'tcx> Resolver<'cx, 'tcx> {
    fn new(fcx: &'cx FnCtxt<'cx, 'tcx>,
           reason: ResolveReason)
           -> Resolver<'cx, 'tcx>
    {
        Resolver::from_infcx(fcx.infcx(), &fcx.writeback_errors, reason)
    }

    fn from_infcx(infcx: &'cx infer::InferCtxt<'cx, 'tcx>,
                  writeback_errors: &'cx Cell<bool>,
                  reason: ResolveReason)
                  -> Resolver<'cx, 'tcx>
    {
        Resolver { infcx: infcx,
                   tcx: infcx.tcx,
                   writeback_errors: writeback_errors,
                   reason: reason }
    }

    fn report_error(&self, e: infer::fixup_err) {
        self.writeback_errors.set(true);
        if !self.tcx.sess.has_errors() {
            match self.reason {
                ResolvingExpr(span) => {
                    span_err!(self.tcx.sess, span, E0101,
                        "cannot determine a type for this expression: {}",
                        infer::fixup_err_to_string(e));
                }

                ResolvingLocal(span) => {
                    span_err!(self.tcx.sess, span, E0102,
                        "cannot determine a type for this local variable: {}",
                        infer::fixup_err_to_string(e));
                }

                ResolvingPattern(span) => {
                    span_err!(self.tcx.sess, span, E0103,
                        "cannot determine a type for this pattern binding: {}",
                        infer::fixup_err_to_string(e));
                }

                ResolvingUpvar(upvar_id) => {
                    let span = self.reason.span(self.tcx);
                    span_err!(self.tcx.sess, span, E0104,
                        "cannot resolve lifetime for captured variable `{}`: {}",
                        ty::local_var_name_str(self.tcx, upvar_id.var_id).get().to_string(),
                        infer::fixup_err_to_string(e));
                }

                ResolvingClosure(_) => {
                    let span = self.reason.span(self.tcx);
                    span_err!(self.tcx.sess, span, E0196,
                              "cannot determine a type for this closure")
                }
            }
        }
    }
}

impl<'cx, 'tcx> TypeFolder<'tcx> for Resolver<'cx, 'tcx> {
    fn tcx<'a>(&'a self) -> &'a ty::ctxt<'tcx> {
        self.tcx
    }

    fn fold_ty(&mut self, t: Ty<'tcx>) -> Ty<'tcx> {
        match self.infcx.fully_resolve(&t) {
            Ok(t) => t,
            Err(e) => {
                debug!("Resolver::fold_ty: input type `{}` not fully resolvable",
                       t.repr(self.tcx));
                self.report_error(e);
                self.tcx().types.err
            }
        }
    }

    fn fold_region(&mut self, r: ty::Region) -> ty::Region {
        match self.infcx.fully_resolve(&r) {
            Ok(r) => r,
            Err(e) => {
                self.report_error(e);
                ty::ReStatic
            }
        }
    }
}

///////////////////////////////////////////////////////////////////////////
// During type check, we store promises with the result of trait
// lookup rather than the actual results (because the results are not
// necessarily available immediately). These routines unwind the
// promises. It is expected that we will have already reported any
// errors that may be encountered, so if the promises store an error,
// a dummy result is returned.
