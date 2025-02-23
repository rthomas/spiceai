/*
Copyright 2024 The Spice.ai OSS Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

#![allow(clippy::missing_errors_doc)]

use std::borrow::Borrow;
use std::net::SocketAddr;
use std::{collections::HashMap, sync::Arc};

use app::App;
use config::Config;
use model::Model;
pub use notify::Error as NotifyError;
use secrets::spicepod_secret_store_type;
use snafu::prelude::*;
use spicepod::component::dataset::Dataset;
use spicepod::component::dataset::Mode;
use spicepod::component::model::Model as SpicepodModel;
use std::time::Duration;
use tokio::time::sleep;
use tokio::{signal, sync::RwLock};

use crate::{dataconnector::DataConnector, datafusion::DataFusion};
pub mod config;
pub mod databackend;
pub mod dataconnector;
pub mod datafusion;
pub mod datapublisher;
pub mod dataupdate;
mod flight;
mod http;
pub mod model;
pub mod modelformat;
pub mod modelruntime;
pub mod modelsource;
mod opentelemetry;
pub mod podswatcher;
pub mod status;
pub mod timing;
pub(crate) mod tracers;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Unable to start HTTP server: {source}"))]
    UnableToStartHttpServer { source: http::Error },

    #[snafu(display("Unable to start Flight server: {source}"))]
    UnableToStartFlightServer { source: flight::Error },

    #[snafu(display("Unable to start OpenTelemetry server: {source}"))]
    UnableToStartOpenTelemetryServer { source: opentelemetry::Error },

    #[snafu(display("Unknown data source: {data_source}"))]
    UnknownDataSource { data_source: String },

    #[snafu(display("Unable to create data backend: {source}"))]
    UnableToCreateBackend { source: datafusion::Error },

    #[snafu(display("Unable to attach view: {source}"))]
    UnableToAttachView { source: datafusion::Error },

    #[snafu(display("Failed to start pods watcher: {source}"))]
    UnableToInitializePodsWatcher { source: NotifyError },

    #[snafu(display("Unable to initialize data connector {data_connector}: {source}"))]
    UnableToInitializeDataConnector {
        source: dataconnector::Error,
        data_connector: String,
    },

    #[snafu(display("Unknown data connector: {data_connector}"))]
    UnknownDataConnector { data_connector: String },

    #[snafu(display("Unable to load secrets for data connector: {data_connector}"))]
    UnableToLoadDataConnectorSecrets { data_connector: String },

    #[snafu(display("Unable to create view: {source}"))]
    InvalidSQLView {
        source: spicepod::component::dataset::Error,
    },

    #[snafu(display("Unable to attach data connector {data_connector}: {source}"))]
    UnableToAttachDataConnector {
        source: datafusion::Error,
        data_connector: String,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Runtime {
    pub app: Arc<RwLock<Option<App>>>,
    pub config: config::Config,
    pub df: Arc<RwLock<DataFusion>>,
    pub models: Arc<RwLock<HashMap<String, Model>>>,
    pub pods_watcher: podswatcher::PodsWatcher,
    pub secrets_provider: Arc<RwLock<secrets::SecretsProvider>>,

    spaced_tracer: Arc<tracers::SpacedTracer>,
}

impl Runtime {
    #[must_use]
    pub async fn new(
        config: Config,
        app: Arc<RwLock<Option<app::App>>>,
        df: Arc<RwLock<DataFusion>>,
        pods_watcher: podswatcher::PodsWatcher,
    ) -> Self {
        dataconnector::register_all().await;
        Runtime {
            app,
            config,
            df,
            models: Arc::new(RwLock::new(HashMap::new())),
            pods_watcher,
            secrets_provider: Arc::new(RwLock::new(secrets::SecretsProvider::new())),
            spaced_tracer: Arc::new(tracers::SpacedTracer::new(Duration::from_secs(15))),
        }
    }

    pub async fn load_secrets(&self) {
        measure_scope_ms!("load_secrets");
        let mut secret_store = self.secrets_provider.write().await;

        let app_lock = self.app.read().await;
        if let Some(app) = app_lock.as_ref() {
            let Some(secret_store_type) = spicepod_secret_store_type(&app.secrets.store) else {
                return;
            };

            secret_store.store = secret_store_type;
        }

        if let Err(e) = secret_store.load_secrets() {
            tracing::warn!("Unable to load secrets: {}", e);
        }
    }

    pub async fn load_datasets(&self) {
        let app_lock = self.app.read().await;
        if let Some(app) = app_lock.as_ref() {
            for ds in &app.datasets {
                status::update_dataset(ds.name.clone(), status::ComponentStatus::Initializing);
                self.load_dataset(ds, &app.datasets);
            }
        }
    }

    // Caller must set `status::update_dataset(...` before calling `load_dataset`. This function will set error/ready statues appropriately.`
    pub fn load_dataset(&self, ds: &Dataset, all_datasets: &[Dataset]) {
        let df = Arc::clone(&self.df);
        let spaced_tracer = Arc::clone(&self.spaced_tracer);
        let shared_secrets_provider: Arc<RwLock<secrets::SecretsProvider>> =
            Arc::clone(&self.secrets_provider);

        let ds = ds.clone();

        let existing_tables = all_datasets
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<String>>();

        tokio::spawn(async move {
            loop {
                let secrets_provider = shared_secrets_provider.read().await;

                if !verify_dependent_tables(&ds, &existing_tables, Arc::clone(&df)).await {
                    status::update_dataset(ds.name.clone(), status::ComponentStatus::Error);
                    metrics::counter!("datasets_load_error").increment(1);
                    return;
                }

                let source = ds.source();

                let params = Arc::new(ds.params.clone());
                let data_connector: Option<Box<dyn DataConnector>> =
                    match Runtime::get_dataconnector_from_source(
                        &source,
                        &secrets_provider,
                        Arc::clone(&params),
                    )
                    .await
                    {
                        Ok(data_connector) => data_connector,
                        Err(err) => {
                            status::update_dataset(ds.name.clone(), status::ComponentStatus::Error);
                            metrics::counter!("datasets_load_error").increment(1);
                            warn_spaced!(
                                spaced_tracer,
                                "Failed to get data connector from source for dataset {}, retrying: {err}",
                                &ds.name
                            );
                            sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };

                if ds.acceleration.is_none()
                    && !ds.is_view()
                    && !has_table_provider(&data_connector)
                {
                    tracing::warn!("No acceleration specified for dataset: {}", ds.name);
                    break;
                };

                match Runtime::initialize_dataconnector(
                    data_connector,
                    Arc::clone(&df),
                    &source,
                    &ds,
                    Arc::clone(&shared_secrets_provider),
                )
                .await
                {
                    Ok(()) => (),
                    Err(err) => {
                        status::update_dataset(ds.name.clone(), status::ComponentStatus::Error);
                        metrics::counter!("datasets_load_error").increment(1);
                        warn_spaced!(
                            spaced_tracer,
                            "Failed to initialize data connector for dataset {}, retrying: {err}",
                            &ds.name
                        );
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                tracing::info!("Loaded dataset: {}", &ds.name);
                let engine = ds.acceleration.map_or_else(
                    || "None".to_string(),
                    |acc| {
                        if acc.enabled {
                            acc.engine().to_string()
                        } else {
                            "None".to_string()
                        }
                    },
                );
                metrics::gauge!("datasets_count", "engine" => engine).increment(1.0);
                status::update_dataset(ds.name.clone(), status::ComponentStatus::Ready);
                break;
            }
        });
    }

    pub async fn remove_dataset(&self, ds: &Dataset) {
        let mut df = self.df.write().await;

        if df.table_exists(&ds.name) {
            if let Err(e) = df.remove_table(&ds.name) {
                tracing::warn!("Unable to unload dataset {}: {}", &ds.name, e);
                return;
            }
        }

        tracing::info!("Unloaded dataset: {}", &ds.name);
        let engine = ds.acceleration.as_ref().map_or_else(
            || "None".to_string(),
            |acc| {
                if acc.enabled {
                    acc.engine().to_string()
                } else {
                    "None".to_string()
                }
            },
        );
        metrics::gauge!("datasets_count", "engine" => engine).decrement(1.0);
    }

    pub async fn update_dataset(&self, ds: &Dataset, all_datasets: &[Dataset]) {
        status::update_dataset(ds.name.clone(), status::ComponentStatus::Refreshing);
        self.remove_dataset(ds).await;
        self.load_dataset(ds, all_datasets);
    }

    async fn get_dataconnector_from_source(
        source: &str,
        secrets_provider: &secrets::SecretsProvider,
        params: Arc<Option<HashMap<String, String>>>,
    ) -> Result<Option<Box<dyn DataConnector>>> {
        let secret = secrets_provider.get_secret(source).await;

        match source {
            "localhost" => Ok(None),
            _ => match dataconnector::create_new_connector(source, secret, params).await {
                Some(dc) => dc.map(Some).context(UnableToInitializeDataConnectorSnafu {
                    data_connector: source,
                }),
                None => UnknownDataConnectorSnafu {
                    data_connector: source,
                }
                .fail(),
            },
        }
    }

    async fn initialize_dataconnector(
        data_connector: Option<Box<dyn DataConnector>>,
        df: Arc<RwLock<DataFusion>>,
        source: &str,
        ds: impl Borrow<Dataset>,
        secrets_provider: Arc<RwLock<secrets::SecretsProvider>>,
    ) -> Result<()> {
        let ds = ds.borrow();
        let view_sql = ds.view_sql().context(InvalidSQLViewSnafu)?;
        let data_backend_publishing_enabled =
            ds.mode() == Mode::ReadWrite || data_connector.is_none();

        if view_sql.is_some() {
            df.read()
                .await
                .attach_view(ds)
                .context(UnableToAttachViewSnafu)?;
            return Ok(());
        }

        if ds.acceleration.is_none() || ds.acceleration.as_ref().map_or(false, |acc| !acc.enabled) {
            if let Some(data_connector) = data_connector {
                df.read()
                    .await
                    .attach_mesh(ds, data_connector)
                    .await
                    .context(UnableToAttachDataConnectorSnafu {
                        data_connector: source,
                    })?;
                return Ok(());
            }
        }

        let data_backend = df
            .read()
            .await
            .new_accelerated_backend(ds, secrets_provider)
            .await
            .context(UnableToCreateBackendSnafu)?;
        let data_backend = Arc::new(data_backend);

        if data_backend_publishing_enabled {
            df.write()
                .await
                .attach_publisher(&ds.name.clone(), ds.clone(), Arc::clone(&data_backend))
                .await
                .context(UnableToAttachDataConnectorSnafu {
                    data_connector: source,
                })?;
        }

        if let Some(data_connector) = data_connector {
            let replicate = ds.replication.as_ref().map_or(false, |r| r.enabled);

            // Attach data publisher only if replicate is true and mode is ReadWrite
            if replicate && ds.mode() == Mode::ReadWrite {
                if let Some(data_publisher) = data_connector.get_data_publisher() {
                    df.write()
                        .await
                        .attach_publisher(&ds.name.clone(), ds.clone(), Arc::new(data_publisher))
                        .await
                        .context(UnableToAttachDataConnectorSnafu {
                            data_connector: source,
                        })?;
                } else {
                    tracing::warn!(
                        "Data connector {source} does not support writes, but dataset {ds_name} is configured to replicate",
                        ds_name = ds.name
                    );
                }
            }

            df.write()
                .await
                .attach_connector_to_publisher(
                    ds.clone(),
                    data_connector,
                    Arc::clone(&data_backend),
                )
                .context(UnableToAttachDataConnectorSnafu {
                    data_connector: source,
                })?;
        }

        Ok(())
    }

    pub async fn load_models(&self) {
        let app_lock = self.app.read().await;
        if let Some(app) = app_lock.as_ref() {
            for model in &app.models {
                status::update_model(model.name.clone(), status::ComponentStatus::Initializing);
                self.load_model(model).await;
            }
        }
    }

    // Caller must set `status::update_model(...` before calling `load_model`. This function will set error/ready statues appropriately.`
    pub async fn load_model(&self, m: &SpicepodModel) {
        measure_scope_ms!("load_model", "model" => m.name, "source" => model::source(&m.from));
        tracing::info!("Loading model [{}] from {}...", m.name, m.from);
        let mut model_map = self.models.write().await;

        let model = m.clone();
        let source = model::source(&model.from);

        let shared_secrets_provider = Arc::clone(&self.secrets_provider);
        let secrets_provider = shared_secrets_provider.read().await;

        match Model::load(
            m.clone(),
            secrets_provider.get_secret(source.as_str()).await,
        )
        .await
        {
            Ok(in_m) => {
                model_map.insert(m.name.clone(), in_m);
                tracing::info!("Model [{}] deployed, ready for inferencing", m.name);
                metrics::gauge!("models_count", "model" => m.name.clone(), "source" => model::source(&m.from)).increment(1.0);
                status::update_model(model.name.clone(), status::ComponentStatus::Ready);
            }
            Err(e) => {
                metrics::counter!("models_load_error").increment(1);
                status::update_model(model.name.clone(), status::ComponentStatus::Error);
                tracing::warn!(
                    "Unable to load runnable model from spicepod {}, error: {}",
                    m.name,
                    e,
                );
            }
        }
    }

    pub async fn remove_model(&self, m: &SpicepodModel) {
        let mut model_map = self.models.write().await;
        if !model_map.contains_key(&m.name) {
            tracing::warn!(
                "Unable to unload runnable model {}: model not found",
                m.name,
            );
            return;
        }
        model_map.remove(&m.name);
        tracing::info!("Model [{}] has been unloaded", m.name);
        metrics::gauge!("models_count", "model" => m.name.clone(), "source" => model::source(&m.from)).decrement(1.0);
    }

    pub async fn update_model(&self, m: &SpicepodModel) {
        status::update_model(m.name.clone(), status::ComponentStatus::Refreshing);
        self.remove_model(m).await;
        self.load_model(m).await;
    }

    pub async fn start_servers(&mut self, with_metrics: Option<SocketAddr>) -> Result<()> {
        let http_server_future = http::start(
            self.config.http_bind_address,
            self.app.clone(),
            self.df.clone(),
            self.models.clone(),
            self.config.clone().into(),
            with_metrics,
        );

        let flight_server_future = flight::start(self.config.flight_bind_address, self.df.clone());
        let open_telemetry_server_future =
            opentelemetry::start(self.config.open_telemetry_bind_address, self.df.clone());
        let pods_watcher_future = self.start_pods_watcher();

        tokio::select! {
            http_res = http_server_future => http_res.context(UnableToStartHttpServerSnafu),
            flight_res = flight_server_future => flight_res.context(UnableToStartFlightServerSnafu),
            open_telemetry_res = open_telemetry_server_future => open_telemetry_res.context(UnableToStartOpenTelemetryServerSnafu),
            pods_watcher_res = pods_watcher_future => pods_watcher_res.context(UnableToInitializePodsWatcherSnafu),
            () = shutdown_signal() => {
                tracing::info!("Goodbye!");
                Ok(())
            },
        }
    }

    pub async fn start_pods_watcher(&mut self) -> notify::Result<()> {
        let mut rx = self.pods_watcher.watch()?;

        while let Some(new_app) = rx.recv().await {
            let mut app_lock = self.app.write().await;
            if let Some(current_app) = app_lock.as_mut() {
                if *current_app == new_app {
                    continue;
                }

                tracing::debug!("Updated pods information: {:?}", new_app);
                tracing::debug!("Previous pods information: {:?}", current_app);

                // check for new and updated datasets
                for ds in &new_app.datasets {
                    if let Some(current_ds) =
                        current_app.datasets.iter().find(|d| d.name == ds.name)
                    {
                        if current_ds != ds {
                            self.update_dataset(ds, &new_app.datasets).await;
                        }
                    } else {
                        status::update_dataset(
                            ds.name.clone(),
                            status::ComponentStatus::Initializing,
                        );
                        self.load_dataset(ds, &new_app.datasets);
                    }
                }

                // check for new and updated models
                for model in &new_app.models {
                    if let Some(current_model) =
                        current_app.models.iter().find(|m| m.name == model.name)
                    {
                        if current_model != model {
                            self.update_model(model).await;
                        }
                    } else {
                        status::update_model(
                            model.name.clone(),
                            status::ComponentStatus::Initializing,
                        );
                        self.load_model(model).await;
                    }
                }

                // Remove models that are no longer in the app
                for model in &current_app.models {
                    if !new_app.models.iter().any(|m| m.name == model.name) {
                        status::update_model(model.name.clone(), status::ComponentStatus::Disabled);
                        self.remove_model(model).await;
                    }
                }

                // Remove datasets that are no longer in the app
                for ds in &current_app.datasets {
                    if !new_app.datasets.iter().any(|d| d.name == ds.name) {
                        status::update_dataset(ds.name.clone(), status::ComponentStatus::Disabled);
                        self.remove_dataset(ds).await;
                    }
                }

                *current_app = new_app;
            } else {
                *app_lock = Some(new_app);
            }
        }

        Ok(())
    }
}

async fn verify_dependent_tables(
    ds: &Dataset,
    existing_tables: &[String],
    df: Arc<RwLock<DataFusion>>,
) -> bool {
    if !ds.is_view() {
        return true;
    }

    let dependent_tables = match df.read().await.get_view_dependent_tables(ds) {
        Ok(tables) => tables,
        Err(err) => {
            tracing::error!(
                "Failed to get dependent tables for view {}: {}",
                &ds.name,
                err
            );
            return false;
        }
    };

    for tbl in &dependent_tables {
        if !existing_tables.contains(tbl) {
            tracing::error!(
                "Failed to load {}. Dependent table {} not found",
                &ds.name,
                &tbl
            );
            return false;
        }
    }

    true
}

fn has_table_provider(data_connector: &Option<Box<dyn DataConnector>>) -> bool {
    data_connector.is_some()
        && data_connector
            .as_ref()
            .is_some_and(|dc| dc.has_table_provider())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let signal_result = signal::ctrl_c().await;
        if let Err(err) = signal_result {
            tracing::error!("Failed to listen to shutdown signal: {err}");
        }
    };

    tokio::select! {
        () = ctrl_c => {},
    }
}
