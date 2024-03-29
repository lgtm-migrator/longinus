/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Servo's compiler plugin/macro crate
//!
//! Attributes this crate provides:
//!
//!  - `#[derive(DenyPublicFields)]` : Forces all fields in a struct/enum to be private
//!  - `#[derive(JSTraceable)]` : Auto-derives an implementation of `JSTraceable` for a struct in the script crate
//!  - `#[unrooted_must_root_lint::must_root]` : Prevents data of the marked type from being used on the stack.
//!                     See the lints module for more details
//!  - `#[dom_struct]` : Implies #[derive(JSTraceable, DenyPublicFields)]`, and `#[unrooted_must_root_lint::must_root]`.
//!                       Use this for structs that correspond to a DOM type

#![deny(unsafe_code)]
#![feature(plugin)]
#![feature(plugin_registrar)]
#![feature(rustc_private)]
#![cfg(feature = "unrooted_must_root_lint")]

#[macro_use]
extern crate matches;
#[macro_use]
extern crate rustc;
extern crate rustc_driver;
extern crate syntax;

use rustc::hir::def_id::DefId;
use rustc::hir::intravisit as visit;
use rustc::hir::{self, ExprKind, HirId};
use rustc::lint::{LateContext, LateLintPass, LintContext, LintPass};
use rustc::ty;
use rustc_driver::plugin::Registry;
use syntax::ast::{AttrKind, Attribute};
use syntax::source_map;
use syntax::source_map::{ExpnKind, MacroKind, Span};
use syntax::symbol::sym;
use syntax::symbol::Symbol;

#[allow(deprecated)]
#[plugin_registrar]
pub fn plugin_registrar(reg: &mut Registry) {
    registrar(reg)
}

fn registrar(reg: &mut Registry) {
    let symbols = Symbols::new();
    reg.lint_store.register_lints(&[&UNROOTED_MUST_ROOT]);
    reg.lint_store
        .register_late_pass(move || Box::new(UnrootedPass::new(symbols.clone())));
}

declare_lint!(
    UNROOTED_MUST_ROOT,
    Deny,
    "Warn and report usage of unrooted jsmanaged objects"
);

/// Lint for ensuring safe usage of unrooted pointers
///
/// This lint (disable with `-A unrooted-must-root`/`#[allow(unrooted_must_root)]`) ensures that
/// `#[unrooted_must_root_lint::must_root]` values are used correctly.
///
/// "Incorrect" usage includes:
///
///  - Not being used in a struct/enum field which is not `#[unrooted_must_root_lint::must_root]` itself
///  - Not being used as an argument to a function (Except onces named `new` and `new_inherited`)
///  - Not being bound locally in a `let` statement, assignment, `for` loop, or `match` statement.
///
/// This helps catch most situations where pointers like `JS<T>` are used in a way that they can be invalidated by a
/// GC pass.
///
/// Structs which have their own mechanism of rooting their unrooted contents (e.g. `ScriptThread`)
/// can be marked as `#[allow(unrooted_must_root)]`. Smart pointers which root their interior type
/// can be marked as `#[unrooted_must_root_lint::allow_unrooted_interior]`
pub(crate) struct UnrootedPass {
    symbols: Symbols,
}

impl UnrootedPass {
    pub fn new(symbols: Symbols) -> UnrootedPass {
        UnrootedPass { symbols }
    }
}

fn has_lint_attr(sym: &Symbols, attrs: &[Attribute], name: Symbol) -> bool {
    attrs.iter().any(|attr| {
        matches!(
            &attr.kind,
            AttrKind::Normal(attr_item)
            if attr_item.path.segments.len() == 2 &&
            attr_item.path.segments[0].ident.name == sym.unrooted_must_root_lint &&
            attr_item.path.segments[1].ident.name == name
        )
    })
}

/// Checks if a type is unrooted or contains any owned unrooted types
fn is_unrooted_ty(sym: &Symbols, cx: &LateContext, ty: &ty::TyS, in_new_function: bool) -> bool {
    let mut ret = false;
    ty.maybe_walk(|t| {
        match t.kind {
            ty::Adt(did, substs) => {
                let has_attr = |did, name| has_lint_attr(sym, &cx.tcx.get_attrs(did), name);
                if has_attr(did.did, sym.must_root) {
                    ret = true;
                    false
                } else if has_attr(did.did, sym.allow_unrooted_interior) {
                    false
                } else if match_def_path(cx, did.did, &[sym.alloc, sym.rc, sym.Rc]) {
                    // Rc<Promise> is okay
                    let inner = substs.type_at(0);
                    if let ty::Adt(did, _) = inner.kind {
                        if has_attr(did.did, sym.allow_unrooted_in_rc) {
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                } else if match_def_path(cx, did.did, &[sym::core, sym.cell, sym.Ref]) ||
                    match_def_path(cx, did.did, &[sym::core, sym.cell, sym.RefMut]) ||
                    match_def_path(cx, did.did, &[sym::core, sym.slice, sym.Iter]) ||
                    match_def_path(cx, did.did, &[sym::core, sym.slice, sym.IterMut]) ||
                    match_def_path(
                        cx,
                        did.did,
                        &[sym::std, sym.collections, sym.hash, sym.map, sym.Entry],
                    ) ||
                    match_def_path(
                        cx,
                        did.did,
                        &[
                            sym::std,
                            sym.collections,
                            sym.hash,
                            sym.map,
                            sym.OccupiedEntry,
                        ],
                    ) ||
                    match_def_path(
                        cx,
                        did.did,
                        &[
                            sym::std,
                            sym.collections,
                            sym.hash,
                            sym.map,
                            sym.VacantEntry,
                        ],
                    ) ||
                    match_def_path(
                        cx,
                        did.did,
                        &[sym::std, sym.collections, sym.hash, sym.map, sym.Iter],
                    ) ||
                    match_def_path(
                        cx,
                        did.did,
                        &[sym::std, sym.collections, sym.hash, sym.set, sym.Iter],
                    )
                {
                    // Structures which are semantically similar to an &ptr.
                    false
                } else if did.is_box() && in_new_function {
                    // box in new() is okay
                    false
                } else {
                    true
                }
            },
            ty::Ref(..) => false,    // don't recurse down &ptrs
            ty::RawPtr(..) => false, // don't recurse down *ptrs
            ty::FnDef(..) | ty::FnPtr(_) => false,
            _ => true,
        }
    });
    ret
}

impl LintPass for UnrootedPass {
    fn name(&self) -> &'static str {
        "ServoUnrootedPass"
    }
}

impl<'a, 'tcx> LateLintPass<'a, 'tcx> for UnrootedPass {
    /// All structs containing #[unrooted_must_root_lint::must_root] types
    /// must be #[unrooted_must_root_lint::must_root] themselves
    fn check_item(&mut self, cx: &LateContext<'a, 'tcx>, item: &'tcx hir::Item) {
        if has_lint_attr(&self.symbols, &item.attrs, self.symbols.must_root) {
            return;
        }
        if let hir::ItemKind::Struct(def, ..) = &item.kind {
            for ref field in def.fields() {
                let def_id = cx.tcx.hir().local_def_id(field.hir_id);
                if is_unrooted_ty(&self.symbols, cx, cx.tcx.type_of(def_id), false) {
                    cx.span_lint(
                        UNROOTED_MUST_ROOT,
                        field.span,
                        "Type must be rooted, use #[unrooted_must_root_lint::must_root] \
                         on the struct definition to propagate",
                    )
                }
            }
        }
    }

    /// All enums containing #[unrooted_must_root_lint::must_root] types
    /// must be #[unrooted_must_root_lint::must_root] themselves
    fn check_variant(&mut self, cx: &LateContext, var: &hir::Variant) {
        let ref map = cx.tcx.hir();
        let parent_item = map.expect_item(map.get_parent_item(var.id));
        if !has_lint_attr(&self.symbols, &parent_item.attrs, self.symbols.must_root) {
            match var.data {
                hir::VariantData::Tuple(ref fields, ..) => {
                    for ref field in fields {
                        let def_id = cx.tcx.hir().local_def_id(field.hir_id);
                        if is_unrooted_ty(&self.symbols, cx, cx.tcx.type_of(def_id), false) {
                            cx.span_lint(
                                UNROOTED_MUST_ROOT,
                                field.ty.span,
                                "Type must be rooted, use #[unrooted_must_root_lint::must_root] on \
                                 the enum definition to propagate",
                            )
                        }
                    }
                },
                _ => (), // Struct variants already caught by check_struct_def
            }
        }
    }
    /// Function arguments that are #[unrooted_must_root_lint::must_root] types are not allowed
    fn check_fn(
        &mut self,
        cx: &LateContext<'a, 'tcx>,
        kind: visit::FnKind<'tcx>,
        decl: &'tcx hir::FnDecl,
        body: &'tcx hir::Body,
        span: source_map::Span,
        id: HirId,
    ) {
        let in_new_function = match kind {
            visit::FnKind::ItemFn(n, _, _, _, _) | visit::FnKind::Method(n, _, _, _) => {
                &*n.as_str() == "new" || n.as_str().starts_with("new_")
            },
            visit::FnKind::Closure(_) => return,
        };

        if !in_derive_expn(span) {
            let def_id = cx.tcx.hir().local_def_id(id);
            let sig = cx.tcx.type_of(def_id).fn_sig(cx.tcx);

            for (arg, ty) in decl.inputs.iter().zip(sig.inputs().skip_binder().iter()) {
                if is_unrooted_ty(&self.symbols, cx, ty, false) {
                    cx.span_lint(UNROOTED_MUST_ROOT, arg.span, "Type must be rooted")
                }
            }

            if !in_new_function {
                if is_unrooted_ty(&self.symbols, cx, sig.output().skip_binder(), false) {
                    cx.span_lint(
                        UNROOTED_MUST_ROOT,
                        decl.output.span(),
                        "Type must be rooted",
                    )
                }
            }
        }

        let mut visitor = FnDefVisitor {
            symbols: &self.symbols,
            cx: cx,
            in_new_function: in_new_function,
        };
        visit::walk_expr(&mut visitor, &body.value);
    }
}

struct FnDefVisitor<'a, 'b: 'a, 'tcx: 'a + 'b> {
    symbols: &'a Symbols,
    cx: &'a LateContext<'b, 'tcx>,
    in_new_function: bool,
}

impl<'a, 'b, 'tcx> visit::Visitor<'tcx> for FnDefVisitor<'a, 'b, 'tcx> {
    fn visit_expr(&mut self, expr: &'tcx hir::Expr) {
        let cx = self.cx;

        let require_rooted = |cx: &LateContext, in_new_function: bool, subexpr: &hir::Expr| {
            let ty = cx.tables.expr_ty(&subexpr);
            if is_unrooted_ty(&self.symbols, cx, ty, in_new_function) {
                cx.span_lint(
                    UNROOTED_MUST_ROOT,
                    subexpr.span,
                    &format!("Expression of type {:?} must be rooted", ty),
                )
            }
        };

        match expr.kind {
            // Trait casts from #[unrooted_must_root_lint::must_root] types are not allowed
            ExprKind::Cast(ref subexpr, _) => require_rooted(cx, self.in_new_function, &*subexpr),
            // This catches assignments... the main point of this would be to catch mutable
            // references to `JS<T>`.
            // FIXME: Enable this? Triggers on certain kinds of uses of DomRefCell.
            // hir::ExprAssign(_, ref rhs) => require_rooted(cx, self.in_new_function, &*rhs),
            // This catches calls; basically, this enforces the constraint that only constructors
            // can call other constructors.
            // FIXME: Enable this? Currently triggers with constructs involving DomRefCell, and
            // constructs like Vec<JS<T>> and RootedVec<JS<T>>.
            // hir::ExprCall(..) if !self.in_new_function => {
            //     require_rooted(cx, self.in_new_function, expr);
            // }
            _ => {
                // TODO(pcwalton): Check generics with a whitelist of allowed generics.
            },
        }

        visit::walk_expr(self, expr);
    }

    fn visit_pat(&mut self, pat: &'tcx hir::Pat) {
        let cx = self.cx;

        // We want to detect pattern bindings that move a value onto the stack.
        // When "default binding modes" https://github.com/rust-lang/rust/issues/42640
        // are implemented, the `Unannotated` case could cause false-positives.
        // These should be fixable by adding an explicit `ref`.
        match pat.kind {
            hir::PatKind::Binding(hir::BindingAnnotation::Unannotated, ..) |
            hir::PatKind::Binding(hir::BindingAnnotation::Mutable, ..) => {
                let ty = cx.tables.pat_ty(pat);
                if is_unrooted_ty(&self.symbols, cx, ty, self.in_new_function) {
                    cx.span_lint(
                        UNROOTED_MUST_ROOT,
                        pat.span,
                        &format!("Expression of type {:?} must be rooted", ty),
                    )
                }
            },
            _ => {},
        }

        visit::walk_pat(self, pat);
    }

    fn visit_ty(&mut self, _: &'tcx hir::Ty) {}

    fn nested_visit_map<'this>(&'this mut self) -> hir::intravisit::NestedVisitorMap<'this, 'tcx> {
        hir::intravisit::NestedVisitorMap::OnlyBodies(&self.cx.tcx.hir())
    }
}

/// check if a DefId's path matches the given absolute type path
/// usage e.g. with
/// `match_def_path(cx, id, &["core", "option", "Option"])`
fn match_def_path(cx: &LateContext, def_id: DefId, path: &[Symbol]) -> bool {
    let krate = &cx.tcx.crate_name(def_id.krate);
    if krate != &path[0] {
        return false;
    }

    let path = &path[1..];
    let other = cx.tcx.def_path(def_id).data;

    if other.len() != path.len() {
        return false;
    }

    other
        .into_iter()
        .zip(path)
        .all(|(e, p)| e.data.as_symbol() == *p)
}

fn in_derive_expn(span: Span) -> bool {
    if let ExpnKind::Macro(MacroKind::Attr, n) = span.ctxt().outer_expn_data().kind {
        n.as_str().contains("derive")
    } else {
        false
    }
}

macro_rules! symbols {
    ($($s: ident)+) => {
        #[derive(Clone)]
        #[allow(non_snake_case)]
        struct Symbols {
            $( $s: Symbol, )+
        }

        impl Symbols {
            fn new() -> Self {
                Symbols {
                    $( $s: Symbol::intern(stringify!($s)), )+
                }
            }
        }
    }
}

symbols! {
    unrooted_must_root_lint
    allow_unrooted_interior
    allow_unrooted_in_rc
    must_root
    alloc
    rc
    Rc
    cell
    Ref
    RefMut
    slice
    Iter
    IterMut
    collections
    hash
    map
    set
    Entry
    OccupiedEntry
    VacantEntry
}
