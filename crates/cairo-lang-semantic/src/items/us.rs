use std::sync::Arc;

use cairo_lang_defs::ids::{
    GlobalUseId, LanguageElementId, LookupItemId, ModuleId, ModuleItemId, UseId,
};
use cairo_lang_diagnostics::{Diagnostics, Maybe, ToMaybe};
use cairo_lang_proc_macros::DebugWithDb;
use cairo_lang_syntax::node::db::SyntaxGroup;
use cairo_lang_syntax::node::helpers::{GetIdentifier, UsePathEx};
use cairo_lang_syntax::node::kind::SyntaxKind;
use cairo_lang_syntax::node::{TypedSyntaxNode, ast};
use cairo_lang_utils::Upcast;
use cairo_lang_utils::ordered_hash_set::OrderedHashSet;
use cairo_lang_utils::unordered_hash_set::UnorderedHashSet;

use super::module::get_module_global_uses;
use super::visibility::peek_visible_in;
use crate::SemanticDiagnostic;
use crate::db::{SemanticGroup, SemanticGroupData};
use crate::diagnostic::SemanticDiagnosticKind::*;
use crate::diagnostic::{
    ElementKind, NotFoundItemType, SemanticDiagnostics, SemanticDiagnosticsBuilder,
};
use crate::expr::inference::InferenceId;
use crate::keyword::SELF_PARAM_KW;
use crate::resolve::{ResolutionContext, ResolvedGenericItem, Resolver, ResolverData};

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct UseData<'db> {
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,
    resolved_item: Maybe<ResolvedGenericItem<'db>>,
    resolver_data: Arc<ResolverData<'db>>,
}

/// Query implementation of [crate::db::SemanticGroup::priv_use_semantic_data].
pub fn priv_use_semantic_data<'db>(
    db: &'db dyn SemanticGroup,
    use_id: UseId<'db>,
) -> Maybe<UseData<'db>> {
    let module_file_id = use_id.module_file_id(db);
    let mut diagnostics = SemanticDiagnostics::default();
    let module_item_id = ModuleItemId::Use(use_id);
    let inference_id = InferenceId::LookupItemDeclaration(LookupItemId::ModuleItem(module_item_id));
    let mut resolver = Resolver::new(db, module_file_id, inference_id);
    // TODO(spapini): when code changes in a file, all the AST items change (as they contain a path
    // to the green root that changes. Once ASTs are rooted on items, use a selector that picks only
    // the item instead of all the module data.
    let use_ast = ast::UsePath::Leaf(db.module_use_by_id(use_id)?.to_maybe()?);
    let item = use_ast.get_item(db);
    resolver.set_feature_config(&use_id, &item, &mut diagnostics);
    let segments = get_use_path_segments(db, use_ast.clone())?;
    let resolved_item = match handle_self_path(db, &mut diagnostics, segments, use_ast) {
        Err(diag_added) => Err(diag_added),
        Ok(segments) => resolver.resolve_generic_path(
            &mut diagnostics,
            segments,
            NotFoundItemType::Identifier,
            ResolutionContext::ModuleItem(module_item_id),
        ),
    };
    let resolver_data: Arc<ResolverData<'_>> = Arc::new(resolver.data);

    Ok(UseData { diagnostics: diagnostics.build(), resolved_item, resolver_data })
}

/// Processes a `self` path in a `use` statement.
///
/// This function checks if the `self` keyword is used correctly in a `use`
/// statement and modifies the path segments accordingly. It also reports
/// diagnostics for invalid usage of `self`.
fn handle_self_path<'db>(
    db: &'db dyn SemanticGroup,
    diagnostics: &mut SemanticDiagnostics<'db>,
    mut segments: Vec<ast::PathSegment<'db>>,
    use_path: ast::UsePath<'db>,
) -> Maybe<Vec<ast::PathSegment<'db>>> {
    if let Some(last) = segments.last() {
        if last.identifier(db) == SELF_PARAM_KW {
            if use_path.as_syntax_node().parent(db).unwrap().kind(db) != SyntaxKind::UsePathList {
                diagnostics.report(use_path.stable_ptr(db), UseSelfNonMulti);
            }
            segments.pop();
        }
    }
    if segments.is_empty() {
        Err(diagnostics.report(use_path.stable_ptr(db), UseSelfEmptyPath))
    } else {
        Ok(segments)
    }
}

/// Returns the segments that are the parts of the use path.
///
/// The segments are returned in the order they appear in the use path.
/// Only `UsePathLeaf` and `UsePathSingle` are supported.
///
/// For example:
/// Given the `c` of `use a::b::{c, d};` will return `[a, b, c]`.
/// Given the `b` of `use a::b::{c, d};` will return `[a, b]`.
pub fn get_use_path_segments<'db>(
    db: &'db dyn SyntaxGroup,
    use_path: ast::UsePath<'db>,
) -> Maybe<Vec<ast::PathSegment<'db>>> {
    let mut rev_segments = vec![];
    match &use_path {
        ast::UsePath::Leaf(use_ast) => rev_segments.push(use_ast.ident(db)),
        ast::UsePath::Single(use_ast) => rev_segments.push(use_ast.ident(db)),
        ast::UsePath::Star(_) => {}
        ast::UsePath::Multi(_) => {
            panic!("Only `UsePathLeaf` and `UsePathSingle` are supported.")
        }
    }
    let mut current_use_path = use_path;
    while let Some(parent_use_path) = get_parent_single_use_path(db, &current_use_path) {
        rev_segments.push(parent_use_path.ident(db));
        current_use_path = ast::UsePath::Single(parent_use_path);
    }
    Ok(rev_segments.into_iter().rev().collect())
}

/// Returns the parent `UsePathSingle` of a use path if it exists.
fn get_parent_single_use_path<'db>(
    db: &'db dyn SyntaxGroup,
    use_path: &ast::UsePath<'db>,
) -> Option<ast::UsePathSingle<'db>> {
    use SyntaxKind::*;
    let mut node = use_path.as_syntax_node();
    loop {
        node = node.parent(db).expect("`UsePath` is not under an `ItemUse`.");
        match node.kind(db) {
            ItemUse => return None,
            UsePathSingle => return Some(ast::UsePathSingle::from_syntax_node(db, node)),
            UsePathList | UsePathMulti => continue,
            UsePathLeaf => unreachable!("`UsePathLeaf` can't be a parent of another `UsePath`."),
            other => unreachable!("`{other:?}` can't be a parent of `UsePath`."),
        };
    }
}

/// Cycle handling for [crate::db::SemanticGroup::priv_use_semantic_data].
pub fn priv_use_semantic_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    use_id: UseId<'db>,
) -> Maybe<UseData<'db>> {
    let module_file_id = use_id.module_file_id(db);
    let mut diagnostics = SemanticDiagnostics::default();
    let use_ast = db.module_use_by_id(use_id)?.to_maybe()?;
    let err = Err(diagnostics.report(use_ast.stable_ptr(db), UseCycle));
    let inference_id =
        InferenceId::LookupItemDeclaration(LookupItemId::ModuleItem(ModuleItemId::Use(use_id)));
    Ok(UseData {
        diagnostics: diagnostics.build(),
        resolved_item: err,
        resolver_data: Arc::new(ResolverData::new(module_file_id, inference_id)),
    })
}

/// Query implementation of [crate::db::SemanticGroup::use_semantic_diagnostics].
pub fn use_semantic_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    use_id: UseId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_use_semantic_data(use_id).map(|data| data.diagnostics).unwrap_or_default()
}

/// Query implementation of [crate::db::SemanticGroup::use_resolver_data].
pub fn use_resolver_data<'db>(
    db: &'db dyn SemanticGroup,
    use_id: UseId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    Ok(db.priv_use_semantic_data(use_id)?.resolver_data)
}

/// Trivial cycle handler for [crate::db::SemanticGroup::use_resolver_data].
pub fn use_resolver_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    use_id: UseId<'db>,
) -> Maybe<Arc<ResolverData<'db>>> {
    // Forwarding (not as a query) cycle handling to `priv_use_semantic_data` cycle handler.
    use_resolver_data(db, use_id)
}

pub trait SemanticUseEx<'a>: Upcast<'a, dyn SemanticGroup> {
    /// Returns the resolved item or an error if it can't be resolved.
    ///
    /// This is not a query as the cycle handling is done in priv_use_semantic_data.
    fn use_resolved_item(&'a self, use_id: UseId<'a>) -> Maybe<ResolvedGenericItem<'a>> {
        let db = self.upcast();
        db.priv_use_semantic_data(use_id)?.resolved_item
    }
}

impl<'a, T: Upcast<'a, dyn SemanticGroup> + ?Sized> SemanticUseEx<'a> for T {}

#[derive(Clone, Debug, PartialEq, Eq, DebugWithDb, salsa::Update)]
#[debug_db(dyn SemanticGroup)]
pub struct UseGlobalData<'db> {
    diagnostics: Diagnostics<'db, SemanticDiagnostic<'db>>,
    imported_module: Maybe<ModuleId<'db>>,
}

/// Query implementation of [crate::db::SemanticGroup::priv_global_use_semantic_data].
pub fn priv_global_use_semantic_data<'db>(
    db: &'db dyn SemanticGroup,
    global_use_id: GlobalUseId<'db>,
) -> Maybe<UseGlobalData<'db>> {
    let module_file_id = global_use_id.module_file_id(db);
    let mut diagnostics = SemanticDiagnostics::default();
    let inference_id = InferenceId::GlobalUseStar(global_use_id);
    let star_ast = ast::UsePath::Star(db.module_global_use_by_id(global_use_id)?.to_maybe()?);
    let mut resolver = Resolver::new(db, module_file_id, inference_id);
    let edition = resolver.settings.edition;
    if edition.ignore_visibility() {
        // We block support for global use where visibility is ignored.
        diagnostics.report(star_ast.stable_ptr(db), GlobalUsesNotSupportedInEdition(edition));
    }

    let item = star_ast.get_item(db);
    let segments = get_use_path_segments(db, star_ast.clone())?;
    if segments.is_empty() {
        let imported_module = Err(diagnostics.report(star_ast.stable_ptr(db), UseStarEmptyPath));
        return Ok(UseGlobalData { diagnostics: diagnostics.build(), imported_module });
    }
    resolver.set_feature_config(&global_use_id, &item, &mut diagnostics);
    let resolved_item = resolver.resolve_generic_path(
        &mut diagnostics,
        segments.clone(),
        NotFoundItemType::Identifier,
        ResolutionContext::Default,
    )?;
    // unwrap always safe as the resolver already resolved the entire path.
    let last_segment = segments.last().unwrap();
    let imported_module = match resolved_item {
        ResolvedGenericItem::Module(module_id) => Ok(module_id),
        _ => Err(diagnostics.report(
            last_segment.stable_ptr(db),
            UnexpectedElement {
                expected: vec![ElementKind::Module],
                actual: (&resolved_item).into(),
            },
        )),
    };
    Ok(UseGlobalData { diagnostics: diagnostics.build(), imported_module })
}

/// Query implementation of [crate::db::SemanticGroup::priv_global_use_imported_module].
pub fn priv_global_use_imported_module<'db>(
    db: &'db dyn SemanticGroup,
    global_use_id: GlobalUseId<'db>,
) -> Maybe<ModuleId<'db>> {
    db.priv_global_use_semantic_data(global_use_id)?.imported_module
}

/// Query implementation of [crate::db::SemanticGroup::global_use_semantic_diagnostics].
pub fn global_use_semantic_diagnostics<'db>(
    db: &'db dyn SemanticGroup,
    global_use_id: GlobalUseId<'db>,
) -> Diagnostics<'db, SemanticDiagnostic<'db>> {
    db.priv_global_use_semantic_data(global_use_id).map(|data| data.diagnostics).unwrap_or_default()
}

/// Cycle handling for [crate::db::SemanticGroup::priv_global_use_semantic_data].
pub fn priv_global_use_semantic_data_cycle<'db>(
    db: &'db dyn SemanticGroup,
    _input: SemanticGroupData,
    global_use_id: GlobalUseId<'db>,
) -> Maybe<UseGlobalData<'db>> {
    let mut diagnostics = SemanticDiagnostics::default();
    let global_use_ast = db.module_global_use_by_id(global_use_id)?.to_maybe()?;
    let star_ast = ast::UsePath::Star(db.module_global_use_by_id(global_use_id)?.to_maybe()?);
    let segments = get_use_path_segments(db, star_ast)?;
    let err = if segments.len() == 1 {
        // `use bad_name::*`, will attempt to find `bad_name` in the current module's global
        // uses, but which includes itself - but we don't want to report a cycle in this case.
        diagnostics.report(
            segments.last().unwrap().stable_ptr(db),
            PathNotFound(NotFoundItemType::Identifier),
        )
    } else {
        diagnostics.report(global_use_ast.stable_ptr(db), UseCycle)
    };
    Ok(UseGlobalData { diagnostics: diagnostics.build(), imported_module: Err(err) })
}

/// The modules that are imported by a module, using global uses.
#[derive(Debug, Clone, PartialEq, Eq, salsa::Update)]
pub struct ImportedModules<'db> {
    /// The imported modules that have a path where each step is visible by the previous module.
    pub accessible: OrderedHashSet<(ModuleId<'db>, ModuleId<'db>)>,
    // TODO(Tomer-StarkWare): consider changing from all_modules to inaccessible_modules
    /// All the imported modules.
    pub all: OrderedHashSet<ModuleId<'db>>,
}
/// Returns the modules that are imported with `use *` in the current module.
/// Query implementation of [crate::db::SemanticGroup::priv_module_use_star_modules].
pub fn priv_module_use_star_modules<'db>(
    db: &'db dyn SemanticGroup,
    module_id: ModuleId<'db>,
) -> Arc<ImportedModules<'db>> {
    let mut visited = UnorderedHashSet::<_>::default();
    let mut stack = vec![(module_id, module_id)];
    let mut accessible_modules = OrderedHashSet::default();
    // Iterate over all modules that are imported through `use *`, and are accessible from the
    // current module.
    while let Some((user_module, containing_module)) = stack.pop() {
        if !visited.insert((user_module, containing_module)) {
            continue;
        }
        let Ok(glob_uses) = get_module_global_uses(db, containing_module) else {
            continue;
        };
        for (glob_use, item_visibility) in glob_uses.iter() {
            let Ok(module_id_found) = db.priv_global_use_imported_module(*glob_use) else {
                continue;
            };
            if peek_visible_in(db, *item_visibility, containing_module, user_module) {
                stack.push((containing_module, module_id_found));
                accessible_modules.insert((containing_module, module_id_found));
            }
        }
    }
    let mut visited = UnorderedHashSet::<_>::default();
    let mut stack = vec![module_id];
    let mut all_modules = OrderedHashSet::default();
    // Iterate over all modules that are imported through `use *`.
    while let Some(curr_module_id) = stack.pop() {
        if !visited.insert(curr_module_id) {
            continue;
        }
        all_modules.insert(curr_module_id);
        let Ok(glob_uses) = get_module_global_uses(db, curr_module_id) else { continue };
        for glob_use in glob_uses.keys() {
            let Ok(module_id_found) = db.priv_global_use_imported_module(*glob_use) else {
                continue;
            };
            stack.push(module_id_found);
        }
    }
    // Remove the current module from the list of all modules, as if items in the module not found
    // previously, it was explicitly ignored.
    all_modules.swap_remove(&module_id);
    Arc::new(ImportedModules { accessible: accessible_modules, all: all_modules })
}
