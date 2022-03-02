// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! This file contains functions related to codegenning MIR functions into gotoc

use crate::context::metadata::HarnessMetadata;
use crate::GotocCtx;
use cbmc::goto_program::{Expr, Stmt, Symbol};
use cbmc::InternString;
use rustc_ast::ast;
use rustc_ast::{Attribute, LitKind};
use rustc_middle::mir::{HasLocalDecls, Local};
use rustc_middle::ty::{self, Instance};
use std::collections::BTreeMap;
use std::convert::TryInto;
use std::iter::FromIterator;
use tracing::{debug, warn};

/// Utility to skip functions that can't currently be successfully codgenned.
impl<'tcx> GotocCtx<'tcx> {
    fn should_skip_current_fn(&self) -> bool {
        match self.current_fn().readable_name() {
            // https://github.com/model-checking/kani/issues/202
            "fmt::ArgumentV1::<'a>::as_usize" => true,
            // https://github.com/model-checking/kani/issues/204
            name if name.ends_with("__getit") => true,
            // https://github.com/model-checking/kani/issues/281
            name if name.starts_with("bridge::client") => true,
            // https://github.com/model-checking/kani/issues/282
            "bridge::closure::Closure::<'a, A, R>::call" => true,
            // Generators
            name if name.starts_with("<std::future::from_generator::GenFuture<T>") => true,
            name if name.contains("reusable_box::ReusableBoxFuture") => true,
            "tokio::sync::Semaphore::acquire_owned::{closure#0}" => true,
            _ => false,
        }
    }
}

/// Codegen MIR functions into gotoc
impl<'tcx> GotocCtx<'tcx> {
    fn codegen_declare_variables(&mut self) {
        let mir = self.current_fn().mir();
        let ldecls = mir.local_decls();
        ldecls.indices().for_each(|lc| {
            if Some(lc) == mir.spread_arg {
                // We have already added this local in the function prelude, so
                // skip adding it again here.
                return;
            }
            let base_name = self.codegen_var_base_name(&lc);
            let name = self.codegen_var_name(&lc);
            let ldata = &ldecls[lc];
            let t = self.monomorphize(ldata.ty);
            let t = self.codegen_ty(t);
            let loc = self.codegen_span(&ldata.source_info.span);
            let sym =
                Symbol::variable(name, base_name, t, self.codegen_span(&ldata.source_info.span))
                    .with_is_hidden(!ldata.is_user_variable());
            let sym_e = sym.to_expr();
            self.symbol_table.insert(sym);

            // Index 0 represents the return value, which does not need to be
            // declared in the first block
            if lc.index() < 1 || lc.index() > mir.arg_count {
                self.current_fn_mut().push_onto_block(Stmt::decl(sym_e, None, loc));
            }
        });
    }

    pub fn codegen_function(&mut self, instance: Instance<'tcx>) {
        self.set_current_fn(instance);
        let name = self.current_fn().name();
        let old_sym = self.symbol_table.lookup(&name).unwrap();
        if old_sym.is_function_definition() {
            warn!("Double codegen of {:?}", old_sym);
        } else if self.should_skip_current_fn() {
            debug!("Skipping function {}", self.current_fn().readable_name());
            let body = self.codegen_fatal_error(
                &GotocCtx::unsupported_msg(
                    &(String::from("The function ") + self.current_fn().readable_name()),
                    None,
                ),
                Some(self.current_fn().mir().span),
            );
            self.symbol_table.update_fn_declaration_with_definition(&name, body);
        } else {
            assert!(old_sym.is_function());
            let mir = self.current_fn().mir();
            self.print_instance(instance, mir);
            self.codegen_function_prelude();
            self.codegen_declare_variables();

            mir.basic_blocks().iter_enumerated().for_each(|(bb, bbd)| self.codegen_block(bb, bbd));

            let loc = self.codegen_span(&mir.span);
            let stmts = self.current_fn_mut().extract_block();
            let body = Stmt::block(stmts, loc);
            self.symbol_table.update_fn_declaration_with_definition(&name, body);

            self.handle_kanitool_attributes();
        }
        self.reset_current_fn();
    }

    /// MIR functions have a `spread_arg` field that specifies whether the
    /// final argument to the function is "spread" at the LLVM/codegen level
    /// from a tuple into its individual components. (Used for the "rust-
    /// call" ABI, necessary because dynamic trait closure cannot have an
    /// argument list in MIR that is both generic and variadic, so Rust
    /// allows a generic tuple).
    ///
    /// If `spread_arg` is Some, then the wrapped value is the local that is
    /// to be "spread"/untupled. However, the MIR function body itself expects
    /// the tuple instead of the individual components, so we need to generate
    /// a function prelude that _retuples_, that is, writes the components
    /// back to the tuple local for use in the body.
    ///
    /// See:
    /// https://rust-lang.zulipchat.com/#narrow/stream/182449-t-compiler.2Fhelp/topic/Determine.20untupled.20closure.20args.20from.20Instance.3F
    fn codegen_function_prelude(&mut self) {
        let mir = self.current_fn().mir();
        if mir.spread_arg.is_none() {
            // No special tuple argument, no work to be done.
            return;
        }
        let spread_arg = mir.spread_arg.unwrap();
        let spread_data = &mir.local_decls()[spread_arg];
        let loc = self.codegen_span(&spread_data.source_info.span);

        // Get the function signature from MIR, _before_ we untuple
        let fntyp = self.current_fn().instance().ty(self.tcx, ty::ParamEnv::reveal_all());
        let sig = match fntyp.kind() {
            ty::FnPtr(..) | ty::FnDef(..) => fntyp.fn_sig(self.tcx).skip_binder(),
            // Closures themselves will have their arguments already untupled,
            // see Zulip link above.
            ty::Closure(..) => unreachable!(
                "Unexpected `spread arg` set for closure, got: {:?}, {:?}",
                fntyp,
                self.current_fn().readable_name()
            ),
            _ => unreachable!(
                "Expected function type for `spread arg` prelude, got: {:?}, {:?}",
                fntyp,
                self.current_fn().readable_name()
            ),
        };

        // When we codegen the function signature elsewhere, we will codegen the untupled version.
        // We then marshall the arguments into a local variable holding the expected tuple.
        // For a function with args f(a: t1, b: t2, c: t3), the tuple type will look like
        // ```
        //    struct T {
        //        0: t1,
        //        1: t2,
        //        2: t3,
        // }
        // ```
        // For e.g., in the test `tupled_closure.rs`, the tuple type looks like:
        // ```
        // struct _8098103865751214180
        // {
        //    unsigned long int 1;
        //    unsigned char 0;
        //    struct _3159196586427472662 2;
        // };
        // ```
        // Note how the compiler has reordered the fields to improve packing.
        let tup_typ = self.codegen_ty(self.monomorphize(spread_data.ty));

        // We need to marshall the arguments into the tuple
        // The arguments themselves have been tacked onto the explicit function paramaters by
        // the code in `pub fn fn_typ(&mut self) -> Type {` in `typ.rs`.
        // By convention, they are given the names `spread<i>`.
        // For e.g., in the test `tupled_closure.rs`, the actual function looks like
        // ```
        // unsigned long int _RNvYNvCscgV8bIzQQb7_14tupled_closure1hINtNtNtCsaGHNm3cehi1_4core3ops8function2FnThjINtNtBH_6option6OptionNtNtNtBH_3num7nonzero12NonZeroUsizeEEE4callB4_(
        //        unsigned long int (*var_1)(unsigned char, unsigned long int, struct _3159196586427472662),
        //        unsigned char spread_2,
        //        unsigned long int spread_3,
        //        struct _3159196586427472662 spread_4) {
        //  struct _8098103865751214180 var_2={ .1=spread_3, .0=spread_2, .2=spread_4 };
        //  unsigned long int var_0=(_RNvCscgV8bIzQQb7_14tupled_closure1h)(var_2.0, var_2.1, var_2.2);
        //  return var_0;
        // }
        // ```

        let tupe = sig.inputs().last().unwrap();
        let args = match tupe.kind() {
            ty::Tuple(substs) => *substs,
            _ => unreachable!("a function's spread argument must be a tuple"),
        };
        let starting_idx = sig.inputs().len();
        let marshalled_tuple_fields =
            BTreeMap::from_iter(args.iter().enumerate().map(|(arg_i, arg_t)| {
                // The components come at the end, so offset by the untupled length.
                // This follows the naming convention defined in `typ.rs`.
                let lc = Local::from_usize(arg_i + starting_idx);
                let (name, base_name) = self.codegen_spread_arg_name(&lc);
                let sym = Symbol::variable(name, base_name, self.codegen_ty(arg_t), loc.clone())
                    .with_is_hidden(false);
                // The spread arguments are additional function paramaters that are patched in
                // They are to the function signature added in the `fn_typ` function.
                // But they were never added to the symbol table, which we currently do here.
                // https://github.com/model-checking/kani/issues/686 to track a better solution.
                self.symbol_table.insert(sym.clone());
                // As discussed above, fields are named like `0: t1`.
                // Follow that pattern for the marshalled data.
                // name:value map is resilliant to rustc reordering fields (see above)
                (arg_i.to_string().intern(), sym.to_expr())
            }));
        let marshalled_tuple_value =
            Expr::struct_expr(tup_typ.clone(), marshalled_tuple_fields, &self.symbol_table)
                .with_location(loc.clone());
        self.declare_variable(
            self.codegen_var_name(&spread_arg),
            self.codegen_var_base_name(&spread_arg),
            tup_typ,
            Some(marshalled_tuple_value),
            loc,
        );
    }

    pub fn declare_function(&mut self, instance: Instance<'tcx>) {
        debug!("declaring {}; {:?}", instance, instance);
        self.set_current_fn(instance);
        self.ensure(&self.current_fn().name(), |ctx, fname| {
            let mir = ctx.current_fn().mir();
            Symbol::function(
                fname,
                ctx.fn_typ(),
                None,
                Some(ctx.current_fn().readable_name()),
                ctx.codegen_span(&mir.span),
            )
        });
        self.reset_current_fn();
    }

    /// This updates the goto context with any information that should be accumulated from a function's
    /// attributes.
    ///
    /// Currently, this is only proof harness annotations.
    /// i.e. `#[kani::proof]` (which kani_macros translates to `#[kanitool::proof]` for us to handle here)
    fn handle_kanitool_attributes(&mut self) {
        let instance = self.current_fn().instance();

        // Vectors for storing instances of each attribute type,
        // TODO: This can be modifed to use Enums when more options are provided
        let mut attribute_vector = vec![];
        let mut proof_attribute_vector = vec![];

        // Loop through instances to get all "kanitool::x" attribute strings
        for attr in self.tcx.get_attrs(instance.def_id()) {
            // Get the string the appears after "kanitool::" in each attribute string.
            // Ex - "proof" | "unwind" etc.
            if let Some(attribute_string) = kanitool_attr_name(attr).as_deref() {
                // Push to proof vector
                if attribute_string == "proof" {
                    proof_attribute_vector.push(attr);
                }
                // Push to attribute vector that can be expanded to a map when more options become available
                else {
                    attribute_vector.push((attribute_string.to_string(), attr));
                }
            }
        }

        // In the case when there's only one proof attribute (correct behavior), create harness and modify it
        // depending on each subsequent attribute that's being called by the user.
        if proof_attribute_vector.len() == 1 {
            let mut harness_metadata = self.handle_kanitool_proof();
            if attribute_vector.len() > 0 {
                // loop through all subsequent attributes
                for attribute_tuple in attribute_vector.iter() {
                    // match with "unwind" attribute and provide the harness for modification
                    match attribute_tuple.0.as_str() {
                        "unwind" => {
                            self.handle_kanitool_unwind(attribute_tuple.1, &mut harness_metadata)
                        }
                        _ => {}
                    }
                }
            }
            // self.proof_harnesses contains the final metadata that's to be parsed
            self.proof_harnesses.push(harness_metadata);
        }
        // User error handling for when there's more than one proof attribute being called
        else if proof_attribute_vector.len() > 1 {
            self.tcx
                .sess
                .span_err(proof_attribute_vector[0].span, "Only one Proof Attribute allowed");
        }
        // User error handling for when there's an attribute being called without #kani::tool
        else if proof_attribute_vector.len() == 0 && attribute_vector.len() > 0 {
            self.tcx.sess.span_err(
                attribute_vector[0].1.span,
                "Please use '#kani[proof]' above the annotation",
            );
        } else {
        }
    }

    /// Update `self` (the goto context) to add the current function as a listed proof harness
    fn handle_kanitool_proof(&mut self) -> HarnessMetadata {
        let current_fn = self.current_fn();
        let pretty_name = current_fn.readable_name().to_owned();
        let mangled_name = current_fn.name();
        let loc = self.codegen_span(&current_fn.mir().span);

        let harness = HarnessMetadata {
            pretty_name,
            mangled_name,
            original_file: loc.filename().unwrap(),
            original_line: loc.line().unwrap().to_string(),
            unwind_value: None,
        };

        harness
    }

    /// Unwind strings of the format 'unwind(x)' and the mangled names are to be parsed for the value 'x'
    fn handle_kanitool_unwind(&mut self, attr: &Attribute, harness: &mut HarnessMetadata) {
        // Check if some unwind value doesnt already exist
        if harness.unwind_value.is_none() {
            // Get Attribute value and if it's not none, assign it to the metadata
            if let Some(integer_value) = extract_integer_argument(attr) {
                // Convert the extracted u128 and convert to u32
                assert!(
                    integer_value < u32::MAX.into(),
                    "Value above maximum permitted value - u32::MAX"
                );
                harness.unwind_value = Some(integer_value.try_into().unwrap());
            } else {
                // Show an Error if there is no integer value assigned to the integer or there's too many values assigned to the annotation
                self.tcx
                    .sess
                    .span_err(attr.span, "Exactly one Unwind Argument as Integer accepted");
            }
        } else {
            // User warning for when there's more than one unwind attribute, in this case, only the first value will be
            self.tcx.sess.span_err(attr.span, "Use only one Unwind Annotation per Harness");
        }
    }
}

/// If the attribute is named `kanitool::name`, this extracts `name`
fn kanitool_attr_name(attr: &ast::Attribute) -> Option<String> {
    match &attr.kind {
        ast::AttrKind::Normal(ast::AttrItem { path: ast::Path { segments, .. }, .. }, _)
            if (!segments.is_empty()) && segments[0].ident.as_str() == "kanitool" =>
        {
            let mut new_string = String::new();
            for index in 1..segments.len() {
                new_string.push_str(segments[index].ident.as_str());
            }
            Some(new_string)
        }
        _ => None,
    }
}

/// Extracts the integer value argument from the any attribute provided
fn extract_integer_argument(attr: &Attribute) -> Option<u128> {
    // Vector of meta items , that contain metadata about the annotation
    let attr_args = attr.meta_item_list().map(|x| x.to_vec())?;
    // Only accept unwind attributes with one integer value as argument
    if attr_args.len() == 1 {
        // Returns the integer value that's the argument for the unwind
        let x = attr_args[0].literal()?;
        match x.kind {
            LitKind::Int(y, ..) => Some(y),
            _ => None,
        }
    }
    // Return none if there are no unwind attributes or if there's too many unwind attributes
    else {
        None
    }
}
