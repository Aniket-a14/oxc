//! [ECMAScript Module Record](https://tc39.es/ecma262/#sec-abstract-module-records)

use std::{
    fmt,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak},
};

use rustc_hash::{FxHashMap, FxHashSet};

use oxc_allocator::{Allocator, CloneIn};
use self_cell::self_cell;

use oxc_semantic::Semantic;
use oxc_span::Span;
use oxc_str::{CompactStr, JSStr};
pub use oxc_syntax::module_record::RequestedModule;

/// ESM Module Record
///
/// All data inside this data structure are for ESM, no commonjs data is allowed.
///
/// See
/// * <https://tc39.es/ecma262/#table-additional-fields-of-source-text-module-records>
/// * <https://tc39.es/ecma262/#cyclic-module-record>
#[derive(Default)]
pub struct ModuleRecord {
    /// This module has ESM syntax: `import` and `export`.
    pub has_module_syntax: bool,

    /// Resolved absolute path to this module record
    pub resolved_absolute_path: PathBuf,

    /// `[[RequestedModules]]`
    ///
    /// A List of all the ModuleSpecifier strings used by the module represented by this record to request the importation of a module. The List is in source text occurrence order.
    ///
    /// Module requests from:
    ///   import ImportClause FromClause
    ///   import ModuleSpecifier
    ///   export ExportFromClause FromClause
    /// Keyed by ModuleSpecifier, valued by all node occurrences
    pub requested_modules:
        hashbrown::HashMap<ModuleSpecifier, Vec<RequestedModule>, rustc_hash::FxBuildHasher>,

    /// `[[LoadedModules]]`
    ///
    /// A map from the specifier strings used by the module represented by this record to request
    /// the importation of a module to the resolved Module Record. The list does not contain two
    /// different Records with the same `[[Specifier]]`.
    ///
    /// Note that Oxc does not support cross-file analysis, so this map will be empty after
    /// [`ModuleRecord`] is created. You must link the module records yourself.
    ///
    /// Use [ModuleRecord::get_loaded_module] to get a `ModuleRecord`.
    loaded_modules: RwLock<FxHashMap<CompactStr, Weak<ModuleRecord>>>,

    /// `[[ImportEntries]]`
    ///
    /// A List of `ImportEntry` records derived from the code of this module
    pub import_entries: Vec<ImportEntry>,

    /// `[[LocalExportEntries]]`
    ///
    /// A List of `ExportEntry` records derived from the code of this module
    /// that correspond to declarations that occur within the module
    pub local_export_entries: Vec<ExportEntry>,

    /// `[[IndirectExportEntries]]`
    ///
    /// A List of `ExportEntry` records derived from the code of this module
    /// that correspond to reexported imports that occur within the module
    /// or exports from `export * as namespace` declarations.
    pub indirect_export_entries: Vec<ExportEntry>,

    /// `[[StarExportEntries]]`
    ///
    /// A List of `ExportEntry` records derived from the code of this module
    /// that correspond to `export *` declarations that occur within the module,
    /// not including `export * as namespace` declarations.
    pub star_export_entries: Vec<ExportEntry>,

    /// Local exported bindings
    pub exported_bindings: FxHashMap<CompactStr, Span>,

    /// Reexported bindings from `export * from 'specifier'`
    /// Keyed by resolved path
    exported_bindings_from_star_export: OnceLock<FxHashMap<PathBuf, Vec<CompactStr>>>,

    /// `export default name`
    ///         ^^^^^^^ span
    pub export_default: Option<Span>,
}

impl fmt::Debug for ModuleRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        // recursively formatting loaded modules can crash when the module graph is cyclic
        let loaded_modules = self
            .loaded_modules
            .read()
            .unwrap()
            .keys()
            .map(ToString::to_string)
            .reduce(|acc, key| format!("{acc}, {key}"))
            .unwrap_or_default();
        let loaded_modules = format!("{{ {loaded_modules} }}");
        f.debug_struct("ModuleRecord")
            .field("has_module_syntax", &self.has_module_syntax)
            .field("resolved_absolute_path", &self.resolved_absolute_path)
            .field("requested_modules", &self.requested_modules)
            .field("loaded_modules", &loaded_modules)
            .field("import_entries", &self.import_entries)
            .field("local_export_entries", &self.local_export_entries)
            .field("indirect_export_entries", &self.indirect_export_entries)
            .field("star_export_entries", &self.star_export_entries)
            .field("exported_bindings", &self.exported_bindings)
            .field("exported_bindings_from_star_export", &self.exported_bindings_from_star_export)
            .field("export_default", &self.export_default)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameSpan {
    pub name: CompactStr,
    pub span: Span,
}

impl NameSpan {
    pub fn new(name: CompactStr, span: Span) -> Self {
        Self { name, span }
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }
}

impl<'a> From<&oxc_syntax::module_record::NameSpan<'a>> for NameSpan {
    fn from(other: &oxc_syntax::module_record::NameSpan<'a>) -> Self {
        Self { name: CompactStr::from(other.name.as_str()), span: other.span }
    }
}

// Each module's specifiers share one arena. The frozen table owns the storage
// independently of the parser arena without requiring a general owned JS string.
self_cell! {
    struct ModuleSpecifierStorage {
        owner: Allocator,
        #[covariant]
        dependent: Specifiers,
    }
}
type Specifiers<'a> = Vec<JSStr<'a>>;

// SAFETY: The arena is used only during construction, before this table is shared.
// No API exposes the owner or mutable access to the dependent strings. Readers
// access only immutable JSStr values, which are Sync. self_cell keeps their arena
// alive until the last owner is dropped.
unsafe impl Sync for ModuleSpecifierStorage {}

/// An owned module specifier retaining its exact JavaScript string value.
#[derive(Clone)]
pub struct ModuleSpecifier(ModuleSpecifierValue);

#[derive(Clone)]
enum ModuleSpecifierValue {
    Utf8(CompactStr),
    Wtf8 { storage: Arc<ModuleSpecifierStorage>, index: usize },
}

impl ModuleSpecifier {
    pub fn as_js_str(&self) -> JSStr<'_> {
        match &self.0 {
            ModuleSpecifierValue::Utf8(name) => JSStr::from(name.as_str()),
            ModuleSpecifierValue::Wtf8 { storage, index } => storage.borrow_dependent()[*index],
        }
    }

    /// Borrow UTF-8 for a resolver or another explicitly UTF-8-only API.
    pub fn as_str(&self) -> Option<&str> {
        self.as_js_str().as_str()
    }
}

impl PartialEq for ModuleSpecifier {
    fn eq(&self, other: &Self) -> bool {
        self.as_js_str() == other.as_js_str()
    }
}
impl Eq for ModuleSpecifier {}
impl Hash for ModuleSpecifier {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_js_str().hash(state);
    }
}
impl hashbrown::Equivalent<ModuleSpecifier> for JSStr<'_> {
    fn equivalent(&self, other: &ModuleSpecifier) -> bool {
        *self == other.as_js_str()
    }
}
impl fmt::Debug for ModuleSpecifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_js_str().fmt(f)
    }
}
impl fmt::Display for ModuleSpecifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        oxc_ast::StaticName::Borrowed(self.as_js_str()).fmt(f)
    }
}
impl<'a> From<&'a ModuleSpecifier> for JSStr<'a> {
    fn from(name: &'a ModuleSpecifier) -> Self {
        name.as_js_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRequest {
    pub name: ModuleSpecifier,
    pub span: Span,
}

impl ModuleRequest {
    pub fn name(&self) -> JSStr<'_> {
        self.name.as_js_str()
    }

    fn from_module_record(
        other: &oxc_syntax::module_record::ModuleRequest<'_>,
        names: &FxHashMap<JSStr<'_>, ModuleSpecifier>,
    ) -> Self {
        Self { name: names[&other.name].clone(), span: other.span }
    }
}

fn own_module_specifiers<'a>(
    other: &oxc_syntax::module_record::ModuleRecord<'a>,
) -> FxHashMap<JSStr<'a>, ModuleSpecifier> {
    let mut owned = FxHashMap::default();
    let mut indices = FxHashMap::default();
    let mut names = Vec::new();
    for name in other
        .requested_modules
        .keys()
        .copied()
        .chain(other.import_entries.iter().map(|entry| entry.module_request.name))
        .chain(
            other
                .local_export_entries
                .iter()
                .chain(other.indirect_export_entries.iter())
                .chain(other.star_export_entries.iter())
                .filter_map(|entry| entry.module_request.as_ref().map(|request| request.name)),
        )
    {
        if let Some(utf8) = name.as_str() {
            owned
                .entry(name)
                .or_insert_with(|| ModuleSpecifier(ModuleSpecifierValue::Utf8(utf8.into())));
            continue;
        }
        indices.entry(name).or_insert_with(|| {
            let index = names.len();
            names.push(name);
            index
        });
    }
    if names.is_empty() {
        return owned;
    }
    let storage = Arc::new(ModuleSpecifierStorage::new(Allocator::new(), |allocator| {
        names.iter().map(|name| name.clone_in(allocator)).collect()
    }));
    owned.extend(indices.into_iter().map(|(name, index)| {
        (name, ModuleSpecifier(ModuleSpecifierValue::Wtf8 { storage: Arc::clone(&storage), index }))
    }));
    owned
}

/// [`ImportEntry`](https://tc39.es/ecma262/#importentry-record)
///
/// ## Examples
///
/// ```ts
/// //     _ local_name
/// import v from "mod";
/// //             ^^^ module_request
///
/// //     ____ is_type will be `true`
/// import type { foo as bar } from "mod";
/// // import_name^^^    ^^^ local_name
///
/// import * as ns from "mod";
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportEntry {
    /// Span of the import statement.
    ///
    /// ## Examples
    ///
    /// ```ts
    /// import { foo } from "mod";
    /// ^^^^^^^^^^^^^^^^^^^^^^^^^
    /// ```
    pub statement_span: Span,

    /// String value of the ModuleSpecifier of the ImportDeclaration.
    ///
    /// ## Examples
    ///
    /// ```ts
    /// import { foo } from "mod";
    /// //                   ^^^
    /// ```
    pub module_request: ModuleRequest,

    /// The name under which the desired binding is exported by the module identified by `[[ModuleRequest]]`.
    ///
    /// ## Examples
    ///
    /// ```ts
    /// import { foo } from "mod";
    /// //       ^^^
    /// import { foo as bar } from "mod";
    /// //       ^^^
    /// ```
    pub import_name: ImportImportName,

    /// The name that is used to locally access the imported value from within the importing module.
    ///
    /// ## Examples
    ///
    /// ```ts
    /// import { foo } from "mod";
    /// //       ^^^
    /// import { foo as bar } from "mod";
    /// //              ^^^
    /// ```
    pub local_name: NameSpan,

    /// Whether this binding is for a TypeScript type-only import. This is a non-standard field.
    /// When creating a [`ModuleRecord`] for a JavaScript file, this will always be false.
    ///
    /// ## Examples
    ///
    /// `is_type` will be `true` for the following imports:
    /// ```ts
    /// import type { foo } from "mod";
    /// import { type foo } from "mod";
    /// ```
    ///
    /// and will be `false` for these imports:
    /// ```ts
    /// import { foo } from "mod";
    /// import { foo as type } from "mod";
    /// ```
    pub is_type: bool,
}

impl ImportEntry {
    fn from_module_record(
        other: &oxc_syntax::module_record::ImportEntry<'_>,
        names: &FxHashMap<JSStr<'_>, ModuleSpecifier>,
    ) -> Self {
        Self {
            statement_span: other.statement_span,
            module_request: ModuleRequest::from_module_record(&other.module_request, names),
            import_name: ImportImportName::from(&other.import_name),
            local_name: NameSpan::from(&other.local_name),
            is_type: other.is_type,
        }
    }
}

/// `ImportName` For `ImportEntry`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportImportName {
    Name(NameSpan),
    NamespaceObject,
    Default(Span),
}

impl ImportImportName {
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default(_))
    }

    pub fn is_namespace_object(&self) -> bool {
        matches!(self, Self::NamespaceObject)
    }
}

impl<'a> From<&oxc_syntax::module_record::ImportImportName<'a>> for ImportImportName {
    fn from(other: &oxc_syntax::module_record::ImportImportName<'a>) -> Self {
        match other {
            oxc_syntax::module_record::ImportImportName::Name(name_span) => {
                Self::Name(NameSpan::from(name_span))
            }
            oxc_syntax::module_record::ImportImportName::NamespaceObject => Self::NamespaceObject,
            oxc_syntax::module_record::ImportImportName::Default(span) => Self::Default(*span),
        }
    }
}

/// [`ExportEntry`](https://tc39.es/ecma262/#exportentry-record)
///
/// Describes a single exported binding from a module. Named export statements that contain more
/// than one binding produce multiple ExportEntry records.
///
/// ## Examples
///
/// ```ts
/// // foo's ExportEntry nas no `module_request` or `import_name.
/// //       ___ local_name
/// export { foo };
/// //       ^^^ export_name. Since there's no alias, it's the same as local_name.
///
/// // re-exports do not produce local bindings, so `local_name` is null.
/// //       ___ import_name    __ module_request
/// export { foo as bar } from "mod";
/// //              ^^^ export_name
///
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExportEntry {
    /// Span of the export statement.
    pub statement_span: Span,

    /// Span for the entire export entry
    pub span: Span,

    /// The String value of the ModuleSpecifier of the ExportDeclaration.
    /// null if the ExportDeclaration does not have a ModuleSpecifier.
    pub module_request: Option<ModuleRequest>,

    /// The name under which the desired binding is exported by the module identified by `[[ModuleRequest]]`.
    /// null if the ExportDeclaration does not have a ModuleSpecifier.
    /// "all" is used for `export * as ns from "mod"`` declarations.
    /// "all-but-default" is used for `export * from "mod" declarations`.
    pub import_name: ExportImportName,

    /// The name used to export this binding by this module.
    pub export_name: ExportExportName,

    /// The name that is used to locally access the exported value from within the importing module.
    /// null if the exported value is not locally accessible from within the module.
    pub local_name: ExportLocalName,

    /// Whether the export is a TypeScript `export type`.
    ///
    /// Examples:
    ///
    /// ```ts
    /// export type * from 'mod'
    /// export type * as ns from 'mod'
    /// export type { foo }
    /// export { type foo }
    /// export type { foo } from 'mod'
    /// ```
    pub is_type: bool,
}

impl ExportEntry {
    fn from_module_record(
        other: &oxc_syntax::module_record::ExportEntry<'_>,
        names: &FxHashMap<JSStr<'_>, ModuleSpecifier>,
    ) -> Self {
        Self {
            statement_span: other.statement_span,
            span: other.span,
            module_request: other
                .module_request
                .as_ref()
                .map(|request| ModuleRequest::from_module_record(request, names)),
            import_name: ExportImportName::from(&other.import_name),
            export_name: ExportExportName::from(&other.export_name),
            local_name: ExportLocalName::from(&other.local_name),
            is_type: other.is_type,
        }
    }
}

/// `ImportName` for `ExportEntry`
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ExportImportName {
    Name(NameSpan),
    /// all is used for export * as ns from "mod" declarations.
    All,
    /// all-but-default is used for export * from "mod" declarations.
    AllButDefault,
    /// the ExportDeclaration does not have a ModuleSpecifier
    #[default]
    Null,
}

impl<'a> From<&oxc_syntax::module_record::ExportImportName<'a>> for ExportImportName {
    fn from(other: &oxc_syntax::module_record::ExportImportName<'a>) -> Self {
        match other {
            oxc_syntax::module_record::ExportImportName::Name(name_span) => {
                Self::Name(NameSpan::from(name_span))
            }
            oxc_syntax::module_record::ExportImportName::All => Self::All,
            oxc_syntax::module_record::ExportImportName::AllButDefault => Self::AllButDefault,
            oxc_syntax::module_record::ExportImportName::Null => Self::Null,
        }
    }
}

impl ExportImportName {
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    pub fn is_all_but_default(&self) -> bool {
        matches!(self, Self::AllButDefault)
    }
}

/// `ExportName` for `ExportEntry`
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ExportExportName {
    Name(NameSpan),
    Default(Span),
    #[default]
    Null,
}

impl ExportExportName {
    /// Returns `true` if this is [`ExportExportName::Default`].
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default(_))
    }

    /// Returns `true` if this is [`ExportExportName::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Returns `true` if this is [`ExportExportName::Name`].
    pub fn is_name(&self) -> bool {
        matches!(self, Self::Name(_))
    }

    /// Attempt to get the [`Span`] of this export name.
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::Name(name) => Some(name.span),
            Self::Default(span) => Some(*span),
            Self::Null => None,
        }
    }
}

impl<'a> From<&oxc_syntax::module_record::ExportExportName<'a>> for ExportExportName {
    fn from(other: &oxc_syntax::module_record::ExportExportName<'a>) -> Self {
        match other {
            oxc_syntax::module_record::ExportExportName::Name(name_span) => {
                Self::Name(NameSpan::from(name_span))
            }
            oxc_syntax::module_record::ExportExportName::Default(span) => Self::Default(*span),
            oxc_syntax::module_record::ExportExportName::Null => Self::Null,
        }
    }
}

/// `LocalName` for `ExportEntry`
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ExportLocalName {
    Name(NameSpan),
    /// `export default name_span`
    Default(NameSpan),
    #[default]
    Null,
}

impl ExportLocalName {
    /// `true` if this is a [`ExportLocalName::Default`].
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default(_))
    }

    /// `true` if this is a [`ExportLocalName::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Get the bound name of this export. [`None`] for [`ExportLocalName::Null`].
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Name(name) | Self::Default(name) => Some(name.name.as_str()),
            Self::Null => None,
        }
    }
}

impl<'a> From<&oxc_syntax::module_record::ExportLocalName<'a>> for ExportLocalName {
    fn from(other: &oxc_syntax::module_record::ExportLocalName<'a>) -> Self {
        match other {
            oxc_syntax::module_record::ExportLocalName::Name(name_span) => {
                Self::Name(NameSpan::from(name_span))
            }
            oxc_syntax::module_record::ExportLocalName::Default(name_span) => {
                Self::Default(NameSpan::from(name_span))
            }
            oxc_syntax::module_record::ExportLocalName::Null => Self::Null,
        }
    }
}

impl ModuleRecord {
    pub fn new(
        path: &Path,
        other: &oxc_syntax::module_record::ModuleRecord,
        _semantic: &Semantic,
    ) -> Self {
        let names = own_module_specifiers(other);
        Self {
            has_module_syntax: other.has_module_syntax,
            resolved_absolute_path: path.to_path_buf(),
            requested_modules: other
                .requested_modules
                .iter()
                .map(|(name, requests)| (names[name].clone(), requests.iter().copied().collect()))
                .collect(),
            import_entries: other
                .import_entries
                .iter()
                .map(|entry| ImportEntry::from_module_record(entry, &names))
                .collect(),
            local_export_entries: other
                .local_export_entries
                .iter()
                .map(|entry| ExportEntry::from_module_record(entry, &names))
                .collect(),
            indirect_export_entries: other
                .indirect_export_entries
                .iter()
                .map(|entry| ExportEntry::from_module_record(entry, &names))
                .collect(),
            star_export_entries: other
                .star_export_entries
                .iter()
                .map(|entry| ExportEntry::from_module_record(entry, &names))
                .collect(),
            exported_bindings: other
                .exported_bindings
                .iter()
                .map(|(name, span)| (CompactStr::from(name.as_str()), *span))
                .collect(),
            export_default: other
                .local_export_entries
                .iter()
                .filter_map(|export_entry| export_entry.export_name.default_export_span())
                .chain(
                    other
                        .indirect_export_entries
                        .iter()
                        .filter_map(|export_entry| export_entry.export_name.default_export_span()),
                )
                .next(),
            ..ModuleRecord::default()
        }
    }

    /// # Panics
    ///
    /// * If the RwLock is poisoned (which only happens if a thread panicked while holding the lock).
    pub fn loaded_modules(&self) -> RwLockReadGuard<'_, FxHashMap<CompactStr, Weak<ModuleRecord>>> {
        self.loaded_modules.read().unwrap()
    }

    /// # Panics
    ///
    /// * If the RwLock is poisoned (which only happens if a thread panicked while holding the lock).
    pub fn write_loaded_modules(
        &self,
    ) -> RwLockWriteGuard<'_, FxHashMap<CompactStr, Weak<ModuleRecord>>> {
        self.loaded_modules.write().unwrap()
    }

    /// Get a loaded module by upgrading the weak reference to an Arc.
    /// Returns None if the module has been dropped or not found.
    ///
    /// # Panics
    ///
    /// * If the RwLock is poisoned (which only happens if a thread panicked while holding the lock).
    /// * If `ModuleRecord` is dropped (fails to Weak::upgrade).
    pub fn get_loaded_module<'a>(&self, key: impl Into<JSStr<'a>>) -> Option<Arc<ModuleRecord>> {
        // Loaded modules are populated only by the UTF-8 filesystem resolver.
        let key = key.into().as_str()?;
        let loaded_modules = self.loaded_modules();
        loaded_modules.get(key).map(|weak| Weak::upgrade(weak).unwrap())
    }

    pub(crate) fn exported_bindings_from_star_export(
        &self,
    ) -> &FxHashMap<PathBuf, Vec<CompactStr>> {
        self.exported_bindings_from_star_export.get_or_init(|| {
            let mut exported_bindings_from_star_export: FxHashMap<PathBuf, Vec<CompactStr>> =
                FxHashMap::default();
            // Walk the `export *` graph with an explicit visited set instead of recursing through
            // each remote module's own memoized `exported_bindings_from_star_export()`. The
            // loaded-module graph can be cyclic (e.g. `a` does `export * from './b'` and `b` does
            // `export * from './a'`), and recursing through the memoized accessor would re-enter
            // *this* `OnceLock` while it is still initializing — which `std`'s `OnceLock` forbids
            // (it panics, or deadlocks). Seed `visited` with our own path so a back edge to this
            // module is skipped.
            let mut visited = FxHashSet::default();
            visited.insert(self.resolved_absolute_path.clone());
            for export_entry in &self.star_export_entries {
                let Some(module_request) = &export_entry.module_request else {
                    continue;
                };
                let Some(remote_module_record) = self.get_loaded_module(module_request.name())
                else {
                    continue;
                };
                // Append the remote's own `bindings` plus its transitive star re-exports.
                let mut remote_bindings: Vec<CompactStr> =
                    remote_module_record.exported_bindings.keys().cloned().collect();
                remote_module_record
                    .collect_star_exported_bindings(&mut visited, &mut remote_bindings);
                exported_bindings_from_star_export
                    .entry(remote_module_record.resolved_absolute_path.clone())
                    .or_default()
                    .extend(remote_bindings);
            }
            exported_bindings_from_star_export
        })
    }

    /// Collect into `out` every binding name reachable through `self`'s `export *` chain, reading
    /// each module's entries directly (not via the memoized accessor) so the walk stays safe on a
    /// cyclic module graph. `visited` is keyed by resolved absolute path; a module already in it
    /// is skipped, breaking cycles.
    fn collect_star_exported_bindings(
        &self,
        visited: &mut FxHashSet<PathBuf>,
        out: &mut Vec<CompactStr>,
    ) {
        for export_entry in &self.star_export_entries {
            let Some(module_request) = &export_entry.module_request else {
                continue;
            };
            let Some(remote_module_record) = self.get_loaded_module(module_request.name()) else {
                continue;
            };
            if !visited.insert(remote_module_record.resolved_absolute_path.clone()) {
                continue;
            }
            out.extend(remote_module_record.exported_bindings.keys().cloned());
            remote_module_record.collect_star_exported_bindings(visited, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    #[test]
    fn jsstr_module_requests_outlive_parser_storage() {
        let record = {
            let allocator = Allocator::new();
            let parsed = Parser::new(
                &allocator,
                r#"
                import "x"; import "x";
                import { v } from "x\uD800"; import "x\ud800";
                export { v };
                export { a } from "x\uD801";
                export * from "x\uDC00";
                export * as ns from "x\uD800\uDC00";
            "#,
                SourceType::mjs(),
            )
            .parse();
            assert!(parsed.diagnostics.is_empty());
            let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
            let record = ModuleRecord::new(Path::new("test.js"), &parsed.module_record, &semantic);
            assert_eq!(record.import_entries.len(), parsed.module_record.import_entries.len());
            assert_eq!(
                record.local_export_entries.len(),
                parsed.module_record.local_export_entries.len()
            );
            assert_eq!(
                record.indirect_export_entries.len(),
                parsed.module_record.indirect_export_entries.len()
            );
            assert_eq!(
                record.star_export_entries.len(),
                parsed.module_record.star_export_entries.len()
            );
            Arc::new(record)
        };
        assert_eq!(record.requested_modules.len(), 5);
        assert_eq!(record.requested_modules[&JSStr::from("x")].len(), 2);
        let copy = Arc::clone(&record);
        let requests = std::thread::spawn(move || {
            let mut requests: Vec<_> = copy
                .requested_modules
                .iter()
                .map(|(name, occurrences)| {
                    (name.as_js_str().encode_utf16().collect::<Vec<_>>(), occurrences.len())
                })
                .collect();
            requests.sort();
            requests
        })
        .join()
        .unwrap();
        assert_eq!(
            requests,
            [
                (vec![0x78], 2),
                (vec![0x78, 0xD800], 2),
                (vec![0x78, 0xD800, 0xDC00], 1),
                (vec![0x78, 0xD801], 1),
                (vec![0x78, 0xDC00], 1),
            ]
        );
        assert!(record.import_entries[0].module_request.name().has_lone_surrogate());
        assert!(
            record.star_export_entries[0]
                .module_request
                .as_ref()
                .unwrap()
                .name()
                .has_lone_surrogate()
        );
    }
}
