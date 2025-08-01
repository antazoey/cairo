use std::collections::BTreeSet;
use std::fmt::Write;
use std::hash::Hash;
use std::sync::Arc;
use std::{mem, panic, vec};

use cairo_lang_debug::DebugWithDb;
use cairo_lang_defs::ids::{
    FunctionTitleId, GenericKind, GenericParamId, ImplAliasId, ImplConstantDefId,
    ImplConstantDefLongId, ImplDefId, ImplFunctionId, ImplFunctionLongId, ImplImplDefId,
    ImplImplDefLongId, ImplItemId, ImplTypeDefId, ImplTypeDefLongId, LanguageElementId,
    LookupItemId, ModuleId, ModuleItemId, NamedLanguageElementId, NamedLanguageElementLongId,
    TopLevelLanguageElementId, TraitConstantId, TraitFunctionId, TraitId, TraitImplId, TraitTypeId,
    UseId,
};
use cairo_lang_diagnostics::{
    DiagnosticAdded, Diagnostics, DiagnosticsBuilder, Maybe, ToMaybe, skip_diagnostic,
};
use cairo_lang_filesystem::ids::{SmolStrId, UnstableSalsaId};
use cairo_lang_proc_macros::{DebugWithDb, SemanticObject};
use cairo_lang_syntax as syntax;
use cairo_lang_syntax::node::ast::OptionTypeClause;
use cairo_lang_utils::ordered_hash_map::OrderedHashMap;
use cairo_lang_utils::ordered_hash_set::OrderedHashSet;
use cairo_lang_utils::unordered_hash_map::UnorderedHashMap;
use cairo_lang_utils::{Intern, define_short_id, extract_matches};
use itertools::{Itertools, chain, izip};
use smol_str::SmolStr;
use syntax::attribute::structured::{Attribute, AttributeListStructurize};
use syntax::node::ast::{self, GenericArg, ImplItem, MaybeImplBody, OptionReturnTypeClause};
use syntax::node::db::SyntaxGroup;
use syntax::node::helpers::OptionWrappedGenericParamListHelper;
use syntax::node::ids::SyntaxStablePtrId;
use syntax::node::{Terminal, TypedStablePtr, TypedSyntaxNode};

use super::constant::{
    ConstValue, ConstValueId, ConstantData, ImplConstantId, constant_semantic_data_cycle_helper,
    constant_semantic_data_helper,
};
use super::enm::SemanticEnumEx;
use super::feature_kind::{FeatureKind, HasFeatureKind};
use super::function_with_body::{FunctionBody, FunctionBodyData, get_inline_config};
use super::functions::{
    FunctionDeclarationData, GenericFunctionId, ImplGenericFunctionId, InlineConfiguration,
    forbid_inline_always_with_impl_generic_param,
};
use super::generics::{
    GenericArgumentHead, GenericParamImpl, GenericParamsData, fmt_generic_args,
    generic_params_to_args, semantic_generic_params,
};
use super::impl_alias::{
    ImplAliasData, impl_alias_generic_params_data_helper, impl_alias_semantic_data_cycle_helper,
    impl_alias_semantic_data_helper,
};
use super::trt::{
    ConcreteTraitConstantId, ConcreteTraitGenericFunctionId, ConcreteTraitGenericFunctionLongId,
    ConcreteTraitImplId,
};
use super::type_aliases::{
    TypeAliasData, type_alias_generic_params_data_helper, type_alias_semantic_data_cycle_helper,
    type_alias_semantic_data_helper,
};
use super::visibility::peek_visible_in;
use super::{TraitOrImplContext, resolve_trait_path};
use crate::corelib::{concrete_destruct_trait, concrete_drop_trait, core_crate};
use crate::db::{SemanticGroup, SemanticGroupData, get_resolver_data_options};
use crate::diagnostic::SemanticDiagnosticKind::{self, *};
use crate::diagnostic::{NotFoundItemType, SemanticDiagnostics, SemanticDiagnosticsBuilder};
use crate::expr::compute::{ComputationContext, ContextFunction, Environment, compute_root_expr};
use crate::expr::fmt::CountingWriter;
use crate::expr::inference::canonic::ResultNoErrEx;
use crate::expr::inference::conform::InferenceConform;
use crate::expr::inference::infers::InferenceEmbeddings;
use crate::expr::inference::solver::{Ambiguity, SolutionSet, enrich_lookup_context_with_ty};
use crate::expr::inference::{
    ImplVarId, ImplVarTraitItemMappings, Inference, InferenceError, InferenceId,
};
use crate::items::function_with_body::get_implicit_precedence;
use crate::items::functions::ImplicitPrecedence;
use crate::items::us::SemanticUseEx;
use crate::resolve::{ResolvedConcreteItem, ResolvedGenericItem, Resolver, ResolverData};
use crate::substitution::{GenericSubstitution, SemanticRewriter};
use crate::types::{ImplTypeId, add_type_based_diagnostics, get_impl_at_context, resolve_type};
use crate::{
    Arenas, ConcreteFunction, ConcreteTraitId, ConcreteTraitLongId, FunctionId, FunctionLongId,
    GenericArgumentId, GenericParam, Mutability, SemanticDiagnostic, TypeId, TypeLongId, semantic,
    semantic_object_for_id,
};

#[cfg(test)]
#[path = "imp_test.rs"]
mod test;

#[derive(Clone, Debug, Hash, PartialEq, Eq, SemanticObject)]
pub struct ConcreteImplLongId<'db> {
    pub impl_def_id: ImplDefId<'db>,
    pub generic_args: Vec<GenericArgumentId<'db>>,
}
define_short_id!(
    ConcreteImplId,
    ConcreteImplLongId<'db>,
    SemanticGroup,
    lookup_intern_concrete_impl,
    intern_concrete_impl
);
semantic_object_for_id!(
    ConcreteImplId<'a>,
    lookup_intern_concrete_impl,
    intern_concrete_impl,
    ConcreteImplLongId<'a>
);
impl<'db> DebugWithDb<'db> for ConcreteImplLongId<'db> {
    type Db = dyn SemanticGroup;

    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        db: &'db (dyn SemanticGroup + 'static),
    ) -> std::fmt::Result {
        let mut f = CountingWriter::new(f);
        write!(f, "{}", self.impl_def_id.full_path(db))?;
        fmt_generic_args(&self.generic_args, &mut f, db)
    }
}
impl<'db> ConcreteImplId<'db> {
    pub fn impl_def_id(&self, db: &'db dyn SemanticGroup) -> ImplDefId<'db> {
        self.long(db).impl_def_id
    }
    pub fn get_impl_function(
        &self,
        db: &'db dyn SemanticGroup,
        function: TraitFunctionId<'db>,
    ) -> Maybe<Option<ImplFunctionId<'db>>> {
        db.impl_function_by_trait_function(self.impl_def_id(db), function)
    }
    pub fn name(&self, db: &dyn SemanticGroup) -> SmolStr {
        self.impl_def_id(db).name(db)
    }
    pub fn full_path(&self, db: &dyn SemanticGroup) -> String {
        format!("{:?}", self.debug(db.elongate()))
    }
    pub fn substitution(&self, db: &'db dyn SemanticGroup) -> Maybe<GenericSubstitution<'db>> {
        Ok(GenericSubstitution::from_impl(ImplLongId::Concrete(*self).intern(db)).concat(
            GenericSubstitution::new(
                &db.impl_def_generic_params(self.impl_def_id(db))?,
                &self.long(db).generic_args,
            ),
        ))
    }
    /// Returns true if the `impl` does not depend on any generics.
    pub fn is_fully_concrete(&self, db: &dyn SemanticGroup) -> bool {
        self.long(db)
            .generic_args
            .iter()
            .all(|generic_argument_id| generic_argument_id.is_fully_concrete(db))
    }
    /// Returns true if the `impl` does not depend on impl or type variables.
    pub fn is_var_free(&self, db: &dyn SemanticGroup) -> bool {
        self.long(db)
            .generic_args
            .iter()
            .all(|generic_argument_id| generic_argument_id.is_var_free(db))
    }
}

/// Represents a "callee" impl that can be referred to in the code.
/// Traits should be resolved to this.
#[derive(Clone, Debug, Hash, PartialEq, Eq, SemanticObject)]
pub enum ImplLongId<'db> {
    Concrete(ConcreteImplId<'db>),
    GenericParameter(GenericParamId<'db>),
    ImplVar(ImplVarId<'db>),
    ImplImpl(ImplImplId<'db>),
    SelfImpl(ConcreteTraitId<'db>),
    GeneratedImpl(GeneratedImplId<'db>),
}
impl<'db> ImplLongId<'db> {
    /// Returns the [ImplHead] of an impl if available.
    pub fn head(&self, db: &'db dyn SemanticGroup) -> Option<ImplHead<'db>> {
        Some(match self {
            ImplLongId::Concrete(concrete) => ImplHead::Concrete(concrete.impl_def_id(db)),
            ImplLongId::GenericParameter(_)
            | ImplLongId::ImplVar(_)
            | ImplLongId::ImplImpl(_)
            | ImplLongId::SelfImpl(_)
            | ImplLongId::GeneratedImpl(_) => {
                return None;
            }
        })
    }
    pub fn name(&self, db: &dyn SemanticGroup) -> SmolStr {
        match self {
            ImplLongId::Concrete(concrete_impl) => concrete_impl.name(db),
            ImplLongId::GenericParameter(generic_param_impl) => {
                generic_param_impl.name(db).unwrap_or_else(|| "_".into())
            }
            ImplLongId::ImplVar(var) => {
                format!("ImplVar({})", var.concrete_trait_id(db).full_path(db)).into()
            }
            ImplLongId::ImplImpl(impl_impl) => format!(
                "{}::{}",
                impl_impl.impl_id().name(db),
                db.impl_impl_concrete_trait(*impl_impl)
                    .map(|trait_impl| trait_impl.full_path(db))
                    .unwrap_or_else(|_| "_".into())
            )
            .into(),
            ImplLongId::SelfImpl(trait_impl) => trait_impl.name(db),
            ImplLongId::GeneratedImpl(generated_impl) => {
                format!("{:?}", generated_impl.debug(db.elongate())).into()
            }
        }
    }
    pub fn format(&self, db: &dyn SemanticGroup) -> String {
        match self {
            ImplLongId::Concrete(concrete_impl) => {
                format!("{:?}", concrete_impl.debug(db.elongate()))
            }
            ImplLongId::GenericParameter(generic_param_impl) => generic_param_impl.format(db),
            ImplLongId::ImplVar(var) => format!("{var:?}"),
            ImplLongId::ImplImpl(impl_impl) => format!("{:?}", impl_impl.debug(db.elongate())),
            ImplLongId::SelfImpl(concrete_trait_id) => {
                format!("{:?}", concrete_trait_id.debug(db.elongate()))
            }
            ImplLongId::GeneratedImpl(generated_impl) => {
                format!("{:?}", generated_impl.debug(db.elongate()))
            }
        }
    }

    /// Returns true if the `impl` does not depend on impl or type variables.
    pub fn is_var_free(&self, db: &dyn SemanticGroup) -> bool {
        match self {
            ImplLongId::Concrete(concrete_impl_id) => concrete_impl_id.is_var_free(db),
            ImplLongId::SelfImpl(concrete_trait_id) => concrete_trait_id.is_var_free(db),
            ImplLongId::GenericParameter(_) => true,
            ImplLongId::ImplVar(_) => false,
            ImplLongId::ImplImpl(impl_impl) => impl_impl.impl_id().is_var_free(db),
            ImplLongId::GeneratedImpl(generated_impl) => {
                generated_impl.concrete_trait(db).is_var_free(db)
                    && generated_impl
                        .long(db)
                        .impl_items
                        .0
                        .values()
                        .all(|type_id| type_id.is_var_free(db))
            }
        }
    }

    /// Returns true if the `impl` does not depend on any generics.
    pub fn is_fully_concrete(&self, db: &dyn SemanticGroup) -> bool {
        match self {
            ImplLongId::Concrete(concrete_impl_id) => concrete_impl_id.is_fully_concrete(db),
            ImplLongId::GenericParameter(_) => false,
            ImplLongId::ImplVar(_) => false,
            ImplLongId::ImplImpl(_) | ImplLongId::SelfImpl(_) => false,
            ImplLongId::GeneratedImpl(generated_impl) => {
                generated_impl.concrete_trait(db).is_fully_concrete(db)
                    && generated_impl
                        .long(db)
                        .impl_items
                        .0
                        .values()
                        .all(|type_id| type_id.is_fully_concrete(db))
            }
        }
    }
}
impl<'db> DebugWithDb<'db> for ImplLongId<'db> {
    type Db = dyn SemanticGroup;

    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        db: &'db (dyn SemanticGroup + 'static),
    ) -> std::fmt::Result {
        match self {
            ImplLongId::Concrete(concrete_impl_id) => {
                write!(f, "{:?}", concrete_impl_id.debug(db))
            }
            ImplLongId::GenericParameter(param) => write!(f, "{}", param.debug_name(db)),
            ImplLongId::ImplVar(var) => write!(f, "?{}", var.long(db).id.0),
            ImplLongId::ImplImpl(impl_impl) => write!(f, "{:?}", impl_impl.debug(db)),
            ImplLongId::SelfImpl(trait_impl) => write!(f, "{:?}", trait_impl.debug(db)),
            ImplLongId::GeneratedImpl(generated_impl) => {
                write!(f, "{:?}", generated_impl.debug(db))
            }
        }
    }
}

define_short_id!(ImplId, ImplLongId<'db>, SemanticGroup, lookup_intern_impl, intern_impl);
semantic_object_for_id!(ImplId<'a>, lookup_intern_impl, intern_impl, ImplLongId<'a>);
impl<'db> ImplId<'db> {
    pub fn concrete_trait(&self, db: &'db dyn SemanticGroup) -> Maybe<ConcreteTraitId<'db>> {
        db.impl_concrete_trait(*self)
    }

    /// Returns true if the `impl` does not depend on any generics.
    pub fn is_fully_concrete(&self, db: &dyn SemanticGroup) -> bool {
        db.priv_impl_is_fully_concrete(*self)
    }

    /// Returns true if the `impl` does not depend on impl or type variables.
    pub fn is_var_free(&self, db: &dyn SemanticGroup) -> bool {
        db.priv_impl_is_var_free(*self)
    }

    /// Returns the [ImplHead] of an impl if available.
    pub fn head(&self, db: &'db dyn SemanticGroup) -> Option<ImplHead<'db>> {
        self.long(db).head(db)
    }

    /// Returns the name of the impl.
    pub fn name(&self, db: &dyn SemanticGroup) -> SmolStr {
        self.long(db).name(db)
    }

    pub fn format(&self, db: &dyn SemanticGroup) -> String {
        self.long(db).format(db)
    }
}

define_short_id!(
    GeneratedImplId,
    GeneratedImplLongId<'db>,
    SemanticGroup,
    lookup_intern_generated_impl,
    intern_generated_impl
);
semantic_object_for_id!(
    GeneratedImplId<'a>,
    lookup_intern_generated_impl,
    intern_generated_impl,
    GeneratedImplLongId<'a>
);

impl<'db> GeneratedImplId<'db> {
    pub fn concrete_trait(self, db: &'db dyn SemanticGroup) -> ConcreteTraitId<'db> {
        db.lookup_intern_generated_impl(self).concrete_trait
    }

    pub fn trait_id(&self, db: &'db dyn SemanticGroup) -> TraitId<'db> {
        self.concrete_trait(db).trait_id(db)
    }
}

/// An impl that is generated by the compiler for a specific trait.
/// There can be only one such impl per concrete trait as otherwise there would be a
/// MultipleImplsFound ambiguity.
#[derive(Clone, Debug, Hash, PartialEq, Eq, SemanticObject)]
pub struct GeneratedImplLongId<'db> {
    pub concrete_trait: ConcreteTraitId<'db>,
    /// The generic params required for the impl. Typically impls and negative impls.
    /// We save the params so that we can validate negative impls.
    pub generic_params: Vec<GenericParam<'db>>,
    pub impl_items: GeneratedImplItems<'db>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, SemanticObject)]
pub struct GeneratedImplItems<'db>(pub OrderedHashMap<TraitTypeId<'db>, TypeId<'db>>);

pub enum GeneratedImplAssociatedTypes<'db> {
    /// The associated types are not yet resolved.
    Unresolved,
    /// The associated types are resolved.
    Resolved(OrderedHashMap<TraitTypeId<'db>, TypeId<'db>>),
}

impl<'db> DebugWithDb<'db> for GeneratedImplLongId<'db> {
    type Db = dyn SemanticGroup;

    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        db: &'db (dyn SemanticGroup + 'static),
    ) -> std::fmt::Result {
        write!(f, "Generated {:?}", self.concrete_trait.debug(db))
    }
}

/// An impl item of kind impl.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, SemanticObject, salsa::Update)]
pub struct ImplImplId<'db> {
    /// The impl the item impl is in.
    impl_id: ImplId<'db>,
    /// The trait impl this impl impl "implements".
    trait_impl_id: TraitImplId<'db>,
}

impl<'db> ImplImplId<'db> {
    /// Creates a new impl impl id. For an impl impl of a concrete impl, asserts that the
    /// trait impl belongs to the same trait that the impl implements (panics if not).
    pub fn new(
        impl_id: ImplId<'db>,
        trait_impl_id: TraitImplId<'db>,
        db: &dyn SemanticGroup,
    ) -> Self {
        if let crate::items::imp::ImplLongId::Concrete(concrete_impl) = impl_id.long(db) {
            let impl_def_id = concrete_impl.impl_def_id(db);
            assert_eq!(Ok(trait_impl_id.trait_id(db)), db.impl_def_trait(impl_def_id));
        }

        ImplImplId { impl_id, trait_impl_id }
    }
    pub fn impl_id(&self) -> ImplId<'db> {
        self.impl_id
    }
    pub fn trait_impl_id(&self) -> TraitImplId<'db> {
        self.trait_impl_id
    }

    pub fn concrete_trait_impl_id(
        &self,
        db: &'db dyn SemanticGroup,
    ) -> Maybe<ConcreteTraitImplId<'db>> {
        Ok(ConcreteTraitImplId::new_from_data(
            db,
            self.impl_id.concrete_trait(db)?,
            self.trait_impl_id,
        ))
    }

    pub fn full_path(&self, db: &dyn SemanticGroup) -> String {
        format!("{:?}", self.debug(db.elongate()))
    }
}
impl<'db> DebugWithDb<'db> for ImplImplId<'db> {
    type Db = dyn SemanticGroup;

    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        db: &'db (dyn SemanticGroup + 'static),
    ) -> std::fmt::Result {
        write!(f, "{:?}::{}", self.impl_id.debug(db), self.trait_impl_id.name(db))
    }
}

impl<'db> UnstableSalsaId for ImplId<'db> {
    fn get_internal_id(&self) -> salsa::Id {
        self.as_intern_id()
    }
}

/// Head of an impl.
///
/// A non-param non-variable impl has a head, which represents the kind of the root node in its tree
/// representation. This is used for caching queries for fast lookups when the impl is not
/// completely inferred yet.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ImplHead<'db> {
    Concrete(ImplDefId<'db>),
}

// === Impl Declaration ===

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplDeclarationData<'db> {
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,
    generic_params: Vec<semantic::GenericParam<'db>>,
    /// The concrete trait this impl implements, or Err if cannot be resolved.
    concrete_trait: Maybe<ConcreteTraitId<'db>>,
    attributes: Vec<Attribute<'db>>,
    resolver_data: Arc<ResolverData<'db>>,
}

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_semantic_declaration_diagnostics].
pub fn impl_semantic_declaration_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_impl_declaration_data(impl_def_id).map(|data| data.diagnostics).unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_def_generic_params_data].
pub fn impl_def_generic_params_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<GenericParamsData<'db>> {
    let module_file_id = impl_def_id.module_file_id(db);
    let mut diagnostics = SemanticDiagnostics::default();

    let impl_ast = db.module_impl_by_id(impl_def_id)?.to_maybe()?;
    let inference_id =
        InferenceId::LookupItemGenerics(LookupItemId::ModuleItem(ModuleItemId::Impl(impl_def_id)));

    let mut resolver = Resolver::new(db, module_file_id, inference_id);
    resolver.set_feature_config(&impl_def_id, &impl_ast, &mut diagnostics);
    let generic_params = semantic_generic_params(
        db,
        &mut diagnostics,
        &mut resolver,
        module_file_id,
        &impl_ast.generic_params(db),
    );
    let inference = &mut resolver.inference();
    inference.finalize(&mut diagnostics, impl_ast.stable_ptr(db).untyped());
    let generic_params = inference.rewrite(generic_params).no_err();
    let resolver_data = Arc::new(resolver.data);
    Ok(GenericParamsData { generic_params, diagnostics: diagnostics.build(), resolver_data })
}

/// Query implementation of [crate::db::SemanticGroup::impl_def_generic_params].
pub fn impl_def_generic_params<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Vec<semantic::GenericParam<'db>>> {
    Ok(db.impl_def_generic_params_data(impl_def_id)?.generic_params)
}

/// Query implementation of [crate::db::SemanticGroup::impl_def_resolver_data].
pub fn impl_def_resolver_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    Ok(db.priv_impl_declaration_data(impl_def_id)?.resolver_data)
}

/// Trivial cycle handler for [crate::db::SemanticGroup::impl_def_resolver_data].
pub fn impl_def_resolver_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    // Forwarding (not as a query) cycle handling to `priv_impl_declaration_data` cycle handler.
    impl_def_resolver_data(db, impl_def_id)
}

/// Query implementation of [crate::db::SemanticGroup::impl_def_concrete_trait].
pub fn impl_def_concrete_trait<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<ConcreteTraitId<'db>> {
    db.priv_impl_declaration_data(impl_def_id)?.concrete_trait
}

/// Trivial cycle handler for [crate::db::SemanticGroup::impl_def_concrete_trait].
pub fn impl_def_concrete_trait_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<ConcreteTraitId<'db>> {
    // Forwarding (not as a query) cycle handling to `priv_impl_declaration_data` cycle handler.
    impl_def_concrete_trait(db, impl_def_id)
}

/// Query implementation of [crate::db::SemanticGroup::impl_def_substitution].
pub fn impl_def_substitution<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<GenericSubstitution<'db>>> {
    let params = db.impl_def_generic_params(impl_def_id)?;
    let generic_args = generic_params_to_args(&params, db);
    Ok(Arc::new(ConcreteImplLongId { impl_def_id, generic_args }.intern(db).substitution(db)?))
}

/// Query implementation of [crate::db::SemanticGroup::impl_def_attributes].
pub fn impl_def_attributes<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Vec<Attribute<'db>>> {
    Ok(db.priv_impl_declaration_data(impl_def_id)?.attributes)
}

/// Query implementation of [crate::db::SemanticGroup::impl_def_trait].
pub fn impl_def_trait<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<TraitId<'db>> {
    let module_file_id = impl_def_id.module_file_id(db);
    let mut diagnostics = SemanticDiagnostics::default();

    let impl_ast = db.module_impl_by_id(impl_def_id)?.to_maybe()?;
    let inference_id = InferenceId::ImplDefTrait(impl_def_id);

    let mut resolver = Resolver::new(db, module_file_id, inference_id);
    resolver.set_feature_config(&impl_def_id, &impl_ast, &mut diagnostics);

    let trait_path_syntax = impl_ast.trait_path(db);

    resolve_trait_path(db, &mut diagnostics, &mut resolver, &trait_path_syntax)
}

/// Query implementation of [crate::db::SemanticGroup::impl_concrete_trait].
pub fn impl_concrete_trait<'db>(
    db: &'db dyn SemanticGroup,
    impl_id: ImplId<'db>,
) -> Maybe<ConcreteTraitId<'db>> {
    match impl_id.long(db) {
        ImplLongId::Concrete(concrete_impl_id) => {
            let long_impl = concrete_impl_id.long(db);
            let substitution = GenericSubstitution::new(
                &db.impl_def_generic_params(long_impl.impl_def_id)?,
                &long_impl.generic_args,
            );

            let impl_concrete_trait_id = db.impl_def_concrete_trait(long_impl.impl_def_id)?;
            substitution.substitute(db, impl_concrete_trait_id)
        }
        ImplLongId::GenericParameter(param) => {
            let param_impl =
                extract_matches!(db.generic_param_semantic(*param)?, GenericParam::Impl);
            param_impl.concrete_trait
        }
        ImplLongId::ImplVar(var) => Ok(var.long(db).concrete_trait_id),
        ImplLongId::ImplImpl(impl_impl) => db.impl_impl_concrete_trait(*impl_impl),
        ImplLongId::SelfImpl(concrete_trait_id) => Ok(*concrete_trait_id),
        ImplLongId::GeneratedImpl(generated_impl) => Ok(generated_impl.concrete_trait(db)),
    }
}

// --- Computation ---

/// Cycle handling for [crate::db::SemanticGroup::priv_impl_declaration_data].
pub fn priv_impl_declaration_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<ImplDeclarationData<'db>> {
    priv_impl_declaration_data_inner(db, impl_def_id, false)
}

/// Query implementation of [crate::db::SemanticGroup::priv_impl_declaration_data].
pub fn priv_impl_declaration_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<ImplDeclarationData<'db>> {
    priv_impl_declaration_data_inner(db, impl_def_id, true)
}

/// Shared code for the query and cycle handling.
/// The cycle handling logic needs to pass resolve_trait=false to prevent the cycle.
pub fn priv_impl_declaration_data_inner<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    resolve_trait: bool,
) -> Maybe<ImplDeclarationData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();

    // TODO(spapini): when code changes in a file, all the AST items change (as they contain a path
    // to the green root that changes. Once ASTs are rooted on items, use a selector that picks only
    // the item instead of all the module data.
    let impl_ast = db.module_impl_by_id(impl_def_id)?.to_maybe()?;
    let inference_id = InferenceId::LookupItemDeclaration(LookupItemId::ModuleItem(
        ModuleItemId::Impl(impl_def_id),
    ));

    // Generic params.
    let generic_params_data = db.impl_def_generic_params_data(impl_def_id)?;
    let generic_params = generic_params_data.generic_params;
    let mut resolver = Resolver::with_data(
        db,
        (*generic_params_data.resolver_data).clone_with_inference_id(db, inference_id),
    );
    resolver.set_feature_config(&impl_def_id, &impl_ast, &mut diagnostics);
    diagnostics.extend(generic_params_data.diagnostics);
    let trait_path_syntax = impl_ast.trait_path(db);

    let concrete_trait = if resolve_trait {
        resolver
            .resolve_concrete_path(&mut diagnostics, &trait_path_syntax, NotFoundItemType::Trait)
            .and_then(|resolved_item| match resolved_item {
                ResolvedConcreteItem::Trait(id) | ResolvedConcreteItem::SelfTrait(id) => Ok(id),
                _ => Err(diagnostics
                    .report(trait_path_syntax.stable_ptr(db), SemanticDiagnosticKind::NotATrait)),
            })
    } else {
        Err(diagnostics.report(trait_path_syntax.stable_ptr(db), ImplRequirementCycle))
    };

    let info = db.core_info();

    // Check for reimplementation of compilers' Traits.
    if let Ok(concrete_trait) = concrete_trait {
        if [
            info.type_eq_trt,
            info.fn_trt,
            info.fn_once_trt,
            info.felt252_dict_value_trt,
            info.numeric_literal_trt,
            info.string_literal_trt,
        ]
        .contains(&concrete_trait.trait_id(db))
            && impl_def_id.parent_module(db).owning_crate(db) != core_crate(db)
        {
            diagnostics.report(
                trait_path_syntax.stable_ptr(db),
                CompilerTraitReImplementation { trait_id: concrete_trait.trait_id(db) },
            );
        }
    }

    // Check fully resolved.
    let inference = &mut resolver.inference();
    inference.finalize(&mut diagnostics, impl_ast.stable_ptr(db).untyped());

    let concrete_trait: Result<ConcreteTraitId<'_>, DiagnosticAdded> =
        inference.rewrite(concrete_trait).no_err();
    let generic_params: Vec<GenericParam<'_>> = inference.rewrite(generic_params).no_err();

    let attributes = impl_ast.attributes(db).structurize(db);
    let mut resolver_data = resolver.data;
    resolver_data.trait_or_impl_ctx = TraitOrImplContext::Impl(impl_def_id);
    Ok(ImplDeclarationData {
        diagnostics: diagnostics.build(),
        generic_params,
        concrete_trait,
        attributes,
        resolver_data: Arc::new(resolver_data),
    })
}

// === Impl Definition ===

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplDefinitionData<'db> {
    /// The diagnostics here are "flat" - that is, only the diagnostics found on the impl level
    /// itself, and don't include the diagnostics of its items. The reason it's this way is that
    /// computing the items' diagnostics require a query about their impl, forming a cycle of
    /// queries. Adding the items' diagnostics only after the whole computation breaks this cycle.
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,

    // AST maps.
    function_asts: OrderedHashMap<ImplFunctionId<'db>, ast::FunctionWithBody<'db>>,
    item_type_asts: Arc<OrderedHashMap<ImplTypeDefId<'db>, ast::ItemTypeAlias<'db>>>,
    item_constant_asts: Arc<OrderedHashMap<ImplConstantDefId<'db>, ast::ItemConstant<'db>>>,
    item_impl_asts: Arc<OrderedHashMap<ImplImplDefId<'db>, ast::ItemImplAlias<'db>>>,

    /// Mapping of item names to their meta data info. All the IDs should appear in one of the AST
    /// maps above.
    item_id_by_name: Arc<OrderedHashMap<SmolStrId<'db>, ImplItemInfo<'db>>>,

    /// Mapping of missing impl names item names to the trait id.
    implicit_impls_id_by_name: Arc<OrderedHashMap<SmolStrId<'db>, TraitImplId<'db>>>,
}

impl<'db> ImplDefinitionData<'db> {
    /// Retrieves impl item information by its name.
    pub fn get_impl_item_info(&self, item_name: SmolStrId<'db>) -> Option<ImplItemInfo<'db>> {
        self.item_id_by_name.get(&item_name).cloned()
    }
}
/// Stores metadata for a impl item, including its ID and feature kind.
#[derive(Clone, Debug, PartialEq, Eq, salsa::Update)]
pub struct ImplItemInfo<'db> {
    /// The unique identifier of the impl item.
    pub id: ImplItemId<'db>,
    /// The feature kind associated with this impl item.
    pub feature_kind: FeatureKind,
}

impl HasFeatureKind for ImplItemInfo<'_> {
    /// Returns the feature kind of this impl item.
    fn feature_kind(&self) -> &FeatureKind {
        &self.feature_kind
    }
}

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_semantic_definition_diagnostics].
pub fn impl_semantic_definition_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    let mut diagnostics = DiagnosticsBuilder::default();

    let Ok(data) = db.priv_impl_definition_data(impl_def_id) else {
        return Diagnostics::default();
    };

    // The diagnostics from `priv_impl_definition_data` are only the diagnostics from the impl
    // level. They should be enriched with the items' diagnostics.
    diagnostics.extend(data.diagnostics);
    for impl_function_id in data.function_asts.keys() {
        diagnostics.extend(db.impl_function_declaration_diagnostics(*impl_function_id));
        diagnostics.extend(db.impl_function_body_diagnostics(*impl_function_id));
    }
    for impl_item_type_id in data.item_type_asts.keys() {
        diagnostics.extend(db.impl_type_def_semantic_diagnostics(*impl_item_type_id));
        if let Ok(ty) = db.impl_type_def_resolved_type(*impl_item_type_id) {
            add_type_based_diagnostics(db, &mut diagnostics, ty, impl_item_type_id.stable_ptr(db));
        }
    }
    for impl_item_constant_id in data.item_constant_asts.keys() {
        diagnostics.extend(db.impl_constant_def_semantic_diagnostics(*impl_item_constant_id));
    }
    for impl_item_impl_id in data.item_impl_asts.keys() {
        diagnostics.extend(db.impl_impl_def_semantic_diagnostics(*impl_item_impl_id));
    }
    for implicit_impl_id in data.implicit_impls_id_by_name.values() {
        diagnostics
            .extend(db.implicit_impl_impl_semantic_diagnostics(impl_def_id, *implicit_impl_id));
    }
    // Diagnostics for special traits.
    if diagnostics.error_count == 0 {
        let concrete_trait =
            db.priv_impl_declaration_data(impl_def_id).unwrap().concrete_trait.unwrap();

        let trait_id = concrete_trait.trait_id(db);
        if trait_id == db.core_info().deref_trt {
            deref_impl_diagnostics(db, impl_def_id, concrete_trait, &mut diagnostics);
        }
    }
    diagnostics.build()
}

/// Represents a chain of dereferences.
#[derive(Clone, Debug, Eq, PartialEq, salsa::Update)]
pub struct DerefChain<'db> {
    pub derefs: Arc<Vec<DerefInfo<'db>>>,
}

/// Represents a single steps in a deref chain.
#[derive(Clone, Debug, Eq, PartialEq, salsa::Update)]
pub struct DerefInfo<'db> {
    /// The concrete `Deref::deref` or `MutDeref::mderef_mut` function.
    pub function_id: FunctionId<'db>,
    /// The mutability of the self argument of the deref function.
    pub self_mutability: Mutability,
    /// The target type of the deref function.
    pub target_ty: TypeId<'db>,
}

/// Cycle handling for  [crate::db::SemanticGroup::deref_chain].
pub fn deref_chain_cycle<'db>(
    _db: &dyn SemanticGroup,
    _input: SemanticGroupData,
    _ty: TypeId<'db>,
    _try_deref_mut: bool,
) -> Maybe<DerefChain<'db>> {
    // `SemanticDiagnosticKind::DerefCycle` will be reported by `deref_impl_diagnostics`.
    Maybe::Err(skip_diagnostic())
}

/// Query implementation of [crate::db::SemanticGroup::deref_chain].
pub fn deref_chain<'db>(
    db: &'db dyn SemanticGroup,
    ty: TypeId<'db>,
    try_deref_mut: bool,
) -> Maybe<DerefChain<'db>> {
    let mut opt_deref = None;
    if try_deref_mut {
        opt_deref = try_get_deref_func_and_target(db, ty, true)?;
    }
    let self_mutability = if opt_deref.is_some() {
        Mutability::Reference
    } else {
        opt_deref = try_get_deref_func_and_target(db, ty, false)?;
        Mutability::Immutable
    };

    let Some((function_id, target_ty)) = opt_deref else {
        return Ok(DerefChain { derefs: Arc::new(vec![]) });
    };

    let inner_chain = db.deref_chain(target_ty, false)?;

    Ok(DerefChain {
        derefs: Arc::new(
            chain!(
                [DerefInfo { function_id, target_ty, self_mutability }],
                inner_chain.derefs.iter().cloned()
            )
            .collect(),
        ),
    })
}

/// Tries to find the deref function and the target type for a given type and deref trait.
fn try_get_deref_func_and_target<'db>(
    db: &'db dyn SemanticGroup,
    ty: TypeId<'db>,
    is_mut_deref: bool,
) -> Result<Option<(FunctionId<'db>, TypeId<'db>)>, DiagnosticAdded> {
    let info = db.core_info();
    let (deref_trait_id, deref_method) = if is_mut_deref {
        (info.deref_mut_trt, info.deref_mut_fn)
    } else {
        (info.deref_trt, info.deref_fn)
    };

    let mut lookup_context = ImplLookupContext::new(deref_trait_id.module_file_id(db).0, vec![]);
    enrich_lookup_context_with_ty(db, ty, &mut lookup_context);
    let concrete_trait = ConcreteTraitLongId {
        trait_id: deref_trait_id,
        generic_args: vec![GenericArgumentId::Type(ty)],
    }
    .intern(db);
    let Ok(deref_impl) = get_impl_at_context(db, lookup_context, concrete_trait, None) else {
        return Ok(None);
    };
    let concrete_impl_id: ConcreteImplId<'db> = match deref_impl.long(db) {
        ImplLongId::Concrete(concrete_impl_id) => *concrete_impl_id,
        _ => panic!("Expected concrete impl"),
    };

    let function_id: FunctionId<'db> = FunctionLongId {
        function: ConcreteFunction {
            generic_function: GenericFunctionId::Impl(ImplGenericFunctionId {
                impl_id: deref_impl,
                function: deref_method,
            }),
            generic_args: vec![],
        },
    }
    .intern(db);

    let data = db.priv_impl_definition_data(concrete_impl_id.impl_def_id(db)).unwrap();
    let mut types_iter = data.item_type_asts.iter();
    let (impl_item_type_id, _) = types_iter.next().unwrap();
    if types_iter.next().is_some() {
        panic!(
            "get_impl_based_on_single_impl_type called with an impl that has more than one type"
        );
    }
    let ty = db.impl_type_def_resolved_type(*impl_item_type_id).unwrap();
    let substitution: GenericSubstitution<'db> = concrete_impl_id.substitution(db)?;
    let ty: TypeId<'db> = substitution.substitute(db, ty).unwrap();

    Ok(Some((function_id, ty)))
}

/// Reports diagnostic for a deref impl.
fn deref_impl_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    mut impl_def_id: ImplDefId<'db>,
    concrete_trait: ConcreteTraitId<'db>,
    diagnostics: &mut DiagnosticsBuilder<'db, SemanticDiagnostic<'db>>,
) {
    let mut visited_impls: OrderedHashSet<ImplDefId<'_>> = OrderedHashSet::default();
    let deref_trait_id = concrete_trait.trait_id(db);

    let impl_module = impl_def_id.module_file_id(db).0;

    let mut impl_in_valid_location = false;
    if impl_module == deref_trait_id.module_file_id(db).0 {
        impl_in_valid_location = true;
    }

    let gargs = concrete_trait.generic_args(db);
    let deref_ty = extract_matches!(gargs[0], GenericArgumentId::Type);
    if let Some(module_id) = deref_ty.long(db).module_id(db) {
        if module_id == impl_module {
            impl_in_valid_location = true;
        }
    }

    if !impl_in_valid_location {
        diagnostics.report(
            impl_def_id.stable_ptr(db),
            SemanticDiagnosticKind::MustBeNextToTypeOrTrait { trait_id: deref_trait_id },
        );
        return;
    }

    loop {
        let Ok(impl_id) = get_impl_based_on_single_impl_type(db, impl_def_id, |ty| {
            ConcreteTraitLongId {
                trait_id: deref_trait_id,
                generic_args: vec![GenericArgumentId::Type(ty)],
            }
            .intern(db)
        }) else {
            // Inference errors are handled when the impl is in actual use. In here we only check
            // for cycles.
            return;
        };

        impl_def_id = match impl_id.long(db) {
            ImplLongId::Concrete(concrete_impl_id) => concrete_impl_id.impl_def_id(db),
            _ => return,
        };

        if !visited_impls.insert(impl_def_id) {
            let deref_chain = visited_impls
                .iter()
                .map(|visited_impl| {
                    format!(
                        "{:?}",
                        db.impl_def_concrete_trait(*visited_impl).unwrap().debug(db.elongate())
                    )
                })
                .join(" -> ");
            diagnostics.report(
                impl_def_id.stable_ptr(db),
                SemanticDiagnosticKind::DerefCycle { deref_chain },
            );
            return;
        }
    }
}

/// Assuming that an impl has a single impl type, extracts the type, and then infers another impl
/// based on it. If the inference fails, returns the inference error and the impl type definition
/// for diagnostics.
fn get_impl_based_on_single_impl_type<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    concrete_trait_id: impl FnOnce(TypeId<'db>) -> ConcreteTraitId<'db>,
) -> Result<ImplId<'db>, (InferenceError<'db>, ImplTypeDefId<'db>)> {
    let data = db.priv_impl_definition_data(impl_def_id).unwrap();
    let mut types_iter = data.item_type_asts.iter();
    let (impl_item_type_id, _) = types_iter.next().unwrap();
    if types_iter.next().is_some() {
        panic!(
            "get_impl_based_on_single_impl_type called with an impl that has more than one type"
        );
    }
    let ty = db.impl_type_def_resolved_type(*impl_item_type_id).unwrap();

    let module_file_id = impl_def_id.module_file_id(db);
    let generic_params = db.impl_def_generic_params(impl_def_id).unwrap();
    let generic_params_ids =
        generic_params.iter().map(|generic_param| generic_param.id()).collect();
    let lookup_context = ImplLookupContext::new(module_file_id.0, generic_params_ids);
    get_impl_at_context(db, lookup_context, concrete_trait_id(ty), None)
        .map_err(|err| (err, *impl_item_type_id))
}

/// Query implementation of [crate::db::SemanticGroup::impl_functions].
pub fn impl_functions<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<OrderedHashMap<SmolStrId<'db>, ImplFunctionId<'db>>> {
    Ok(db
        .priv_impl_definition_data(impl_def_id)?
        .function_asts
        .keys()
        .map(|function_id| {
            let function_long_id = function_id.long(db);
            (function_long_id.name(db).intern(db), *function_id)
        })
        .collect())
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_by_trait_function].
pub fn impl_function_by_trait_function<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_function_id: TraitFunctionId<'db>,
) -> Maybe<Option<ImplFunctionId<'db>>> {
    let name = trait_function_id.name(db);
    for impl_function_id in db.priv_impl_definition_data(impl_def_id)?.function_asts.keys() {
        if impl_function_id.long(db).name(db) == name {
            return Ok(Some(*impl_function_id));
        }
    }
    Ok(None)
}

/// Query implementation of [crate::db::SemanticGroup::impl_item_by_name].
pub fn impl_item_by_name<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    name: SmolStrId<'db>,
) -> Maybe<Option<ImplItemId<'db>>> {
    Ok(db.priv_impl_definition_data(impl_def_id)?.item_id_by_name.get(&name).map(|info| info.id))
}

/// Query implementation of [crate::db::SemanticGroup::impl_item_info_by_name].
pub fn impl_item_info_by_name<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    name: SmolStrId<'db>,
) -> Maybe<Option<ImplItemInfo<'db>>> {
    let impl_definition_data = db.priv_impl_definition_data(impl_def_id)?;
    Ok(impl_definition_data.get_impl_item_info(name))
}

/// Query implementation of [crate::db::SemanticGroup::impl_implicit_impl_by_name].
pub fn impl_implicit_impl_by_name<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    name: SmolStrId<'db>,
) -> Maybe<Option<TraitImplId<'db>>> {
    Ok(db.priv_impl_definition_data(impl_def_id)?.implicit_impls_id_by_name.get(&name).cloned())
}

/// Query implementation of [SemanticGroup::impl_all_used_uses].
pub fn impl_all_used_uses<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<OrderedHashSet<UseId<'db>>>> {
    let mut all_used_uses = db.impl_def_resolver_data(impl_def_id)?.used_uses.clone();
    let data = db.priv_impl_definition_data(impl_def_id)?;
    for item in data.item_id_by_name.values() {
        for resolver_data in get_resolver_data_options(LookupItemId::ImplItem(item.id), db) {
            all_used_uses.extend(resolver_data.used_uses.iter().cloned());
        }
    }
    Ok(all_used_uses.into())
}

/// Query implementation of [crate::db::SemanticGroup::impl_types].
pub fn impl_types<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<OrderedHashMap<ImplTypeDefId<'db>, ast::ItemTypeAlias<'db>>>> {
    Ok(db.priv_impl_definition_data(impl_def_id)?.item_type_asts)
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_ids].
pub fn impl_type_ids<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<Vec<ImplTypeDefId<'db>>>> {
    Ok(Arc::new(db.impl_types(impl_def_id)?.keys().copied().collect_vec()))
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_by_id].
pub fn impl_type_by_id<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_id: ImplTypeDefId<'db>,
) -> Maybe<Option<ast::ItemTypeAlias<'db>>> {
    let impl_types = db.impl_types(impl_type_id.impl_def_id(db))?;
    Ok(impl_types.get(&impl_type_id).cloned())
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_by_trait_type].
pub fn impl_type_by_trait_type<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_type_id: TraitTypeId<'db>,
) -> Maybe<ImplTypeDefId<'db>> {
    if trait_type_id.trait_id(db) != db.impl_def_trait(impl_def_id)? {
        unreachable!(
            "impl_type_by_trait_type called with a trait type that does not belong to the impl's \
             trait"
        )
    }

    let name = trait_type_id.name(db).intern(db);
    // If the trait type's name is not found, then a missing item diagnostic is reported.
    db.impl_item_by_name(impl_def_id, name).and_then(|maybe_item_id| match maybe_item_id {
        Some(item_id) => Ok(extract_matches!(item_id, ImplItemId::Type)),
        None => Err(skip_diagnostic()),
    })
}

/// Query implementation of [crate::db::SemanticGroup::impl_constants].
pub fn impl_constants<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<OrderedHashMap<ImplConstantDefId<'db>, ast::ItemConstant<'db>>>> {
    Ok(db.priv_impl_definition_data(impl_def_id)?.item_constant_asts)
}

/// Query implementation of [crate::db::SemanticGroup::impl_constant_by_trait_constant].
pub fn impl_constant_by_trait_constant<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_constant_id: TraitConstantId<'db>,
) -> Maybe<ImplConstantDefId<'db>> {
    if trait_constant_id.trait_id(db) != db.impl_def_trait(impl_def_id)? {
        unreachable!(
            "impl_constant_by_trait_constant called with a trait constant that does not belong to \
             the impl's trait"
        )
    }

    let name = trait_constant_id.name(db);
    // If the trait constant's name is not found, then a missing item diagnostic is reported.
    db.impl_item_by_name(impl_def_id, name.intern(db)).and_then(|maybe_item_id| match maybe_item_id
    {
        Some(item_id) => Ok(extract_matches!(item_id, ImplItemId::Constant)),
        None => Err(skip_diagnostic()),
    })
}

/// Query implementation of [crate::db::SemanticGroup::impl_impls].
pub fn impl_impls<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<OrderedHashMap<ImplImplDefId<'db>, ast::ItemImplAlias<'db>>>> {
    Ok(db.priv_impl_definition_data(impl_def_id)?.item_impl_asts)
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_ids].
pub fn impl_impl_ids<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<Arc<Vec<ImplImplDefId<'db>>>> {
    Ok(Arc::new(db.impl_impls(impl_def_id)?.keys().copied().collect_vec()))
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_by_id].
pub fn impl_impl_by_id<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_id: ImplImplDefId<'db>,
) -> Maybe<Option<ast::ItemImplAlias<'db>>> {
    let impl_impls = db.impl_impls(impl_impl_id.impl_def_id(db))?;
    Ok(impl_impls.get(&impl_impl_id).cloned())
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_by_trait_impl].
pub fn impl_impl_by_trait_impl<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_impl_id: TraitImplId<'db>,
) -> Maybe<ImplImplDefId<'db>> {
    if trait_impl_id.trait_id(db) != db.impl_def_trait(impl_def_id)? {
        unreachable!(
            "impl_impl_by_trait_impl called with a trait impl that does not belong to the impl's \
             trait"
        )
    }

    let name = trait_impl_id.name(db).intern(db);
    // If the trait impl's name is not found, then a missing item diagnostic is reported.
    db.impl_item_by_name(impl_def_id, name).and_then(|maybe_item_id| match maybe_item_id {
        Some(item_id) => Ok(extract_matches!(item_id, ImplItemId::Impl)),
        None => Err(skip_diagnostic()),
    })
}

/// Query implementation of [crate::db::SemanticGroup::is_implicit_impl_impl].
pub fn is_implicit_impl_impl<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_impl_id: TraitImplId<'db>,
) -> Maybe<bool> {
    if trait_impl_id.trait_id(db) != db.impl_def_trait(impl_def_id)? {
        unreachable!(
            "impl_impl_by_trait_impl called with a trait impl that does not belong to the impl's \
             trait"
        )
    }

    let name = trait_impl_id.name(db).intern(db);
    // If the trait impl's name is not found, then a missing item diagnostic is reported.
    Ok(db.impl_implicit_impl_by_name(impl_def_id, name)?.is_some())
}

// --- Computation ---

/// Query implementation of [crate::db::SemanticGroup::priv_impl_definition_data].
pub fn priv_impl_definition_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<ImplDefinitionData<'db>> {
    let module_file_id = impl_def_id.module_file_id(db);
    let mut diagnostics = SemanticDiagnostics::default();

    let generic_params = db.impl_def_generic_params(impl_def_id)?;
    let concrete_trait = db.priv_impl_declaration_data(impl_def_id)?.concrete_trait?;

    let impl_ast = db.module_impl_by_id(impl_def_id)?.to_maybe()?;

    let generic_params_ids =
        generic_params.iter().map(|generic_param| generic_param.id()).collect();
    let lookup_context = ImplLookupContext::new(module_file_id.0, generic_params_ids);
    check_special_impls(
        db,
        &mut diagnostics,
        lookup_context,
        concrete_trait,
        impl_ast.stable_ptr(db).untyped(),
    )
    // Ignore the result.
    .ok();

    let mut function_asts = OrderedHashMap::default();
    let mut item_type_asts = OrderedHashMap::default();
    let mut item_constant_asts = OrderedHashMap::default();
    let mut item_impl_asts = OrderedHashMap::default();
    let mut item_id_by_name = OrderedHashMap::default();

    if let MaybeImplBody::Some(body) = impl_ast.body(db) {
        for item in body.items(db).elements(db) {
            match item {
                ImplItem::Module(module) => {
                    report_invalid_impl_item(db, &mut diagnostics, module.module_kw(db))
                }

                ImplItem::Use(use_item) => {
                    report_invalid_impl_item(db, &mut diagnostics, use_item.use_kw(db))
                }
                ImplItem::ExternFunction(extern_func) => {
                    report_invalid_impl_item(db, &mut diagnostics, extern_func.extern_kw(db))
                }
                ImplItem::ExternType(extern_type) => {
                    report_invalid_impl_item(db, &mut diagnostics, extern_type.extern_kw(db))
                }
                ImplItem::Trait(trt) => {
                    report_invalid_impl_item(db, &mut diagnostics, trt.trait_kw(db))
                }
                ImplItem::Struct(structure) => {
                    report_invalid_impl_item(db, &mut diagnostics, structure.struct_kw(db))
                }
                ImplItem::Enum(enm) => {
                    report_invalid_impl_item(db, &mut diagnostics, enm.enum_kw(db))
                }
                ImplItem::Function(func) => {
                    let impl_function_id =
                        ImplFunctionLongId(module_file_id, func.stable_ptr(db)).intern(db);
                    let name_node = func.declaration(db).name(db);
                    let name = name_node.text(db).intern(db);
                    let feature_kind =
                        FeatureKind::from_ast(db, &mut diagnostics, &func.attributes(db));
                    if item_id_by_name
                        .insert(
                            name,
                            ImplItemInfo {
                                id: ImplItemId::Function(impl_function_id),
                                feature_kind,
                            },
                        )
                        .is_some()
                    {
                        diagnostics
                            .report(name_node.stable_ptr(db), NameDefinedMultipleTimes(name));
                    }
                    function_asts.insert(impl_function_id, func);
                }
                ImplItem::Type(ty) => {
                    let impl_type_id =
                        ImplTypeDefLongId(module_file_id, ty.stable_ptr(db)).intern(db);
                    let name_node = ty.name(db);
                    let name = name_node.text(db).intern(db);
                    let feature_kind =
                        FeatureKind::from_ast(db, &mut diagnostics, &ty.attributes(db));
                    if item_id_by_name
                        .insert(
                            name,
                            ImplItemInfo { id: ImplItemId::Type(impl_type_id), feature_kind },
                        )
                        .is_some()
                    {
                        diagnostics
                            .report(name_node.stable_ptr(db), NameDefinedMultipleTimes(name));
                    }
                    item_type_asts.insert(impl_type_id, ty);
                }
                ImplItem::Constant(constant) => {
                    let impl_constant_id =
                        ImplConstantDefLongId(module_file_id, constant.stable_ptr(db)).intern(db);
                    let name_node = constant.name(db);
                    let name = name_node.text(db).intern(db);
                    let feature_kind =
                        FeatureKind::from_ast(db, &mut diagnostics, &constant.attributes(db));
                    if item_id_by_name
                        .insert(
                            name,
                            ImplItemInfo {
                                id: ImplItemId::Constant(impl_constant_id),
                                feature_kind,
                            },
                        )
                        .is_some()
                    {
                        diagnostics.report(
                            name_node.stable_ptr(db),
                            SemanticDiagnosticKind::NameDefinedMultipleTimes(name),
                        );
                    }
                    item_constant_asts.insert(impl_constant_id, constant);
                }
                ImplItem::Impl(imp) => {
                    let impl_impl_id =
                        ImplImplDefLongId(module_file_id, imp.stable_ptr(db)).intern(db);
                    let name_node = imp.name(db);
                    let name = name_node.text(db).intern(db);
                    let feature_kind =
                        FeatureKind::from_ast(db, &mut diagnostics, &imp.attributes(db));
                    if item_id_by_name
                        .insert(
                            name,
                            ImplItemInfo { id: ImplItemId::Impl(impl_impl_id), feature_kind },
                        )
                        .is_some()
                    {
                        diagnostics.report(
                            name_node.stable_ptr(db),
                            SemanticDiagnosticKind::NameDefinedMultipleTimes(name),
                        );
                    }
                    item_impl_asts.insert(impl_impl_id, imp);
                }
                // Report nothing, a parser diagnostic is reported.
                ImplItem::Missing(_) => {}
            }
        }
    }

    let mut implicit_impls_id_by_name = OrderedHashMap::default();

    let trait_id = concrete_trait.long(db).trait_id;
    for trait_impl_id in db.trait_impls(trait_id)? {
        if item_id_by_name.contains_key(&trait_impl_id.0) {
            continue;
        }
        implicit_impls_id_by_name.insert(trait_impl_id.0, trait_impl_id.1);
    }

    // It is later verified that all items in this impl match items from `concrete_trait`.
    // To ensure exact match (up to trait functions with default implementation), it is sufficient
    // to verify here that all items in `concrete_trait` appear in this impl.
    let impl_item_names: OrderedHashSet<SmolStrId<'db>> = item_id_by_name.keys().copied().collect();

    let trait_required_item_names = db.trait_required_item_names(trait_id)?;
    let missing_items_in_impl =
        trait_required_item_names.difference(&impl_item_names).cloned().collect::<Vec<_>>();
    if !missing_items_in_impl.is_empty() {
        diagnostics.report(
            // TODO(yuval): change this to point to impl declaration (need to add ImplDeclaration
            // in cairo_spec).
            // TODO(TomerStarkware): make sure we do not report missing if the trait item is
            // unsupported in impl.
            impl_ast.name(db).stable_ptr(db),
            SemanticDiagnosticKind::MissingItemsInImpl(missing_items_in_impl),
        );
    }

    Ok(ImplDefinitionData {
        diagnostics: diagnostics.build(),
        function_asts,
        item_type_asts: item_type_asts.into(),
        item_id_by_name: item_id_by_name.into(),
        item_constant_asts: item_constant_asts.into(),
        item_impl_asts: item_impl_asts.into(),
        implicit_impls_id_by_name: implicit_impls_id_by_name.into(),
    })
}

/// A helper function to report diagnostics of items in an impl (used in
/// priv_impl_definition_data).
fn report_invalid_impl_item<'db, Terminal: syntax::node::Terminal<'db>>(
    db: &'db dyn SyntaxGroup,
    diagnostics: &mut SemanticDiagnostics<'db>,
    kw_terminal: Terminal,
) {
    diagnostics.report(
        kw_terminal.as_syntax_node().stable_ptr(db),
        InvalidImplItem(kw_terminal.text(db).intern(db)),
    );
}

/// Handle special cases such as Copy and Drop checking.
fn check_special_impls<'db>(
    db: &'db dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics<'db>,
    lookup_context: ImplLookupContext<'db>,
    concrete_trait: ConcreteTraitId<'db>,
    stable_ptr: SyntaxStablePtrId<'db>,
) -> Maybe<()> {
    let ConcreteTraitLongId { trait_id, generic_args } = concrete_trait.long(db);
    let info = db.core_info();
    let copy = info.copy_trt;
    let drop = info.drop_trt;

    if *trait_id == copy {
        let tys = get_inner_types(db, extract_matches!(generic_args[0], GenericArgumentId::Type))?;
        for inference_error in tys
            .into_iter()
            .map(|ty| db.type_info(lookup_context.clone(), ty))
            .flat_map(|info| info.copyable.err())
        {
            if matches!(
                inference_error,
                InferenceError::Ambiguity(Ambiguity::MultipleImplsFound { .. })
            ) {
                // Having multiple drop implementations for a member is not an actual error.
                continue;
            }
            return Err(diagnostics.report(stable_ptr, InvalidCopyTraitImpl(inference_error)));
        }
    }
    if *trait_id == drop {
        let tys = get_inner_types(db, extract_matches!(generic_args[0], GenericArgumentId::Type))?;
        for inference_error in tys
            .into_iter()
            .map(|ty| db.type_info(lookup_context.clone(), ty))
            .flat_map(|info| info.droppable.err())
        {
            if matches!(
                inference_error,
                InferenceError::Ambiguity(Ambiguity::MultipleImplsFound { .. })
            ) {
                // Having multiple drop implementations for a member is not an actual error.
                continue;
            }
            return Err(diagnostics.report(stable_ptr, InvalidDropTraitImpl(inference_error)));
        }
    }

    Ok(())
}

/// Retrieves all the inner types (members of a struct / tuple or variants of an enum).
///
/// These are the types that are required to implement some trait,
/// in order for the original type to be able to implement this trait.
///
/// For example, a struct containing a type T can implement Drop only if T implements Drop.
fn get_inner_types<'db>(db: &'db dyn SemanticGroup, ty: TypeId<'db>) -> Maybe<Vec<TypeId<'db>>> {
    Ok(match ty.long(db) {
        TypeLongId::Concrete(concrete_type_id) => {
            // Look for Copy and Drop trait in the defining module.
            match concrete_type_id {
                crate::ConcreteTypeId::Struct(concrete_struct_id) => db
                    .concrete_struct_members(*concrete_struct_id)?
                    .values()
                    .map(|member| member.ty)
                    .collect(),
                crate::ConcreteTypeId::Enum(concrete_enum_id) => db
                    .concrete_enum_variants(*concrete_enum_id)?
                    .into_iter()
                    .map(|variant| variant.ty)
                    .collect(),
                crate::ConcreteTypeId::Extern(_) => vec![],
            }
        }
        TypeLongId::Tuple(tys) => tys.to_vec(),
        TypeLongId::Snapshot(_) | TypeLongId::Closure(_) => vec![],
        TypeLongId::GenericParameter(_) => {
            return Err(skip_diagnostic());
        }
        TypeLongId::Var(_) | TypeLongId::ImplType(_) => {
            panic!("Types should be fully resolved at this point.")
        }
        TypeLongId::Coupon(_) => vec![],
        TypeLongId::FixedSizeArray { type_id, .. } => vec![*type_id],
        TypeLongId::Missing(diag_added) => {
            return Err(*diag_added);
        }
    })
}

// === Trait Filter ===

/// A filter for trait lookup that is not based on current inference state. This is
/// used for caching queries.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TraitFilter<'db> {
    pub trait_id: TraitId<'db>,
    /// The filter on the generic arguments.
    pub generics_filter: GenericsHeadFilter<'db>,
}

/// A lookup filter on generic arguments that is not based on current inference state.
/// This is used for caching queries.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum GenericsHeadFilter<'db> {
    /// No filter is applied. When nothing is known about the generics, this will lead to a
    /// wider search.
    NoFilter,
    /// Generics exists and the first generic parameter has a filter.
    /// This is usually enough to considerably reduce the number of searched items.
    FirstGenericFilter(GenericArgumentHead<'db>),
    /// Generics must not exist.
    NoGenerics,
}

/// Query implementation of [crate::db::SemanticGroup::module_impl_ids_for_trait_filter].
pub fn module_impl_ids_for_trait_filter<'db>(
    db: &'db dyn SemanticGroup,
    module_id: ModuleId<'db>,
    trait_filter: TraitFilter<'db>,
) -> Maybe<Vec<UninferredImpl<'db>>> {
    // Get the impls first from the module, do not change this order.
    let mut uninferred_impls: OrderedHashSet<UninferredImpl<'_>> =
        OrderedHashSet::from_iter(module_impl_ids(db, module_id, module_id)?);
    for (user_module, containing_module) in &db.priv_module_use_star_modules(module_id).accessible {
        if let Ok(star_module_uninferred_impls) =
            module_impl_ids(db, *user_module, *containing_module)
        {
            uninferred_impls.extend(star_module_uninferred_impls);
        }
    }
    let mut res = Vec::new();
    for uninferred_impl in uninferred_impls {
        let Ok(trait_id) = uninferred_impl.trait_id(db) else { continue };
        if trait_id != trait_filter.trait_id {
            continue;
        }
        let Ok(concrete_trait_id) = uninferred_impl.concrete_trait(db) else {
            continue;
        };
        if let Ok(true) = concrete_trait_fits_trait_filter(db, concrete_trait_id, &trait_filter) {
            res.push(uninferred_impl);
        }
    }
    Ok(res)
}

/// Returns the uninferred impls in a module.
fn module_impl_ids<'db>(
    db: &'db dyn SemanticGroup,
    user_module: ModuleId<'db>,
    containing_module: ModuleId<'db>,
) -> Maybe<Vec<UninferredImpl<'db>>> {
    let mut uninferred_impls = Vec::new();
    for item in db.priv_module_semantic_data(containing_module)?.items.values() {
        if !matches!(
            item.item_id,
            ModuleItemId::Impl(_) | ModuleItemId::ImplAlias(_) | ModuleItemId::Use(_)
        ) {
            continue;
        }
        if !peek_visible_in(db, item.visibility, containing_module, user_module) {
            continue;
        }
        match item.item_id {
            ModuleItemId::Impl(impl_def_id) => {
                uninferred_impls.push(UninferredImpl::Def(impl_def_id));
            }
            ModuleItemId::ImplAlias(impl_alias_id) => {
                uninferred_impls.push(UninferredImpl::ImplAlias(impl_alias_id));
            }
            ModuleItemId::Use(use_id) => match db.use_resolved_item(use_id) {
                Ok(ResolvedGenericItem::Impl(impl_def_id)) => {
                    uninferred_impls.push(UninferredImpl::Def(impl_def_id));
                }
                Ok(ResolvedGenericItem::GenericImplAlias(impl_alias_id)) => {
                    uninferred_impls.push(UninferredImpl::ImplAlias(impl_alias_id));
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(uninferred_impls)
}

/// Cycle handling for [crate::db::SemanticGroup::module_impl_ids_for_trait_filter].
pub fn module_impl_ids_for_trait_filter_cycle<'db>(
    _db: &dyn SemanticGroup,
    _input: SemanticGroupData,
    _module_id: ModuleId<'db>,
    _trait_filter: TraitFilter<'db>,
) -> Maybe<Vec<UninferredImpl<'db>>> {
    // The diagnostics will be reported from the calling function, specifically from
    // `priv_impl_declaration_data_inner`.
    Err(skip_diagnostic())
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_ids_for_trait_filter].
pub fn impl_impl_ids_for_trait_filter<'db>(
    db: &'db dyn SemanticGroup,
    impl_id: ImplId<'db>,
    trait_filter: TraitFilter<'db>,
) -> Maybe<Vec<UninferredImpl<'db>>> {
    let mut uninferred_impls = Vec::new();
    for (_, trait_impl_id) in db.trait_impls(impl_id.concrete_trait(db)?.trait_id(db))?.iter() {
        uninferred_impls.push(UninferredImpl::ImplImpl(ImplImplId::new(
            impl_id,
            *trait_impl_id,
            db,
        )));
    }
    let mut res = Vec::new();
    for uninferred_impl in uninferred_impls {
        let Ok(trait_id) = uninferred_impl.trait_id(db) else { continue };
        if trait_id != trait_filter.trait_id {
            continue;
        }
        let Ok(concrete_trait_id) = uninferred_impl.concrete_trait(db) else {
            continue;
        };
        if let Ok(true) = concrete_trait_fits_trait_filter(db, concrete_trait_id, &trait_filter) {
            res.push(uninferred_impl);
        }
    }

    Ok(res)
}
/// Cycle handling for [crate::db::SemanticGroup::impl_impl_ids_for_trait_filter].
pub fn impl_impl_ids_for_trait_filter_cycle<'db>(
    _db: &dyn SemanticGroup,
    _input: SemanticGroupData,
    _imp: ImplId<'db>,
    _trait_filter: TraitFilter<'db>,
) -> Maybe<Vec<UninferredImpl<'db>>> {
    // The diagnostics will be reported from the calling function, specifically from
    // `priv_impl_declaration_data_inner`.
    Err(skip_diagnostic())
}

/// Checks whether an [ImplDefId] passes a [TraitFilter].
fn concrete_trait_fits_trait_filter(
    db: &dyn SemanticGroup,
    concrete_trait_id: ConcreteTraitId<'_>,
    trait_filter: &TraitFilter<'_>,
) -> Maybe<bool> {
    if trait_filter.trait_id != concrete_trait_id.trait_id(db) {
        return Ok(false);
    }
    let generic_args = concrete_trait_id.generic_args(db);
    let first_generic = generic_args.first();
    Ok(match &trait_filter.generics_filter {
        GenericsHeadFilter::NoFilter => true,
        GenericsHeadFilter::FirstGenericFilter(constraint_head) => {
            let Some(first_generic) = first_generic else {
                return Ok(false);
            };
            let Some(first_generic_head) = first_generic.head(db) else {
                return Ok(true);
            };
            &first_generic_head == constraint_head
        }
        GenericsHeadFilter::NoGenerics => first_generic.is_none(),
    })
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, salsa::Update)]
pub enum ImplOrModuleById<'db> {
    Impl(ImplId<'db>),
    Module(ModuleId<'db>),
}
impl<'db> From<ImplId<'db>> for ImplOrModuleById<'db> {
    fn from(impl_id: ImplId<'db>) -> Self {
        ImplOrModuleById::Impl(impl_id)
    }
}
impl<'db> From<ModuleId<'db>> for ImplOrModuleById<'db> {
    fn from(module_id: ModuleId<'db>) -> Self {
        ImplOrModuleById::Module(module_id)
    }
}

impl<'db> Ord for ImplOrModuleById<'db> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (ImplOrModuleById::Impl(imp), ImplOrModuleById::Impl(other_impl)) => {
                imp.get_internal_id().cmp(&other_impl.get_internal_id())
            }
            (ImplOrModuleById::Module(module), ImplOrModuleById::Module(other_module)) => {
                match (module, other_module) {
                    (ModuleId::CrateRoot(crate_id), ModuleId::CrateRoot(other_crate_id)) => {
                        crate_id.get_internal_id().cmp(&other_crate_id.get_internal_id())
                    }
                    (ModuleId::CrateRoot(_), ModuleId::Submodule(_)) => std::cmp::Ordering::Less,
                    (ModuleId::Submodule(_), ModuleId::CrateRoot(_)) => std::cmp::Ordering::Greater,
                    (ModuleId::Submodule(module_id), ModuleId::Submodule(other_module_id)) => {
                        module_id.get_internal_id().cmp(&other_module_id.get_internal_id())
                    }
                }
            }
            (ImplOrModuleById::Impl(_), ImplOrModuleById::Module(_)) => std::cmp::Ordering::Less,
            (ImplOrModuleById::Module(_), ImplOrModuleById::Impl(_)) => std::cmp::Ordering::Greater,
        }
    }
}
impl<'db> PartialOrd for ImplOrModuleById<'db> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplLookupContext<'db> {
    pub modules_and_impls: BTreeSet<ImplOrModuleById<'db>>,
    pub generic_params: Vec<GenericParamId<'db>>,
}
impl<'db> ImplLookupContext<'db> {
    pub fn new(
        module_id: ModuleId<'db>,
        generic_params: Vec<GenericParamId<'db>>,
    ) -> ImplLookupContext<'db> {
        Self { modules_and_impls: [ImplOrModuleById::Module(module_id)].into(), generic_params }
    }
    pub fn insert_lookup_scope(&mut self, db: &'db dyn SemanticGroup, imp: &UninferredImpl<'db>) {
        let item = match imp {
            UninferredImpl::Def(impl_def_id) => impl_def_id.module_file_id(db).0.into(),
            UninferredImpl::ImplAlias(impl_alias_id) => impl_alias_id.module_file_id(db).0.into(),
            UninferredImpl::GenericParam(param) => param.module_file_id(db).0.into(),
            UninferredImpl::ImplImpl(impl_impl_id) => impl_impl_id.impl_id.into(),
            UninferredImpl::GeneratedImpl(_) => {
                // GeneratedImpls do not extend the lookup context.
                return;
            }
        };
        self.modules_and_impls.insert(item);
    }
    pub fn insert_module(&mut self, module_id: ModuleId<'db>) -> bool {
        self.modules_and_impls.insert(ImplOrModuleById::Module(module_id))
    }

    pub fn insert_impl(&mut self, impl_id: ImplId<'db>) -> bool {
        self.modules_and_impls.insert(ImplOrModuleById::Impl(impl_id))
    }
}

/// A candidate impl for later inference.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, SemanticObject, salsa::Update)]
pub enum UninferredImpl<'db> {
    Def(ImplDefId<'db>),
    ImplAlias(ImplAliasId<'db>),
    GenericParam(GenericParamId<'db>),
    ImplImpl(ImplImplId<'db>),
    GeneratedImpl(UninferredGeneratedImplId<'db>),
}
impl<'db> UninferredImpl<'db> {
    pub fn concrete_trait(&self, db: &'db dyn SemanticGroup) -> Maybe<ConcreteTraitId<'db>> {
        match self {
            UninferredImpl::Def(impl_def_id) => db.impl_def_concrete_trait(*impl_def_id),
            UninferredImpl::ImplAlias(impl_alias_id) => {
                let impl_id = db.impl_alias_resolved_impl(*impl_alias_id)?;
                impl_id.concrete_trait(db)
            }
            UninferredImpl::GenericParam(param) => {
                let param =
                    extract_matches!(db.generic_param_semantic(*param)?, GenericParam::Impl);
                param.concrete_trait
            }
            UninferredImpl::ImplImpl(impl_impl_id) => db.impl_impl_concrete_trait(*impl_impl_id),
            UninferredImpl::GeneratedImpl(generated_impl) => Ok(generated_impl.concrete_trait(db)),
        }
    }

    fn trait_id(&self, db: &'db dyn SemanticGroup) -> Maybe<TraitId<'db>> {
        match self {
            UninferredImpl::Def(impl_def_id) => db.impl_def_trait(*impl_def_id),
            UninferredImpl::ImplAlias(impl_alias_id) => {
                let impl_def_id = db.impl_alias_impl_def(*impl_alias_id)?;
                db.impl_def_trait(impl_def_id)
            }
            UninferredImpl::GenericParam(param) => {
                let param =
                    extract_matches!(db.generic_param_semantic(*param)?, GenericParam::Impl);
                param.concrete_trait.map(|concrete_trait| concrete_trait.trait_id(db))
            }
            UninferredImpl::ImplImpl(impl_impl_id) => db
                .impl_impl_concrete_trait(*impl_impl_id)
                .map(|concrete_trait| concrete_trait.trait_id(db)),
            UninferredImpl::GeneratedImpl(generated_impl) => Ok(generated_impl.trait_id(db)),
        }
    }

    pub fn lookup_scope(&self, db: &'db dyn SemanticGroup) -> ImplOrModuleById<'db> {
        match self {
            UninferredImpl::Def(impl_def_id) => impl_def_id.module_file_id(db).0.into(),
            UninferredImpl::ImplAlias(impl_alias_id) => impl_alias_id.module_file_id(db).0.into(),
            UninferredImpl::GenericParam(param) => param.module_file_id(db).0.into(),
            UninferredImpl::ImplImpl(impl_impl_id) => impl_impl_id.impl_id.into(),
            UninferredImpl::GeneratedImpl(generated_impl) => {
                generated_impl.concrete_trait(db).trait_id(db).module_file_id(db).0.into()
            }
        }
    }
}
impl<'db> DebugWithDb<'db> for UninferredImpl<'db> {
    type Db = dyn SemanticGroup;

    fn fmt(&self, f: &mut std::fmt::Formatter<'_>, db: &'db dyn SemanticGroup) -> std::fmt::Result {
        match self {
            UninferredImpl::Def(impl_def) => write!(f, "{:?}", impl_def.full_path(db)),
            UninferredImpl::ImplAlias(impl_alias) => {
                write!(f, "{:?}", impl_alias.full_path(db))
            }
            UninferredImpl::GenericParam(param) => {
                write!(f, "generic param {}", param.name(db).unwrap_or_else(|| "_".into()))
            }
            UninferredImpl::ImplImpl(impl_impl) => impl_impl.fmt(f, db.elongate()),
            UninferredImpl::GeneratedImpl(generated_impl) => generated_impl.fmt(f, db.elongate()),
        }
    }
}

define_short_id!(
    UninferredGeneratedImplId,
    UninferredGeneratedImplLongId<'db>,
    SemanticGroup,
    lookup_intern_uninferred_generated_impl,
    intern_uninferred_generated_impl
);
semantic_object_for_id!(
    UninferredGeneratedImplId<'a>,
    lookup_intern_uninferred_generated_impl,
    intern_uninferred_generated_impl,
    UninferredGeneratedImplLongId<'a>
);

impl<'db> UninferredGeneratedImplId<'db> {
    pub fn concrete_trait(self, db: &'db dyn SemanticGroup) -> ConcreteTraitId<'db> {
        db.lookup_intern_uninferred_generated_impl(self).concrete_trait
    }

    pub fn trait_id(&self, db: &'db dyn SemanticGroup) -> TraitId<'db> {
        self.concrete_trait(db).trait_id(db)
    }
}

/// Generated impls before inference, see GeneratedImplLongId for more details.
#[derive(Clone, Debug, Hash, PartialEq, Eq, SemanticObject)]
pub struct UninferredGeneratedImplLongId<'db> {
    pub concrete_trait: ConcreteTraitId<'db>,
    pub generic_params: Vec<GenericParam<'db>>,
    pub impl_items: GeneratedImplItems<'db>,
}

impl<'db> DebugWithDb<'db> for UninferredGeneratedImplLongId<'db> {
    type Db = dyn SemanticGroup;

    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        db: &'db (dyn SemanticGroup + 'static),
    ) -> std::fmt::Result {
        write!(f, "Generated {:?}", self.concrete_trait.debug(db))
    }
}

/// Finds all the implementations of a concrete trait, in a specific lookup context.
pub fn find_candidates_at_context<'db>(
    db: &'db dyn SemanticGroup,
    lookup_context: &ImplLookupContext<'db>,
    filter: &TraitFilter<'db>,
) -> Maybe<OrderedHashSet<UninferredImpl<'db>>> {
    let mut res = OrderedHashSet::default();
    for generic_param_id in &lookup_context.generic_params {
        if !matches!(generic_param_id.kind(db), GenericKind::Impl) {
            continue;
        };
        let Ok(trait_id) = db.generic_impl_param_trait(*generic_param_id) else {
            continue;
        };
        if filter.trait_id != trait_id {
            continue;
        }
        let Ok(generic_param_semantic) = db.generic_param_semantic(*generic_param_id) else {
            continue;
        };
        let param = extract_matches!(generic_param_semantic, GenericParam::Impl);
        let Ok(imp_concrete_trait_id) = param.concrete_trait else { continue };
        let Ok(trait_fits_filter) =
            concrete_trait_fits_trait_filter(db, imp_concrete_trait_id, filter)
        else {
            continue;
        };
        if !trait_fits_filter {
            continue;
        }
        res.insert(UninferredImpl::GenericParam(*generic_param_id));
    }
    for module_or_impl_id in &lookup_context.modules_and_impls {
        let Ok(imps) = (match module_or_impl_id {
            ImplOrModuleById::Module(module_id) => {
                db.module_impl_ids_for_trait_filter(*module_id, filter.clone())
            }
            ImplOrModuleById::Impl(impl_id) => {
                db.impl_impl_ids_for_trait_filter(*impl_id, filter.clone())
            }
        }) else {
            continue;
        };
        for imp in imps {
            res.insert(imp);
        }
    }
    Ok(res)
}

/// Finds the generated candidate for a concrete trait.
pub fn find_closure_generated_candidate<'db>(
    db: &'db dyn SemanticGroup,
    concrete_trait_id: ConcreteTraitId<'db>,
) -> Option<UninferredImpl<'db>> {
    let GenericArgumentId::Type(closure_type) = *concrete_trait_id.generic_args(db).first()? else {
        return None;
    };
    let TypeLongId::Closure(closure_type_long) = closure_type.long(db) else {
        return None;
    };

    let info = db.core_info();

    // Handles the special cases of `Copy`, `Drop`, `Destruct` and `PanicDestruct`.
    let mem_trait_generic_params = |trait_id, neg_impl_trait: Option<_>| {
        let id = db.trait_generic_params(trait_id).unwrap().first().unwrap().id();
        chain!(
            closure_type_long.captured_types.iter().unique().map(|ty| {
                GenericParam::Impl(GenericParamImpl {
                    id,
                    concrete_trait: Maybe::Ok(db.intern_concrete_trait(ConcreteTraitLongId {
                        trait_id,
                        generic_args: vec![GenericArgumentId::Type(*ty)],
                    })),
                    type_constraints: Default::default(),
                })
            }),
            neg_impl_trait.map(|neg_impl_trait| {
                GenericParam::NegImpl(GenericParamImpl {
                    id,
                    concrete_trait: Maybe::Ok(neg_impl_trait),
                    type_constraints: Default::default(),
                })
            })
        )
        .collect()
    };
    let handle_mem_trait = |trait_id, neg_impl_trait: Option<_>| {
        (concrete_trait_id, mem_trait_generic_params(trait_id, neg_impl_trait), [].into())
    };
    let (concrete_trait, generic_params, impl_items) = match concrete_trait_id.trait_id(db) {
        trait_id if trait_id == info.fn_once_trt => {
            let concrete_trait = ConcreteTraitLongId {
                trait_id,
                generic_args: vec![
                    GenericArgumentId::Type(closure_type),
                    GenericArgumentId::Type(
                        TypeLongId::Tuple(closure_type_long.param_tys.clone()).intern(db),
                    ),
                ],
            }
            .intern(db);
            let ret_ty = db
                .trait_type_by_name(trait_id, SmolStr::from("Output").intern(db))
                .unwrap()
                .unwrap();

            let id = db.trait_generic_params(trait_id).unwrap().first().unwrap().id();
            // FnOnce is generated only if there is no fn trait.
            let param: GenericParam<'_> = GenericParam::NegImpl(GenericParamImpl {
                id,
                concrete_trait: Maybe::Ok(
                    ConcreteTraitLongId {
                        trait_id: info.fn_trt,
                        generic_args: vec![
                            GenericArgumentId::Type(closure_type),
                            GenericArgumentId::Type(
                                TypeLongId::Tuple(closure_type_long.param_tys.clone()).intern(db),
                            ),
                        ],
                    }
                    .intern(db),
                ),
                type_constraints: Default::default(),
            });
            (concrete_trait, vec![param], [(ret_ty, closure_type_long.ret_ty)].into())
        }
        trait_id if trait_id == info.fn_trt => {
            let concrete_trait = ConcreteTraitLongId {
                trait_id,
                generic_args: vec![
                    GenericArgumentId::Type(closure_type),
                    GenericArgumentId::Type(
                        TypeLongId::Tuple(closure_type_long.param_tys.clone()).intern(db),
                    ),
                ],
            }
            .intern(db);
            let ret_ty = db
                .trait_type_by_name(trait_id, SmolStr::from("Output").intern(db))
                .unwrap()
                .unwrap();

            (
                concrete_trait,
                // Makes the generated impl of fn_trait available only if the closure is copyable.
                mem_trait_generic_params(info.copy_trt, None),
                [(ret_ty, closure_type_long.ret_ty)].into(),
            )
        }
        trait_id if trait_id == info.drop_trt => handle_mem_trait(trait_id, None),
        trait_id if trait_id == info.destruct_trt => {
            handle_mem_trait(trait_id, Some(concrete_drop_trait(db, closure_type)))
        }
        trait_id if trait_id == info.panic_destruct_trt => {
            handle_mem_trait(trait_id, Some(concrete_destruct_trait(db, closure_type)))
        }
        trait_id if trait_id == info.copy_trt => handle_mem_trait(trait_id, None),
        _ => return None,
    };
    Some(UninferredImpl::GeneratedImpl(
        UninferredGeneratedImplLongId {
            concrete_trait,
            generic_params,
            impl_items: GeneratedImplItems(impl_items),
        }
        .intern(db),
    ))
}

/// Checks if an impl of a trait function with a given self_ty exists.
/// This function does not change the state of the inference context.
///
/// `inference_errors` are aggregated here but are not reported here as diagnostics.
/// The caller has to make sure the diagnostics are reported appropriately.
pub fn can_infer_impl_by_self<'db>(
    ctx: &ComputationContext<'db, '_>,
    inference_errors: &mut Vec<(TraitFunctionId<'db>, InferenceError<'db>)>,
    trait_function_id: TraitFunctionId<'db>,
    self_ty: TypeId<'db>,
    stable_ptr: SyntaxStablePtrId<'db>,
) -> bool {
    let mut temp_inference_data = ctx.resolver.data.inference_data.temporary_clone();
    let mut temp_inference = temp_inference_data.inference(ctx.db);
    let lookup_context = ctx.resolver.impl_lookup_context();

    let Some((concrete_trait_id, _)) = temp_inference.infer_concrete_trait_by_self(
        trait_function_id,
        self_ty,
        &lookup_context,
        Some(stable_ptr),
        inference_errors,
    ) else {
        return false;
    };
    // Find impls for it.
    if let Err(err_set) = temp_inference.solve() {
        // Error is propagated and will be reported later.
        if let Some(err) = temp_inference.consume_error_without_reporting(err_set) {
            inference_errors.push((trait_function_id, err));
        }
    }
    match temp_inference.trait_solution_set(
        concrete_trait_id,
        ImplVarTraitItemMappings::default(),
        lookup_context.clone(),
    ) {
        Ok(SolutionSet::Unique(_) | SolutionSet::Ambiguous(_)) => true,
        Ok(SolutionSet::None) => {
            inference_errors
                .push((trait_function_id, InferenceError::NoImplsFound(concrete_trait_id)));
            false
        }
        Err(err_set) => {
            // Error is propagated and will be reported later.
            if let Some(err) = temp_inference.consume_error_without_reporting(err_set) {
                inference_errors.push((trait_function_id, err));
            }
            false
        }
    }
}

/// Returns an impl of a given trait function with a given self_ty, as well as the number of
/// snapshots needed to be added to it.
pub fn infer_impl_by_self<'db>(
    ctx: &mut ComputationContext<'db, '_>,
    trait_function_id: TraitFunctionId<'db>,
    self_ty: TypeId<'db>,
    stable_ptr: SyntaxStablePtrId<'db>,
    generic_args_syntax: Option<Vec<GenericArg<'db>>>,
) -> Maybe<(FunctionId<'db>, usize)> {
    let lookup_context = ctx.resolver.impl_lookup_context();
    let (concrete_trait_id, n_snapshots) = ctx
        .resolver
        .inference()
        .infer_concrete_trait_by_self_without_errors(
            trait_function_id,
            self_ty,
            &lookup_context,
            Some(stable_ptr),
        )
        .ok_or_else(skip_diagnostic)?;

    let concrete_trait_function_id =
        ConcreteTraitGenericFunctionLongId::new(ctx.db, concrete_trait_id, trait_function_id)
            .intern(ctx.db);
    let trait_func_generic_params =
        ctx.db.concrete_trait_function_generic_params(concrete_trait_function_id).unwrap();

    let impl_lookup_context = ctx.resolver.impl_lookup_context();
    let inference = &mut ctx.resolver.inference();
    let generic_function = inference.infer_trait_generic_function(
        concrete_trait_function_id,
        &impl_lookup_context,
        Some(stable_ptr),
    );
    let generic_args = ctx.resolver.resolve_generic_args(
        ctx.diagnostics,
        GenericSubstitution::from_impl(generic_function.impl_id),
        &trait_func_generic_params,
        &generic_args_syntax.unwrap_or_default(),
        stable_ptr,
    )?;

    Ok((
        FunctionLongId {
            function: ConcreteFunction {
                generic_function: GenericFunctionId::Impl(generic_function),
                generic_args,
            },
        }
        .intern(ctx.db),
        n_snapshots,
    ))
}

/// Returns all the trait functions that fit the given function name, can be called on the given
/// `self_ty`, and have at least one implementation in context.
///
/// `inference_errors` are aggregated here but are not reported here as diagnostics.
/// The caller has to make sure the diagnostics are reported appropriately.
pub fn filter_candidate_traits<'db>(
    ctx: &mut ComputationContext<'db, '_>,
    inference_errors: &mut Vec<(TraitFunctionId<'db>, InferenceError<'db>)>,
    self_ty: TypeId<'db>,
    candidate_traits: &[TraitId<'db>],
    function_name: SmolStrId<'db>,
    stable_ptr: SyntaxStablePtrId<'db>,
) -> Vec<TraitFunctionId<'db>> {
    let mut candidates = Vec::new();
    for trait_id in candidate_traits.iter().copied() {
        let Ok(trait_functions) = ctx.db.trait_functions(trait_id) else {
            continue;
        };
        for (name, trait_function) in trait_functions {
            if name == function_name
                && can_infer_impl_by_self(
                    ctx,
                    inference_errors,
                    trait_function,
                    self_ty,
                    stable_ptr,
                )
            {
                candidates.push(trait_function);
            }
        }
    }
    candidates
}

// === Impl Item Type definition ===

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplItemTypeData<'db> {
    type_alias_data: TypeAliasData<'db>,
    trait_type_id: Maybe<TraitTypeId<'db>>,
    /// The diagnostics of the module type alias, including the ones for the type alias itself.
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,
}

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_type_def_semantic_diagnostics].
pub fn impl_type_def_semantic_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_impl_type_semantic_data(impl_type_def_id, false)
        .map(|data| data.diagnostics)
        .unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_def_resolved_type].
pub fn impl_type_def_resolved_type<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Maybe<TypeId<'db>> {
    db.priv_impl_type_semantic_data(impl_type_def_id, false)?.type_alias_data.resolved_type
}

/// Cycle handling for [crate::db::SemanticGroup::impl_type_def_resolved_type].
pub fn impl_type_def_resolved_type_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Maybe<TypeId<'db>> {
    db.priv_impl_type_semantic_data(impl_type_def_id, true)?.type_alias_data.resolved_type
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_def_generic_params].
pub fn impl_type_def_generic_params<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Maybe<Vec<GenericParam<'db>>> {
    Ok(db.priv_impl_type_def_generic_params_data(impl_type_def_id)?.generic_params)
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_def_attributes].
pub fn impl_type_def_attributes<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Maybe<Vec<Attribute<'db>>> {
    Ok(db.priv_impl_type_semantic_data(impl_type_def_id, false)?.type_alias_data.attributes)
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_def_resolver_data].
pub fn impl_type_def_resolver_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    Ok(db.priv_impl_type_semantic_data(impl_type_def_id, false)?.type_alias_data.resolver_data)
}

/// Query implementation of [crate::db::SemanticGroup::impl_type_def_trait_type].
pub fn impl_type_def_trait_type<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Maybe<TraitTypeId<'db>> {
    db.priv_impl_type_semantic_data(impl_type_def_id, false)?.trait_type_id
}

// --- Computation ---

/// Query implementation of [crate::db::SemanticGroup::priv_impl_type_semantic_data].
pub fn priv_impl_type_semantic_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
    in_cycle: bool,
) -> Maybe<ImplItemTypeData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();
    let impl_type_defs = db.impl_types(impl_type_def_id.impl_def_id(db))?;
    let impl_type_def_ast = impl_type_defs.get(&impl_type_def_id).to_maybe()?;
    let generic_params_data = db.priv_impl_type_def_generic_params_data(impl_type_def_id)?;
    let lookup_item_id = LookupItemId::ImplItem(ImplItemId::Type(impl_type_def_id));

    let trait_type_id =
        validate_impl_item_type(db, &mut diagnostics, impl_type_def_id, impl_type_def_ast);

    if in_cycle {
        Ok(ImplItemTypeData {
            type_alias_data: type_alias_semantic_data_cycle_helper(
                db,
                &mut diagnostics,
                impl_type_def_ast,
                lookup_item_id,
                generic_params_data,
            )?,
            trait_type_id,
            diagnostics: diagnostics.build(),
        })
    } else {
        // TODO(yuval): resolve type aliases later, like in module type aliases, to avoid cycles in
        // non-cyclic chains.
        Ok(ImplItemTypeData {
            type_alias_data: type_alias_semantic_data_helper(
                db,
                &mut diagnostics,
                impl_type_def_ast,
                lookup_item_id,
                generic_params_data,
            )?,
            trait_type_id,
            diagnostics: diagnostics.build(),
        })
    }
}

/// Cycle handling for [crate::db::SemanticGroup::priv_impl_type_semantic_data].
pub fn priv_impl_type_semantic_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_type_def_id: ImplTypeDefId<'db>,
    _in_cycle: bool,
) -> Maybe<ImplItemTypeData<'db>> {
    // Forwarding cycle handling to `priv_impl_type_semantic_data` handler.
    priv_impl_type_semantic_data(db, impl_type_def_id, true)
}

/// Query implementation of [crate::db::SemanticGroup::priv_impl_type_def_generic_params_data].
pub fn priv_impl_type_def_generic_params_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_def_id: ImplTypeDefId<'db>,
) -> Maybe<GenericParamsData<'db>> {
    let module_file_id = impl_type_def_id.module_file_id(db);
    let impl_type_def_ast = db.impl_type_by_id(impl_type_def_id)?.to_maybe()?;
    let lookup_item_id = LookupItemId::ImplItem(ImplItemId::Type(impl_type_def_id));

    let impl_resolver_data = db.impl_def_resolver_data(impl_type_def_id.impl_def_id(db))?;
    type_alias_generic_params_data_helper(
        db,
        module_file_id,
        &impl_type_def_ast,
        lookup_item_id,
        Some(impl_resolver_data),
    )
}

/// Validates the impl item type, and returns the matching trait type id.
fn validate_impl_item_type<'db>(
    db: &'db dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics<'db>,
    impl_type_def_id: ImplTypeDefId<'db>,
    impl_type_ast: &ast::ItemTypeAlias<'db>,
) -> Maybe<TraitTypeId<'db>> {
    let impl_def_id = impl_type_def_id.impl_def_id(db);
    let concrete_trait_id = db.impl_def_concrete_trait(impl_def_id)?;
    let trait_id = concrete_trait_id.trait_id(db);
    let type_name = impl_type_def_id.name(db).intern(db);
    let trait_type_id = db.trait_type_by_name(trait_id, type_name)?.ok_or_else(|| {
        diagnostics.report(
            impl_type_ast.stable_ptr(db),
            ImplItemNotInTrait {
                impl_def_id,
                impl_item_name: type_name,
                trait_id,
                item_kind: "type".into(),
            },
        )
    })?;

    // TODO(yuval): add validations for generic parameters, then remove this.
    // Generic parameters are not yet supported, make sure there are none.
    let generic_params_node = impl_type_ast.generic_params(db);
    if !generic_params_node.is_empty(db) {
        diagnostics.report(
            generic_params_node.stable_ptr(db),
            GenericsNotSupportedInItem { scope: "Impl".into(), item_kind: "type".into() },
        );
    }

    Ok(trait_type_id)
}

// === Impl Type ===

/// Query implementation of [crate::db::SemanticGroup::impl_type_concrete_implized].
pub fn impl_type_concrete_implized<'db>(
    db: &'db dyn SemanticGroup,
    impl_type_id: ImplTypeId<'db>,
) -> Maybe<TypeId<'db>> {
    let concrete_impl = match impl_type_id.impl_id().long(db) {
        ImplLongId::Concrete(concrete_impl) => concrete_impl,
        ImplLongId::ImplImpl(imp_impl_id) => {
            let ImplLongId::Concrete(concrete_impl) =
                db.impl_impl_concrete_implized(*imp_impl_id)?.long(db)
            else {
                return Ok(TypeLongId::ImplType(impl_type_id).intern(db));
            };
            concrete_impl
        }
        ImplLongId::GenericParameter(_) | ImplLongId::SelfImpl(_) | ImplLongId::ImplVar(_) => {
            return Ok(TypeLongId::ImplType(impl_type_id).intern(db));
        }
        ImplLongId::GeneratedImpl(generated) => {
            return Ok(*generated.long(db).impl_items.0.get(&impl_type_id.ty()).unwrap());
        }
    };

    let impl_def_id = concrete_impl.impl_def_id(db);
    let ty = db.trait_type_implized_by_context(impl_type_id.ty(), impl_def_id);
    let Ok(ty) = ty else {
        return ty;
    };
    concrete_impl.substitution(db)?.substitute(db, ty)
}

/// Cycle handling for [crate::db::SemanticGroup::impl_type_concrete_implized].
pub fn impl_type_concrete_implized_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_type_id: ImplTypeId<'db>,
) -> Maybe<TypeId<'db>> {
    // Forwarding cycle handling to `priv_impl_type_semantic_data` handler.
    impl_type_concrete_implized(db, impl_type_id)
}

// === Impl Item Constant definition ===

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplItemConstantData<'db> {
    constant_data: ConstantData<'db>,
    trait_constant_id: Maybe<TraitConstantId<'db>>,
    /// The diagnostics of the impl constant, including the ones for the constant itself.
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,
}

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_constant_def_semantic_diagnostics].
pub fn impl_constant_def_semantic_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_def_id: ImplConstantDefId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_impl_constant_semantic_data(impl_constant_def_id, false)
        .map(|data| data.diagnostics)
        .unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_constant_def_value].
pub fn impl_constant_def_value<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_def_id: ImplConstantDefId<'db>,
) -> Maybe<ConstValueId<'db>> {
    Ok(db.priv_impl_constant_semantic_data(impl_constant_def_id, false)?.constant_data.const_value)
}

/// Cycle handling for [crate::db::SemanticGroup::impl_constant_def_value].
pub fn impl_constant_def_value_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_constant_def_id: ImplConstantDefId<'db>,
) -> Maybe<ConstValueId<'db>> {
    Ok(db.priv_impl_constant_semantic_data(impl_constant_def_id, true)?.constant_data.const_value)
}

/// Query implementation of [crate::db::SemanticGroup::impl_constant_def_resolver_data].
pub fn impl_constant_def_resolver_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_def_id: ImplConstantDefId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    Ok(db
        .priv_impl_constant_semantic_data(impl_constant_def_id, false)?
        .constant_data
        .resolver_data)
}

/// Query implementation of [crate::db::SemanticGroup::impl_constant_def_trait_constant].
pub fn impl_constant_def_trait_constant<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_def_id: ImplConstantDefId<'db>,
) -> Maybe<TraitConstantId<'db>> {
    db.priv_impl_constant_semantic_data(impl_constant_def_id, false)?.trait_constant_id
}

// --- Computation ---

/// Query implementation of [crate::db::SemanticGroup::priv_impl_constant_semantic_data].
pub fn priv_impl_constant_semantic_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_def_id: ImplConstantDefId<'db>,
    in_cycle: bool,
) -> Maybe<ImplItemConstantData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();
    let impl_def_id = impl_constant_def_id.impl_def_id(db);
    let impl_constant_defs = db.impl_constants(impl_def_id)?;
    let impl_constant_def_ast = impl_constant_defs.get(&impl_constant_def_id).to_maybe()?;
    let lookup_item_id = LookupItemId::ImplItem(ImplItemId::Constant(impl_constant_def_id));

    let inference_id = InferenceId::LookupItemGenerics(LookupItemId::ImplItem(
        ImplItemId::Constant(impl_constant_def_id),
    ));
    let resolver_data = db.impl_def_resolver_data(impl_def_id)?;
    let mut resolver =
        Resolver::with_data(db, resolver_data.clone_with_inference_id(db, inference_id));

    let trait_constant_id = validate_impl_item_constant(
        db,
        &mut diagnostics,
        impl_constant_def_id,
        impl_constant_def_ast,
        &mut resolver,
    );
    let mut constant_data = if in_cycle {
        constant_semantic_data_cycle_helper(
            db,
            impl_constant_def_ast,
            lookup_item_id,
            Some(Arc::new(resolver.data)),
            &impl_def_id,
        )?
    } else {
        constant_semantic_data_helper(
            db,
            impl_constant_def_ast,
            lookup_item_id,
            Some(Arc::new(resolver.data)),
            &impl_def_id,
        )?
    };
    diagnostics.extend(mem::take(&mut constant_data.diagnostics));
    Ok(ImplItemConstantData { constant_data, trait_constant_id, diagnostics: diagnostics.build() })
}

/// Cycle handling for [crate::db::SemanticGroup::priv_impl_constant_semantic_data].
pub fn priv_impl_constant_semantic_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_constant_def_id: ImplConstantDefId<'db>,
    _in_cycle: bool,
) -> Maybe<ImplItemConstantData<'db>> {
    // Forwarding cycle handling to `priv_impl_constant_semantic_data` handler.
    priv_impl_constant_semantic_data(db, impl_constant_def_id, true)
}

/// Validates the impl item constant, and returns the matching trait constant id.
fn validate_impl_item_constant<'db>(
    db: &'db dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics<'db>,
    impl_constant_def_id: ImplConstantDefId<'db>,
    impl_constant_ast: &ast::ItemConstant<'db>,
    resolver: &mut Resolver<'db>,
) -> Maybe<TraitConstantId<'db>> {
    let impl_def_id = impl_constant_def_id.impl_def_id(db);
    let concrete_trait_id = db.impl_def_concrete_trait(impl_def_id)?;
    let trait_id = concrete_trait_id.trait_id(db);
    let constant_name = impl_constant_def_id.name(db);

    let trait_constant_id =
        db.trait_constant_by_name(trait_id, constant_name.clone())?.ok_or_else(|| {
            diagnostics.report(
                impl_constant_ast.stable_ptr(db),
                ImplItemNotInTrait {
                    impl_def_id,
                    impl_item_name: constant_name.intern(db),
                    trait_id,
                    item_kind: "const".into(),
                },
            )
        })?;
    let concrete_trait_constant =
        ConcreteTraitConstantId::new_from_data(db, concrete_trait_id, trait_constant_id);
    let concrete_trait_constant_ty = db.concrete_trait_constant_type(concrete_trait_constant)?;

    let impl_constant_type_clause_ast = impl_constant_ast.type_clause(db);

    let constant_ty =
        resolve_type(db, diagnostics, resolver, &impl_constant_type_clause_ast.ty(db));

    let inference = &mut resolver.inference();

    let expected_ty = inference.rewrite(concrete_trait_constant_ty).no_err();
    let actual_ty = inference.rewrite(constant_ty).no_err();
    if expected_ty != actual_ty {
        diagnostics.report(
            impl_constant_type_clause_ast.stable_ptr(db),
            WrongType { expected_ty, actual_ty },
        );
    }
    Ok(trait_constant_id)
}

// === Impl Constant ===

/// Query implementation of [crate::db::SemanticGroup::impl_constant_implized_by_context].
pub fn impl_constant_implized_by_context<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_id: ImplConstantId<'db>,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<ConstValueId<'db>> {
    let impl_constant_def_id: ImplConstantDefId<'_> =
        db.impl_constant_by_trait_constant(impl_def_id, impl_constant_id.trait_constant_id())?;

    db.impl_constant_def_value(impl_constant_def_id)
}

/// Cycle handling for [crate::db::SemanticGroup::impl_constant_implized_by_context].
pub fn impl_constant_implized_by_context_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_constant_id: ImplConstantId<'db>,
    impl_def_id: ImplDefId<'db>,
) -> Maybe<ConstValueId<'db>> {
    // Forwarding cycle handling to `priv_impl_constant_semantic_data` handler.
    impl_constant_implized_by_context(db, impl_constant_id, impl_def_id)
}

/// Query implementation of [crate::db::SemanticGroup::impl_constant_concrete_implized_value].
pub fn impl_constant_concrete_implized_value<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_id: ImplConstantId<'db>,
) -> Maybe<ConstValueId<'db>> {
    if let ImplLongId::Concrete(concrete_impl) = impl_constant_id.impl_id().long(db) {
        let impl_def_id = concrete_impl.impl_def_id(db);
        let constant: ConstValueId<'db> =
            db.impl_constant_implized_by_context(impl_constant_id, impl_def_id)?;
        return concrete_impl.substitution(db)?.substitute(db, constant);
    }
    let substitution: GenericSubstitution<'db> =
        GenericSubstitution::from_impl(impl_constant_id.impl_id());
    let substitution_id = substitution.substitute(db, impl_constant_id)?;
    let const_val: ConstValue<'db> = ConstValue::ImplConstant(substitution_id);
    Ok(const_val.intern(db))
}

/// Cycle handling for [crate::db::SemanticGroup::impl_constant_concrete_implized_value].
pub fn impl_constant_concrete_implized_value_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_constant_id: ImplConstantId<'db>,
) -> Maybe<ConstValueId<'db>> {
    // Forwarding cycle handling to `priv_impl_const_semantic_data` handler.
    impl_constant_concrete_implized_value(db, impl_constant_id)
}

/// Query implementation of [crate::db::SemanticGroup::impl_constant_concrete_implized_type].
pub fn impl_constant_concrete_implized_type<'db>(
    db: &'db dyn SemanticGroup,
    impl_constant_id: ImplConstantId<'db>,
) -> Maybe<TypeId<'db>> {
    let concrete_trait_id = match impl_constant_id.impl_id().long(db) {
        ImplLongId::Concrete(concrete_impl) => {
            let impl_def_id = concrete_impl.impl_def_id(db);
            let ty = db.impl_constant_implized_by_context(impl_constant_id, impl_def_id)?.ty(db)?;
            return concrete_impl.substitution(db)?.substitute(db, ty);
        }
        ImplLongId::GenericParameter(param) => {
            let param_impl =
                extract_matches!(db.generic_param_semantic(*param)?, GenericParam::Impl);
            param_impl.concrete_trait?
        }
        ImplLongId::ImplVar(var) => var.long(db).concrete_trait_id,
        ImplLongId::ImplImpl(impl_impl) => db.impl_impl_concrete_trait(*impl_impl)?,
        ImplLongId::SelfImpl(concrete_trait_id) => *concrete_trait_id,
        ImplLongId::GeneratedImpl(generated_impl) => generated_impl.concrete_trait(db),
    };

    let ty = db.concrete_trait_constant_type(ConcreteTraitConstantId::new_from_data(
        db,
        concrete_trait_id,
        impl_constant_id.trait_constant_id(),
    ))?;
    GenericSubstitution::from_impl(impl_constant_id.impl_id()).substitute(db, ty)
}

/// Cycle handling for [crate::db::SemanticGroup::impl_constant_concrete_implized_type].
pub fn impl_constant_concrete_implized_type_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_constant_id: ImplConstantId<'db>,
) -> Maybe<TypeId<'db>> {
    // Forwarding cycle handling to `priv_impl_const_semantic_data` handler.
    impl_constant_concrete_implized_type(db, impl_constant_id)
}

// === Impl Item Impl definition ===

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplItemImplData<'db> {
    impl_data: ImplAliasData<'db>,
    trait_impl_id: Maybe<TraitImplId<'db>>,
    /// The diagnostics of the impl impl, including the ones for the impl itself.
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,
}

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_impl_def_semantic_diagnostics].
pub fn impl_impl_def_semantic_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_def_id: ImplImplDefId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_impl_impl_semantic_data(impl_impl_def_id, false)
        .map(|data| data.diagnostics)
        .unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_def_resolver_data].
pub fn impl_impl_def_resolver_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_def_id: ImplImplDefId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    Ok(db.priv_impl_impl_semantic_data(impl_impl_def_id, false)?.impl_data.resolver_data)
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_def_trait_impl].
pub fn impl_impl_def_trait_impl<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_def_id: ImplImplDefId<'db>,
) -> Maybe<TraitImplId<'db>> {
    db.priv_impl_impl_semantic_data(impl_impl_def_id, false)?.trait_impl_id
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_def_impl].
pub fn impl_impl_def_impl<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_def_id: ImplImplDefId<'db>,
    in_cycle: bool,
) -> Maybe<ImplId<'db>> {
    db.priv_impl_impl_semantic_data(impl_impl_def_id, in_cycle)?.impl_data.resolved_impl
}
/// Cycle handling for [crate::db::SemanticGroup::impl_impl_def_impl].
pub fn impl_impl_def_impl_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_impl_def_id: ImplImplDefId<'db>,
    _in_cycle: bool,
) -> Maybe<ImplId<'db>> {
    db.priv_impl_impl_semantic_data(impl_impl_def_id, true)?.impl_data.resolved_impl
}

// --- Computation ---

/// Query implementation of [crate::db::SemanticGroup::priv_impl_impl_semantic_data].
pub fn priv_impl_impl_semantic_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_def_id: ImplImplDefId<'db>,
    in_cycle: bool,
) -> Maybe<ImplItemImplData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();
    let impl_def_id = impl_impl_def_id.impl_def_id(db);
    let impl_impl_defs = db.impl_impls(impl_def_id)?;
    let impl_impl_def_ast = impl_impl_defs.get(&impl_impl_def_id).to_maybe()?;
    let generic_params_data = db.priv_impl_impl_def_generic_params_data(impl_impl_def_id)?;
    let lookup_item_id = LookupItemId::ImplItem(ImplItemId::Impl(impl_impl_def_id));

    let inference_id = InferenceId::LookupItemGenerics(lookup_item_id);
    let resolver_data = db.impl_def_resolver_data(impl_def_id)?;
    let mut resolver =
        Resolver::with_data(db, resolver_data.clone_with_inference_id(db, inference_id));

    let mut impl_data = if in_cycle {
        impl_alias_semantic_data_cycle_helper(
            db,
            impl_impl_def_ast,
            lookup_item_id,
            generic_params_data,
        )?
    } else {
        impl_alias_semantic_data_helper(db, impl_impl_def_ast, lookup_item_id, generic_params_data)?
    };

    diagnostics.extend(mem::take(&mut impl_data.diagnostics));

    let trait_impl_id = validate_impl_item_impl(
        db,
        &mut diagnostics,
        impl_impl_def_id,
        impl_impl_def_ast,
        &impl_data,
        &mut resolver,
    );

    Ok(ImplItemImplData { impl_data, trait_impl_id, diagnostics: diagnostics.build() })
}

/// Cycle handling for [crate::db::SemanticGroup::priv_impl_impl_semantic_data].
pub fn priv_impl_impl_semantic_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_impl_def_id: ImplImplDefId<'db>,
    _in_cycle: bool,
) -> Maybe<ImplItemImplData<'db>> {
    // Forwarding cycle handling to `priv_impl_impl_semantic_data` handler.
    priv_impl_impl_semantic_data(db, impl_impl_def_id, true)
}

/// Query implementation of [crate::db::SemanticGroup::priv_impl_impl_def_generic_params_data].
pub fn priv_impl_impl_def_generic_params_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_def_id: ImplImplDefId<'db>,
) -> Maybe<GenericParamsData<'db>> {
    let module_file_id = impl_impl_def_id.module_file_id(db);
    let impl_impl_def_ast = db.impl_impl_by_id(impl_impl_def_id)?.to_maybe()?;
    let lookup_item_id = LookupItemId::ImplItem(ImplItemId::Impl(impl_impl_def_id));

    let impl_resolver_data = db.impl_def_resolver_data(impl_impl_def_id.impl_def_id(db))?;
    impl_alias_generic_params_data_helper(
        db,
        module_file_id,
        &impl_impl_def_ast,
        lookup_item_id,
        Some(impl_resolver_data),
    )
}

/// Validates the impl item impl, and returns the matching trait impl id.
fn validate_impl_item_impl<'db>(
    db: &'db dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics<'db>,
    impl_impl_def_id: ImplImplDefId<'db>,
    impl_impl_ast: &ast::ItemImplAlias<'db>,
    impl_data: &ImplAliasData<'db>,
    resolver: &mut Resolver<'db>,
) -> Maybe<TraitImplId<'db>> {
    let impl_def_id = impl_impl_def_id.impl_def_id(db);
    let concrete_trait_id = db.impl_def_concrete_trait(impl_def_id)?;
    let trait_id = concrete_trait_id.trait_id(db);
    let impl_name = impl_impl_def_id.name(db);
    let trait_impl_id = db.trait_impl_by_name(trait_id, impl_name.clone())?.ok_or_else(|| {
        diagnostics.report(
            impl_impl_ast.stable_ptr(db),
            ImplItemNotInTrait {
                impl_def_id,
                impl_item_name: impl_name.intern(db),
                trait_id,
                item_kind: "impl".into(),
            },
        )
    })?;

    // TODO(TomerStarkware): add validations for generic parameters, then remove this.
    // Generic parameters are not yet supported, make sure there are none.
    let generic_params_node = impl_impl_ast.generic_params(db);
    if !generic_params_node.is_empty(db) {
        diagnostics.report(
            generic_params_node.stable_ptr(db),
            GenericsNotSupportedInItem { scope: "Impl".into(), item_kind: "impl".into() },
        );
    }

    let concrete_trait_impl =
        ConcreteTraitImplId::new_from_data(db, concrete_trait_id, trait_impl_id);
    let impl_def_substitution = db.impl_def_substitution(impl_def_id)?;

    let concrete_trait_impl_concrete_trait = db
        .concrete_trait_impl_concrete_trait(concrete_trait_impl)
        .and_then(|concrete_trait_id| impl_def_substitution.substitute(db, concrete_trait_id));

    let resolved_impl_concrete_trait =
        impl_data.resolved_impl.and_then(|imp| imp.concrete_trait(db));
    // used an IIFE to allow the use of the `?` operator.
    let _ = (|| -> Result<(), DiagnosticAdded> {
        if resolver
            .inference()
            .conform_traits(resolved_impl_concrete_trait?, concrete_trait_impl_concrete_trait?)
            .is_err()
        {
            diagnostics.report(
                impl_impl_ast.stable_ptr(db),
                TraitMismatch {
                    expected_trt: concrete_trait_impl_concrete_trait?,
                    actual_trt: resolved_impl_concrete_trait?,
                },
            );
        }
        Ok(())
    })();

    Ok(trait_impl_id)
}

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplicitImplImplData<'db> {
    resolved_impl: Maybe<ImplId<'db>>,
    trait_impl_id: TraitImplId<'db>,
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,
}

/// Query implementation of [crate::db::SemanticGroup::implicit_impl_impl_semantic_diagnostics].
pub fn implicit_impl_impl_semantic_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_impl_id: TraitImplId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_implicit_impl_impl_semantic_data(impl_def_id, trait_impl_id, false)
        .map(|data| data.diagnostics)
        .unwrap_or_default()
}
/// Query implementation of [crate::db::SemanticGroup::implicit_impl_impl_impl].
pub fn implicit_impl_impl_impl<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_impl_id: TraitImplId<'db>,
    in_cycle: bool,
) -> Maybe<ImplId<'db>> {
    db.priv_implicit_impl_impl_semantic_data(impl_def_id, trait_impl_id, in_cycle)?.resolved_impl
}
/// Cycle handling for [crate::db::SemanticGroup::implicit_impl_impl_impl].
pub fn implicit_impl_impl_impl_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_def_id: ImplDefId<'db>,
    trait_impl_id: TraitImplId<'db>,
    _in_cycle: bool,
) -> Maybe<ImplId<'db>> {
    db.priv_implicit_impl_impl_semantic_data(impl_def_id, trait_impl_id, true)?.resolved_impl
}

/// Query implementation of [crate::db::SemanticGroup::priv_implicit_impl_impl_semantic_data].
pub fn priv_implicit_impl_impl_semantic_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_def_id: ImplDefId<'db>,
    trait_impl_id: TraitImplId<'db>,
    in_cycle: bool,
) -> Maybe<ImplicitImplImplData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();
    if in_cycle {
        let err = Err(diagnostics.report(impl_def_id.stable_ptr(db).untyped(), ImplAliasCycle));
        return Ok(ImplicitImplImplData {
            resolved_impl: err,
            trait_impl_id,
            diagnostics: diagnostics.build(),
        });
    }
    let lookup_item_id = LookupItemId::ModuleItem(ModuleItemId::Impl(impl_def_id));

    let inference_id = InferenceId::LookupItemGenerics(lookup_item_id);
    let resolver_data = db.impl_def_resolver_data(impl_def_id)?;

    let mut resolver =
        Resolver::with_data(db, resolver_data.clone_with_inference_id(db, inference_id));
    // We cannot use `Self` as it will always find the implicit impl.
    resolver.trait_or_impl_ctx = TraitOrImplContext::None;

    let concrete_trait_impl_concrete_trait = db
        .impl_def_concrete_trait(impl_def_id)
        .and_then(|concrete_trait_id| {
            db.concrete_trait_impl_concrete_trait(ConcreteTraitImplId::new_from_data(
                db,
                concrete_trait_id,
                trait_impl_id,
            ))
        })
        .and_then(|concrete_trait_id| {
            let impl_def_substitution = db.impl_def_substitution(impl_def_id)?;
            impl_def_substitution.substitute(db, concrete_trait_id)
        });
    let impl_lookup_context = resolver.impl_lookup_context();
    let resolved_impl = concrete_trait_impl_concrete_trait.and_then(|concrete_trait_id| {
        let imp = resolver.inference().new_impl_var(concrete_trait_id, None, impl_lookup_context);
        resolver.inference().finalize_without_reporting().map_err(|(err_set, _)| {
            diagnostics.report(
                impl_def_id.stable_ptr(db).untyped(),
                ImplicitImplNotInferred { trait_impl_id, concrete_trait_id },
            );
            resolver.inference().report_on_pending_error(
                err_set,
                &mut diagnostics,
                impl_def_id.stable_ptr(db).untyped(),
            )
        })?;
        resolver.inference().rewrite(imp).map_err(|_| skip_diagnostic())
    });

    Ok(ImplicitImplImplData { resolved_impl, trait_impl_id, diagnostics: diagnostics.build() })
}
/// Cycle handling for [crate::db::SemanticGroup::priv_implicit_impl_impl_semantic_data].
pub fn priv_implicit_impl_impl_semantic_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_def_id: ImplDefId<'db>,
    trait_impl_id: TraitImplId<'db>,
    _in_cycle: bool,
) -> Maybe<ImplicitImplImplData<'db>> {
    // Forwarding cycle handling to `priv_implicit_impl_impl_semantic_data` handler.
    priv_implicit_impl_impl_semantic_data(db, impl_def_id, trait_impl_id, true)
}

// === Impl Impl ===

/// Query implementation of [crate::db::SemanticGroup::impl_impl_implized_by_context].
pub fn impl_impl_implized_by_context<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_id: ImplImplId<'db>,
    impl_def_id: ImplDefId<'db>,
    in_cycle: bool,
) -> Maybe<ImplId<'db>> {
    if db.is_implicit_impl_impl(impl_def_id, impl_impl_id.trait_impl_id())? {
        return db.implicit_impl_impl_impl(impl_def_id, impl_impl_id.trait_impl_id(), in_cycle);
    }

    let impl_impl_def_id = db.impl_impl_by_trait_impl(impl_def_id, impl_impl_id.trait_impl_id())?;

    db.impl_impl_def_impl(impl_impl_def_id, in_cycle)
}

/// Cycle handling for [crate::db::SemanticGroup::impl_impl_implized_by_context].
pub fn impl_impl_implized_by_context_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_impl_id: ImplImplId<'db>,
    impl_def_id: ImplDefId<'db>,
    _in_cycle: bool,
) -> Maybe<ImplId<'db>> {
    // Forwarding cycle handling to `priv_impl_impl_semantic_data` handler.
    impl_impl_implized_by_context(db, impl_impl_id, impl_def_id, true)
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_concrete_implized].
pub fn impl_impl_concrete_implized<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_id: ImplImplId<'db>,
) -> Maybe<ImplId<'db>> {
    impl_impl_concrete_implized_ex(db, impl_impl_id, false)
}

/// Cycle handling for [crate::db::SemanticGroup::impl_impl_concrete_implized].
pub fn impl_impl_concrete_implized_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    impl_impl_id: ImplImplId<'db>,
) -> Maybe<ImplId<'db>> {
    impl_impl_concrete_implized_ex(db, impl_impl_id, true)
}

fn impl_impl_concrete_implized_ex<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_id: ImplImplId<'db>,
    in_cycle: bool,
) -> Maybe<ImplId<'db>> {
    if let ImplLongId::Concrete(concrete_impl) = impl_impl_id.impl_id().long(db) {
        let impl_def_id = concrete_impl.impl_def_id(db);
        let imp = db.impl_impl_implized_by_context(impl_impl_id, impl_def_id, in_cycle)?;
        return concrete_impl.substitution(db)?.substitute(db, imp);
    }

    Ok(ImplLongId::ImplImpl(
        GenericSubstitution::from_impl(impl_impl_id.impl_id()).substitute(db, impl_impl_id)?,
    )
    .intern(db))
}

/// Query implementation of [crate::db::SemanticGroup::impl_impl_concrete_trait].
pub fn impl_impl_concrete_trait<'db>(
    db: &'db dyn SemanticGroup,
    impl_impl_id: ImplImplId<'db>,
) -> Maybe<ConcreteTraitId<'db>> {
    let concrete_trait_impl = impl_impl_id.concrete_trait_impl_id(db)?;
    db.concrete_trait_impl_concrete_trait(concrete_trait_impl).and_then(|concrete_trait_id| {
        GenericSubstitution::from_impl(impl_impl_id.impl_id()).substitute(db, concrete_trait_id)
    })
}

// === Impl Function Declaration ===

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct ImplFunctionDeclarationData<'db> {
    pub function_declaration_data: FunctionDeclarationData<'db>,
    trait_function_id: Maybe<TraitFunctionId<'db>>,
}

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_function_declaration_diagnostics].
pub fn impl_function_declaration_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_impl_function_declaration_data(impl_function_id)
        .map(|data| data.function_declaration_data.diagnostics)
        .unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_signature].
pub fn impl_function_signature<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<semantic::Signature<'db>> {
    Ok(db
        .priv_impl_function_declaration_data(impl_function_id)?
        .function_declaration_data
        .signature)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_generic_params].
pub fn impl_function_generic_params<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<Vec<semantic::GenericParam<'db>>> {
    Ok(db.priv_impl_function_generic_params_data(impl_function_id)?.generic_params)
}

/// Query implementation of [crate::db::SemanticGroup::priv_impl_function_generic_params_data].
pub fn priv_impl_function_generic_params_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<GenericParamsData<'db>> {
    let module_file_id = impl_function_id.module_file_id(db);
    let mut diagnostics = SemanticDiagnostics::default();
    let impl_def_id = impl_function_id.impl_def_id(db);
    let data = db.priv_impl_definition_data(impl_def_id)?;
    let function_syntax = &data.function_asts[&impl_function_id];
    let declaration = function_syntax.declaration(db);
    let inference_id = InferenceId::LookupItemGenerics(LookupItemId::ImplItem(
        ImplItemId::Function(impl_function_id),
    ));
    let resolver_data = db.impl_def_resolver_data(impl_def_id)?;
    let mut resolver =
        Resolver::with_data(db, resolver_data.clone_with_inference_id(db, inference_id));
    let generic_params = semantic_generic_params(
        db,
        &mut diagnostics,
        &mut resolver,
        module_file_id,
        &declaration.generic_params(db),
    );
    let inference = &mut resolver.inference();
    inference.finalize(&mut diagnostics, function_syntax.stable_ptr(db).untyped());

    let generic_params = inference.rewrite(generic_params).no_err();
    let resolver_data = Arc::new(resolver.data);
    Ok(GenericParamsData { generic_params, diagnostics: diagnostics.build(), resolver_data })
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_attributes].
pub fn impl_function_attributes<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<Vec<Attribute<'db>>> {
    Ok(db
        .priv_impl_function_declaration_data(impl_function_id)?
        .function_declaration_data
        .attributes)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_resolver_data].
pub fn impl_function_resolver_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    Ok(db
        .priv_impl_function_declaration_data(impl_function_id)?
        .function_declaration_data
        .resolver_data)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_declaration_inline_config].
pub fn impl_function_declaration_inline_config<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<InlineConfiguration<'db>> {
    Ok(db
        .priv_impl_function_declaration_data(impl_function_id)?
        .function_declaration_data
        .inline_config)
}

/// Query implementation of [SemanticGroup::impl_function_declaration_implicit_precedence].
pub fn impl_function_declaration_implicit_precedence<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<ImplicitPrecedence<'db>> {
    Ok(db
        .priv_impl_function_declaration_data(impl_function_id)?
        .function_declaration_data
        .implicit_precedence)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_declaration_implicits].
pub fn impl_function_declaration_implicits<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<Vec<TypeId<'db>>> {
    Ok(db
        .priv_impl_function_declaration_data(impl_function_id)?
        .function_declaration_data
        .signature
        .implicits)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_trait_function].
pub fn impl_function_trait_function<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<TraitFunctionId<'db>> {
    db.priv_impl_function_declaration_data(impl_function_id)?.trait_function_id
}

// --- Computation ---

/// Query implementation of [crate::db::SemanticGroup::priv_impl_function_declaration_data].
pub fn priv_impl_function_declaration_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<ImplFunctionDeclarationData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();
    let impl_def_id = impl_function_id.impl_def_id(db);
    let data = db.priv_impl_definition_data(impl_def_id)?;
    let function_syntax = &data.function_asts[&impl_function_id];
    let declaration = function_syntax.declaration(db);

    let generic_params_data = db.priv_impl_function_generic_params_data(impl_function_id)?;
    let generic_params = generic_params_data.generic_params;
    let lookup_item_id = LookupItemId::ImplItem(ImplItemId::Function(impl_function_id));
    let inference_id = InferenceId::LookupItemGenerics(lookup_item_id);
    let mut resolver = Resolver::with_data(
        db,
        (*generic_params_data.resolver_data).clone_with_inference_id(db, inference_id),
    );
    diagnostics.extend(generic_params_data.diagnostics);
    resolver.set_feature_config(&impl_function_id, function_syntax, &mut diagnostics);

    let mut environment = Environment::empty();
    let signature = semantic::Signature::from_ast(
        &mut diagnostics,
        db,
        &mut resolver,
        &declaration,
        FunctionTitleId::Impl(impl_function_id),
        &mut environment,
    );

    let attributes = function_syntax.attributes(db).structurize(db);
    let (implicit_precedence, _) =
        get_implicit_precedence(db, &mut diagnostics, &mut resolver, &attributes);

    let inference = &mut resolver.inference();
    // Check fully resolved.
    inference.finalize(&mut diagnostics, function_syntax.stable_ptr(db).untyped());
    let signature_syntax = declaration.signature(db);
    let trait_function_id = validate_impl_function_signature(
        db,
        &mut diagnostics,
        inference,
        ValidateImplFunctionSignatureParams {
            impl_function_id,
            signature_syntax: &signature_syntax,
            signature: &signature,
            impl_function_syntax: function_syntax,
            impl_func_generics: &generic_params,
        },
    );

    let inline_config = get_inline_config(db, &mut diagnostics, &attributes)?;

    forbid_inline_always_with_impl_generic_param(&mut diagnostics, &generic_params, &inline_config);

    let signature = inference.rewrite(signature).no_err();
    let generic_params = inference.rewrite(generic_params).no_err();

    let resolver_data = Arc::new(resolver.data);
    Ok(ImplFunctionDeclarationData {
        function_declaration_data: FunctionDeclarationData {
            diagnostics: diagnostics.build(),
            signature,
            generic_params,
            environment,
            attributes,
            resolver_data,
            inline_config,
            implicit_precedence,
        },
        trait_function_id,
    })
}

/// Struct for the parameters of [validate_impl_function_signature].
struct ValidateImplFunctionSignatureParams<'a, 'r> {
    /// The impl function to validate the signature of.
    impl_function_id: ImplFunctionId<'a>,
    /// The signature syntax.
    signature_syntax: &'r ast::FunctionSignature<'a>,
    // The semantic signature.
    signature: &'r semantic::Signature<'a>,
    /// The impl function syntax.
    impl_function_syntax: &'r ast::FunctionWithBody<'a>,
    /// The generic parameters of the impl function.
    impl_func_generics: &'r [GenericParam<'a>],
}

/// Validates the impl function, and returns the matching trait function id.
fn validate_impl_function_signature<'db>(
    db: &'db dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics<'db>,
    inference: &mut Inference<'db, '_>,
    ValidateImplFunctionSignatureParams {
        impl_function_id,
        signature_syntax,
        signature,
        impl_function_syntax,
        impl_func_generics,
    }: ValidateImplFunctionSignatureParams<'db, '_>,
) -> Maybe<TraitFunctionId<'db>> {
    let impl_def_id = impl_function_id.impl_def_id(db);
    let concrete_trait_id = db.impl_def_concrete_trait(impl_def_id)?;
    let trait_id = concrete_trait_id.trait_id(db);
    let function_name = impl_function_id.name(db).intern(db);
    let trait_function_id =
        db.trait_function_by_name(trait_id, function_name)?.ok_or_else(|| {
            diagnostics.report(
                impl_function_syntax.stable_ptr(db),
                ImplItemNotInTrait {
                    impl_def_id,
                    impl_item_name: function_name,
                    trait_id,
                    item_kind: "function".into(),
                },
            )
        })?;
    let concrete_trait_function =
        ConcreteTraitGenericFunctionId::new_from_data(db, concrete_trait_id, trait_function_id);
    let concrete_trait_signature = db.concrete_trait_function_signature(concrete_trait_function)?;

    // Match generics of the function.
    // TODO(spapini): Compare the actual kinds and traits for the generic params.

    let func_generics = db.concrete_trait_function_generic_params(concrete_trait_function)?;
    if impl_func_generics.len() != func_generics.len() {
        diagnostics.report(
            impl_function_syntax.declaration(db).name(db).stable_ptr(db),
            WrongNumberOfGenericParamsForImplFunction {
                expected: func_generics.len(),
                actual: impl_func_generics.len(),
            },
        );
        return Ok(trait_function_id);
    }
    let impl_def_substitution = db.impl_def_substitution(impl_def_id)?;
    let func_generics: Vec<GenericParam<'_>> =
        impl_def_substitution.substitute(db, func_generics)?;

    let function_substitution =
        GenericSubstitution::new(&func_generics, &generic_params_to_args(impl_func_generics, db));

    for (trait_generic_param, generic_param) in izip!(func_generics, impl_func_generics.iter()) {
        if let Some(name) = trait_generic_param.id().name(db) {
            if Some(name.clone()) != generic_param.id().name(db) {
                diagnostics.report(
                    generic_param.stable_ptr(db),
                    WrongParameterName {
                        impl_def_id,
                        impl_function_id,
                        trait_id,
                        expected_name: name.intern(db),
                    },
                );
            }
        }
        match (generic_param, trait_generic_param) {
            (GenericParam::Type(_), GenericParam::Type(_)) => {}
            (GenericParam::Impl(generic_param), GenericParam::Impl(trait_generic_param))
            | (GenericParam::NegImpl(generic_param), GenericParam::NegImpl(trait_generic_param)) => {
                let rewritten_trait_param_trait =
                    function_substitution.substitute(db, trait_generic_param.concrete_trait)?;
                let rewritten_trait_param_type_constraints =
                    function_substitution.substitute(db, trait_generic_param.type_constraints)?;
                generic_param
                    .concrete_trait
                    .map(|actual_trait| {
                        rewritten_trait_param_trait
                            .map(|expected_trait| {
                                if actual_trait != expected_trait
                                    || generic_param.type_constraints
                                        != rewritten_trait_param_type_constraints
                                {
                                    diagnostics.report(
                                        generic_param.id.stable_ptr(db),
                                        WrongGenericParamTraitForImplFunction {
                                            impl_def_id,
                                            impl_function_id,
                                            trait_id,
                                            expected_trait,
                                            actual_trait,
                                        },
                                    );
                                }
                            })
                            .ok();
                    })
                    .ok();
            }
            (GenericParam::Const(generic_param), GenericParam::Const(trait_generic_param)) => {
                let expected_ty = function_substitution.substitute(db, trait_generic_param.ty)?;
                if generic_param.ty != expected_ty {
                    diagnostics.report(
                        generic_param.id.stable_ptr(db),
                        WrongParameterType {
                            impl_def_id,
                            impl_function_id,
                            trait_id,
                            expected_ty,
                            actual_ty: generic_param.ty,
                        },
                    );
                }
            }
            (generic_param, trait_generic_param) => {
                diagnostics.report(
                    generic_param.stable_ptr(db),
                    WrongGenericParamKindForImplFunction {
                        impl_def_id,
                        impl_function_id,
                        trait_id,
                        expected_kind: trait_generic_param.kind(),
                        actual_kind: generic_param.kind(),
                    },
                );
            }
        }
    }

    let concrete_trait_signature =
        function_substitution.substitute(db, concrete_trait_signature)?;

    if signature.params.len() != concrete_trait_signature.params.len() {
        diagnostics.report(
            signature_syntax.parameters(db).stable_ptr(db),
            WrongNumberOfParameters {
                impl_def_id,
                impl_function_id,
                trait_id,
                expected: concrete_trait_signature.params.len(),
                actual: signature.params.len(),
            },
        );
    }
    let concrete_trait_signature =
        impl_def_substitution.substitute(db, concrete_trait_signature)?;
    for (param, trait_param) in
        izip!(signature.params.iter(), concrete_trait_signature.params.iter())
    {
        let expected_ty = inference.rewrite(trait_param.ty).no_err();
        let actual_ty = inference.rewrite(param.ty).no_err();

        if expected_ty != actual_ty && !expected_ty.is_missing(db) && !actual_ty.is_missing(db) {
            diagnostics.report(
                extract_matches!(
                    param.stable_ptr(db).lookup(db).type_clause(db),
                    OptionTypeClause::TypeClause
                )
                .ty(db)
                .stable_ptr(db),
                WrongParameterType {
                    impl_def_id,
                    impl_function_id,
                    trait_id,
                    expected_ty,
                    actual_ty,
                },
            );
        }

        if trait_param.mutability != param.mutability {
            if trait_param.mutability == Mutability::Reference {
                diagnostics.report(
                    param.stable_ptr(db).lookup(db).modifiers(db).stable_ptr(db),
                    ParameterShouldBeReference { impl_def_id, impl_function_id, trait_id },
                );
            }

            if param.mutability == Mutability::Reference {
                diagnostics.report(
                    param.stable_ptr(db).lookup(db).modifiers(db).stable_ptr(db),
                    ParameterShouldNotBeReference { impl_def_id, impl_function_id, trait_id },
                );
            }
        }

        if trait_param.name != param.name {
            diagnostics.report(
                param.stable_ptr(db).lookup(db).name(db).stable_ptr(db),
                WrongParameterName {
                    impl_def_id,
                    impl_function_id,
                    trait_id,
                    expected_name: trait_param.name,
                },
            );
        }
    }

    if !concrete_trait_signature.panicable && signature.panicable {
        diagnostics.report(
            signature_syntax.stable_ptr(db),
            PassPanicAsNopanic { impl_function_id, trait_id },
        );
    }

    if concrete_trait_signature.is_const && !signature.is_const {
        diagnostics.report(
            signature_syntax.stable_ptr(db),
            PassConstAsNonConst { impl_function_id, trait_id },
        );
    }

    let expected_ty = inference.rewrite(concrete_trait_signature.return_type).no_err();
    let actual_ty = inference.rewrite(signature.return_type).no_err();

    if expected_ty != actual_ty && !expected_ty.is_missing(db) && !actual_ty.is_missing(db) {
        let location_ptr = match signature_syntax.ret_ty(db) {
            OptionReturnTypeClause::ReturnTypeClause(ret_ty) => ret_ty.ty(db).as_syntax_node(),
            OptionReturnTypeClause::Empty(_) => {
                impl_function_syntax.body(db).lbrace(db).as_syntax_node()
            }
        }
        .stable_ptr(db);
        diagnostics.report(
            location_ptr,
            WrongReturnTypeForImpl {
                impl_def_id,
                impl_function_id,
                trait_id,
                expected_ty,
                actual_ty,
            },
        );
    }
    Ok(trait_function_id)
}

// === Impl Function Body ===

// --- Selectors ---

/// Query implementation of [crate::db::SemanticGroup::impl_function_body_diagnostics].
pub fn impl_function_body_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_impl_function_body_data(impl_function_id)
        .map(|data| data.diagnostics)
        .unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_body].
pub fn impl_function_body<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<Arc<FunctionBody<'db>>> {
    Ok(db.priv_impl_function_body_data(impl_function_id)?.body)
}

/// Query implementation of [crate::db::SemanticGroup::impl_function_body_resolver_data].
pub fn impl_function_body_resolver_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    Ok(db.priv_impl_function_body_data(impl_function_id)?.resolver_data)
}

// --- Computation ---

/// Query implementation of [crate::db::SemanticGroup::priv_impl_function_body_data].
pub fn priv_impl_function_body_data<'db>(
    db: &'db dyn SemanticGroup,
    impl_function_id: ImplFunctionId<'db>,
) -> Maybe<FunctionBodyData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();
    let impl_def_id = impl_function_id.impl_def_id(db);
    let data = db.priv_impl_definition_data(impl_def_id)?;
    let function_syntax = &data.function_asts[&impl_function_id];
    // Compute declaration semantic.
    let declaration = db.priv_impl_function_declaration_data(impl_function_id)?;
    let parent_resolver_data = declaration.function_declaration_data.resolver_data;
    let inference_id = InferenceId::LookupItemDefinition(LookupItemId::ImplItem(
        ImplItemId::Function(impl_function_id),
    ));
    let resolver =
        Resolver::with_data(db, (*parent_resolver_data).clone_with_inference_id(db, inference_id));
    let environment: Environment<'_> = declaration.function_declaration_data.environment;

    let function_id = (|| {
        let trait_function_id = db.impl_function_trait_function(impl_function_id)?;
        let generic_parameters = db.impl_def_generic_params(impl_def_id)?;

        let generic_function = GenericFunctionId::Impl(ImplGenericFunctionId {
            impl_id: ImplLongId::Concrete(
                ConcreteImplLongId {
                    impl_def_id,
                    generic_args: generic_params_to_args(&generic_parameters, db),
                }
                .intern(db),
            )
            .intern(db),
            function: trait_function_id,
        });

        Ok(FunctionLongId::from_generic(db, generic_function)?.intern(db))
    })();
    // Compute body semantic expr.
    let mut ctx = ComputationContext::new(
        db,
        &mut diagnostics,
        resolver,
        Some(&declaration.function_declaration_data.signature),
        environment,
        ContextFunction::Function(function_id),
    );
    let function_body = function_syntax.body(db);
    let return_type = declaration.function_declaration_data.signature.return_type;
    let body_expr = compute_root_expr(&mut ctx, &function_body, return_type)?;
    let ComputationContext { arenas: Arenas { exprs, patterns, statements }, resolver, .. } = ctx;

    let expr_lookup: UnorderedHashMap<_, _> =
        exprs.iter().map(|(expr_id, expr)| (expr.stable_ptr(), expr_id)).collect();
    let pattern_lookup: UnorderedHashMap<_, _> =
        patterns.iter().map(|(pattern_id, pattern)| (pattern.stable_ptr(), pattern_id)).collect();
    let resolver_data = Arc::new(resolver.data);
    Ok(FunctionBodyData {
        diagnostics: diagnostics.build(),
        expr_lookup,
        pattern_lookup,
        resolver_data,
        body: Arc::new(FunctionBody { arenas: Arenas { exprs, patterns, statements }, body_expr }),
    })
}

pub fn priv_impl_is_fully_concrete(db: &dyn SemanticGroup, impl_id: ImplId<'_>) -> bool {
    impl_id.long(db).is_fully_concrete(db)
}

pub fn priv_impl_is_var_free(db: &dyn SemanticGroup, impl_id: ImplId<'_>) -> bool {
    impl_id.long(db).is_var_free(db)
}
