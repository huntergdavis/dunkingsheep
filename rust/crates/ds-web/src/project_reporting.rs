//! On-demand Poolrad history/collection and task decisions. No board cache or refresh timer.
use crate::{
    ops_collector::{Collector, CollectorConfig, DaemonCalls, GhCli, SourceError},
    routes::{
        ops_board::{OpsBoardBackend, OpsBoardError, OpsBoardFuture},
        ops_metrics::{OpsMetricsBackend, OpsMetricsError, OpsMetricsFuture},
    },
};
use chrono::Utc;
use ds_core::client::{FlockClient, UnixTransport};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
pub type OpenDb = fn(&Path) -> rusqlite::Result<Connection>;
#[derive(Clone)]
pub struct ReportingConfig {
    pub ops_db: PathBuf,
    pub host_watch_state: PathBuf,
    pub opencode_db: PathBuf,
    pub collector: CollectorConfig,
    pub github: GhCli,
}
#[derive(Clone)]
pub struct SharedDaemon {
    client: Arc<Mutex<FlockClient<UnixTransport>>>,
}
impl SharedDaemon {
    pub fn new(client: FlockClient<UnixTransport>) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
        }
    }
    fn request(&self, command: &str, args: Map<String, Value>) -> Result<Value, String> {
        self.client
            .lock()
            .map_err(|_| "Daemon client unavailable".to_owned())?
            .call(command, args)
            .map_err(|e| e.to_string())
    }
}
impl DaemonCalls for SharedDaemon {
    fn call(&mut self, command: &str, args: Map<String, Value>) -> Result<Value, SourceError> {
        let mut client = self.client.lock().unwrap_or_else(|e| e.into_inner());
        <FlockClient<UnixTransport> as DaemonCalls>::call(&mut client, command, args)
    }
}

#[derive(Clone)]
pub struct ProjectReporting {
    pub config: ReportingConfig,
    pub daemon: SharedDaemon,
    pub open_db: OpenDb,
}
impl ProjectReporting {
    async fn call(&self, name: &'static str, args: Value) -> Result<Value, OpsBoardError> {
        let daemon = self.daemon.clone();
        tokio::task::spawn_blocking(move || {
            daemon.request(name, args.as_object().cloned().unwrap_or_default())
        })
        .await
        .map_err(|_| OpsBoardError::Failure("Task decision unavailable".into()))?
        .map_err(|_| OpsBoardError::Failure("Task decision unavailable".into()))
    }
}
impl OpsBoardBackend for ProjectReporting {
    fn refresh<'a>(&'a self) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
        Box::pin(async { Err(OpsBoardError::Failure("Operations board retired".into())) })
    }
    fn defer_question<'a>(
        &'a self,
        id: &'a str,
    ) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
        Box::pin(async move {
            self.call("task_update", json!({"id":id,"priority":"low"}))
                .await
        })
    }
    fn owner_question<'a>(
        &'a self,
        id: &'a str,
    ) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
        Box::pin(async move { self.call("task_owner_question", json!({"id":id})).await })
    }
    fn owner_answer<'a>(&'a self, args: Value) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
        Box::pin(async move { self.call("task_owner_answer", args).await })
    }
}
impl OpsMetricsBackend for ProjectReporting {
    fn history<'a>(&'a self, hours: u16) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
        let path = self.config.ops_db.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::ops_history::history(&path, Utc::now(), hours)
            })
            .await
            .map_err(|_| OpsMetricsError::other("History unavailable"))?
            .map_err(|_| OpsMetricsError::database("History unavailable"))
        })
    }
    fn refresh<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
        let report = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut conn = (report.open_db)(&report.config.ops_db)
                    .map_err(|_| OpsMetricsError::database("Reporting unavailable"))?;
                let mut collector =
                    Collector::new(report.config.collector, report.daemon, report.config.github);
                collector
                    .collect(&mut conn, Utc::now(), true, |conn, now| {
                        crate::ops_summary::summary(
                            conn,
                            now,
                            &report.config.host_watch_state,
                            &report.config.opencode_db,
                        )
                    })
                    .map_err(|_| OpsMetricsError::other("Collection unavailable"))
            })
            .await
            .map_err(|_| OpsMetricsError::other("Collection unavailable"))?
        })
    }
    fn board_snapshot<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
        Box::pin(async { Err(OpsMetricsError::other("Operations board retired")) })
    }
}
