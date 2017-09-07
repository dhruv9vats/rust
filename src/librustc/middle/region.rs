// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This file builds up the `ScopeTree`, which describes
//! the parent links in the region hierarchy.
//!
//! Most of the documentation on regions can be found in
//! `middle/infer/region_inference/README.md`

use ich::{StableHashingContext, NodeIdHashingMode};
use util::nodemap::{FxHashMap, FxHashSet};
use ty;

use std::mem;
use std::rc::Rc;
use syntax::codemap;
use syntax::ast;
use syntax_pos::{Span, DUMMY_SP};
use ty::TyCtxt;
use ty::maps::Providers;

use hir;
use hir::def_id::DefId;
use hir::intravisit::{self, Visitor, NestedVisitorMap};
use hir::{Block, Arm, Pat, PatKind, Stmt, Expr, Local};
use mir::transform::MirSource;
use rustc_data_structures::stable_hasher::{HashStable, StableHasher,
                                           StableHasherResult};

/// Scope represents a statically-describable scope that can be
/// used to bound the lifetime/region for values.
///
/// `Node(node_id)`: Any AST node that has any scope at all has the
/// `Node(node_id)` scope. Other variants represent special cases not
/// immediately derivable from the abstract syntax tree structure.
///
/// `DestructionScope(node_id)` represents the scope of destructors
/// implicitly-attached to `node_id` that run immediately after the
/// expression for `node_id` itself. Not every AST node carries a
/// `DestructionScope`, but those that are `terminating_scopes` do;
/// see discussion with `ScopeTree`.
///
/// `Remainder(BlockRemainder { block, statement_index })` represents
/// the scope of user code running immediately after the initializer
/// expression for the indexed statement, until the end of the block.
///
/// So: the following code can be broken down into the scopes beneath:
/// ```
/// let a = f().g( 'b: { let x = d(); let y = d(); x.h(y)  }   ) ;
/// ```
///
///                                                              +-+ (D12.)
///                                                        +-+       (D11.)
///                                              +---------+         (R10.)
///                                              +-+                  (D9.)
///                                   +----------+                    (M8.)
///                                 +----------------------+          (R7.)
///                                 +-+                               (D6.)
///                      +----------+                                 (M5.)
///                    +-----------------------------------+          (M4.)
///         +--------------------------------------------------+      (M3.)
///         +--+                                                      (M2.)
/// +-----------------------------------------------------------+     (M1.)
///
///  (M1.): Node scope of the whole `let a = ...;` statement.
///  (M2.): Node scope of the `f()` expression.
///  (M3.): Node scope of the `f().g(..)` expression.
///  (M4.): Node scope of the block labeled `'b:`.
///  (M5.): Node scope of the `let x = d();` statement
///  (D6.): DestructionScope for temporaries created during M5.
///  (R7.): Remainder scope for block `'b:`, stmt 0 (let x = ...).
///  (M8.): Node scope of the `let y = d();` statement.
///  (D9.): DestructionScope for temporaries created during M8.
/// (R10.): Remainder scope for block `'b:`, stmt 1 (let y = ...).
/// (D11.): DestructionScope for temporaries and bindings from block `'b:`.
/// (D12.): DestructionScope for temporaries created during M1 (e.g. f()).
///
/// Note that while the above picture shows the destruction scopes
/// as following their corresponding node scopes, in the internal
/// data structures of the compiler the destruction scopes are
/// represented as enclosing parents. This is sound because we use the
/// enclosing parent relationship just to ensure that referenced
/// values live long enough; phrased another way, the starting point
/// of each range is not really the important thing in the above
/// picture, but rather the ending point.
///
/// FIXME (pnkfelix): This currently derives `PartialOrd` and `Ord` to
/// placate the same deriving in `ty::FreeRegion`, but we may want to
/// actually attach a more meaningful ordering to scopes than the one
/// generated via deriving here.
#[derive(Clone, PartialEq, PartialOrd, Eq, Ord, Hash, Debug, Copy, RustcEncodable, RustcDecodable)]
pub enum Scope {
    Node(hir::ItemLocalId),

    // Scope of the call-site for a function or closure
    // (outlives the arguments as well as the body).
    CallSite(hir::ItemLocalId),

    // Scope of arguments passed to a function or closure
    // (they outlive its body).
    Arguments(hir::ItemLocalId),

    // Scope of destructors for temporaries of node-id.
    Destruction(hir::ItemLocalId),

    // Scope following a `let id = expr;` binding in a block.
    Remainder(BlockRemainder)
}

/// Represents a subscope of `block` for a binding that is introduced
/// by `block.stmts[first_statement_index]`. Such subscopes represent
/// a suffix of the block. Note that each subscope does not include
/// the initializer expression, if any, for the statement indexed by
/// `first_statement_index`.
///
/// For example, given `{ let (a, b) = EXPR_1; let c = EXPR_2; ... }`:
///
/// * the subscope with `first_statement_index == 0` is scope of both
///   `a` and `b`; it does not include EXPR_1, but does include
///   everything after that first `let`. (If you want a scope that
///   includes EXPR_1 as well, then do not use `Scope::Remainder`,
///   but instead another `Scope` that encompasses the whole block,
///   e.g. `Scope::Node`.
///
/// * the subscope with `first_statement_index == 1` is scope of `c`,
///   and thus does not include EXPR_2, but covers the `...`.
#[derive(Clone, PartialEq, PartialOrd, Eq, Ord, Hash, RustcEncodable,
         RustcDecodable, Debug, Copy)]
pub struct BlockRemainder {
    pub block: hir::ItemLocalId,
    pub first_statement_index: u32,
}

impl Scope {
    /// Returns a item-local id associated with this scope.
    ///
    /// NB: likely to be replaced as API is refined; e.g. pnkfelix
    /// anticipates `fn entry_node_id` and `fn each_exit_node_id`.
    pub fn item_local_id(&self) -> hir::ItemLocalId {
        match *self {
            Scope::Node(id) => id,

            // These cases all return rough approximations to the
            // precise scope denoted by `self`.
            Scope::Remainder(br) => br.block,
            Scope::Destruction(id) |
            Scope::CallSite(id) |
            Scope::Arguments(id) => id,
        }
    }

    pub fn node_id(&self, tcx: TyCtxt, scope_tree: &ScopeTree) -> ast::NodeId {
        match scope_tree.root_body {
            Some(hir_id) => {
                tcx.hir.hir_to_node_id(hir::HirId {
                    owner: hir_id.owner,
                    local_id: self.item_local_id()
                })
            }
            None => ast::DUMMY_NODE_ID
        }
    }

    /// Returns the span of this Scope.  Note that in general the
    /// returned span may not correspond to the span of any node id in
    /// the AST.
    pub fn span(&self, tcx: TyCtxt, scope_tree: &ScopeTree) -> Span {
        let node_id = self.node_id(tcx, scope_tree);
        if node_id == ast::DUMMY_NODE_ID {
            return DUMMY_SP;
        }
        let span = tcx.hir.span(node_id);
        if let Scope::Remainder(r) = *self {
            if let hir::map::NodeBlock(ref blk) = tcx.hir.get(node_id) {
                // Want span for scope starting after the
                // indexed statement and ending at end of
                // `blk`; reuse span of `blk` and shift `lo`
                // forward to end of indexed statement.
                //
                // (This is the special case aluded to in the
                // doc-comment for this method)

                let stmt_span = blk.stmts[r.first_statement_index as usize].span;

                // To avoid issues with macro-generated spans, the span
                // of the statement must be nested in that of the block.
                if span.lo() <= stmt_span.lo() && stmt_span.lo() <= span.hi() {
                    return Span::new(stmt_span.lo(), span.hi(), span.ctxt());
                }
            }
         }
         span
    }
}

/// The region scope tree encodes information about region relationships.
#[derive(Default)]
pub struct ScopeTree {
    /// If not empty, this body is the root of this region hierarchy.
    root_body: Option<hir::HirId>,

    /// The parent of the root body owner, if the latter is an
    /// an associated const or method, as impls/traits can also
    /// have lifetime parameters free in this body.
    root_parent: Option<ast::NodeId>,

    /// `parent_map` maps from a scope id to the enclosing scope id;
    /// this is usually corresponding to the lexical nesting, though
    /// in the case of closures the parent scope is the innermost
    /// conditional expression or repeating block. (Note that the
    /// enclosing scope id for the block associated with a closure is
    /// the closure itself.)
    parent_map: FxHashMap<Scope, Scope>,

    /// `var_map` maps from a variable or binding id to the block in
    /// which that variable is declared.
    var_map: FxHashMap<hir::ItemLocalId, Scope>,

    /// maps from a node-id to the associated destruction scope (if any)
    destruction_scopes: FxHashMap<hir::ItemLocalId, Scope>,

    /// `rvalue_scopes` includes entries for those expressions whose cleanup scope is
    /// larger than the default. The map goes from the expression id
    /// to the cleanup scope id. For rvalues not present in this
    /// table, the appropriate cleanup scope is the innermost
    /// enclosing statement, conditional expression, or repeating
    /// block (see `terminating_scopes`).
    /// In constants, None is used to indicate that certain expressions
    /// escape into 'static and should have no local cleanup scope.
    rvalue_scopes: FxHashMap<hir::ItemLocalId, Option<Scope>>,

    /// Encodes the hierarchy of fn bodies. Every fn body (including
    /// closures) forms its own distinct region hierarchy, rooted in
    /// the block that is the fn body. This map points from the id of
    /// that root block to the id of the root block for the enclosing
    /// fn, if any. Thus the map structures the fn bodies into a
    /// hierarchy based on their lexical mapping. This is used to
    /// handle the relationships between regions in a fn and in a
    /// closure defined by that fn. See the "Modeling closures"
    /// section of the README in infer::region_inference for
    /// more details.
    closure_tree: FxHashMap<hir::ItemLocalId, hir::ItemLocalId>,

    /// If there are any `yield` nested within a scope, this map
    /// stores the `Span` of the last one and the number of expressions
    /// which came before it in a generator body.
    yield_in_scope: FxHashMap<Scope, (Span, usize)>,

    /// The number of visit_expr calls done in the body.
    /// Used to sanity check visit_expr call count when
    /// calculating geneartor interiors.
    body_expr_count: FxHashMap<hir::BodyId, usize>,
}

#[derive(Debug, Copy, Clone)]
pub struct Context {
    /// the root of the current region tree. This is typically the id
    /// of the innermost fn body. Each fn forms its own disjoint tree
    /// in the region hierarchy. These fn bodies are themselves
    /// arranged into a tree. See the "Modeling closures" section of
    /// the README in infer::region_inference for more
    /// details.
    root_id: Option<hir::ItemLocalId>,

    /// the scope that contains any new variables declared
    var_parent: Option<Scope>,

    /// region parent of expressions etc
    parent: Option<Scope>,
}

struct RegionResolutionVisitor<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,

    // The number of expressions visited in the current body
    expr_count: usize,

    // Generated scope tree:
    scope_tree: ScopeTree,

    cx: Context,

    /// `terminating_scopes` is a set containing the ids of each
    /// statement, or conditional/repeating expression. These scopes
    /// are calling "terminating scopes" because, when attempting to
    /// find the scope of a temporary, by default we search up the
    /// enclosing scopes until we encounter the terminating scope. A
    /// conditional/repeating expression is one which is not
    /// guaranteed to execute exactly once upon entering the parent
    /// scope. This could be because the expression only executes
    /// conditionally, such as the expression `b` in `a && b`, or
    /// because the expression may execute many times, such as a loop
    /// body. The reason that we distinguish such expressions is that,
    /// upon exiting the parent scope, we cannot statically know how
    /// many times the expression executed, and thus if the expression
    /// creates temporaries we cannot know statically how many such
    /// temporaries we would have to cleanup. Therefore we ensure that
    /// the temporaries never outlast the conditional/repeating
    /// expression, preventing the need for dynamic checks and/or
    /// arbitrary amounts of stack space. Terminating scopes end
    /// up being contained in a DestructionScope that contains the
    /// destructor's execution.
    terminating_scopes: FxHashSet<hir::ItemLocalId>,
}


impl<'tcx> ScopeTree {
    pub fn record_scope_parent(&mut self, child: Scope, parent: Option<Scope>) {
        debug!("{:?}.parent = {:?}", child, parent);

        if let Some(p) = parent {
            let prev = self.parent_map.insert(child, p);
            assert!(prev.is_none());
        }

        // record the destruction scopes for later so we can query them
        if let Scope::Destruction(n) = child {
            self.destruction_scopes.insert(n, child);
        }
    }

    pub fn each_encl_scope<E>(&self, mut e:E) where E: FnMut(Scope, Scope) {
        for (&child, &parent) in &self.parent_map {
            e(child, parent)
        }
    }

    pub fn each_var_scope<E>(&self, mut e:E) where E: FnMut(&hir::ItemLocalId, Scope) {
        for (child, &parent) in self.var_map.iter() {
            e(child, parent)
        }
    }

    pub fn opt_destruction_scope(&self, n: hir::ItemLocalId) -> Option<Scope> {
        self.destruction_scopes.get(&n).cloned()
    }

    /// Records that `sub_closure` is defined within `sup_closure`. These ids
    /// should be the id of the block that is the fn body, which is
    /// also the root of the region hierarchy for that fn.
    fn record_closure_parent(&mut self,
                             sub_closure: hir::ItemLocalId,
                             sup_closure: hir::ItemLocalId) {
        debug!("record_closure_parent(sub_closure={:?}, sup_closure={:?})",
               sub_closure, sup_closure);
        assert!(sub_closure != sup_closure);
        let previous = self.closure_tree.insert(sub_closure, sup_closure);
        assert!(previous.is_none());
    }

    fn closure_is_enclosed_by(&self,
                              mut sub_closure: hir::ItemLocalId,
                              sup_closure: hir::ItemLocalId) -> bool {
        loop {
            if sub_closure == sup_closure { return true; }
            match self.closure_tree.get(&sub_closure) {
                Some(&s) => { sub_closure = s; }
                None => { return false; }
            }
        }
    }

    fn record_var_scope(&mut self, var: hir::ItemLocalId, lifetime: Scope) {
        debug!("record_var_scope(sub={:?}, sup={:?})", var, lifetime);
        assert!(var != lifetime.item_local_id());
        self.var_map.insert(var, lifetime);
    }

    fn record_rvalue_scope(&mut self, var: hir::ItemLocalId, lifetime: Option<Scope>) {
        debug!("record_rvalue_scope(sub={:?}, sup={:?})", var, lifetime);
        if let Some(lifetime) = lifetime {
            assert!(var != lifetime.item_local_id());
        }
        self.rvalue_scopes.insert(var, lifetime);
    }

    pub fn opt_encl_scope(&self, id: Scope) -> Option<Scope> {
        //! Returns the narrowest scope that encloses `id`, if any.
        self.parent_map.get(&id).cloned()
    }

    #[allow(dead_code)] // used in cfg
    pub fn encl_scope(&self, id: Scope) -> Scope {
        //! Returns the narrowest scope that encloses `id`, if any.
        self.opt_encl_scope(id).unwrap()
    }

    /// Returns the lifetime of the local variable `var_id`
    pub fn var_scope(&self, var_id: hir::ItemLocalId) -> Scope {
        match self.var_map.get(&var_id) {
            Some(&r) => r,
            None => { bug!("no enclosing scope for id {:?}", var_id); }
        }
    }

    pub fn temporary_scope(&self, expr_id: hir::ItemLocalId) -> Option<Scope> {
        //! Returns the scope when temp created by expr_id will be cleaned up

        // check for a designated rvalue scope
        if let Some(&s) = self.rvalue_scopes.get(&expr_id) {
            debug!("temporary_scope({:?}) = {:?} [custom]", expr_id, s);
            return s;
        }

        // else, locate the innermost terminating scope
        // if there's one. Static items, for instance, won't
        // have an enclosing scope, hence no scope will be
        // returned.
        let mut id = Scope::Node(expr_id);

        while let Some(&p) = self.parent_map.get(&id) {
            match p {
                Scope::Destruction(..) => {
                    debug!("temporary_scope({:?}) = {:?} [enclosing]",
                           expr_id, id);
                    return Some(id);
                }
                _ => id = p
            }
        }

        debug!("temporary_scope({:?}) = None", expr_id);
        return None;
    }

    pub fn var_region(&self, id: hir::ItemLocalId) -> ty::RegionKind {
        //! Returns the lifetime of the variable `id`.

        let scope = ty::ReScope(self.var_scope(id));
        debug!("var_region({:?}) = {:?}", id, scope);
        scope
    }

    pub fn scopes_intersect(&self, scope1: Scope, scope2: Scope)
                            -> bool {
        self.is_subscope_of(scope1, scope2) ||
        self.is_subscope_of(scope2, scope1)
    }

    /// Returns true if `subscope` is equal to or is lexically nested inside `superscope` and false
    /// otherwise.
    pub fn is_subscope_of(&self,
                          subscope: Scope,
                          superscope: Scope)
                          -> bool {
        let mut s = subscope;
        debug!("is_subscope_of({:?}, {:?})", subscope, superscope);
        while superscope != s {
            match self.opt_encl_scope(s) {
                None => {
                    debug!("is_subscope_of({:?}, {:?}, s={:?})=false",
                           subscope, superscope, s);
                    return false;
                }
                Some(scope) => s = scope
            }
        }

        debug!("is_subscope_of({:?}, {:?})=true",
               subscope, superscope);

        return true;
    }

    /// Finds the nearest common ancestor (if any) of two scopes.  That is, finds the smallest
    /// scope which is greater than or equal to both `scope_a` and `scope_b`.
    pub fn nearest_common_ancestor(&self,
                                   scope_a: Scope,
                                   scope_b: Scope)
                                   -> Scope {
        if scope_a == scope_b { return scope_a; }

        // [1] The initial values for `a_buf` and `b_buf` are not used.
        // The `ancestors_of` function will return some prefix that
        // is re-initialized with new values (or else fallback to a
        // heap-allocated vector).
        let mut a_buf: [Scope; 32] = [scope_a /* [1] */; 32];
        let mut a_vec: Vec<Scope> = vec![];
        let mut b_buf: [Scope; 32] = [scope_b /* [1] */; 32];
        let mut b_vec: Vec<Scope> = vec![];
        let parent_map = &self.parent_map;
        let a_ancestors = ancestors_of(parent_map, scope_a, &mut a_buf, &mut a_vec);
        let b_ancestors = ancestors_of(parent_map, scope_b, &mut b_buf, &mut b_vec);
        let mut a_index = a_ancestors.len() - 1;
        let mut b_index = b_ancestors.len() - 1;

        // Here, [ab]_ancestors is a vector going from narrow to broad.
        // The end of each vector will be the item where the scope is
        // defined; if there are any common ancestors, then the tails of
        // the vector will be the same.  So basically we want to walk
        // backwards from the tail of each vector and find the first point
        // where they diverge.  If one vector is a suffix of the other,
        // then the corresponding scope is a superscope of the other.

        if a_ancestors[a_index] != b_ancestors[b_index] {
            // In this case, the two regions belong to completely
            // different functions.  Compare those fn for lexical
            // nesting. The reasoning behind this is subtle.  See the
            // "Modeling closures" section of the README in
            // infer::region_inference for more details.
            let a_root_scope = a_ancestors[a_index];
            let b_root_scope = a_ancestors[a_index];
            return match (a_root_scope, b_root_scope) {
                (Scope::Destruction(a_root_id),
                 Scope::Destruction(b_root_id)) => {
                    if self.closure_is_enclosed_by(a_root_id, b_root_id) {
                        // `a` is enclosed by `b`, hence `b` is the ancestor of everything in `a`
                        scope_b
                    } else if self.closure_is_enclosed_by(b_root_id, a_root_id) {
                        // `b` is enclosed by `a`, hence `a` is the ancestor of everything in `b`
                        scope_a
                    } else {
                        // neither fn encloses the other
                        bug!()
                    }
                }
                _ => {
                    // root ids are always Node right now
                    bug!()
                }
            };
        }

        loop {
            // Loop invariant: a_ancestors[a_index] == b_ancestors[b_index]
            // for all indices between a_index and the end of the array
            if a_index == 0 { return scope_a; }
            if b_index == 0 { return scope_b; }
            a_index -= 1;
            b_index -= 1;
            if a_ancestors[a_index] != b_ancestors[b_index] {
                return a_ancestors[a_index + 1];
            }
        }

        fn ancestors_of<'a, 'tcx>(parent_map: &FxHashMap<Scope, Scope>,
                                  scope: Scope,
                                  buf: &'a mut [Scope; 32],
                                  vec: &'a mut Vec<Scope>)
                                  -> &'a [Scope] {
            // debug!("ancestors_of(scope={:?})", scope);
            let mut scope = scope;

            let mut i = 0;
            while i < 32 {
                buf[i] = scope;
                match parent_map.get(&scope) {
                    Some(&superscope) => scope = superscope,
                    _ => return &buf[..i+1]
                }
                i += 1;
            }

            *vec = Vec::with_capacity(64);
            vec.extend_from_slice(buf);
            loop {
                vec.push(scope);
                match parent_map.get(&scope) {
                    Some(&superscope) => scope = superscope,
                    _ => return &*vec
                }
            }
        }
    }

    /// Assuming that the provided region was defined within this `ScopeTree`,
    /// returns the outermost `Scope` that the region outlives.
    pub fn early_free_scope<'a, 'gcx>(&self, tcx: TyCtxt<'a, 'gcx, 'tcx>,
                                       br: &ty::EarlyBoundRegion)
                                       -> Scope {
        let param_owner = tcx.parent_def_id(br.def_id).unwrap();

        let param_owner_id = tcx.hir.as_local_node_id(param_owner).unwrap();
        let scope = tcx.hir.maybe_body_owned_by(param_owner_id).map(|body_id| {
            tcx.hir.body(body_id).value.hir_id.local_id
        }).unwrap_or_else(|| {
            // The lifetime was defined on node that doesn't own a body,
            // which in practice can only mean a trait or an impl, that
            // is the parent of a method, and that is enforced below.
            assert_eq!(Some(param_owner_id), self.root_parent,
                       "free_scope: {:?} not recognized by the \
                        region scope tree for {:?} / {:?}",
                       param_owner,
                       self.root_parent.map(|id| tcx.hir.local_def_id(id)),
                       self.root_body.map(|hir_id| DefId::local(hir_id.owner)));

            // The trait/impl lifetime is in scope for the method's body.
            self.root_body.unwrap().local_id
        });

        Scope::CallSite(scope)
    }

    /// Assuming that the provided region was defined within this `ScopeTree`,
    /// returns the outermost `Scope` that the region outlives.
    pub fn free_scope<'a, 'gcx>(&self, tcx: TyCtxt<'a, 'gcx, 'tcx>, fr: &ty::FreeRegion)
                                 -> Scope {
        let param_owner = match fr.bound_region {
            ty::BoundRegion::BrNamed(def_id, _) => {
                tcx.parent_def_id(def_id).unwrap()
            }
            _ => fr.scope
        };

        // Ensure that the named late-bound lifetimes were defined
        // on the same function that they ended up being freed in.
        assert_eq!(param_owner, fr.scope);

        let param_owner_id = tcx.hir.as_local_node_id(param_owner).unwrap();
        let body_id = tcx.hir.body_owned_by(param_owner_id);
        Scope::CallSite(tcx.hir.body(body_id).value.hir_id.local_id)
    }

    /// Checks whether the given scope contains a `yield`. If so,
    /// returns `Some((span, expr_count))` with the span of a yield we found and
    /// the number of expressions appearing before the `yield` in the body.
    pub fn yield_in_scope(&self, scope: Scope) -> Option<(Span, usize)> {
        self.yield_in_scope.get(&scope).cloned()
    }

    /// Gives the number of expressions visited in a body.
    /// Used to sanity check visit_expr call count when
    /// calculating geneartor interiors.
    pub fn body_expr_count(&self, body_id: hir::BodyId) -> Option<usize> {
        self.body_expr_count.get(&body_id).map(|r| *r)
    }
}

/// Records the lifetime of a local variable as `cx.var_parent`
fn record_var_lifetime(visitor: &mut RegionResolutionVisitor,
                       var_id: hir::ItemLocalId,
                       _sp: Span) {
    match visitor.cx.var_parent {
        None => {
            // this can happen in extern fn declarations like
            //
            // extern fn isalnum(c: c_int) -> c_int
        }
        Some(parent_scope) =>
            visitor.scope_tree.record_var_scope(var_id, parent_scope),
    }
}

fn resolve_block<'a, 'tcx>(visitor: &mut RegionResolutionVisitor<'a, 'tcx>, blk: &'tcx hir::Block) {
    debug!("resolve_block(blk.id={:?})", blk.id);

    let prev_cx = visitor.cx;

    // We treat the tail expression in the block (if any) somewhat
    // differently from the statements. The issue has to do with
    // temporary lifetimes. Consider the following:
    //
    //    quux({
    //        let inner = ... (&bar()) ...;
    //
    //        (... (&foo()) ...) // (the tail expression)
    //    }, other_argument());
    //
    // Each of the statements within the block is a terminating
    // scope, and thus a temporary (e.g. the result of calling
    // `bar()` in the initalizer expression for `let inner = ...;`)
    // will be cleaned up immediately after its corresponding
    // statement (i.e. `let inner = ...;`) executes.
    //
    // On the other hand, temporaries associated with evaluating the
    // tail expression for the block are assigned lifetimes so that
    // they will be cleaned up as part of the terminating scope
    // *surrounding* the block expression. Here, the terminating
    // scope for the block expression is the `quux(..)` call; so
    // those temporaries will only be cleaned up *after* both
    // `other_argument()` has run and also the call to `quux(..)`
    // itself has returned.

    visitor.enter_node_scope_with_dtor(blk.hir_id.local_id);
    visitor.cx.var_parent = visitor.cx.parent;

    {
        // This block should be kept approximately in sync with
        // `intravisit::walk_block`. (We manually walk the block, rather
        // than call `walk_block`, in order to maintain precise
        // index information.)

        for (i, statement) in blk.stmts.iter().enumerate() {
            if let hir::StmtDecl(..) = statement.node {
                // Each StmtDecl introduces a subscope for bindings
                // introduced by the declaration; this subscope covers
                // a suffix of the block . Each subscope in a block
                // has the previous subscope in the block as a parent,
                // except for the first such subscope, which has the
                // block itself as a parent.
                visitor.enter_scope(
                    Scope::Remainder(BlockRemainder {
                        block: blk.hir_id.local_id,
                        first_statement_index: i as u32
                    })
                );
                visitor.cx.var_parent = visitor.cx.parent;
            }
            visitor.visit_stmt(statement)
        }
        walk_list!(visitor, visit_expr, &blk.expr);
    }

    visitor.cx = prev_cx;
}

fn resolve_arm<'a, 'tcx>(visitor: &mut RegionResolutionVisitor<'a, 'tcx>, arm: &'tcx hir::Arm) {
    visitor.terminating_scopes.insert(arm.body.hir_id.local_id);

    if let Some(ref expr) = arm.guard {
        visitor.terminating_scopes.insert(expr.hir_id.local_id);
    }

    intravisit::walk_arm(visitor, arm);
}

fn resolve_pat<'a, 'tcx>(visitor: &mut RegionResolutionVisitor<'a, 'tcx>, pat: &'tcx hir::Pat) {
    visitor.record_child_scope(Scope::Node(pat.hir_id.local_id));

    // If this is a binding then record the lifetime of that binding.
    if let PatKind::Binding(..) = pat.node {
        record_var_lifetime(visitor, pat.hir_id.local_id, pat.span);
    }

    intravisit::walk_pat(visitor, pat);
}

fn resolve_stmt<'a, 'tcx>(visitor: &mut RegionResolutionVisitor<'a, 'tcx>, stmt: &'tcx hir::Stmt) {
    let stmt_id = visitor.tcx.hir.node_to_hir_id(stmt.node.id()).local_id;
    debug!("resolve_stmt(stmt.id={:?})", stmt_id);

    // Every statement will clean up the temporaries created during
    // execution of that statement. Therefore each statement has an
    // associated destruction scope that represents the scope of the
    // statement plus its destructors, and thus the scope for which
    // regions referenced by the destructors need to survive.
    visitor.terminating_scopes.insert(stmt_id);

    let prev_parent = visitor.cx.parent;
    visitor.enter_node_scope_with_dtor(stmt_id);

    intravisit::walk_stmt(visitor, stmt);

    visitor.cx.parent = prev_parent;
}

fn resolve_expr<'a, 'tcx>(visitor: &mut RegionResolutionVisitor<'a, 'tcx>, expr: &'tcx hir::Expr) {
    debug!("resolve_expr(expr.id={:?})", expr.id);

    visitor.expr_count += 1;

    let prev_cx = visitor.cx;
    visitor.enter_node_scope_with_dtor(expr.hir_id.local_id);

    {
        let terminating_scopes = &mut visitor.terminating_scopes;
        let mut terminating = |id: hir::ItemLocalId| {
            terminating_scopes.insert(id);
        };
        match expr.node {
            // Conditional or repeating scopes are always terminating
            // scopes, meaning that temporaries cannot outlive them.
            // This ensures fixed size stacks.

            hir::ExprBinary(codemap::Spanned { node: hir::BiAnd, .. }, _, ref r) |
            hir::ExprBinary(codemap::Spanned { node: hir::BiOr, .. }, _, ref r) => {
                // For shortcircuiting operators, mark the RHS as a terminating
                // scope since it only executes conditionally.
                terminating(r.hir_id.local_id);
            }

            hir::ExprIf(ref expr, ref then, Some(ref otherwise)) => {
                terminating(expr.hir_id.local_id);
                terminating(then.hir_id.local_id);
                terminating(otherwise.hir_id.local_id);
            }

            hir::ExprIf(ref expr, ref then, None) => {
                terminating(expr.hir_id.local_id);
                terminating(then.hir_id.local_id);
            }

            hir::ExprLoop(ref body, _, _) => {
                terminating(body.hir_id.local_id);
            }

            hir::ExprWhile(ref expr, ref body, _) => {
                terminating(expr.hir_id.local_id);
                terminating(body.hir_id.local_id);
            }

            hir::ExprMatch(..) => {
                visitor.cx.var_parent = visitor.cx.parent;
            }

            hir::ExprAssignOp(..) | hir::ExprIndex(..) |
            hir::ExprUnary(..) | hir::ExprCall(..) | hir::ExprMethodCall(..) => {
                // FIXME(#6268) Nested method calls
                //
                // The lifetimes for a call or method call look as follows:
                //
                // call.id
                // - arg0.id
                // - ...
                // - argN.id
                // - call.callee_id
                //
                // The idea is that call.callee_id represents *the time when
                // the invoked function is actually running* and call.id
                // represents *the time to prepare the arguments and make the
                // call*.  See the section "Borrows in Calls" borrowck/README.md
                // for an extended explanation of why this distinction is
                // important.
                //
                // record_superlifetime(new_cx, expr.callee_id);
            }

            hir::ExprYield(..) => {
                // Mark this expr's scope and all parent scopes as containing `yield`.
                let mut scope = Scope::Node(expr.hir_id.local_id);
                loop {
                    visitor.scope_tree.yield_in_scope.insert(scope,
                        (expr.span, visitor.expr_count));

                    // Keep traversing up while we can.
                    match visitor.scope_tree.parent_map.get(&scope) {
                        // Don't cross from closure bodies to their parent.
                        Some(&Scope::CallSite(_)) => break,
                        Some(&superscope) => scope = superscope,
                        None => break
                    }
                }
            }

            _ => {}
        }
    }

    match expr.node {
        // Manually recurse over closures, because they are the only
        // case of nested bodies that share the parent environment.
        hir::ExprClosure(.., body, _, _) => {
            let body = visitor.tcx.hir.body(body);
            visitor.visit_body(body);
        }

        _ => intravisit::walk_expr(visitor, expr)
    }

    visitor.cx = prev_cx;
}

fn resolve_local<'a, 'tcx>(visitor: &mut RegionResolutionVisitor<'a, 'tcx>,
                           pat: Option<&'tcx hir::Pat>,
                           init: Option<&'tcx hir::Expr>) {
    debug!("resolve_local(pat={:?}, init={:?})", pat, init);

    let blk_scope = visitor.cx.var_parent;

    // As an exception to the normal rules governing temporary
    // lifetimes, initializers in a let have a temporary lifetime
    // of the enclosing block. This means that e.g. a program
    // like the following is legal:
    //
    //     let ref x = HashMap::new();
    //
    // Because the hash map will be freed in the enclosing block.
    //
    // We express the rules more formally based on 3 grammars (defined
    // fully in the helpers below that implement them):
    //
    // 1. `E&`, which matches expressions like `&<rvalue>` that
    //    own a pointer into the stack.
    //
    // 2. `P&`, which matches patterns like `ref x` or `(ref x, ref
    //    y)` that produce ref bindings into the value they are
    //    matched against or something (at least partially) owned by
    //    the value they are matched against. (By partially owned,
    //    I mean that creating a binding into a ref-counted or managed value
    //    would still count.)
    //
    // 3. `ET`, which matches both rvalues like `foo()` as well as lvalues
    //    based on rvalues like `foo().x[2].y`.
    //
    // A subexpression `<rvalue>` that appears in a let initializer
    // `let pat [: ty] = expr` has an extended temporary lifetime if
    // any of the following conditions are met:
    //
    // A. `pat` matches `P&` and `expr` matches `ET`
    //    (covers cases where `pat` creates ref bindings into an rvalue
    //     produced by `expr`)
    // B. `ty` is a borrowed pointer and `expr` matches `ET`
    //    (covers cases where coercion creates a borrow)
    // C. `expr` matches `E&`
    //    (covers cases `expr` borrows an rvalue that is then assigned
    //     to memory (at least partially) owned by the binding)
    //
    // Here are some examples hopefully giving an intuition where each
    // rule comes into play and why:
    //
    // Rule A. `let (ref x, ref y) = (foo().x, 44)`. The rvalue `(22, 44)`
    // would have an extended lifetime, but not `foo()`.
    //
    // Rule B. `let x = &foo().x`. The rvalue ``foo()` would have extended
    // lifetime.
    //
    // In some cases, multiple rules may apply (though not to the same
    // rvalue). For example:
    //
    //     let ref x = [&a(), &b()];
    //
    // Here, the expression `[...]` has an extended lifetime due to rule
    // A, but the inner rvalues `a()` and `b()` have an extended lifetime
    // due to rule C.
    //
    // FIXME(#6308) -- Note that `[]` patterns work more smoothly post-DST.

    if let Some(expr) = init {
        record_rvalue_scope_if_borrow_expr(visitor, &expr, blk_scope);

        if let Some(pat) = pat {
            if is_binding_pat(pat) {
                record_rvalue_scope(visitor, &expr, blk_scope);
            }
        }
    }

    if let Some(pat) = pat {
        visitor.visit_pat(pat);
    }
    if let Some(expr) = init {
        visitor.visit_expr(expr);
    }

    /// True if `pat` match the `P&` nonterminal:
    ///
    ///     P& = ref X
    ///        | StructName { ..., P&, ... }
    ///        | VariantName(..., P&, ...)
    ///        | [ ..., P&, ... ]
    ///        | ( ..., P&, ... )
    ///        | box P&
    fn is_binding_pat(pat: &hir::Pat) -> bool {
        // Note that the code below looks for *explicit* refs only, that is, it won't
        // know about *implicit* refs as introduced in #42640.
        //
        // This is not a problem. For example, consider
        //
        //      let (ref x, ref y) = (Foo { .. }, Bar { .. });
        //
        // Due to the explicit refs on the left hand side, the below code would signal
        // that the temporary value on the right hand side should live until the end of
        // the enclosing block (as opposed to being dropped after the let is complete).
        //
        // To create an implicit ref, however, you must have a borrowed value on the RHS
        // already, as in this example (which won't compile before #42640):
        //
        //      let Foo { x, .. } = &Foo { x: ..., ... };
        //
        // in place of
        //
        //      let Foo { ref x, .. } = Foo { ... };
        //
        // In the former case (the implicit ref version), the temporary is created by the
        // & expression, and its lifetime would be extended to the end of the block (due
        // to a different rule, not the below code).
        match pat.node {
            PatKind::Binding(hir::BindingAnnotation::Ref, ..) |
            PatKind::Binding(hir::BindingAnnotation::RefMut, ..) => true,

            PatKind::Struct(_, ref field_pats, _) => {
                field_pats.iter().any(|fp| is_binding_pat(&fp.node.pat))
            }

            PatKind::Slice(ref pats1, ref pats2, ref pats3) => {
                pats1.iter().any(|p| is_binding_pat(&p)) ||
                pats2.iter().any(|p| is_binding_pat(&p)) ||
                pats3.iter().any(|p| is_binding_pat(&p))
            }

            PatKind::TupleStruct(_, ref subpats, _) |
            PatKind::Tuple(ref subpats, _) => {
                subpats.iter().any(|p| is_binding_pat(&p))
            }

            PatKind::Box(ref subpat) => {
                is_binding_pat(&subpat)
            }

            _ => false,
        }
    }

    /// If `expr` matches the `E&` grammar, then records an extended rvalue scope as appropriate:
    ///
    ///     E& = & ET
    ///        | StructName { ..., f: E&, ... }
    ///        | [ ..., E&, ... ]
    ///        | ( ..., E&, ... )
    ///        | {...; E&}
    ///        | box E&
    ///        | E& as ...
    ///        | ( E& )
    fn record_rvalue_scope_if_borrow_expr<'a, 'tcx>(
        visitor: &mut RegionResolutionVisitor<'a, 'tcx>,
        expr: &hir::Expr,
        blk_id: Option<Scope>)
    {
        match expr.node {
            hir::ExprAddrOf(_, ref subexpr) => {
                record_rvalue_scope_if_borrow_expr(visitor, &subexpr, blk_id);
                record_rvalue_scope(visitor, &subexpr, blk_id);
            }
            hir::ExprStruct(_, ref fields, _) => {
                for field in fields {
                    record_rvalue_scope_if_borrow_expr(
                        visitor, &field.expr, blk_id);
                }
            }
            hir::ExprArray(ref subexprs) |
            hir::ExprTup(ref subexprs) => {
                for subexpr in subexprs {
                    record_rvalue_scope_if_borrow_expr(
                        visitor, &subexpr, blk_id);
                }
            }
            hir::ExprCast(ref subexpr, _) => {
                record_rvalue_scope_if_borrow_expr(visitor, &subexpr, blk_id)
            }
            hir::ExprBlock(ref block) => {
                if let Some(ref subexpr) = block.expr {
                    record_rvalue_scope_if_borrow_expr(
                        visitor, &subexpr, blk_id);
                }
            }
            _ => {}
        }
    }

    /// Applied to an expression `expr` if `expr` -- or something owned or partially owned by
    /// `expr` -- is going to be indirectly referenced by a variable in a let statement. In that
    /// case, the "temporary lifetime" or `expr` is extended to be the block enclosing the `let`
    /// statement.
    ///
    /// More formally, if `expr` matches the grammar `ET`, record the rvalue scope of the matching
    /// `<rvalue>` as `blk_id`:
    ///
    ///     ET = *ET
    ///        | ET[...]
    ///        | ET.f
    ///        | (ET)
    ///        | <rvalue>
    ///
    /// Note: ET is intended to match "rvalues or lvalues based on rvalues".
    fn record_rvalue_scope<'a, 'tcx>(visitor: &mut RegionResolutionVisitor<'a, 'tcx>,
                                     expr: &hir::Expr,
                                     blk_scope: Option<Scope>) {
        let mut expr = expr;
        loop {
            // Note: give all the expressions matching `ET` with the
            // extended temporary lifetime, not just the innermost rvalue,
            // because in trans if we must compile e.g. `*rvalue()`
            // into a temporary, we request the temporary scope of the
            // outer expression.
            visitor.scope_tree.record_rvalue_scope(expr.hir_id.local_id, blk_scope);

            match expr.node {
                hir::ExprAddrOf(_, ref subexpr) |
                hir::ExprUnary(hir::UnDeref, ref subexpr) |
                hir::ExprField(ref subexpr, _) |
                hir::ExprTupField(ref subexpr, _) |
                hir::ExprIndex(ref subexpr, _) => {
                    expr = &subexpr;
                }
                _ => {
                    return;
                }
            }
        }
    }
}

impl<'a, 'tcx> RegionResolutionVisitor<'a, 'tcx> {
    /// Records the current parent (if any) as the parent of `child_scope`.
    fn record_child_scope(&mut self, child_scope: Scope) {
        let parent = self.cx.parent;
        self.scope_tree.record_scope_parent(child_scope, parent);
    }

    /// Records the current parent (if any) as the parent of `child_scope`,
    /// and sets `child_scope` as the new current parent.
    fn enter_scope(&mut self, child_scope: Scope) {
        self.record_child_scope(child_scope);
        self.cx.parent = Some(child_scope);
    }

    fn enter_node_scope_with_dtor(&mut self, id: hir::ItemLocalId) {
        // If node was previously marked as a terminating scope during the
        // recursive visit of its parent node in the AST, then we need to
        // account for the destruction scope representing the scope of
        // the destructors that run immediately after it completes.
        if self.terminating_scopes.contains(&id) {
            self.enter_scope(Scope::Destruction(id));
        }
        self.enter_scope(Scope::Node(id));
    }
}

impl<'a, 'tcx> Visitor<'tcx> for RegionResolutionVisitor<'a, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'tcx> {
        NestedVisitorMap::None
    }

    fn visit_block(&mut self, b: &'tcx Block) {
        resolve_block(self, b);
    }

    fn visit_body(&mut self, body: &'tcx hir::Body) {
        let body_id = body.id();
        let owner_id = self.tcx.hir.body_owner(body_id);

        debug!("visit_body(id={:?}, span={:?}, body.id={:?}, cx.parent={:?})",
               owner_id,
               self.tcx.sess.codemap().span_to_string(body.value.span),
               body_id,
               self.cx.parent);

        let outer_ec = mem::replace(&mut self.expr_count, 0);
        let outer_cx = self.cx;
        let outer_ts = mem::replace(&mut self.terminating_scopes, FxHashSet());
        self.terminating_scopes.insert(body.value.hir_id.local_id);

        if let Some(root_id) = self.cx.root_id {
            self.scope_tree.record_closure_parent(body.value.hir_id.local_id, root_id);
        }
        self.cx.root_id = Some(body.value.hir_id.local_id);

        self.enter_scope(Scope::CallSite(body.value.hir_id.local_id));
        self.enter_scope(Scope::Arguments(body.value.hir_id.local_id));

        // The arguments and `self` are parented to the fn.
        self.cx.var_parent = self.cx.parent.take();
        for argument in &body.arguments {
            self.visit_pat(&argument.pat);
        }

        // The body of the every fn is a root scope.
        self.cx.parent = self.cx.var_parent;
        if let MirSource::Fn(_) = MirSource::from_node(self.tcx, owner_id) {
            self.visit_expr(&body.value);
        } else {
            // Only functions have an outer terminating (drop) scope, while
            // temporaries in constant initializers may be 'static, but only
            // according to rvalue lifetime semantics, using the same
            // syntactical rules used for let initializers.
            //
            // E.g. in `let x = &f();`, the temporary holding the result from
            // the `f()` call lives for the entirety of the surrounding block.
            //
            // Similarly, `const X: ... = &f();` would have the result of `f()`
            // live for `'static`, implying (if Drop restrictions on constants
            // ever get lifted) that the value *could* have a destructor, but
            // it'd get leaked instead of the destructor running during the
            // evaluation of `X` (if at all allowed by CTFE).
            //
            // However, `const Y: ... = g(&f());`, like `let y = g(&f());`,
            // would *not* let the `f()` temporary escape into an outer scope
            // (i.e. `'static`), which means that after `g` returns, it drops,
            // and all the associated destruction scope rules apply.
            self.cx.var_parent = None;
            resolve_local(self, None, Some(&body.value));
        }

        if body.is_generator {
            self.scope_tree.body_expr_count.insert(body_id, self.expr_count);
        }

        // Restore context we had at the start.
        self.expr_count = outer_ec;
        self.cx = outer_cx;
        self.terminating_scopes = outer_ts;
    }

    fn visit_arm(&mut self, a: &'tcx Arm) {
        resolve_arm(self, a);
    }
    fn visit_pat(&mut self, p: &'tcx Pat) {
        resolve_pat(self, p);
    }
    fn visit_stmt(&mut self, s: &'tcx Stmt) {
        resolve_stmt(self, s);
    }
    fn visit_expr(&mut self, ex: &'tcx Expr) {
        resolve_expr(self, ex);
    }
    fn visit_local(&mut self, l: &'tcx Local) {
        resolve_local(self, Some(&l.pat), l.init.as_ref().map(|e| &**e));
    }
}

fn region_scope_tree<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId)
    -> Rc<ScopeTree>
{
    let closure_base_def_id = tcx.closure_base_def_id(def_id);
    if closure_base_def_id != def_id {
        return tcx.region_scope_tree(closure_base_def_id);
    }

    let id = tcx.hir.as_local_node_id(def_id).unwrap();
    let scope_tree = if let Some(body_id) = tcx.hir.maybe_body_owned_by(id) {
        let mut visitor = RegionResolutionVisitor {
            tcx,
            scope_tree: ScopeTree::default(),
            expr_count: 0,
            cx: Context {
                root_id: None,
                parent: None,
                var_parent: None,
            },
            terminating_scopes: FxHashSet(),
        };

        let body = tcx.hir.body(body_id);
        visitor.scope_tree.root_body = Some(body.value.hir_id);

        // If the item is an associated const or a method,
        // record its impl/trait parent, as it can also have
        // lifetime parameters free in this body.
        match tcx.hir.get(id) {
            hir::map::NodeImplItem(_) |
            hir::map::NodeTraitItem(_) => {
                visitor.scope_tree.root_parent = Some(tcx.hir.get_parent(id));
            }
            _ => {}
        }

        visitor.visit_body(body);

        visitor.scope_tree
    } else {
        ScopeTree::default()
    };

    Rc::new(scope_tree)
}

pub fn provide(providers: &mut Providers) {
    *providers = Providers {
        region_scope_tree,
        ..*providers
    };
}

impl<'gcx> HashStable<StableHashingContext<'gcx>> for ScopeTree {
    fn hash_stable<W: StableHasherResult>(&self,
                                          hcx: &mut StableHashingContext<'gcx>,
                                          hasher: &mut StableHasher<W>) {
        let ScopeTree {
            root_body,
            root_parent,
            ref parent_map,
            ref var_map,
            ref destruction_scopes,
            ref rvalue_scopes,
            ref closure_tree,
            ref yield_in_scope,
        } = *self;

        hcx.with_node_id_hashing_mode(NodeIdHashingMode::HashDefPath, |hcx| {
            root_body.hash_stable(hcx, hasher);
            root_parent.hash_stable(hcx, hasher);
        });

        parent_map.hash_stable(hcx, hasher);
        var_map.hash_stable(hcx, hasher);
        destruction_scopes.hash_stable(hcx, hasher);
        rvalue_scopes.hash_stable(hcx, hasher);
        closure_tree.hash_stable(hcx, hasher);
        yield_in_scope.hash_stable(hcx, hasher);
    }
}
