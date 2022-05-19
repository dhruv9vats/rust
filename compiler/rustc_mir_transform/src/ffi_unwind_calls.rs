use rustc_hir::def::DefKind;
use rustc_hir::def_id::{LocalDefId, LOCAL_CRATE};
use rustc_middle::mir::*;
use rustc_middle::ty::layout;
use rustc_middle::ty::query::Providers;
use rustc_middle::ty::{self, TyCtxt};
use rustc_session::lint::builtin::FFI_UNWIND_CALLS;
use rustc_target::spec::abi::Abi;
use rustc_target::spec::PanicStrategy;

fn abi_can_unwind(abi: Abi) -> bool {
    use Abi::*;
    match abi {
        C { unwind }
        | System { unwind }
        | Cdecl { unwind }
        | Stdcall { unwind }
        | Fastcall { unwind }
        | Vectorcall { unwind }
        | Thiscall { unwind }
        | Aapcs { unwind }
        | Win64 { unwind }
        | SysV64 { unwind } => unwind,
        PtxKernel
        | Msp430Interrupt
        | X86Interrupt
        | AmdGpuKernel
        | EfiApi
        | AvrInterrupt
        | AvrNonBlockingInterrupt
        | CCmseNonSecureCall
        | Wasm
        | RustIntrinsic
        | PlatformIntrinsic
        | Unadjusted => false,
        Rust | RustCall | RustCold => true,
    }
}

// Check if the body of this def_id can possibly leak a foreign unwind into Rust code.
fn has_ffi_unwind_calls(tcx: TyCtxt<'_>, local_def_id: LocalDefId) -> bool {
    debug!("has_ffi_unwind_calls({local_def_id:?})");

    // Only perform check on functions because constants cannot call FFI functions.
    let def_id = local_def_id.to_def_id();
    let kind = tcx.def_kind(def_id);
    let is_function = match kind {
        DefKind::Fn | DefKind::AssocFn | DefKind::Ctor(..) => true,
        _ => tcx.is_closure(def_id),
    };
    if !is_function {
        return false;
    }

    let body = &*tcx.mir_built(ty::WithOptConstParam::unknown(local_def_id)).borrow();

    let body_ty = tcx.type_of(def_id);
    let body_abi = match body_ty.kind() {
        ty::FnDef(..) => body_ty.fn_sig(tcx).abi(),
        ty::Closure(..) => Abi::RustCall,
        ty::Generator(..) => Abi::Rust,
        _ => span_bug!(body.span, "unexpected body ty: {:?}", body_ty),
    };
    let body_can_unwind = layout::fn_can_unwind(tcx, Some(def_id), body_abi);

    // Foreign unwinds cannot leak past functions that themselves cannot unwind.
    if !body_can_unwind {
        return false;
    }

    let mut tainted = false;

    for block in body.basic_blocks() {
        if block.is_cleanup {
            continue;
        }
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { func, .. } = &terminator.kind else { continue };

        let ty = func.ty(body, tcx);
        let sig = ty.fn_sig(tcx);

        // Rust calls cannot themselves create foreign unwinds.
        if let Abi::Rust | Abi::RustCall | Abi::RustCold = sig.abi() {
            continue;
        };

        let fn_def_id = match ty.kind() {
            ty::FnPtr(_) => None,
            &ty::FnDef(def_id, _) => {
                // Rust calls cannot themselves create foreign unwinds.
                if !tcx.is_foreign_item(def_id) {
                    continue;
                }
                Some(def_id)
            }
            _ => bug!("invalid callee of type {:?}", ty),
        };

        if layout::fn_can_unwind(tcx, fn_def_id, sig.abi()) && abi_can_unwind(sig.abi()) {
            // We have detected a call that can possibly leak foreign unwind.
            //
            // Because the function body itself can unwind, we are not aborting this function call
            // upon unwind, so this call can possibly leak foreign unwind into Rust code if the
            // panic runtime linked is panic-abort.

            let lint_root = body.source_scopes[terminator.source_info.scope]
                .local_data
                .as_ref()
                .assert_crate_local()
                .lint_root;
            let span = terminator.source_info.span;

            tcx.struct_span_lint_hir(FFI_UNWIND_CALLS, lint_root, span, |lint| {
                let msg = match fn_def_id {
                    Some(_) => "call to foreign function with FFI-unwind ABI",
                    None => "call to function pointer with FFI-unwind ABI",
                };
                let mut db = lint.build(msg);
                db.span_label(span, msg);
                db.emit();
            });

            tainted = true;
        }
    }

    tainted
}

fn required_panic_strategy(tcx: TyCtxt<'_>, (): ()) -> Option<PanicStrategy> {
    if tcx.is_panic_runtime(LOCAL_CRATE) {
        return Some(tcx.sess.panic_strategy());
    }

    if tcx.sess.panic_strategy() == PanicStrategy::Abort {
        return Some(PanicStrategy::Abort);
    }

    for def_id in tcx.hir().body_owners() {
        if tcx.has_ffi_unwind_calls(def_id) {
            return Some(PanicStrategy::Unwind);
        }
    }

    // This crate can be linked with either runtime.
    None
}

pub(crate) fn provide(providers: &mut Providers) {
    *providers = Providers { has_ffi_unwind_calls, required_panic_strategy, ..*providers };
}
