// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementation of grpc service for flow node

use std::net::SocketAddr;
use std::sync::Arc;

use api::v1::{RowDeleteRequests, RowInsertRequests};
use cache::{TABLE_FLOWNODE_SET_CACHE_NAME, TABLE_ROUTE_CACHE_NAME};
use catalog::CatalogManagerRef;
use common_base::Plugins;
use common_error::ext::BoxedError;
use common_meta::cache::{LayeredCacheRegistryRef, TableFlownodeSetCacheRef, TableRouteCacheRef};
use common_meta::ddl::ProcedureExecutorRef;
use common_meta::key::flow::FlowMetadataManagerRef;
use common_meta::key::TableMetadataManagerRef;
use common_meta::kv_backend::KvBackendRef;
use common_meta::node_manager::{Flownode, NodeManagerRef};
use common_query::Output;
use common_telemetry::tracing::info;
use futures::{FutureExt, TryStreamExt};
use greptime_proto::v1::flow::{flow_server, FlowRequest, FlowResponse, InsertRequests};
use itertools::Itertools;
use operator::delete::Deleter;
use operator::insert::Inserter;
use operator::statement::StatementExecutor;
use partition::manager::PartitionRuleManager;
use query::{QueryEngine, QueryEngineFactory};
use servers::error::{AlreadyStartedSnafu, StartGrpcSnafu, TcpBindSnafu, TcpIncomingSnafu};
use servers::http::{HttpServer, HttpServerBuilder};
use servers::metrics_handler::MetricsHandler;
use servers::server::Server;
use session::context::{QueryContextBuilder, QueryContextRef};
use snafu::{ensure, OptionExt, ResultExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot, Mutex};
use tonic::codec::CompressionEncoding;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status};

use crate::adapter::{create_worker, CreateFlowArgs, FlowWorkerManagerRef};
use crate::error::{
    to_status_with_last_err, CacheRequiredSnafu, CreateFlowSnafu, ExternalSnafu, FlowNotFoundSnafu,
    ListFlowsSnafu, ParseAddrSnafu, ShutdownServerSnafu, StartServerSnafu, UnexpectedSnafu,
};
use crate::heartbeat::HeartbeatTask;
use crate::metrics::{METRIC_FLOW_PROCESSING_TIME, METRIC_FLOW_ROWS};
use crate::transform::register_function_to_query_engine;
use crate::utils::{SizeReportSender, StateReportHandler};
use crate::{Error, FlowWorkerManager, FlownodeOptions};

pub const FLOW_NODE_SERVER_NAME: &str = "FLOW_NODE_SERVER";
/// wrapping flow node manager to avoid orphan rule with Arc<...>
#[derive(Clone)]
pub struct FlowService {
    pub manager: FlowWorkerManagerRef,
}

impl FlowService {
    pub fn new(manager: FlowWorkerManagerRef) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl flow_server::Flow for FlowService {
    async fn handle_create_remove(
        &self,
        request: Request<FlowRequest>,
    ) -> Result<Response<FlowResponse>, Status> {
        let _timer = METRIC_FLOW_PROCESSING_TIME
            .with_label_values(&["ddl"])
            .start_timer();

        let request = request.into_inner();
        self.manager
            .handle(request)
            .await
            .map_err(|err| {
                common_telemetry::error!(err; "Failed to handle flow request");
                err
            })
            .map(Response::new)
            .map_err(to_status_with_last_err)
    }

    async fn handle_mirror_request(
        &self,
        request: Request<InsertRequests>,
    ) -> Result<Response<FlowResponse>, Status> {
        let _timer = METRIC_FLOW_PROCESSING_TIME
            .with_label_values(&["insert"])
            .start_timer();

        let request = request.into_inner();
        // TODO(discord9): fix protobuf import order shenanigans to remove this duplicated define
        let mut row_count = 0;
        let request = api::v1::region::InsertRequests {
            requests: request
                .requests
                .into_iter()
                .map(|insert| {
                    insert.rows.as_ref().inspect(|x| row_count += x.rows.len());
                    api::v1::region::InsertRequest {
                        region_id: insert.region_id,
                        rows: insert.rows,
                    }
                })
                .collect_vec(),
        };

        METRIC_FLOW_ROWS
            .with_label_values(&["in"])
            .inc_by(row_count as u64);

        self.manager
            .handle_inserts(request)
            .await
            .map(Response::new)
            .map_err(to_status_with_last_err)
    }
}

pub struct FlownodeServer {
    shutdown_tx: Mutex<Option<broadcast::Sender<()>>>,
    flow_service: FlowService,
}

impl FlownodeServer {
    pub fn new(flow_service: FlowService) -> Self {
        Self {
            flow_service,
            shutdown_tx: Mutex::new(None),
        }
    }
}

impl FlownodeServer {
    pub fn create_flow_service(&self) -> flow_server::FlowServer<impl flow_server::Flow> {
        flow_server::FlowServer::new(self.flow_service.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Zstd)
            .send_compressed(CompressionEncoding::Zstd)
    }
}

#[async_trait::async_trait]
impl servers::server::Server for FlownodeServer {
    async fn shutdown(&self) -> Result<(), servers::error::Error> {
        let mut shutdown_tx = self.shutdown_tx.lock().await;
        if let Some(tx) = shutdown_tx.take() {
            if tx.send(()).is_err() {
                info!("Receiver dropped, the flow node server has already shutdown");
            }
        }
        info!("Shutdown flow node server");

        Ok(())
    }
    async fn start(&self, addr: SocketAddr) -> Result<SocketAddr, servers::error::Error> {
        let (tx, rx) = broadcast::channel::<()>(1);
        let mut rx_server = tx.subscribe();
        let (incoming, addr) = {
            let mut shutdown_tx = self.shutdown_tx.lock().await;
            ensure!(
                shutdown_tx.is_none(),
                AlreadyStartedSnafu { server: "flow" }
            );
            let listener = TcpListener::bind(addr)
                .await
                .context(TcpBindSnafu { addr })?;
            let addr = listener.local_addr().context(TcpBindSnafu { addr })?;
            let incoming =
                TcpIncoming::from_listener(listener, true, None).context(TcpIncomingSnafu)?;
            info!("flow server is bound to {}", addr);

            *shutdown_tx = Some(tx);

            (incoming, addr)
        };

        let builder = tonic::transport::Server::builder().add_service(self.create_flow_service());

        let _handle = common_runtime::spawn_global(async move {
            let _result = builder
                .serve_with_incoming_shutdown(incoming, rx_server.recv().map(drop))
                .await
                .context(StartGrpcSnafu);
        });

        let manager_ref = self.flow_service.manager.clone();
        let _handle = manager_ref.clone().run_background(Some(rx));

        Ok(addr)
    }

    fn name(&self) -> &str {
        FLOW_NODE_SERVER_NAME
    }
}

/// The flownode server instance.
pub struct FlownodeInstance {
    server: FlownodeServer,
    addr: SocketAddr,
    /// only used for health check
    http_server: HttpServer,
    http_addr: SocketAddr,
    heartbeat_task: Option<HeartbeatTask>,
}

impl FlownodeInstance {
    pub async fn start(&mut self) -> Result<(), crate::Error> {
        if let Some(task) = &self.heartbeat_task {
            task.start().await?;
        }

        self.addr = self
            .server
            .start(self.addr)
            .await
            .context(StartServerSnafu)?;

        self.http_server
            .start(self.http_addr)
            .await
            .context(StartServerSnafu)?;

        Ok(())
    }
    pub async fn shutdown(&self) -> Result<(), crate::Error> {
        self.server.shutdown().await.context(ShutdownServerSnafu)?;

        if let Some(task) = &self.heartbeat_task {
            task.shutdown();
        }

        self.http_server
            .shutdown()
            .await
            .context(ShutdownServerSnafu)?;

        Ok(())
    }

    pub fn flow_worker_manager(&self) -> FlowWorkerManagerRef {
        self.server.flow_service.manager.clone()
    }
}

/// [`FlownodeInstance`] Builder
pub struct FlownodeBuilder {
    opts: FlownodeOptions,
    plugins: Plugins,
    table_meta: TableMetadataManagerRef,
    catalog_manager: CatalogManagerRef,
    flow_metadata_manager: FlowMetadataManagerRef,
    heartbeat_task: Option<HeartbeatTask>,
    /// receive a oneshot sender to send state size report
    state_report_handler: Option<StateReportHandler>,
}

impl FlownodeBuilder {
    /// init flownode builder
    pub fn new(
        opts: FlownodeOptions,
        plugins: Plugins,
        table_meta: TableMetadataManagerRef,
        catalog_manager: CatalogManagerRef,
        flow_metadata_manager: FlowMetadataManagerRef,
    ) -> Self {
        Self {
            opts,
            plugins,
            table_meta,
            catalog_manager,
            flow_metadata_manager,
            heartbeat_task: None,
            state_report_handler: None,
        }
    }

    pub fn with_heartbeat_task(self, heartbeat_task: HeartbeatTask) -> Self {
        let (sender, receiver) = SizeReportSender::new();
        Self {
            heartbeat_task: Some(heartbeat_task.with_query_stat_size(sender)),
            state_report_handler: Some(receiver),
            ..self
        }
    }

    pub async fn build(mut self) -> Result<FlownodeInstance, Error> {
        // TODO(discord9): does this query engine need those?
        let query_engine_factory = QueryEngineFactory::new_with_plugins(
            // query engine in flownode is only used for translate plan with resolved table source.
            self.catalog_manager.clone(),
            None,
            None,
            None,
            None,
            false,
            Default::default(),
        );
        let manager = Arc::new(
            self.build_manager(query_engine_factory.query_engine())
                .await?,
        );

        if let Err(err) = self.recover_flows(&manager).await {
            common_telemetry::error!(err; "Failed to recover flows");
        }

        let server = FlownodeServer::new(FlowService::new(manager.clone()));

        let http_addr = self.opts.http.addr.parse().context(ParseAddrSnafu {
            addr: self.opts.http.addr.clone(),
        })?;
        let http_server = HttpServerBuilder::new(self.opts.http)
            .with_metrics_handler(MetricsHandler)
            .build();

        let heartbeat_task = self.heartbeat_task;

        let addr = self.opts.grpc.bind_addr;
        let instance = FlownodeInstance {
            server,
            addr: addr.parse().context(ParseAddrSnafu { addr })?,
            http_server,
            http_addr,
            heartbeat_task,
        };
        Ok(instance)
    }

    /// recover all flow tasks in this flownode in distributed mode(nodeid is Some(<num>))
    ///
    /// or recover all existing flow tasks if in standalone mode(nodeid is None)
    ///
    /// TODO(discord9): persistent flow tasks with internal state
    async fn recover_flows(&self, manager: &FlowWorkerManagerRef) -> Result<usize, Error> {
        let nodeid = self.opts.node_id;
        let to_be_recovered: Vec<_> = if let Some(nodeid) = nodeid {
            let to_be_recover = self
                .flow_metadata_manager
                .flownode_flow_manager()
                .flows(nodeid)
                .try_collect::<Vec<_>>()
                .await
                .context(ListFlowsSnafu { id: Some(nodeid) })?;
            to_be_recover.into_iter().map(|(id, _)| id).collect()
        } else {
            let all_catalogs = self
                .catalog_manager
                .catalog_names()
                .await
                .map_err(BoxedError::new)
                .context(ExternalSnafu)?;
            let mut all_flow_ids = vec![];
            for catalog in all_catalogs {
                let flows = self
                    .flow_metadata_manager
                    .flow_name_manager()
                    .flow_names(&catalog)
                    .await
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(BoxedError::new)
                    .context(ExternalSnafu)?;

                all_flow_ids.extend(flows.into_iter().map(|(_, id)| id.flow_id()));
            }
            all_flow_ids
        };
        let cnt = to_be_recovered.len();

        // TODO(discord9): recover in parallel
        for flow_id in to_be_recovered {
            let info = self
                .flow_metadata_manager
                .flow_info_manager()
                .get(flow_id)
                .await
                .map_err(BoxedError::new)
                .context(ExternalSnafu)?
                .context(FlowNotFoundSnafu { id: flow_id })?;

            let sink_table_name = [
                info.sink_table_name().catalog_name.clone(),
                info.sink_table_name().schema_name.clone(),
                info.sink_table_name().table_name.clone(),
            ];
            let args = CreateFlowArgs {
                flow_id: flow_id as _,
                sink_table_name,
                source_table_ids: info.source_table_ids().to_vec(),
                // because recover should only happen on restart the `create_if_not_exists` and `or_replace` can be arbitrary value(since flow doesn't exist)
                // but for the sake of consistency and to make sure recover of flow actually happen, we set both to true
                // (which is also fine since checks for not allow both to be true is on metasrv and we already pass that)
                create_if_not_exists: true,
                or_replace: true,
                expire_after: info.expire_after(),
                comment: Some(info.comment().clone()),
                sql: info.raw_sql().clone(),
                flow_options: info.options().clone(),
                query_ctx: Some(
                    QueryContextBuilder::default()
                        .current_catalog(info.catalog_name().clone())
                        .build(),
                ),
            };
            manager
                .create_flow(args)
                .await
                .map_err(BoxedError::new)
                .with_context(|_| CreateFlowSnafu {
                    sql: info.raw_sql().clone(),
                })?;
        }

        Ok(cnt)
    }

    /// build [`FlowWorkerManager`], note this doesn't take ownership of `self`,
    /// nor does it actually start running the worker.
    async fn build_manager(
        &mut self,
        query_engine: Arc<dyn QueryEngine>,
    ) -> Result<FlowWorkerManager, Error> {
        let table_meta = self.table_meta.clone();

        register_function_to_query_engine(&query_engine);

        let num_workers = self.opts.flow.num_workers;

        let node_id = self.opts.node_id.map(|id| id as u32);

        let mut man = FlowWorkerManager::new(node_id, query_engine, table_meta);
        for worker_id in 0..num_workers {
            let (tx, rx) = oneshot::channel();

            let _handle = std::thread::Builder::new()
                .name(format!("flow-worker-{}", worker_id))
                .spawn(move || {
                    let (handle, mut worker) = create_worker();
                    let _ = tx.send(handle);
                    info!("Flow Worker started in new thread");
                    worker.run();
                });
            let worker_handle = rx.await.map_err(|e| {
                UnexpectedSnafu {
                    reason: format!("Failed to receive worker handle: {}", e),
                }
                .build()
            })?;
            man.add_worker_handle(worker_handle);
        }
        if let Some(handler) = self.state_report_handler.take() {
            man = man.with_state_report_handler(handler).await;
        }
        info!("Flow Node Manager started");
        Ok(man)
    }
}

#[derive(Clone)]
pub struct FrontendInvoker {
    inserter: Arc<Inserter>,
    deleter: Arc<Deleter>,
    statement_executor: Arc<StatementExecutor>,
}

impl FrontendInvoker {
    pub fn new(
        inserter: Arc<Inserter>,
        deleter: Arc<Deleter>,
        statement_executor: Arc<StatementExecutor>,
    ) -> Self {
        Self {
            inserter,
            deleter,
            statement_executor,
        }
    }

    pub async fn build_from(
        flow_worker_manager: FlowWorkerManagerRef,
        catalog_manager: CatalogManagerRef,
        kv_backend: KvBackendRef,
        layered_cache_registry: LayeredCacheRegistryRef,
        procedure_executor: ProcedureExecutorRef,
        node_manager: NodeManagerRef,
    ) -> Result<FrontendInvoker, Error> {
        let table_route_cache: TableRouteCacheRef =
            layered_cache_registry.get().context(CacheRequiredSnafu {
                name: TABLE_ROUTE_CACHE_NAME,
            })?;

        let partition_manager = Arc::new(PartitionRuleManager::new(
            kv_backend.clone(),
            table_route_cache.clone(),
        ));

        let table_flownode_cache: TableFlownodeSetCacheRef =
            layered_cache_registry.get().context(CacheRequiredSnafu {
                name: TABLE_FLOWNODE_SET_CACHE_NAME,
            })?;

        let inserter = Arc::new(Inserter::new(
            catalog_manager.clone(),
            partition_manager.clone(),
            node_manager.clone(),
            table_flownode_cache,
        ));

        let deleter = Arc::new(Deleter::new(
            catalog_manager.clone(),
            partition_manager.clone(),
            node_manager.clone(),
        ));

        let query_engine = flow_worker_manager.query_engine.clone();

        let statement_executor = Arc::new(StatementExecutor::new(
            catalog_manager.clone(),
            query_engine.clone(),
            procedure_executor.clone(),
            kv_backend.clone(),
            layered_cache_registry.clone(),
            inserter.clone(),
            table_route_cache,
        ));

        let invoker = FrontendInvoker::new(inserter, deleter, statement_executor);
        Ok(invoker)
    }
}

impl FrontendInvoker {
    pub async fn row_inserts(
        &self,
        requests: RowInsertRequests,
        ctx: QueryContextRef,
    ) -> common_frontend::error::Result<Output> {
        let _timer = METRIC_FLOW_PROCESSING_TIME
            .with_label_values(&["output_insert"])
            .start_timer();

        self.inserter
            .handle_row_inserts(requests, ctx, &self.statement_executor)
            .await
            .map_err(BoxedError::new)
            .context(common_frontend::error::ExternalSnafu)
    }

    pub async fn row_deletes(
        &self,
        requests: RowDeleteRequests,
        ctx: QueryContextRef,
    ) -> common_frontend::error::Result<Output> {
        let _timer = METRIC_FLOW_PROCESSING_TIME
            .with_label_values(&["output_delete"])
            .start_timer();

        self.deleter
            .handle_row_deletes(requests, ctx)
            .await
            .map_err(BoxedError::new)
            .context(common_frontend::error::ExternalSnafu)
    }

    pub fn statement_executor(&self) -> Arc<StatementExecutor> {
        self.statement_executor.clone()
    }
}

/// get all flow ids in this flownode
pub(crate) async fn get_all_flow_ids(
    flow_metadata_manager: &FlowMetadataManagerRef,
    catalog_manager: &CatalogManagerRef,
    nodeid: Option<u64>,
) -> Result<Vec<u32>, Error> {
    let ret = if let Some(nodeid) = nodeid {
        let flow_ids_one_node = flow_metadata_manager
            .flownode_flow_manager()
            .flows(nodeid)
            .try_collect::<Vec<_>>()
            .await
            .context(ListFlowsSnafu { id: Some(nodeid) })?;
        flow_ids_one_node.into_iter().map(|(id, _)| id).collect()
    } else {
        let all_catalogs = catalog_manager
            .catalog_names()
            .await
            .map_err(BoxedError::new)
            .context(ExternalSnafu)?;
        let mut all_flow_ids = vec![];
        for catalog in all_catalogs {
            let flows = flow_metadata_manager
                .flow_name_manager()
                .flow_names(&catalog)
                .await
                .try_collect::<Vec<_>>()
                .await
                .map_err(BoxedError::new)
                .context(ExternalSnafu)?;

            all_flow_ids.extend(flows.into_iter().map(|(_, id)| id.flow_id()));
        }
        all_flow_ids
    };

    Ok(ret)
}
