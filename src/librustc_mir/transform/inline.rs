// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Inlining pass for MIR functions

use rustc::hir::def_id::DefId;

use rustc_data_structures::bitvec::BitVector;
use rustc_data_structures::indexed_vec::{Idx, IndexVec};
use rustc_data_structures::graph;

use rustc::dep_graph::DepNode;
use rustc::mir::mir_map::MirMap;
use rustc::mir::repr::*;
use rustc::mir::transform::{MirMapPass, MirPass, MirPassHook, MirSource, Pass};
use rustc::mir::visit::*;
use rustc::traits;
use rustc::ty::{self, Ty, TyCtxt};
use rustc::ty::subst::{Subst,Substs};
use rustc::util::nodemap::{DefIdMap, DefIdSet};

use super::simplify_cfg::{remove_dead_blocks, CfgSimplifier};
use super::copy_prop::CopyPropagation;

use syntax::attr;
use syntax::abi::Abi;
use syntax_pos::Span;

use callgraph;

const DEFAULT_THRESHOLD : usize = 50;
const HINT_THRESHOLD : usize = 100;

const INSTR_COST : usize = 5;
const CALL_PENALTY : usize = 25;

const UNKNOWN_SIZE_COST : usize = 10;

use std::rc::Rc;

pub struct Inline;

impl<'tcx> MirMapPass<'tcx> for Inline {
    fn run_pass<'a>(
        &mut self,
        tcx: TyCtxt<'a, 'tcx, 'tcx>,
        map: &mut MirMap<'tcx>,
        hooks: &mut [Box<for<'s> MirPassHook<'s>>]) {

        if tcx.sess.opts.mir_opt_level < 2 { return; }

        let _ignore = tcx.dep_graph.in_ignore();

        let callgraph = callgraph::CallGraph::build(map);

        let mut inliner = Inliner {
            tcx: tcx,
            foreign_mirs: DefIdMap()
        };

        let def_ids = map.map.keys();
        for &def_id in &def_ids {
            let _task = tcx.dep_graph.in_task(DepNode::Mir(def_id));
            let mir = map.map.get_mut(&def_id).unwrap();
            let id = tcx.map.as_local_node_id(def_id).unwrap();
            let src = MirSource::from_node(tcx, id);

            for hook in &mut *hooks {
                hook.on_mir_pass(tcx, src, mir, self, false);
            }
        }

        for scc in callgraph.scc_iter() {
            inliner.inline_scc(map, &callgraph, &scc);
        }

        for def_id in def_ids {
            let _task = tcx.dep_graph.in_task(DepNode::Mir(def_id));
            let mir = map.map.get_mut(&def_id).unwrap();
            let id = tcx.map.as_local_node_id(def_id).unwrap();
            let src = MirSource::from_node(tcx, id);

            for hook in &mut *hooks {
                hook.on_mir_pass(tcx, src, mir, self, true);
            }
        }
    }
}

impl<'tcx> Pass for Inline { }

struct Inliner<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    foreign_mirs: DefIdMap<Rc<Mir<'tcx>>>,
}

#[derive(Copy, Clone)]
struct CallSite<'tcx> {
    caller: DefId,
    callee: DefId,
    substs: &'tcx Substs<'tcx>,
    bb: BasicBlock,
    location: SourceInfo,
}

impl<'a, 'tcx> Inliner<'a, 'tcx> {
    fn inline_scc(&mut self, map: &mut MirMap<'tcx>,
                            callgraph: &callgraph::CallGraph, scc: &[graph::NodeIndex]) -> bool {
        let mut callsites = Vec::new();
        let mut in_scc = DefIdSet();

        let mut inlined_into = DefIdSet();

        for &node in scc {
            let def_id = callgraph.def_id(node);

            // Don't inspect functions from other crates
            let id = if let Some(id) = self.tcx.map.as_local_node_id(def_id) {
                id
            } else {
                continue;
            };
            let src = MirSource::from_node(self.tcx, id);
            if let MirSource::Fn(_) = src {
                let mir = if let Some(m) = map.map.get(&def_id) {
                    m
                } else {
                    continue;
                };
                for (bb, bb_data) in mir.basic_blocks().iter_enumerated() {
                    // Don't inline calls that are in cleanup blocks.
                    if bb_data.is_cleanup { continue; }

                    // Only consider direct calls to functions
                    let terminator = bb_data.terminator();
                    if let TerminatorKind::Call {
                        func: Operand::Constant(ref f), .. } = terminator.kind {
                        if let ty::TyFnDef(callee_def_id, substs, _) = f.ty.sty {
                            callsites.push(CallSite {
                                caller: def_id,
                                callee: callee_def_id,
                                substs: substs,
                                bb: bb,
                                location: terminator.source_info
                            });
                        }
                    }
                }

                in_scc.insert(def_id);
            }
        }

        // Move callsites that are in the the SCC to the end so
        // they're inlined after calls to outside the SCC
        let mut first_call_in_scc = callsites.len();

        let mut i = 0;
        while i < first_call_in_scc {
            let f = callsites[i].caller;
            if in_scc.contains(&f) {
                first_call_in_scc -= 1;
                callsites.swap(i, first_call_in_scc);
            } else {
                i += 1;
            }
        }

        let mut local_change;
        let mut changed = false;

        loop {
            local_change = false;
            let mut csi = 0;
            while csi < callsites.len() {
                let foreign_mir;

                let callsite = callsites[csi];
                csi += 1;

                let callee_mir = {
                    let callee_mir : Option<&Mir<'tcx>> = if callsite.callee.is_local() {
                        map.map.get(&callsite.callee)
                    } else {
                        foreign_mir = self.get_foreign_mir(callsite.callee);
                        foreign_mir.as_ref().map(|m| &**m)
                    };

                    let callee_mir = if let Some(m) = callee_mir {
                        m
                    } else {
                        continue;
                    };

                    if !self.should_inline(callsite, callee_mir) {
                        continue;
                    }

                    callee_mir.subst(self.tcx, callsite.substs)
                };

                let caller_mir = map.map.get_mut(&callsite.caller).unwrap();

                let start = caller_mir.basic_blocks().len();

                if !self.inline_call(callsite, caller_mir, callee_mir) {
                    continue;
                }

                inlined_into.insert(callsite.caller);

                // Add callsites from inlined function
                for (bb, bb_data) in caller_mir.basic_blocks().iter_enumerated().skip(start) {
                    // Only consider direct calls to functions
                    let terminator = bb_data.terminator();
                    if let TerminatorKind::Call {
                        func: Operand::Constant(ref f), .. } = terminator.kind {
                        if let ty::TyFnDef(callee_def_id, substs, _) = f.ty.sty {
                            // Don't inline the same function multiple times.
                            if callsite.callee != callee_def_id {
                                callsites.push(CallSite {
                                    caller: callsite.caller,
                                    callee: callee_def_id,
                                    substs: substs,
                                    bb: bb,
                                    location: terminator.source_info
                                });
                            }
                        }
                    }
                }


                csi -= 1;
                if scc.len() == 1 {
                    callsites.swap_remove(csi);
                } else {
                    callsites.remove(csi);
                }

                local_change = true;
                changed = true;
            }

            if !local_change {
                break;
            }
        }

        // Simplify functions we inlined into.
        for def_id in inlined_into {
            let caller_mir = map.map.get_mut(&def_id).unwrap();
            debug!("Running simplify cfg on {:?}", def_id);
            CfgSimplifier::new(caller_mir).simplify();
            remove_dead_blocks(caller_mir);
        }
        changed
    }

    fn get_foreign_mir(&mut self, def_id: DefId) -> Option<Rc<Mir<'tcx>>> {
        if let Some(mir) = self.foreign_mirs.get(&def_id).cloned() {
            return Some(mir);
        }
        // Cache the foreign MIR
        let mir = self.tcx.sess.cstore.maybe_get_item_mir(self.tcx, def_id);
        let mir = mir.map(Rc::new);
        if let Some(ref mir) = mir {
            self.foreign_mirs.insert(def_id, mir.clone());
        }

        mir
    }

    fn should_inline(&self, callsite: CallSite<'tcx>,
                     callee_mir: &'a Mir<'tcx>) -> bool {

        let tcx = self.tcx;

        // Don't inline closures that have captures
        // FIXME: Handle closures better
        if callee_mir.upvar_decls.len() > 0 {
            return false;
        }

        // Don't inline calls to trait methods
        // FIXME: Should try to resolve it to a concrete method, and
        // only bail if that isn't possible
        let trait_def = tcx.trait_of_item(callsite.callee);
        if trait_def.is_some() { return false; }

        let attrs = tcx.get_attrs(callsite.callee);
        let hint = attr::find_inline_attr(None, &attrs[..]);

        let hinted = match hint {
            // Just treat inline(always) as a hint for now,
            // there are cases that prevent inlining that we
            // need to check for first.
            attr::InlineAttr::Always => true,
            attr::InlineAttr::Never => return false,
            attr::InlineAttr::Hint => true,
            attr::InlineAttr::None => false,
        };

        // Only inline local functions if they would be eligible for
        // cross-crate inlining. This ensures that any symbols they
        // use are reachable cross-crate
        // FIXME(#36594): This shouldn't be necessary, and is more conservative
        // than it could be, but trans should generate the reachable set from
        // the MIR anyway, making any check obsolete.
        if callsite.callee.is_local() {
            // No type substs and no inline hint means this function
            // wouldn't be eligible for cross-crate inlining
            if callsite.substs.types().count() == 0 && !hinted {
                return false;
            }

        }

        let mut threshold = if hinted {
            HINT_THRESHOLD
        } else {
            DEFAULT_THRESHOLD
        };

        // Significantly lower the threshold for inlining cold functions
        if attr::contains_name(&attrs[..], "cold") {
            threshold /= 5;
        }

        // Give a bonus functions with a small number of blocks,
        // We normally have two or three blocks for even
        // very small functions.
        if callee_mir.basic_blocks().len() <= 3 {
            threshold += threshold / 4;
        }

        // FIXME: Give a bonus to functions with only a single caller

        let id = tcx.map.as_local_node_id(callsite.caller).expect("Caller not local");
        let param_env = ty::ParameterEnvironment::for_item(tcx, id);

        let mut first_block = true;
        let mut cost = 0;

        // Traverse the MIR manually so we can account for the effects of
        // inlining on the CFG.
        let mut work_list = vec![START_BLOCK];
        let mut visited = BitVector::new(callee_mir.basic_blocks.len());
        while let Some(bb) = work_list.pop() {
            if !visited.insert(bb.index()) { continue; }
            let blk = &callee_mir.basic_blocks[bb];

            for stmt in &blk.statements {
                // Don't count StorageLive/StorageDead in the inlining cost.
                match stmt.kind {
                    StatementKind::StorageLive(_) |
                    StatementKind::StorageDead(_) |
                    StatementKind::Nop => {}
                    _ => cost += INSTR_COST
                }
            }
            let term = blk.terminator();
            let mut is_drop = false;
            match term.kind {
                TerminatorKind::Drop { ref location, target, unwind } |
                TerminatorKind::DropAndReplace { ref location, target, unwind, .. } => {
                    is_drop = true;
                    work_list.push(target);
                    // If the location doesn't actually need dropping, treat it like
                    // a regular goto.
                    let ty = location.ty(&callee_mir, tcx).subst(tcx, callsite.substs);
                    let ty = ty.to_ty(tcx);
                    if tcx.type_needs_drop_given_env(ty, &param_env) {
                        cost += CALL_PENALTY;
                        if let Some(unwind) = unwind {
                            work_list.push(unwind);
                        }
                    } else {
                        cost += INSTR_COST;
                    }
                }

                TerminatorKind::Unreachable |
                TerminatorKind::Call { destination: None, .. } if first_block => {
                    // If the function always diverges, don't inline
                    // unless the cost is zero
                    threshold = 0;
                }

                TerminatorKind::Call {func: Operand::Constant(ref f), .. } => {
                    if let ty::TyFnDef(.., f) = f.ty.sty {
                        // Don't give intrinsics the extra penalty for calls
                        if f.abi == Abi::RustIntrinsic || f.abi == Abi::PlatformIntrinsic {
                            cost += INSTR_COST;
                        } else {
                            cost += CALL_PENALTY;
                        }
                    }
                }
                TerminatorKind::Assert { .. } => cost += CALL_PENALTY,
                _ => cost += INSTR_COST
            }

            if !is_drop {
                for &succ in &term.successors()[..] {
                    work_list.push(succ);
                }
            }

            first_block = false;
        }

        // Count up the cost of local variables and temps, if we know the size
        // use that, otherwise we use a moderately-large dummy cost.

        let ptr_size = tcx.data_layout.pointer_size.bytes();

        for v in &callee_mir.var_decls {
            let ty = v.ty.subst(tcx, callsite.substs);
            // Cost of the var is the size in machine-words, if we know
            // it.
            if let Some(size) = type_size_of(tcx, param_env.clone(), ty) {
                cost += (size / ptr_size) as usize;
            } else {
                cost += UNKNOWN_SIZE_COST;
            }
        }
        for t in &callee_mir.temp_decls {
            let ty = t.ty.subst(tcx, callsite.substs);
            // Cost of the var is the size in machine-words, if we know
            // it.
            if let Some(size) = type_size_of(tcx, param_env.clone(), ty) {
                cost += (size / ptr_size) as usize;
            } else {
                cost += UNKNOWN_SIZE_COST;
            }
        }

        debug!("Inline cost for {:?} is {}", callsite.callee, cost);

        if let attr::InlineAttr::Always = hint {
            true
        } else {
            cost <= threshold
        }
    }


    fn inline_call(&self, callsite: CallSite<'tcx>,
                             caller_mir: &mut Mir<'tcx>, callee_mir: Mir<'tcx>) -> bool {

        // Don't inline a function into itself
        if callsite.caller == callsite.callee { return false; }

        let _task = self.tcx.dep_graph.in_task(DepNode::Mir(callsite.caller));


        let terminator = caller_mir[callsite.bb].terminator.take().unwrap();
        let cm = self.tcx.sess.codemap();
        match terminator.kind {
            // FIXME: Handle inlining of diverging calls
            TerminatorKind::Call { args, destination: Some(destination), cleanup, .. } => {

                debug!("Inlined {:?} into {:?}", callsite.callee, callsite.caller);

                let is_box_free = Some(callsite.callee) == self.tcx.lang_items.box_free_fn();

                let mut var_map = IndexVec::with_capacity(callee_mir.var_decls.len());
                let mut temp_map = IndexVec::with_capacity(callee_mir.temp_decls.len());
                let mut scope_map = IndexVec::with_capacity(callee_mir.visibility_scopes.len());
                let mut promoted_map = IndexVec::with_capacity(callee_mir.promoted.len());

                for mut scope in callee_mir.visibility_scopes {
                    if scope.parent_scope.is_none() {
                        scope.parent_scope = Some(callsite.location.scope);
                        scope.span = callee_mir.span;
                    }

                    if !cm.is_valid_span(scope.span) {
                        scope.span = callsite.location.span;
                    }

                    let idx = caller_mir.visibility_scopes.push(scope);
                    scope_map.push(idx);
                }

                for mut var in callee_mir.var_decls {
                    var.source_info.scope = scope_map[var.source_info.scope];

                    if !cm.is_valid_span(var.source_info.span) {
                        var.source_info.span = callsite.location.span;
                    }
                    let idx = caller_mir.var_decls.push(var);
                    var_map.push(idx);
                }

                for temp in callee_mir.temp_decls {
                    let idx = caller_mir.temp_decls.push(temp);
                    temp_map.push(idx);
                }

                for p in callee_mir.promoted {
                    let idx = caller_mir.promoted.push(p);
                    promoted_map.push(idx);
                }

                // If the call is something like `a[*i] = f(i)`, where
                // `i : &mut usize`, then just duplicating the `a[*i]`
                // Lvalue could result in two different locations if `f`
                // writes to `i`. To prevent this we need to create a temporary
                // borrow of the lvalue and pass the destination as `*temp` instead.
                fn dest_needs_borrow(lval: &Lvalue) -> bool {
                    match *lval {
                        Lvalue::Projection(ref p) => {
                            match p.elem {
                                ProjectionElem::Deref |
                                ProjectionElem::Index(_) => true,
                                _ => dest_needs_borrow(&p.base)
                            }
                        }
                        // Static variables need a borrow because the callee
                        // might modify the same static.
                        Lvalue::Static(_) => true,
                        _ => false
                    }
                }

                let dest = if dest_needs_borrow(&destination.0) {
                    debug!("Creating temp for return destination");
                    let dest = Rvalue::Ref(
                        self.tcx.mk_region(ty::ReErased),
                        BorrowKind::Mut,
                        destination.0);

                    let ty = dest.ty(caller_mir, self.tcx).expect("Rvalue has no type!");

                    let temp = TempDecl { ty: ty };
                    let tmp = caller_mir.temp_decls.push(temp);
                    let tmp = Lvalue::Temp(tmp);

                    let stmt = Statement {
                        source_info: callsite.location,
                        kind: StatementKind::Assign(tmp.clone(), dest)
                    };
                    caller_mir[callsite.bb]
                        .statements.push(stmt);
                    tmp.deref()
                } else {
                    destination.0
                };

                let return_block = destination.1;

                let args : Vec<_> = if is_box_free {
                    assert!(args.len() == 1);
                    // box_free takes a Box, but is defined with a *mut T, inlining
                    // needs to generate the cast.
                    // FIXME: we should probably just generate correct MIR in the first place...

                    let arg = if let Operand::Consume(ref lval) = args[0] {
                        lval.clone()
                    } else {
                        bug!("Constant arg to \"box_free\"");
                    };

                    let ptr_ty = args[0].ty(caller_mir, self.tcx);
                    vec![self.cast_box_free_arg(arg, ptr_ty, &callsite, caller_mir)]
                } else {
                    // Copy the arguments if needed.
                    self.make_call_args(args, &callsite, caller_mir)
                };

                let bb_len = caller_mir.basic_blocks.len();
                let mut integrator = Integrator {
                    tcx: self.tcx,
                    block_idx: bb_len,
                    args: &args,
                    var_map: var_map,
                    tmp_map: temp_map,
                    scope_map: scope_map,
                    promoted_map: promoted_map,
                    callsite: callsite,
                    destination: dest,
                    return_block: return_block,
                    cleanup_block: cleanup,
                    in_cleanup_block: false
                };


                for (bb, mut block) in callee_mir.basic_blocks.into_iter_enumerated() {
                    integrator.visit_basic_block_data(bb, &mut block);
                    caller_mir.basic_blocks_mut().push(block);
                }

                let terminator = Terminator {
                    source_info: callsite.location,
                    kind: TerminatorKind::Goto { target: BasicBlock::new(bb_len) }
                };

                caller_mir[callsite.bb].terminator = Some(terminator);

                if let Some(id) = self.tcx.map.as_local_node_id(callsite.caller) {
                    // Run copy propagation on the function to clean up any unnecessary
                    // assignments from integration. This also increases the chance that
                    // this function will be inlined as well
                    debug!("Running copy propagation");
                    let src = MirSource::from_node(self.tcx, id);
                    MirPass::run_pass(&mut CopyPropagation, self.tcx, src, caller_mir);
                };

                true
            }
            kind => {
                caller_mir[callsite.bb].terminator = Some(Terminator {
                    source_info: terminator.source_info,
                    kind: kind
                });
                false
            }
        }
    }

    fn cast_box_free_arg(&self, arg: Lvalue<'tcx>, ptr_ty: Ty<'tcx>,
                         callsite: &CallSite<'tcx>, caller_mir: &mut Mir<'tcx>) -> Operand<'tcx> {
        let arg = Rvalue::Ref(
            self.tcx.mk_region(ty::ReErased),
            BorrowKind::Mut,
            arg.deref());

        let ty = arg.ty(caller_mir, self.tcx).expect("Rvalue has no type!");
        let ref_tmp = TempDecl { ty: ty };
        let ref_tmp = caller_mir.temp_decls.push(ref_tmp);
        let ref_tmp = Lvalue::Temp(ref_tmp);

        let ref_stmt = Statement {
            source_info: callsite.location,
            kind: StatementKind::Assign(ref_tmp.clone(), arg)
        };

        caller_mir[callsite.bb]
            .statements.push(ref_stmt);

        let pointee_ty = match ptr_ty.sty {
            ty::TyBox(ty) => ty,
            ty::TyRawPtr(tm) | ty::TyRef(_, tm) => tm.ty,
            _ => bug!("Invalid type `{:?}` for call to box_free", ptr_ty)
        };
        let ptr_ty = self.tcx.mk_mut_ptr(pointee_ty);

        let raw_ptr = Rvalue::Cast(CastKind::Misc, Operand::Consume(ref_tmp), ptr_ty);

        let cast_tmp = TempDecl { ty: ptr_ty };
        let cast_tmp = caller_mir.temp_decls.push(cast_tmp);
        let cast_tmp = Lvalue::Temp(cast_tmp);

        let cast_stmt = Statement {
            source_info: callsite.location,
            kind: StatementKind::Assign(cast_tmp.clone(), raw_ptr)
        };

        caller_mir[callsite.bb]
            .statements.push(cast_stmt);

        Operand::Consume(cast_tmp)
    }

    fn make_call_args(&self, args: Vec<Operand<'tcx>>,
                      callsite: &CallSite<'tcx>, caller_mir: &mut Mir<'tcx>) -> Vec<Operand<'tcx>> {
        let tcx = self.tcx;
        // FIXME: Analysis of the usage of the arguments to avoid
        // unnecessary temporaries.
        args.into_iter().map(|a| {
            if let Operand::Consume(Lvalue::Temp(_)) = a {
                // Reuse the operand if it's a temporary already
                return a;
            }

            debug!("Creating temp for argument");
            // Otherwise, create a temporary for the arg
            let arg = Rvalue::Use(a);

            let ty = arg.ty(caller_mir, tcx).expect("arg has no type!");

            let arg_tmp = TempDecl { ty: ty };
            let arg_tmp = caller_mir.temp_decls.push(arg_tmp);
            let arg_tmp = Lvalue::Temp(arg_tmp);

            let stmt = Statement {
                source_info: callsite.location,
                kind: StatementKind::Assign(arg_tmp.clone(), arg)
            };
            caller_mir[callsite.bb].statements.push(stmt);
            Operand::Consume(arg_tmp)
        }).collect()
    }
}

fn type_size_of<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, param_env: ty::ParameterEnvironment<'tcx>,
                          ty: Ty<'tcx>) -> Option<u64> {
    tcx.infer_ctxt(None, Some(param_env), traits::Reveal::All).enter(|infcx| {
        ty.layout(&infcx).ok().map(|layout| {
            layout.size(&tcx.data_layout).bytes()
        })
    })
}

/**
 * Integrator.
 *
 * Integrates blocks from the callee function into the calling function.
 * Updates block indices, references to locals and other control flow
 * stuff.
 */
struct Integrator<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    block_idx: usize,
    args: &'a [Operand<'tcx>],
    var_map: IndexVec<Var, Var>,
    tmp_map: IndexVec<Temp, Temp>,
    scope_map: IndexVec<VisibilityScope, VisibilityScope>,
    promoted_map: IndexVec<Promoted, Promoted>,
    callsite: CallSite<'tcx>,
    destination: Lvalue<'tcx>,
    return_block: BasicBlock,
    cleanup_block: Option<BasicBlock>,
    in_cleanup_block: bool,
}

impl<'a, 'tcx> Integrator<'a, 'tcx> {
    fn update_target(&self, tgt: BasicBlock) -> BasicBlock {
        let new = BasicBlock::new(tgt.index() + self.block_idx);
        debug!("Updating target `{:?}`, new: `{:?}`", tgt, new);
        new
    }

    fn update_span(&self, span: Span) -> Span {
        let cm = self.tcx.sess.codemap();
        if cm.is_valid_span(span) {
            span
        } else {
            self.callsite.location.span
        }
    }
}

impl<'a, 'tcx> MutVisitor<'tcx> for Integrator<'a, 'tcx> {
    fn visit_lvalue(&mut self,
                    lvalue: &mut Lvalue<'tcx>,
                    _ctxt: LvalueContext<'tcx>,
                    _location: Location) {
        match *lvalue {
            Lvalue::Var(ref mut var) => {
                if let Some(v) = self.var_map.get(*var).cloned() {
                    *var = v;
                }
            }
            Lvalue::Temp(ref mut tmp) => {
                if let Some(t) = self.tmp_map.get(*tmp).cloned() {
                    *tmp = t;
                }
            }
            Lvalue::ReturnPointer => {
                *lvalue = self.destination.clone();
            }
            Lvalue::Arg(arg) => {
                let idx = arg.index();
                if let Operand::Consume(ref lval) = self.args[idx] {
                    *lvalue = lval.clone();
                } else {
                    bug!("Arg operand `{:?}` is not an Lvalue use.", arg)
                }
            }
            _ => self.super_lvalue(lvalue, _ctxt, _location)
        }
    }

    fn visit_operand(&mut self, operand: &mut Operand<'tcx>, location: Location) {
        if let Operand::Consume(Lvalue::Arg(arg)) = *operand {
            let idx = arg.index();
            let new_arg = self.args[idx].clone();
            *operand = new_arg;
        } else {
            self.super_operand(operand, location);
        }
    }

    fn visit_basic_block_data(&mut self, block: BasicBlock, data: &mut BasicBlockData<'tcx>) {
        self.in_cleanup_block = data.is_cleanup;
        self.super_basic_block_data(block, data);
        self.in_cleanup_block = false;
    }

    fn visit_terminator_kind(&mut self, block: BasicBlock,
                             kind: &mut TerminatorKind<'tcx>, loc: Location) {
        self.super_terminator_kind(block, kind, loc);

        match *kind {
            TerminatorKind::Goto { ref mut target} => {
                *target = self.update_target(*target);
            }
            TerminatorKind::If { ref mut targets, .. } => {
                targets.0 = self.update_target(targets.0);
                targets.1 = self.update_target(targets.1);
            }
            TerminatorKind::Switch { ref mut targets, .. } |
            TerminatorKind::SwitchInt { ref mut targets, .. } => {
                for tgt in targets {
                    *tgt = self.update_target(*tgt);
                }
            }
            TerminatorKind::Drop { ref mut target, ref mut unwind, .. } |
            TerminatorKind::DropAndReplace { ref mut target, ref mut unwind, .. } => {
                *target = self.update_target(*target);
                if let Some(tgt) = *unwind {
                    *unwind = Some(self.update_target(tgt));
                } else if !self.in_cleanup_block {
                    // Unless this drop is in a cleanup block, add an unwind edge to
                    // the orignal call's cleanup block
                    *unwind = self.cleanup_block;
                }
            }
            TerminatorKind::Call { ref mut destination, ref mut cleanup, .. } => {
                if let Some((_, ref mut tgt)) = *destination {
                    *tgt = self.update_target(*tgt);
                }
                if let Some(tgt) = *cleanup {
                    *cleanup = Some(self.update_target(tgt));
                } else if !self.in_cleanup_block {
                    // Unless this call is in a cleanup block, add an unwind edge to
                    // the orignal call's cleanup block
                    *cleanup = self.cleanup_block;
                }
            }
            TerminatorKind::Assert { ref mut target, ref mut cleanup, .. } => {
                *target = self.update_target(*target);
                if let Some(tgt) = *cleanup {
                    *cleanup = Some(self.update_target(tgt));
                } else if !self.in_cleanup_block {
                    // Unless this assert is in a cleanup block, add an unwind edge to
                    // the orignal call's cleanup block
                    *cleanup = self.cleanup_block;
                }
            }
            TerminatorKind::Return => {
                *kind = TerminatorKind::Goto { target: self.return_block };
            }
            TerminatorKind::Resume => {
                if let Some(tgt) = self.cleanup_block {
                    *kind = TerminatorKind::Goto { target: tgt }
                }
            }
            TerminatorKind::Unreachable => { }
        }
    }

    fn visit_visibility_scope(&mut self, scope: &mut VisibilityScope) {
        *scope = self.scope_map[*scope];
    }

    fn visit_span(&mut self, span: &mut Span) {
        *span = self.update_span(*span);
    }

    fn visit_literal(&mut self, literal: &mut Literal<'tcx>, loc: Location) {
        if let Literal::Promoted { ref mut index } = *literal {
            if let Some(p) = self.promoted_map.get(*index).cloned() {
                *index = p;
            }
        } else {
            self.super_literal(literal, loc);
        }
    }
}