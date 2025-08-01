use std::sync::Arc;

use crate::construct_version_graph_orchestrator::{
    ConstructVersionGraphError, ConstructVersionGraphOrchestrator,
};
use crate::operators::compute_versions_to_delete_from_graph::{
    CollectionVersionAction, ComputeVersionsToDeleteError, ComputeVersionsToDeleteInput,
    ComputeVersionsToDeleteOperator, ComputeVersionsToDeleteOutput,
};
use crate::operators::delete_unused_files::{
    DeleteUnusedFilesError, DeleteUnusedFilesInput, DeleteUnusedFilesOperator,
    DeleteUnusedFilesOutput,
};
use crate::operators::delete_unused_logs::{
    DeleteUnusedLogsError, DeleteUnusedLogsInput, DeleteUnusedLogsOperator, DeleteUnusedLogsOutput,
};
use crate::operators::delete_versions_at_sysdb::{
    DeleteVersionsAtSysDbError, DeleteVersionsAtSysDbInput, DeleteVersionsAtSysDbOperator,
    DeleteVersionsAtSysDbOutput,
};
use crate::operators::list_files_at_version::{
    ListFilesAtVersionError, ListFilesAtVersionInput, ListFilesAtVersionOutput,
    ListFilesAtVersionsOperator,
};
use crate::operators::mark_versions_at_sysdb::{
    MarkVersionsAtSysDbError, MarkVersionsAtSysDbInput, MarkVersionsAtSysDbOperator,
    MarkVersionsAtSysDbOutput,
};
use crate::types::{
    version_graph_to_collection_dependency_graph, CleanupMode, GarbageCollectorResponse,
    VersionGraph,
};
use async_trait::async_trait;
use chroma_blockstore::RootManager;
use chroma_error::{ChromaError, ErrorCodes};
use chroma_log::Log;
use chroma_storage::Storage;
use chroma_sysdb::{GetCollectionsOptions, SysDb};
use chroma_system::{
    wrap, ChannelError, ComponentContext, ComponentHandle, Dispatcher, Handler, Orchestrator,
    OrchestratorContext, PanicError, System, TaskError, TaskResult,
};
use chroma_types::chroma_proto::{CollectionVersionFile, VersionListForCollection};
use chroma_types::{CollectionUuid, DeleteCollectionError};
use chrono::{DateTime, Utc};
use petgraph::algo::toposort;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::SystemTime;
use thiserror::Error;
use tokio::sync::oneshot::{error::RecvError, Sender};
use tracing::Span;
use wal3::LogPosition;

#[derive(Debug)]
pub struct GarbageCollectorOrchestrator {
    collection_id: CollectionUuid,
    version_file_path: String,
    lineage_file_path: Option<String>,
    version_absolute_cutoff_time: DateTime<Utc>,
    collection_soft_delete_absolute_cutoff_time: DateTime<Utc>,
    sysdb_client: SysDb,
    context: OrchestratorContext,
    system: System,
    storage: Storage,
    logs: Log,
    root_manager: RootManager,
    result_channel: Option<Sender<Result<GarbageCollectorResponse, GarbageCollectorError>>>,
    cleanup_mode: CleanupMode,
    version_files: HashMap<CollectionUuid, Arc<CollectionVersionFile>>,
    versions_to_delete_output: Option<ComputeVersionsToDeleteOutput>,
    pending_mark_versions_at_sysdb_tasks: HashSet<CollectionUuid>,
    pending_list_files_at_version_tasks: HashSet<(CollectionUuid, i64)>,
    delete_unused_file_output: Option<DeleteUnusedFilesOutput>,
    delete_unused_log_output: Option<DeleteUnusedLogsOutput>,
    file_ref_counts: HashMap<String, u32>,
    num_pending_tasks: usize,
    min_versions_to_keep: u32,
    enable_log_gc: bool,
    graph: Option<VersionGraph>,
    soft_deleted_collections_to_gc: HashSet<CollectionUuid>,
    tenant: Option<String>,
    database_name: Option<String>,

    num_files_deleted: u32,
    num_versions_deleted: u32,

    enable_dangerous_option_to_ignore_min_versions_for_wal3: bool,
}

#[allow(clippy::too_many_arguments)]
impl GarbageCollectorOrchestrator {
    pub fn new(
        collection_id: CollectionUuid,
        version_file_path: String,
        lineage_file_path: Option<String>,
        version_absolute_cutoff_time: DateTime<Utc>,
        collection_soft_delete_absolute_cutoff_time: DateTime<Utc>,
        sysdb_client: SysDb,
        dispatcher: ComponentHandle<Dispatcher>,
        system: System,
        storage: Storage,
        logs: Log,
        root_manager: RootManager,
        cleanup_mode: CleanupMode,
        min_versions_to_keep: u32,
        enable_log_gc: bool,
        enable_dangerous_option_to_ignore_min_versions_for_wal3: bool,
    ) -> Self {
        Self {
            collection_id,
            version_file_path,
            lineage_file_path,
            version_absolute_cutoff_time,
            collection_soft_delete_absolute_cutoff_time,
            sysdb_client,
            context: OrchestratorContext::new(dispatcher),
            system,
            storage,
            logs,
            root_manager,
            cleanup_mode,
            result_channel: None,
            version_files: HashMap::new(),
            file_ref_counts: HashMap::new(),
            versions_to_delete_output: None,
            pending_mark_versions_at_sysdb_tasks: HashSet::new(),
            pending_list_files_at_version_tasks: HashSet::new(),
            delete_unused_file_output: None,
            delete_unused_log_output: None,
            num_pending_tasks: 0,
            min_versions_to_keep,
            enable_log_gc,
            graph: None,
            soft_deleted_collections_to_gc: HashSet::new(),
            tenant: None,
            database_name: None,

            num_files_deleted: 0,
            num_versions_deleted: 0,

            enable_dangerous_option_to_ignore_min_versions_for_wal3,
        }
    }
}

#[derive(Error, Debug)]
pub enum GarbageCollectorError {
    #[error("Panic during compaction: {0}")]
    Panic(#[from] PanicError),
    #[error("Error sending message through channel: {0}")]
    Channel(#[from] ChannelError),
    #[error("Error receiving final result: {0}")]
    Result(#[from] RecvError),
    #[error("{0}")]
    Generic(#[from] Box<dyn ChromaError>),
    #[error("{0}")]
    Wal3(#[from] wal3::Error),
    #[error("The task was aborted because resources were exhausted")]
    Aborted,

    #[error("Failed to construct version graph: {0}")]
    ConstructVersionGraph(#[from] ConstructVersionGraphError),
    #[error("Failed to compute versions to delete: {0}")]
    ComputeVersionsToDelete(#[from] ComputeVersionsToDeleteError),
    #[error("Failed to mark versions at sysdb: {0}")]
    MarkVersionsAtSysDb(#[from] MarkVersionsAtSysDbError),
    #[error("Failed to list files at version: {0}")]
    ListFilesAtVersion(#[from] ListFilesAtVersionError),
    #[error("Failed to delete unused files: {0}")]
    DeleteUnusedFiles(#[from] DeleteUnusedFilesError),
    #[error("Failed to delete unused logs: {0}")]
    DeleteUnusedLogs(#[from] DeleteUnusedLogsError),
    #[error("Failed to delete versions at sysdb: {0}")]
    DeleteVersionsAtSysDb(#[from] DeleteVersionsAtSysDbError),

    #[error("Expected version file missing for collection {0}")]
    MissingVersionFile(CollectionUuid),
    #[error("Invariant violation: {0}")]
    InvariantViolation(String),
    #[error("Could not parse UUID: {0}")]
    UnparsableUuid(#[from] uuid::Error),
    #[error("Collection deletion failed: {0}")]
    CollectionDeletionFailed(#[from] DeleteCollectionError),
    #[error("SysDb method failed: {0}")]
    SysDbMethodFailed(String),
}

impl ChromaError for GarbageCollectorError {
    fn code(&self) -> ErrorCodes {
        ErrorCodes::Internal
    }
}

impl<E> From<TaskError<E>> for GarbageCollectorError
where
    E: Into<GarbageCollectorError>,
{
    fn from(value: TaskError<E>) -> Self {
        match value {
            TaskError::Panic(e) => GarbageCollectorError::Panic(e),
            TaskError::TaskFailed(e) => e.into(),
            TaskError::Aborted => GarbageCollectorError::Aborted,
        }
    }
}

#[derive(Debug)]
struct ConstructVersionGraphRequest;

#[async_trait]
impl Orchestrator for GarbageCollectorOrchestrator {
    type Output = GarbageCollectorResponse;
    type Error = GarbageCollectorError;

    fn dispatcher(&self) -> ComponentHandle<Dispatcher> {
        self.context.dispatcher.clone()
    }

    fn context(&self) -> &OrchestratorContext {
        &self.context
    }

    async fn on_start(&mut self, ctx: &ComponentContext<Self>) {
        ctx.receiver()
            .send(ConstructVersionGraphRequest, Some(Span::current()))
            .await
            .expect("Failed to send ConstructVersionGraphRequest");
    }

    fn set_result_channel(
        &mut self,
        sender: Sender<Result<GarbageCollectorResponse, GarbageCollectorError>>,
    ) {
        self.result_channel = Some(sender);
    }

    fn take_result_channel(
        &mut self,
    ) -> Sender<Result<GarbageCollectorResponse, GarbageCollectorError>> {
        self.result_channel
            .take()
            .expect("The result channel should be set before take")
    }
}

impl GarbageCollectorOrchestrator {
    async fn get_eligible_soft_deleted_collections_to_gc(
        &mut self,
        all_collection_ids: Vec<CollectionUuid>,
    ) -> Result<HashSet<CollectionUuid>, GarbageCollectorError> {
        let soft_delete_statuses = self
            .sysdb_client
            .batch_get_collection_soft_delete_status(all_collection_ids.clone())
            .await
            .map_err(|e| GarbageCollectorError::SysDbMethodFailed(e.to_string()))?;

        let all_collection_ids: HashSet<CollectionUuid> = all_collection_ids.into_iter().collect();

        let soft_deleted_collections_to_gc = soft_delete_statuses
            .iter()
            .filter_map(|(collection_id, status)| {
                if *status && all_collection_ids.contains(collection_id) {
                    Some(*collection_id)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let collections = self
            .sysdb_client
            .get_collections(GetCollectionsOptions {
                collection_ids: Some(soft_deleted_collections_to_gc),
                include_soft_deleted: true,
                ..Default::default()
            })
            .await
            .map_err(|e| GarbageCollectorError::SysDbMethodFailed(e.to_string()))?;

        let mut eligible_ids = HashSet::new();
        let cutoff_time: SystemTime = self.collection_soft_delete_absolute_cutoff_time.into();
        for collection in collections {
            if collection.updated_at < cutoff_time {
                eligible_ids.insert(collection.collection_id);
            } else {
                tracing::trace!(
                    "Will not transition soft-deleted collection {} because it was updated after the cutoff time.",
                    collection.collection_id
                );
            }
        }

        Ok(eligible_ids)
    }

    async fn handle_construct_version_graph_request(
        &mut self,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        let orchestrator = ConstructVersionGraphOrchestrator::new(
            self.dispatcher(),
            self.storage.clone(),
            self.sysdb_client.clone(),
            self.collection_id,
            self.version_file_path.clone(),
            self.lineage_file_path.clone(),
        );
        let output = orchestrator.run(self.system.clone()).await?;

        let collection_ids = output.version_files.keys().cloned().collect::<Vec<_>>();

        self.soft_deleted_collections_to_gc = self
            .get_eligible_soft_deleted_collections_to_gc(collection_ids)
            .await?;

        tracing::debug!(
            "Soft deleted collections to GC: {:#?}",
            self.soft_deleted_collections_to_gc
        );

        self.version_files = output.version_files;
        self.graph = Some(output.graph.clone());

        let task = wrap(
            Box::new(ComputeVersionsToDeleteOperator {}),
            ComputeVersionsToDeleteInput {
                graph: output.graph,
                soft_deleted_collections: self.soft_deleted_collections_to_gc.clone(),
                cutoff_time: self.version_absolute_cutoff_time,
                min_versions_to_keep: self.min_versions_to_keep,
            },
            ctx.receiver(),
            self.context.task_cancellation_token.clone(),
        );

        self.dispatcher()
            .send(task, Some(Span::current()))
            .await
            .map_err(GarbageCollectorError::Channel)?;
        Ok(())
    }

    async fn handle_compute_versions_to_delete_output(
        &mut self,
        output: ComputeVersionsToDeleteOutput,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        if output.versions.is_empty() {
            tracing::debug!("No versions to delete");
            let response = GarbageCollectorResponse {
                num_versions_deleted: 0,
                num_files_deleted: 0,
                collection_id: self.collection_id,
                ..Default::default()
            };
            self.terminate_with_result(Ok(response), ctx).await;
            return Ok(());
        }

        self.pending_list_files_at_version_tasks = output
            .versions
            .iter()
            .flat_map(|(collection_id, versions)| {
                versions
                    .keys()
                    .map(|version| (*collection_id, *version))
                    .collect::<HashSet<_>>()
            })
            .collect();

        self.pending_mark_versions_at_sysdb_tasks = output.versions.keys().cloned().collect();

        for (collection_id, versions) in &output.versions {
            let version_file = self
                .version_files
                .get(collection_id)
                .ok_or(GarbageCollectorError::MissingVersionFile(*collection_id))?;

            let collection_info = version_file.collection_info_immutable.as_ref().ok_or(
                GarbageCollectorError::InvariantViolation(
                    "Expected collection_info_immutable to be set".to_string(),
                ),
            )?;

            // Spawn task to mark versions as deleted
            let versions_to_mark = versions
                .iter()
                .filter_map(|(version, action)| {
                    if *action == CollectionVersionAction::Delete {
                        Some(*version)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            let mark_deleted_versions_task = wrap(
                Box::new(MarkVersionsAtSysDbOperator {}),
                MarkVersionsAtSysDbInput {
                    version_file: version_file.clone(),
                    versions_to_delete: VersionListForCollection {
                        collection_id: collection_id.to_string(),
                        versions: versions_to_mark,
                        tenant_id: collection_info.tenant_id.clone(),
                        database_id: collection_info.database_id.clone(),
                    },
                    // TODO(@codetheweb): remove unused fields
                    sysdb_client: self.sysdb_client.clone(),
                    epoch_id: 0,
                    oldest_version_to_keep: 0,
                },
                ctx.receiver(),
                self.context.task_cancellation_token.clone(),
            );
            self.dispatcher()
                .send(mark_deleted_versions_task, Some(Span::current()))
                .await
                .map_err(GarbageCollectorError::Channel)?;

            for version in versions.keys() {
                let task = wrap(
                    Box::new(ListFilesAtVersionsOperator {}),
                    ListFilesAtVersionInput::new(
                        self.root_manager.clone(),
                        version_file.clone(),
                        *version,
                    ),
                    ctx.receiver(),
                    self.context.task_cancellation_token.clone(),
                );

                self.dispatcher()
                    .send(task, Some(Span::current()))
                    .await
                    .map_err(GarbageCollectorError::Channel)?;
            }
        }

        self.versions_to_delete_output = Some(output);

        Ok(())
    }

    async fn handle_mark_versions_at_sysdb_output(
        &mut self,
        output: MarkVersionsAtSysDbOutput,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        let collection_id = CollectionUuid::from_str(&output.versions_to_delete.collection_id)
            .map_err(GarbageCollectorError::UnparsableUuid)?;

        self.pending_mark_versions_at_sysdb_tasks
            .remove(&collection_id);

        self.try_start_delete_unused_logs_operator(ctx).await?;

        self.try_start_delete_unused_files_operator(ctx).await?;

        Ok(())
    }

    async fn try_start_delete_unused_logs_operator(
        &mut self,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        if !self.pending_mark_versions_at_sysdb_tasks.is_empty() {
            return Ok(());
        }
        let collections_to_destroy = self.soft_deleted_collections_to_gc.clone();
        let mut collections_to_garbage_collect = HashMap::new();
        let versions_to_delete = self.versions_to_delete_output.as_ref().ok_or(
            GarbageCollectorError::InvariantViolation(
                "Expected versions_to_delete_output to be set".to_string(),
            ),
        )?;

        for (collection_id, versions) in &versions_to_delete.versions {
            let Some(&min_version_to_keep) = versions
                .iter()
                .filter_map(|(version, action)| {
                    (matches!(action, CollectionVersionAction::Keep) && *version > 0)
                        .then_some(version)
                })
                .min()
            else {
                continue;
            };
            let version_file = self.version_files.get(collection_id).ok_or(
                GarbageCollectorError::InvariantViolation(
                    "Expected version file to be present".to_string(),
                ),
            )?;
            let version_history = &version_file
                .as_ref()
                .version_history
                .as_ref()
                .ok_or(GarbageCollectorError::InvariantViolation(
                    "Expected version file to contain version history".to_string(),
                ))?
                .versions;
            let min_compaction_log_offset = version_history
                .iter()
                .find(|version_info| version_info.version == min_version_to_keep)
                .ok_or(GarbageCollectorError::InvariantViolation(
                    "Expected min kept version to be present in version history".to_string(),
                ))?
                .collection_info_mutable
                .as_ref()
                .ok_or(GarbageCollectorError::InvariantViolation(
                    "Expected collection info to be present in version history".to_string(),
                ))?
                .current_log_position
                .try_into()
                .map_err(|_| {
                    GarbageCollectorError::InvariantViolation(
                        "Expected log offset to be unsigned".to_string(),
                    )
                })?;
            collections_to_garbage_collect.insert(
                *collection_id,
                // The minimum offset to keep is one after the minimum compaction offset
                LogPosition::from_offset(min_compaction_log_offset) + 1u64,
            );
        }

        let task = wrap(
            Box::new(DeleteUnusedLogsOperator {
                enabled: self.enable_log_gc,
                mode: self.cleanup_mode,
                storage: self.storage.clone(),
                logs: self.logs.clone(),
                enable_dangerous_option_to_ignore_min_versions_for_wal3: self
                    .enable_dangerous_option_to_ignore_min_versions_for_wal3,
            }),
            DeleteUnusedLogsInput {
                collections_to_destroy,
                collections_to_garbage_collect,
            },
            ctx.receiver(),
            self.context.task_cancellation_token.clone(),
        );
        self.dispatcher()
            .send(task, Some(Span::current()))
            .await
            .map_err(GarbageCollectorError::Channel)?;
        Ok(())
    }

    async fn handle_list_files_at_version_output(
        &mut self,
        output: ListFilesAtVersionOutput,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        let version_action = self
            .versions_to_delete_output
            .as_ref()
            .ok_or(GarbageCollectorError::InvariantViolation(
                "Expected versions_to_delete_output to be set".to_string(),
            ))?
            .versions
            .get(&output.collection_id)
            .ok_or(GarbageCollectorError::InvariantViolation(format!(
                "Expected versions_to_delete_output to contain collection {}",
                output.collection_id
            )))?
            .get(&output.version)
            .ok_or(GarbageCollectorError::InvariantViolation(format!(
                "Expected versions_to_delete_output to contain version {} for collection {}",
                output.version, output.collection_id
            )))?;

        tracing::trace!(
            "Received ListFilesAtVersionOutput for collection {} at version {}. Action: {:?}. File paths: {:#?}",
            output.collection_id,
            output.version,
            version_action,
            output.file_paths,
        );

        if output.file_paths.is_empty() {
            // We only allow empty file paths if the version is 0 and all ancestors are also at v0. Otherwise, compaction should have flushed new file paths. This check is defensive and should never fail.
            let graph = self
                .graph
                .as_ref()
                .ok_or(GarbageCollectorError::InvariantViolation(
                    "Expected graph to be set".to_string(),
                ))?;

            let this_node = graph
                .node_indices()
                .find(|&n| {
                    let node = graph.node_weight(n).expect("Node should exist");
                    node.collection_id == output.collection_id && node.version == output.version
                })
                .ok_or(GarbageCollectorError::InvariantViolation(format!(
                    "Expected to find node for collection {} at version {}",
                    output.collection_id, output.version
                )))?;

            let root_index = graph
                .node_indices()
                .find(|&n| {
                    graph
                        .neighbors_directed(n, petgraph::Direction::Incoming)
                        .next()
                        .is_none()
                })
                .ok_or(GarbageCollectorError::InvariantViolation(
                    "Expected to find root node".to_string(),
                ))?;
            let root = graph.node_weight(root_index).expect("Node should exist");

            let versions_from_root_to_this_node = petgraph::algo::astar(
                graph,
                root_index,
                |finish| finish == this_node,
                |_| 1,
                |_| 0,
            )
            .ok_or(GarbageCollectorError::InvariantViolation(format!(
                "Expected to find path from root ({}@v{}) to node for {}@v{}",
                root.collection_id, root.version, output.collection_id, output.version
            )))?
            .1
            .into_iter()
            .map(|i| {
                let node = graph.node_weight(i).expect("Node should exist");
                node.version
            })
            .collect::<Vec<_>>();
            let are_all_versions_v0 = versions_from_root_to_this_node
                .iter()
                .all(|&version| version == 0);

            if !are_all_versions_v0 {
                return Err(GarbageCollectorError::InvariantViolation(format!(
                    "Version {} of collection {} has no file paths, but has non-v0 ancestors. This should never happen.",
                    output.version, output.collection_id
                )));
            }
        }

        // Update the file ref counts. Counts in the map should:
        // - be 0 if we know about the file but it is unused
        // - be > 0 if we know about the file and it is used
        // We accomplish this by incrementing the count for files that are used and populating the map with 0 (if the entry does not exist) for files that are unused.
        match version_action {
            CollectionVersionAction::Keep => {
                tracing::debug!(
                    "Marking {} files as used for collection {} at version {}",
                    output.file_paths.len(),
                    output.collection_id,
                    output.version
                );

                for file_path in output.file_paths {
                    let count = self.file_ref_counts.entry(file_path).or_insert(0);
                    *count += 1;
                }
            }
            CollectionVersionAction::Delete => {
                tracing::debug!(
                    "Marking {} files as unused for collection {} at version {}",
                    output.file_paths.len(),
                    output.collection_id,
                    output.version
                );

                for file_path in output.file_paths {
                    self.file_ref_counts.entry(file_path).or_insert(0);
                }
            }
        }

        self.pending_list_files_at_version_tasks
            .remove(&(output.collection_id, output.version));

        self.try_start_delete_unused_files_operator(ctx).await?;

        Ok(())
    }

    async fn try_start_delete_unused_files_operator(
        &mut self,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        if !self.pending_list_files_at_version_tasks.is_empty() {
            return Ok(());
        }
        if !self.pending_mark_versions_at_sysdb_tasks.is_empty() {
            return Ok(());
        }

        // We now have results for all ListFilesAtVersionsOperator tasks that we spawned
        tracing::trace!("File ref counts: {:#?}", self.file_ref_counts);
        let file_paths_to_delete = self
            .file_ref_counts
            .iter()
            .filter_map(|(path, count)| {
                if *count == 0 {
                    Some(path.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let delete_percentage =
            file_paths_to_delete.len() as f32 / self.file_ref_counts.len() as f32 * 100.0;

        tracing::debug!(
            delete_percentage = delete_percentage,
            "Deleting {} files out of a total of {}",
            file_paths_to_delete.len(),
            self.file_ref_counts.len()
        );

        if file_paths_to_delete.is_empty() {
            tracing::debug!("No files to delete.");
        }

        let version_file =
            self.version_files
                .values()
                .next()
                .ok_or(GarbageCollectorError::InvariantViolation(
                    "Expected there to be at least one version file".to_string(),
                ))?;
        // Assumes that all collections in a fork tree are under the same tenant
        let collection_info = version_file.collection_info_immutable.as_ref().ok_or(
            GarbageCollectorError::InvariantViolation(
                "Expected collection_info_immutable to be set".to_string(),
            ),
        )?;
        let tenant_id = collection_info.tenant_id.clone();
        self.tenant = Some(tenant_id.clone());
        let database_name = collection_info.database_name.clone();
        self.database_name = Some(database_name.clone());

        let task = wrap(
            Box::new(DeleteUnusedFilesOperator::new(
                self.storage.clone(),
                self.cleanup_mode,
                tenant_id,
            )),
            DeleteUnusedFilesInput {
                unused_s3_files: file_paths_to_delete,
                hnsw_prefixes_for_deletion: vec![],
            },
            ctx.receiver(),
            self.context.task_cancellation_token.clone(),
        );
        self.dispatcher()
            .send(task, Some(Span::current()))
            .await
            .map_err(GarbageCollectorError::Channel)?;

        Ok(())
    }

    async fn try_start_delete_versions_at_sysdb_operator(
        &mut self,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        let Some(output) = self.delete_unused_file_output.as_ref() else {
            return Ok(());
        };

        if self.delete_unused_log_output.is_none() {
            return Ok(());
        }

        if self.cleanup_mode == CleanupMode::DryRun {
            tracing::info!("Dry run mode, skipping actual deletion");
            let response = GarbageCollectorResponse {
                num_versions_deleted: 0,
                num_files_deleted: 0,
                collection_id: self.collection_id,
                ..Default::default()
            };
            self.terminate_with_result(Ok(response), ctx).await;
            return Ok(());
        }

        self.num_files_deleted += output.deleted_files.len() as u32;

        let versions_to_delete = self.versions_to_delete_output.as_ref().ok_or(
            GarbageCollectorError::InvariantViolation(
                "Expected versions_to_delete_output to be set".to_string(),
            ),
        )?;

        let versions_to_delete = versions_to_delete
            .versions
            .iter()
            .filter_map(|(collection_id, versions)| {
                let versions = versions
                    .iter()
                    .filter_map(|(version, action)| {
                        if *action == CollectionVersionAction::Delete {
                            Some(*version)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                if versions.is_empty() {
                    None
                } else {
                    Some((*collection_id, versions))
                }
            })
            .collect::<HashMap<_, _>>();

        let total_num_versions_to_delete = versions_to_delete
            .values()
            .map(|versions| versions.len())
            .sum::<usize>();

        if total_num_versions_to_delete == 0 {
            tracing::debug!("No versions to delete");
            let response = GarbageCollectorResponse {
                num_versions_deleted: 0,
                num_files_deleted: 0,
                collection_id: self.collection_id,
                ..Default::default()
            };
            self.terminate_with_result(Ok(response), ctx).await;
            return Ok(());
        }

        tracing::debug!(
            "Deleting {} versions from sysdb across {} collections",
            total_num_versions_to_delete,
            versions_to_delete.len()
        );
        self.num_pending_tasks += versions_to_delete.len();

        for (collection_id, versions) in versions_to_delete {
            let version_file = self
                .version_files
                .get(&collection_id)
                .ok_or(GarbageCollectorError::MissingVersionFile(collection_id))?;

            let collection_info = version_file.collection_info_immutable.as_ref().ok_or(
                GarbageCollectorError::InvariantViolation(
                    "Expected collection_info_immutable to be set".to_string(),
                ),
            )?;

            let delete_versions_task = wrap(
                Box::new(DeleteVersionsAtSysDbOperator {
                    storage: self.storage.clone(),
                }),
                DeleteVersionsAtSysDbInput {
                    version_file: version_file.clone(),
                    epoch_id: 0,
                    sysdb_client: self.sysdb_client.clone(),
                    versions_to_delete: VersionListForCollection {
                        tenant_id: collection_info.tenant_id.clone(),
                        database_id: collection_info.database_id.clone(),
                        collection_id: collection_id.to_string(),
                        versions,
                    },
                    unused_s3_files: output.deleted_files.clone(),
                },
                ctx.receiver(),
                self.context.task_cancellation_token.clone(),
            );

            self.dispatcher()
                .send(delete_versions_task, Some(Span::current()))
                .await
                .map_err(GarbageCollectorError::Channel)?;
        }

        Ok(())
    }

    async fn handle_delete_versions_output(
        &mut self,
        output: DeleteVersionsAtSysDbOutput,
        ctx: &ComponentContext<Self>,
    ) -> Result<(), GarbageCollectorError> {
        tracing::trace!("Received DeleteVersionsAtSysDbOutput: {:#?}", output);
        self.num_versions_deleted += output.versions_to_delete.versions.len() as u32;

        self.num_pending_tasks -= 1;
        if self.num_pending_tasks == 0 {
            let graph = self
                .graph
                .as_ref()
                .ok_or(GarbageCollectorError::InvariantViolation(
                    "Expected graph to be set".to_string(),
                ))?;

            let mut ordered_soft_deleted_to_hard_delete_collections = vec![];

            // We cannot finalize collection deletion (perform a hard delete) if there are any forked collections downstream that are still alive. If we violated this invariant, there would be a missing edge in the lineage file (resulting in an unconnected graph).
            // We must also delete collections in reverse topological order, so that we delete children before parents.
            let collection_dependency_graph = version_graph_to_collection_dependency_graph(graph);
            let topo = toposort(&collection_dependency_graph, None).map_err(|_| {
                GarbageCollectorError::InvariantViolation(
                    "Failed to topologically sort collection dependency graph".to_string(),
                )
            })?;

            for collection_id in topo.iter().rev() {
                // This check is not strictly needed as are_all_children_soft_deleted will be false if the current node is not soft deleted, but it avoids unnecessary computation and prevents misleading logs.
                if !self.soft_deleted_collections_to_gc.contains(collection_id) {
                    continue;
                }

                let are_all_children_soft_deleted = petgraph::algo::dijkstra(
                    &collection_dependency_graph,
                    *collection_id,
                    None,
                    |_| 1,
                )
                .keys()
                .all(|child_id| self.soft_deleted_collections_to_gc.contains(child_id));

                if are_all_children_soft_deleted {
                    ordered_soft_deleted_to_hard_delete_collections.push(*collection_id);
                } else {
                    tracing::trace!(
                        "Skipping hard delete for collection {} because not all children are soft deleted",
                        collection_id
                    );
                }
            }

            tracing::debug!(
                "Hard deleting collections {:#?}",
                ordered_soft_deleted_to_hard_delete_collections
            );

            for collection_id in ordered_soft_deleted_to_hard_delete_collections {
                self.sysdb_client
                    .finish_collection_deletion(
                        self.tenant
                            .clone()
                            .ok_or(GarbageCollectorError::InvariantViolation(
                                "Expected tenant to be set".to_string(),
                            ))?,
                        self.database_name.clone().ok_or(
                            GarbageCollectorError::InvariantViolation(
                                "Expected database to be set".to_string(),
                            ),
                        )?,
                        collection_id,
                    )
                    .await?;
            }

            let response = GarbageCollectorResponse {
                num_files_deleted: self.num_files_deleted,
                num_versions_deleted: self.num_versions_deleted,
                collection_id: self.collection_id,
                ..Default::default()
            };

            self.terminate_with_result(Ok(response), ctx).await;
        }

        Ok(())
    }
}

#[async_trait]
impl Handler<ConstructVersionGraphRequest> for GarbageCollectorOrchestrator {
    type Result = ();

    async fn handle(
        &mut self,
        _: ConstructVersionGraphRequest,
        ctx: &ComponentContext<GarbageCollectorOrchestrator>,
    ) {
        let res = self.handle_construct_version_graph_request(ctx).await;
        self.ok_or_terminate(res, ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<ComputeVersionsToDeleteOutput, ComputeVersionsToDeleteError>>
    for GarbageCollectorOrchestrator
{
    type Result = ();

    async fn handle(
        &mut self,
        message: TaskResult<ComputeVersionsToDeleteOutput, ComputeVersionsToDeleteError>,
        ctx: &ComponentContext<GarbageCollectorOrchestrator>,
    ) {
        let output = match self.ok_or_terminate(message.into_inner(), ctx).await {
            Some(output) => output,
            None => return,
        };
        let res = self
            .handle_compute_versions_to_delete_output(output, ctx)
            .await;
        self.ok_or_terminate(res, ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<MarkVersionsAtSysDbOutput, MarkVersionsAtSysDbError>>
    for GarbageCollectorOrchestrator
{
    type Result = ();

    async fn handle(
        &mut self,
        message: TaskResult<MarkVersionsAtSysDbOutput, MarkVersionsAtSysDbError>,
        ctx: &ComponentContext<GarbageCollectorOrchestrator>,
    ) {
        let output = match self.ok_or_terminate(message.into_inner(), ctx).await {
            Some(output) => output,
            None => return,
        };
        let res = self.handle_mark_versions_at_sysdb_output(output, ctx).await;
        self.ok_or_terminate(res, ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<ListFilesAtVersionOutput, ListFilesAtVersionError>>
    for GarbageCollectorOrchestrator
{
    type Result = ();

    async fn handle(
        &mut self,
        message: TaskResult<ListFilesAtVersionOutput, ListFilesAtVersionError>,
        ctx: &ComponentContext<GarbageCollectorOrchestrator>,
    ) {
        let output = match self.ok_or_terminate(message.into_inner(), ctx).await {
            Some(output) => output,
            None => return,
        };
        let res = self.handle_list_files_at_version_output(output, ctx).await;
        self.ok_or_terminate(res, ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<DeleteUnusedFilesOutput, DeleteUnusedFilesError>>
    for GarbageCollectorOrchestrator
{
    type Result = ();

    async fn handle(
        &mut self,
        message: TaskResult<DeleteUnusedFilesOutput, DeleteUnusedFilesError>,
        ctx: &ComponentContext<GarbageCollectorOrchestrator>,
    ) {
        let output = match self.ok_or_terminate(message.into_inner(), ctx).await {
            Some(output) => output,
            None => return,
        };

        self.delete_unused_file_output = Some(output);
        let res = self.try_start_delete_versions_at_sysdb_operator(ctx).await;
        self.ok_or_terminate(res, ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<DeleteUnusedLogsOutput, DeleteUnusedLogsError>>
    for GarbageCollectorOrchestrator
{
    type Result = ();

    async fn handle(
        &mut self,
        message: TaskResult<DeleteUnusedLogsOutput, DeleteUnusedLogsError>,
        ctx: &ComponentContext<GarbageCollectorOrchestrator>,
    ) {
        let output = match self.ok_or_terminate(message.into_inner(), ctx).await {
            Some(output) => output,
            None => return,
        };

        self.delete_unused_log_output = Some(output);
        let res = self.try_start_delete_versions_at_sysdb_operator(ctx).await;
        self.ok_or_terminate(res, ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<DeleteVersionsAtSysDbOutput, DeleteVersionsAtSysDbError>>
    for GarbageCollectorOrchestrator
{
    type Result = ();

    async fn handle(
        &mut self,
        message: TaskResult<DeleteVersionsAtSysDbOutput, DeleteVersionsAtSysDbError>,
        ctx: &ComponentContext<GarbageCollectorOrchestrator>,
    ) {
        // Stage 6: Final stage - versions deleted, complete the garbage collection process
        let output = match self.ok_or_terminate(message.into_inner(), ctx).await {
            Some(output) => output,
            None => return,
        };

        let res = self.handle_delete_versions_output(output, ctx).await;
        self.ok_or_terminate(res, ctx).await;
    }
}

#[cfg(test)]
mod tests {
    use super::GarbageCollectorOrchestrator;
    use chroma_blockstore::RootManager;
    use chroma_cache::nop::NopCache;
    use chroma_log::Log;
    use chroma_storage::test_storage;
    use chroma_sysdb::{GetCollectionsOptions, TestSysDb};
    use chroma_system::{Dispatcher, Orchestrator, System};
    use chroma_types::{
        CollectionUuid, Segment, SegmentFlushInfo, SegmentScope, SegmentType, SegmentUuid,
    };
    use chrono::DateTime;
    use std::{collections::HashMap, sync::Arc, time::SystemTime};

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_on_empty_file_paths() {
        let (_storage_dir, storage) = test_storage();
        let mut test_sysdb = TestSysDb::new();
        test_sysdb.set_storage(Some(storage.clone()));
        let mut sysdb = chroma_sysdb::SysDb::Test(test_sysdb);

        let system = System::new();
        let dispatcher = Dispatcher::new(Default::default());
        let dispatcher_handle = system.start_component(dispatcher);
        let root_manager = RootManager::new(storage.clone(), Box::new(NopCache));

        let tenant = "test_tenant".to_string();
        let database = "test_database".to_string();

        let root_collection_id = CollectionUuid::new();
        let segment_id = SegmentUuid::new();
        let segment = Segment {
            id: segment_id,
            r#type: SegmentType::BlockfileMetadata,
            scope: SegmentScope::METADATA,
            collection: root_collection_id,
            metadata: None,
            file_path: HashMap::new(),
        };

        sysdb
            .create_collection(
                tenant.clone(),
                database,
                root_collection_id,
                "Root Collection".to_string(),
                vec![segment],
                None,
                None,
                None,
                false,
            )
            .await
            .unwrap();

        // Create v1 with no file paths
        sysdb
            .flush_compaction(
                tenant,
                root_collection_id,
                0,
                0,
                Arc::new([SegmentFlushInfo {
                    segment_id,
                    file_paths: HashMap::new(),
                }]),
                0,
                0,
            )
            .await
            .unwrap();

        // Should fail
        let mut collections = sysdb
            .get_collections(GetCollectionsOptions {
                collection_id: Some(root_collection_id),
                ..Default::default()
            })
            .await
            .unwrap();
        let root_collection = collections.pop().unwrap();
        let now = DateTime::from_timestamp(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            0,
        )
        .unwrap();
        let logs = Log::InMemory(chroma_log::in_memory_log::InMemoryLog::default());
        let orchestrator = GarbageCollectorOrchestrator::new(
            root_collection_id,
            root_collection.version_file_path.unwrap(),
            None,
            now,
            now,
            sysdb,
            dispatcher_handle,
            system.clone(),
            storage,
            logs,
            root_manager,
            crate::types::CleanupMode::Delete,
            1,
            true,
            false,
        );
        let result = orchestrator.run(system).await;
        assert!(result.is_err());
        assert!(format!("{:?}", result).contains("no file paths"));
    }
}
