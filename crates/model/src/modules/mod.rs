use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    sync::LazyLock,
};

use anyhow::Context;
use common::{
    components::{
        CanonicalizedComponentFunctionPath,
        CanonicalizedComponentModulePath,
        ComponentDefinitionId,
    },
    document::{
        ParsedDocument,
        ResolvedDocument,
    },
    interval::{
        BinaryKey,
        Interval,
    },
    query::{
        IndexRange,
        IndexRangeExpression,
        Order,
        Query,
    },
    runtime::Runtime,
    types::{
        IndexName,
        ModuleEnvironment,
    },
    value::{
        ConvexValue,
        ResolvedDocumentId,
        VALUE_TOO_LARGE_SHORT_MSG,
    },
};
use database::{
    defaults::system_index,
    unauthorized_error,
    BootstrapComponentsModel,
    ResolvedQuery,
    SystemMetadataModel,
    Transaction,
};
use errors::{
    ErrorMetadata,
    ErrorMetadataAnyhowExt,
};
use metrics::{
    get_module_metadata_timer,
    get_module_version_timer,
};
use sync_types::CanonicalizedModulePath;
use value::{
    values_to_bytes,
    FieldPath,
    TableName,
};

use self::{
    module_versions::{
        AnalyzedFunction,
        AnalyzedModule,
        FullModuleSource,
        ModuleSource,
        ModuleVersion,
        ModuleVersionMetadata,
        SourceMap,
    },
    types::ModuleMetadata,
    user_error::{
        FunctionNotFoundError,
        ModuleNotFoundError,
    },
};
use crate::{
    config::{
        module_loader::ModuleLoader,
        types::{
            ModuleConfig,
            ModuleDiff,
        },
    },
    source_packages::types::SourcePackageId,
    SystemIndex,
    SystemTable,
};

pub mod function_validators;
mod metrics;
pub mod module_versions;
pub mod types;
pub mod user_error;

/// Table name for user modules.
pub static MODULES_TABLE: LazyLock<TableName> =
    LazyLock::new(|| "_modules".parse().expect("Invalid built-in module table"));

/// Table name for the versions of a module.
pub static MODULE_VERSIONS_TABLE: LazyLock<TableName> = LazyLock::new(|| {
    "_module_versions"
        .parse()
        .expect("Invalid built-in module table")
});

/// Field pointing to the `ModuleMetadata` document from
/// `ModuleVersionMetadata`.
static MODULE_ID_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "module_id".parse().expect("Invalid built-in field"));
/// Field for a module's version in `ModuleVersionMetadata`.
static VERSION_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "version".parse().expect("Invalid built-in field"));

/// Field for a module's path in `ModuleMetadata`.
static PATH_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "path".parse().expect("Invalid built-in field"));
/// Field for a module's deleted flag in `ModuleMetadata`.
static DELETED_FIELD: LazyLock<FieldPath> =
    LazyLock::new(|| "deleted".parse().expect("Invalid built-in field"));

pub static MODULE_INDEX_BY_PATH: LazyLock<IndexName> =
    LazyLock::new(|| system_index(&MODULES_TABLE, "by_path"));
pub static MODULE_INDEX_BY_DELETED: LazyLock<IndexName> =
    LazyLock::new(|| system_index(&MODULES_TABLE, "by_deleted"));
pub static MODULE_VERSION_INDEX: LazyLock<IndexName> =
    LazyLock::new(|| system_index(&MODULE_VERSIONS_TABLE, "by_module_and_version"));

pub struct ModulesTable;
impl SystemTable for ModulesTable {
    fn table_name(&self) -> &'static TableName {
        &MODULES_TABLE
    }

    fn indexes(&self) -> Vec<SystemIndex> {
        vec![
            SystemIndex {
                name: MODULE_INDEX_BY_PATH.clone(),
                fields: vec![PATH_FIELD.clone()].try_into().unwrap(),
            },
            SystemIndex {
                name: MODULE_INDEX_BY_DELETED.clone(),
                fields: vec![DELETED_FIELD.clone(), PATH_FIELD.clone()]
                    .try_into()
                    .unwrap(),
            },
        ]
    }

    fn validate_document(&self, document: ResolvedDocument) -> anyhow::Result<()> {
        ParsedDocument::<ModuleMetadata>::try_from(document).map(|_| ())
    }
}
pub struct ModuleVersionsTable;
impl SystemTable for ModuleVersionsTable {
    fn table_name(&self) -> &'static TableName {
        &MODULE_VERSIONS_TABLE
    }

    fn indexes(&self) -> Vec<SystemIndex> {
        vec![SystemIndex {
            name: MODULE_VERSION_INDEX.clone(),
            fields: vec![MODULE_ID_FIELD.clone(), VERSION_FIELD.clone()]
                .try_into()
                .unwrap(),
        }]
    }

    fn validate_document(&self, document: ResolvedDocument) -> anyhow::Result<()> {
        ParsedDocument::<ModuleVersionMetadata>::try_from(document).map(|_| ())
    }
}

pub struct ModuleModel<'a, RT: Runtime> {
    tx: &'a mut Transaction<RT>,
}

impl<'a, RT: Runtime> ModuleModel<'a, RT> {
    pub fn new(tx: &'a mut Transaction<RT>) -> Self {
        Self { tx }
    }

    pub async fn apply(
        &mut self,
        component: ComponentDefinitionId,
        modules: Vec<ModuleConfig>,
        source_package_id: Option<SourcePackageId>,
        mut analyze_results: BTreeMap<CanonicalizedModulePath, AnalyzedModule>,
    ) -> anyhow::Result<ModuleDiff> {
        if modules.iter().any(|c| c.path.is_system()) {
            anyhow::bail!("You cannot push functions under the '_system/' directory.");
        }

        let mut added_modules = BTreeSet::new();

        // Add new modules.
        let mut remaining_modules: BTreeSet<_> = self
            .get_application_metadata(component)
            .await?
            .into_iter()
            .map(|module| module.into_value().path)
            .collect();
        for module in modules {
            let path = module.path.canonicalize();
            if !remaining_modules.remove(&path) {
                added_modules.insert(path.clone());
            }
            let analyze_result = if !path.is_deps() {
                // We expect AnalyzeResult to always be set for non-dependency modules.
                let analyze_result = analyze_results.remove(&path).context(format!(
                    "Missing analyze result for module {}",
                    path.as_str()
                ))?;
                Some(analyze_result)
            } else {
                // We don't analyze dependencies.
                None
            };
            self.put(
                CanonicalizedComponentModulePath {
                    component,
                    module_path: path.clone(),
                },
                module.source,
                source_package_id,
                module.source_map,
                analyze_result,
                module.environment,
            )
            .await?;
        }

        let mut removed_modules = BTreeSet::new();
        for path in remaining_modules {
            removed_modules.insert(path.clone());
            ModuleModel::new(self.tx)
                .delete(CanonicalizedComponentModulePath {
                    component,
                    module_path: path,
                })
                .await?;
        }
        ModuleDiff::new(added_modules, removed_modules)
    }

    /// Returns the registered modules metadata, including system modules.
    pub async fn get_all_metadata(
        &mut self,
        component: ComponentDefinitionId,
    ) -> anyhow::Result<Vec<ParsedDocument<ModuleMetadata>>> {
        let index_query = Query::full_table_scan(MODULES_TABLE.clone(), Order::Asc);
        let mut query_stream = ResolvedQuery::new(self.tx, component.into(), index_query)?;

        let mut modules = Vec::new();
        while let Some(metadata_document) = query_stream.next(self.tx, None).await? {
            let metadata: ParsedDocument<ModuleMetadata> = metadata_document.try_into()?;
            modules.push(metadata);
        }
        Ok(modules)
    }

    pub async fn get_application_metadata(
        &mut self,
        component: ComponentDefinitionId,
    ) -> anyhow::Result<Vec<ParsedDocument<ModuleMetadata>>> {
        let modules = self
            .get_all_metadata(component)
            .await?
            .into_iter()
            .filter(|metadata| !metadata.path.is_system())
            .collect();
        Ok(modules)
    }

    /// Returns all registered modules that aren't system modules.
    pub async fn get_application_modules(
        &mut self,
        component: ComponentDefinitionId,
        module_loader: &dyn ModuleLoader<RT>,
    ) -> anyhow::Result<BTreeMap<CanonicalizedModulePath, ModuleConfig>> {
        let mut modules = BTreeMap::new();
        for metadata in self.get_all_metadata(component).await? {
            let path = metadata.path.clone();
            if !path.is_system() {
                let environment = metadata.environment;
                let full_source = module_loader
                    .get_module_with_metadata(self.tx, metadata)
                    .await?;
                let module_config = ModuleConfig {
                    path: path.clone().into(),
                    source: full_source.source.clone(),
                    source_map: full_source.source_map.clone(),
                    environment,
                };
                if modules.insert(path.clone(), module_config).is_some() {
                    panic!("Duplicate application module at {:?}", path);
                }
            }
        }
        Ok(modules)
    }

    pub async fn get_version(
        &mut self,
        module_id: ResolvedDocumentId,
        version: ModuleVersion,
    ) -> anyhow::Result<ParsedDocument<ModuleVersionMetadata>> {
        let timer = get_module_version_timer();
        let module_id_value: ConvexValue = module_id.into();
        let index_range = IndexRange {
            index_name: MODULE_VERSION_INDEX.clone(),
            range: vec![IndexRangeExpression::Eq(
                MODULE_ID_FIELD.clone(),
                module_id_value.into(),
            )],
            order: Order::Asc,
        };
        let module_query = Query::index_range(index_range);
        let namespace = self
            .tx
            .table_mapping()
            .tablet_namespace(module_id.table().tablet_id)?;
        let mut query_stream = ResolvedQuery::new(self.tx, namespace, module_query)?;
        let module_version: ParsedDocument<ModuleVersionMetadata> = query_stream
            .expect_at_most_one(self.tx)
            .await?
            .context(format!(
                "Dangling module version reference: {module_id}@{version}"
            ))?
            .try_into()?;
        anyhow::ensure!(module_version.version == Some(version));
        timer.finish();
        Ok(module_version)
    }

    pub async fn get_source_from_db(
        &mut self,
        module_id: ResolvedDocumentId,
        version: ModuleVersion,
    ) -> anyhow::Result<FullModuleSource> {
        let module_version = self.get_version(module_id, version).await?.into_value();
        Ok(FullModuleSource {
            source: module_version.source,
            source_map: module_version.source_map,
        })
    }

    pub async fn get_metadata_for_function(
        &mut self,
        path: CanonicalizedComponentFunctionPath,
    ) -> anyhow::Result<Option<ParsedDocument<ModuleMetadata>>> {
        let module_path = BootstrapComponentsModel::new(self.tx)
            .function_path_to_module(path.clone())
            .await?;
        let module_metadata = self.get_metadata(module_path).await?;
        Ok(module_metadata)
    }

    /// Helper function to get a module at the latest version.
    pub async fn get_metadata(
        &mut self,
        path: CanonicalizedComponentModulePath,
    ) -> anyhow::Result<Option<ParsedDocument<ModuleMetadata>>> {
        let timer = get_module_metadata_timer();

        let is_system = path.module_path.is_system();
        if is_system && !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("get_module"))
        }
        let module_metadata = match self.module_metadata(path).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        timer.finish();
        Ok(Some(module_metadata))
    }

    /// Write a isolate-environment module to _module_versions, without source
    /// package.
    ///
    /// This transaction must never be committed.
    /// All future reads of modules on this transaction will read from the
    /// database instead of the source package.
    pub async fn put_standalone(
        &mut self,
        path: CanonicalizedComponentModulePath,
        source: ModuleSource,
        source_map: Option<SourceMap>,
        analyze_result: AnalyzedModule,
    ) -> anyhow::Result<()> {
        if !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("put_standalone_module"));
        }
        if path.module_path.is_system() {
            anyhow::bail!("You cannot push a function under '_system/'");
        }
        let component = path.component;
        let (module_id, version) = self
            .put_module_metadata(path, None, Some(analyze_result), ModuleEnvironment::Isolate)
            .await?;
        self.put_module_source_into_db(module_id, version, source, source_map, component)
            .await
    }

    /// Put a module's source at a given path.
    pub async fn put(
        &mut self,
        path: CanonicalizedComponentModulePath,
        source: ModuleSource,
        source_package_id: Option<SourcePackageId>,
        source_map: Option<SourceMap>,
        analyze_result: Option<AnalyzedModule>,
        environment: ModuleEnvironment,
    ) -> anyhow::Result<()> {
        if !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("put_module"));
        }
        if path.module_path.is_system() {
            anyhow::bail!("You cannot push a function under '_system/'");
        }
        anyhow::ensure!(
            path.module_path.is_deps() || analyze_result.is_some(),
            "AnalyzedModule is required for non-dependency modules"
        );
        let component = path.component;
        let (module_id, version) = self
            .put_module_metadata(path, source_package_id, analyze_result, environment)
            .await?;
        self.put_module_source_into_db(module_id, version, source, source_map, component)
            .await
    }

    async fn put_module_metadata(
        &mut self,
        path: CanonicalizedComponentModulePath,
        source_package_id: Option<SourcePackageId>,
        analyze_result: Option<AnalyzedModule>,
        environment: ModuleEnvironment,
    ) -> anyhow::Result<(ResolvedDocumentId, ModuleVersion)> {
        let (module_id, version) = match self.module_metadata(path.clone()).await? {
            Some(module_metadata) => {
                let previous_version = module_metadata.latest_version;

                // Delete the old module version since it has no more references.
                let previous_version_id = self
                    .get_version(module_metadata.id(), previous_version)
                    .await?
                    .id();

                let latest_version = previous_version + 1;
                let new_metadata = ModuleMetadata {
                    path: path.module_path,
                    latest_version,
                    source_package_id,
                    environment,
                    analyze_result: analyze_result.clone(),
                };
                SystemMetadataModel::new(self.tx, path.component.into())
                    .replace(module_metadata.id(), new_metadata.try_into()?)
                    .await?;

                SystemMetadataModel::new(self.tx, path.component.into())
                    .delete(previous_version_id)
                    .await?;

                (module_metadata.id(), latest_version)
            },
            None => {
                let version = 0;
                let new_metadata = ModuleMetadata {
                    path: path.module_path,
                    latest_version: version,
                    source_package_id,
                    environment,
                    analyze_result: analyze_result.clone(),
                };

                let document_id = SystemMetadataModel::new(self.tx, path.component.into())
                    .insert(&MODULES_TABLE, new_metadata.try_into()?)
                    .await?;
                (document_id, version)
            },
        };
        Ok((module_id, version))
    }

    async fn put_module_source_into_db(
        &mut self,
        module_id: ResolvedDocumentId,
        version: ModuleVersion,
        source: ModuleSource,
        source_map: Option<SourceMap>,
        component: ComponentDefinitionId,
    ) -> anyhow::Result<()> {
        let new_version = ModuleVersionMetadata {
            module_id: module_id.into(),
            source,
            source_map,
            version: Some(version),
        }.try_into()
        .map_err(|e: anyhow::Error| e.map_error_metadata(|em| {
            if em.short_msg == VALUE_TOO_LARGE_SHORT_MSG {
                // Remap the ValueTooLargeError message to something more specific
                // to the modules use case.
                let message = format!(
                    "The functions, source maps, and their dependencies in \"convex/\" are too large. See our docs (https://docs.convex.dev/using/writing-convex-functions#using-libraries) for more details. You can also run `npx convex deploy -v` to print out each source file's bundled size.\n{}", em.msg
                );
                ErrorMetadata::bad_request(
                    "ModulesTooLarge",
                    message,
                )
            } else {
                em
            }
        }))?;
        SystemMetadataModel::new(self.tx, component.into())
            .insert(&MODULE_VERSIONS_TABLE, new_version)
            .await?;
        Ok(())
    }

    /// Delete a module, making it inaccessible for subsequent transactions.
    pub async fn delete(&mut self, path: CanonicalizedComponentModulePath) -> anyhow::Result<()> {
        if !(self.tx.identity().is_admin() || self.tx.identity().is_system()) {
            anyhow::bail!(unauthorized_error("delete_module"));
        }
        let namespace = path.component.into();
        if let Some(module_metadata) = self.module_metadata(path).await? {
            let module_id = module_metadata.id();
            SystemMetadataModel::new(self.tx, namespace)
                .delete(module_id)
                .await?;

            // Delete the module version since it has no more references.
            let module_version = self
                .get_version(module_id, module_metadata.latest_version)
                .await?;
            SystemMetadataModel::new(self.tx, namespace)
                .delete(module_version.id())
                .await?;
        }
        Ok(())
    }

    #[convex_macro::instrument_future]
    async fn module_metadata(
        &mut self,
        path: CanonicalizedComponentModulePath,
    ) -> anyhow::Result<Option<ParsedDocument<ModuleMetadata>>> {
        let namespace = path.component.into();
        let module_path = ConvexValue::try_from(path.module_path.as_str())?;
        let index_range = IndexRange {
            index_name: MODULE_INDEX_BY_PATH.clone(),
            range: vec![IndexRangeExpression::Eq(
                PATH_FIELD.clone(),
                module_path.into(),
            )],
            order: Order::Asc,
        };
        let module_query = Query::index_range(index_range);
        let mut query_stream = ResolvedQuery::new(self.tx, namespace, module_query)?;
        let module_document: ParsedDocument<ModuleMetadata> =
            match query_stream.expect_at_most_one(self.tx).await? {
                Some(v) => v.try_into()?,
                None => return Ok(None),
            };
        Ok(Some(module_document))
    }

    // Helper method that returns the AnalyzedFunction for the specified path.
    // It returns a user error if the module or function does not exist.
    // Note that using this method will error if AnalyzedResult is not backfilled,
    pub async fn get_analyzed_function(
        &mut self,
        path: &CanonicalizedComponentFunctionPath,
    ) -> anyhow::Result<anyhow::Result<AnalyzedFunction>> {
        let udf_path = &path.udf_path;
        let Some(module) = self.get_metadata_for_function(path.clone()).await? else {
            let err = ModuleNotFoundError::new(udf_path.module().as_str());
            return Ok(Err(ErrorMetadata::bad_request(
                "ModuleNotFound",
                err.to_string(),
            )
            .into()));
        };

        // Dependency modules don't have AnalyzedModule.
        if !udf_path.module().is_deps() {
            let analyzed_module = module
                .analyze_result
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Expected analyze result for {udf_path:?}"))?;

            for function in &analyzed_module.functions {
                if &function.name == udf_path.function_name() {
                    return Ok(Ok(function.clone()));
                }
            }
        }

        Ok(Err(ErrorMetadata::bad_request(
            "FunctionNotFound",
            FunctionNotFoundError::new(udf_path.function_name(), udf_path.module().as_str())
                .to_string(),
        )
        .into()))
    }

    pub fn record_module_version_read_dependency(
        &mut self,
        module_id: ResolvedDocumentId,
    ) -> anyhow::Result<()> {
        let fields = vec![MODULE_ID_FIELD.clone()];
        let values = vec![Some(ConvexValue::from(module_id))];
        let namespace = self
            .tx
            .table_mapping()
            .tablet_namespace(module_id.table().tablet_id)?;
        let module_index_name = MODULE_VERSION_INDEX
            .clone()
            .map_table(&self.tx.table_mapping().namespace(namespace).name_to_id())?
            .into();
        self.tx.record_system_table_cache_hit(
            module_index_name,
            fields.try_into().expect("Must be valid"),
            Interval::prefix(BinaryKey::from(values_to_bytes(&values[..]))),
        );
        Ok(())
    }

    pub async fn has_http(&mut self) -> anyhow::Result<bool> {
        let path = CanonicalizedComponentModulePath {
            component: ComponentDefinitionId::Root,
            module_path: "http.js".parse()?,
        };
        Ok(self.get_metadata(path).await?.is_some())
    }
}
