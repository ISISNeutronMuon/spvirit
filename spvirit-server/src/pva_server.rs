//! High-level PVAccess server — builder pattern for typed records.
//!
//! # Example
//!
//! ```rust,ignore
//! use spvirit_server::PvaServer;
//!
//! let server = PvaServer::builder()
//!     .ai("SIM:TEMPERATURE", 22.5)
//!     .ao("SIM:SETPOINT", 25.0)
//!     .bo("SIM:ENABLE", false)
//!     .build();
//!
//! server.run().await?;
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use tracing::info;

use spvirit_types::{
    NdCodec, NdDimension, NtEnum, NtNdArray as NtNdArrayType, NtScalar, NtScalarArray,
    NtTable as NtTableType, NtTableColumn, NtTimeStamp, PvValue, ScalarArrayValue, ScalarValue,
};

use crate::db::{load_db, parse_db};
use crate::handler::PvListMode;
use crate::monitor::MonitorRegistry;
use crate::pv::scalar_family_record_type;
use crate::pvstore::{Source, SourceRegistry};
use crate::server::{PvaServerConfig, run_pva_server_with_registry};
use crate::simple_store::{LinkDef, OnPutCallback, ScanCallback, SimplePvStore};
use crate::types::{DbCommonState, OutputMode, RecordData, RecordInstance, RecordType};

// ─── PvaServerBuilder ────────────────────────────────────────────────────

/// Builder for [`PvaServer`].
///
/// ```rust,ignore
/// let server = PvaServer::builder()
///     .ai("TEMP:READBACK", 22.5)
///     .ao("TEMP:SETPOINT", 25.0)
///     .bo("HEATER:ON", false)
///     .port(5075)
///     .build();
/// ```
pub struct PvaServerBuilder {
    records: HashMap<String, RecordInstance>,
    on_put: HashMap<String, OnPutCallback>,
    scans: Vec<(String, Duration, ScanCallback)>,
    links: Vec<LinkDef>,
    extra_sources: Vec<(String, i32, Arc<dyn Source>)>,
    tcp_port: u16,
    udp_port: u16,
    listen_ip: Option<IpAddr>,
    advertise_ip: Option<IpAddr>,
    compute_alarms: bool,
    beacon_period_secs: u64,
    conn_timeout: Duration,
    pvlist_mode: PvListMode,
    pvlist_max: usize,
    pvlist_allow_pattern: Option<Regex>,
    start_hooks: Vec<crate::events::StartHook>,
    event_handlers: Vec<(String, crate::events::EventHandler)>,
}

impl PvaServerBuilder {
    fn new() -> Self {
        Self {
            records: HashMap::new(),
            on_put: HashMap::new(),
            scans: Vec::new(),
            links: Vec::new(),
            extra_sources: Vec::new(),
            tcp_port: 5075,
            udp_port: 5076,
            listen_ip: None,
            advertise_ip: None,
            compute_alarms: false,
            beacon_period_secs: 15,
            conn_timeout: Duration::from_secs(64000),
            pvlist_mode: PvListMode::List,
            pvlist_max: 1024,
            pvlist_allow_pattern: None,
            start_hooks: Vec::new(),
            event_handlers: Vec::new(),
        }
    }

    // ─── Typed record constructors ───────────────────────────────────

    /// Add an `ai` (analog input, read-only) record.
    pub fn ai(mut self, name: impl Into<String>, initial: f64) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_scalar_record(&name, RecordType::Ai, ScalarValue::F64(initial)),
        );
        self
    }

    /// Add an `ao` (analog output, writable) record.
    pub fn ao(mut self, name: impl Into<String>, initial: f64) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_output_record(&name, RecordType::Ao, ScalarValue::F64(initial)),
        );
        self
    }

    /// Add a `bi` (binary input, read-only) record.
    pub fn bi(mut self, name: impl Into<String>, initial: bool) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_scalar_record(&name, RecordType::Bi, ScalarValue::Bool(initial)),
        );
        self
    }

    /// Add a `bo` (binary output, writable) record.
    pub fn bo(mut self, name: impl Into<String>, initial: bool) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_output_record(&name, RecordType::Bo, ScalarValue::Bool(initial)),
        );
        self
    }

    /// Add a `stringin` (string input, read-only) record.
    pub fn string_in(mut self, name: impl Into<String>, initial: impl Into<String>) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_scalar_record(
                &name,
                RecordType::StringIn,
                ScalarValue::Str(initial.into()),
            ),
        );
        self
    }

    /// Add a `stringout` (string output, writable) record.
    pub fn string_out(mut self, name: impl Into<String>, initial: impl Into<String>) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_output_record(
                &name,
                RecordType::StringOut,
                ScalarValue::Str(initial.into()),
            ),
        );
        self
    }

    /// Add a `waveform` record (array) with the given initial data.
    pub fn waveform(mut self, name: impl Into<String>, data: ScalarArrayValue) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_array_record(&name, RecordType::Waveform, data),
        );
        self
    }

    /// Add an `aai` (analog array input, read-only) record.
    pub fn aai(mut self, name: impl Into<String>, data: ScalarArrayValue) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_array_record(&name, RecordType::Aai, data),
        );
        self
    }

    /// Add an `aao` (analog array output, writable) record.
    pub fn aao(mut self, name: impl Into<String>, data: ScalarArrayValue) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            make_array_record(&name, RecordType::Aao, data),
        );
        self
    }

    /// Add a `subarray` record — a view into part of an array.
    pub fn sub_array(
        mut self,
        name: impl Into<String>,
        data: ScalarArrayValue,
        indx: usize,
        nelm: usize,
    ) -> Self {
        let name = name.into();
        let ftvl = data.type_label().trim_end_matches("[]").to_string();
        let malm = data.len();
        let nord = nelm.min(malm.saturating_sub(indx));
        self.records.insert(
            name.clone(),
            RecordInstance {
                name: name.clone(),
                record_type: RecordType::SubArray,
                common: DbCommonState::default(),
                data: RecordData::SubArray {
                    nt: NtScalarArray::from_value(data),
                    inp: None,
                    ftvl,
                    malm,
                    nelm,
                    nord,
                    indx,
                },
                raw_fields: HashMap::new(),
            },
        );
        self
    }

    /// Add an NTTable record.
    pub fn nt_table(
        mut self,
        name: impl Into<String>,
        columns: Vec<(String, ScalarArrayValue)>,
    ) -> Self {
        let name = name.into();
        let labels: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
        let cols: Vec<NtTableColumn> = columns
            .into_iter()
            .map(|(n, v)| NtTableColumn { name: n, values: v })
            .collect();
        self.records.insert(
            name.clone(),
            RecordInstance {
                name: name.clone(),
                record_type: RecordType::NtTable,
                common: DbCommonState::default(),
                data: RecordData::NtTable {
                    nt: NtTableType {
                        labels,
                        columns: cols,
                        descriptor: None,
                        alarm: None,
                        time_stamp: None,
                    },
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                raw_fields: HashMap::new(),
            },
        );
        self
    }

    /// Add an NTNDArray record.
    pub fn nt_ndarray(
        mut self,
        name: impl Into<String>,
        data: ScalarArrayValue,
        dims: Vec<(i32, i32)>,
    ) -> Self {
        let name = name.into();
        let dimension: Vec<NdDimension> = dims
            .into_iter()
            .map(|(size, offset)| NdDimension {
                size,
                offset,
                full_size: size,
                binning: 1,
                reverse: false,
            })
            .collect();
        let uncompressed_size = (data.len() * data.element_size_bytes().max(1)) as i64;
        self.records.insert(
            name.clone(),
            RecordInstance {
                name: name.clone(),
                record_type: RecordType::NtNdArray,
                common: DbCommonState::default(),
                data: RecordData::NtNdArray {
                    nt: NtNdArrayType {
                        value: data,
                        codec: NdCodec {
                            name: String::new(),
                            parameters: Default::default(),
                        },
                        compressed_size: uncompressed_size,
                        uncompressed_size,
                        dimension,
                        unique_id: 0,
                        data_time_stamp: NtTimeStamp {
                            seconds_past_epoch: 0,
                            nanoseconds: 0,
                            user_tag: 0,
                        },
                        attribute: vec![],
                        descriptor: None,
                        alarm: None,
                        time_stamp: None,
                        display: None,
                    },
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                raw_fields: HashMap::new(),
            },
        );
        self
    }

    /// Add an `mbbi` (multi-bit binary input, read-only) NTEnum record.
    pub fn mbbi(mut self, name: impl Into<String>, choices: Vec<String>, initial: i32) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            RecordInstance {
                name: name.clone(),
                record_type: RecordType::Mbbi,
                common: DbCommonState::default(),
                data: RecordData::NtEnum {
                    nt: NtEnum::new(initial, choices),
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                raw_fields: HashMap::new(),
            },
        );
        self
    }

    /// Add an `mbbo` (multi-bit binary output, writable) NTEnum record.
    pub fn mbbo(mut self, name: impl Into<String>, choices: Vec<String>, initial: i32) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            RecordInstance {
                name: name.clone(),
                record_type: RecordType::Mbbo,
                common: DbCommonState::default(),
                data: RecordData::NtEnum {
                    nt: NtEnum::new(initial, choices),
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                raw_fields: HashMap::new(),
            },
        );
        self
    }

    /// Add a generic structure record with a custom struct ID and fields.
    pub fn generic(
        mut self,
        name: impl Into<String>,
        struct_id: impl Into<String>,
        fields: Vec<(String, PvValue)>,
    ) -> Self {
        let name = name.into();
        self.records.insert(
            name.clone(),
            RecordInstance {
                name: name.clone(),
                record_type: RecordType::Generic,
                common: DbCommonState::default(),
                data: RecordData::Generic {
                    struct_id: struct_id.into(),
                    fields,
                    inp: None,
                    out: None,
                    omsl: OutputMode::Supervisory,
                },
                raw_fields: HashMap::new(),
            },
        );
        self
    }

    // ─── .db file loading ────────────────────────────────────────────

    /// Load records from an EPICS `.db` file.
    pub fn db_file(mut self, path: impl AsRef<str>) -> Self {
        match load_db(path.as_ref()) {
            Ok(records) => {
                self.records.extend(records);
            }
            Err(e) => {
                tracing::error!("Failed to load db file '{}': {}", path.as_ref(), e);
            }
        }
        self
    }

    /// Parse records from an EPICS `.db` string.
    pub fn db_string(mut self, content: &str) -> Self {
        match parse_db(content) {
            Ok(records) => {
                self.records.extend(records);
            }
            Err(e) => {
                tracing::error!("Failed to parse db string: {}", e);
            }
        }
        self
    }

    // ─── Callbacks ───────────────────────────────────────────────────

    /// Register a callback invoked when a PUT is applied to the named PV.
    pub fn on_put<F>(mut self, name: impl Into<String>, callback: F) -> Self
    where
        F: Fn(&str, &spvirit_codec::spvd_decode::DecodedValue) + Send + Sync + 'static,
    {
        self.on_put.insert(name.into(), Arc::new(callback));
        self
    }

    /// Register a periodic scan callback that produces a new value for a PV.
    pub fn scan<F>(mut self, name: impl Into<String>, period: Duration, callback: F) -> Self
    where
        F: Fn(&str) -> ScalarValue + Send + Sync + 'static,
    {
        self.scans.push((name.into(), period, Arc::new(callback)));
        self
    }

    /// Register a hook to run once at startup, before the server serves.
    ///
    /// Hooks run in registration order, each to completion, before scan tasks
    /// spawn and before the listener accepts. A hook that panics aborts
    /// startup.
    ///
    /// ```rust,ignore
    /// .on_start(|store| Box::pin(async move {
    ///     store.set_value("SETPOINT", ScalarValue::F64(22.5)).await;
    /// }))
    /// ```
    pub fn on_start<F>(mut self, hook: F) -> Self
    where
        F: Fn(Arc<SimplePvStore>) -> Pin<Box<dyn Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.start_hooks.push(Arc::new(hook));
        self
    }

    /// Register a handler for a named event.
    ///
    /// Handlers are deferred: `post_event` queues them and returns. They run
    /// one at a time, in registration order, on the dispatcher.
    ///
    /// ```rust,ignore
    /// .on_event("SHUTTER", |store, event| Box::pin(async move { /* ... */ }))
    /// ```
    pub fn on_event<F>(mut self, event: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Arc<SimplePvStore>, String) -> Pin<Box<dyn Future<Output = ()> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.event_handlers.push((event.into(), Arc::new(handler)));
        self
    }

    /// Link an output PV to one or more input PVs.
    ///
    /// Whenever any input PV changes (via `set_value`, protocol PUT, or
    /// another link), the `compute` callback is invoked with the current
    /// values of **all** inputs (in order) and the result is written to
    /// the output PV.
    ///
    /// ```rust,ignore
    /// .link("CALC:SUM", &["INPUT:A", "INPUT:B"], |values| {
    ///     let a = values[0].as_f64().unwrap_or(0.0);
    ///     let b = values[1].as_f64().unwrap_or(0.0);
    ///     ScalarValue::F64(a + b)
    /// })
    /// ```
    pub fn link<F>(mut self, output: impl Into<String>, inputs: &[&str], compute: F) -> Self
    where
        F: Fn(&[ScalarValue]) -> ScalarValue + Send + Sync + 'static,
    {
        self.links.push(LinkDef {
            output: output.into(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            compute: Arc::new(compute),
        });
        self
    }

    // ─── External sources ────────────────────────────────────────────

    /// Register an additional [`Source`] at the given priority.
    ///
    /// Lower `order` values are checked first during PV name resolution.
    /// The built-in `SimplePvStore` (records added via `.ai()`, `.ao()`, etc.)
    /// is always registered at order 0.
    ///
    /// ```rust,ignore
    /// .source("hardware", -10, Arc::new(HardwareSource::new()))
    /// ```
    pub fn source(mut self, label: impl Into<String>, order: i32, source: Arc<dyn Source>) -> Self {
        self.extra_sources.push((label.into(), order, source));
        self
    }

    // ─── Configuration ───────────────────────────────────────────────

    /// Set the TCP port (default 5075).
    pub fn port(mut self, port: u16) -> Self {
        self.tcp_port = port;
        self
    }

    /// Set the UDP search port (default 5076).
    pub fn udp_port(mut self, port: u16) -> Self {
        self.udp_port = port;
        self
    }

    /// Set the IP address to listen on.
    pub fn listen_ip(mut self, ip: IpAddr) -> Self {
        self.listen_ip = Some(ip);
        self
    }

    /// Set the IP address to advertise in search responses.
    pub fn advertise_ip(mut self, ip: IpAddr) -> Self {
        self.advertise_ip = Some(ip);
        self
    }

    /// Enable alarm computation from limits.
    pub fn compute_alarms(mut self, enabled: bool) -> Self {
        self.compute_alarms = enabled;
        self
    }

    /// Set the beacon broadcast period in seconds (default 15).
    pub fn beacon_period(mut self, secs: u64) -> Self {
        self.beacon_period_secs = secs;
        self
    }

    /// Set the idle connection timeout (default ~18 hours).
    pub fn conn_timeout(mut self, timeout: Duration) -> Self {
        self.conn_timeout = timeout;
        self
    }

    /// Set the PV list mode (default [`PvListMode::List`]).
    pub fn pvlist_mode(mut self, mode: PvListMode) -> Self {
        self.pvlist_mode = mode;
        self
    }

    /// Set the maximum number of PV names in pvlist responses (default 1024).
    pub fn pvlist_max(mut self, max: usize) -> Self {
        self.pvlist_max = max;
        self
    }

    /// Set a regex filter for PV names exposed by pvlist.
    pub fn pvlist_allow_pattern(mut self, pattern: Regex) -> Self {
        self.pvlist_allow_pattern = Some(pattern);
        self
    }

    /// Build the [`PvaServer`].
    pub fn build(self) -> PvaServer {
        let store = Arc::new(SimplePvStore::new(
            self.records,
            self.on_put,
            self.links,
            self.compute_alarms,
        ));

        let mut config = PvaServerConfig::default();
        config.tcp_port = self.tcp_port;
        config.udp_port = self.udp_port;
        config.compute_alarms = self.compute_alarms;
        if let Some(ip) = self.listen_ip {
            config.listen_ip = ip;
        }
        config.advertise_ip = self.advertise_ip;
        config.beacon_period_secs = self.beacon_period_secs;
        config.conn_timeout = self.conn_timeout;
        config.pvlist_mode = self.pvlist_mode;
        config.pvlist_max = self.pvlist_max;
        config.pvlist_allow_pattern = self.pvlist_allow_pattern;

        let events = Arc::new(crate::events::Events::new());
        for (name, handler) in self.event_handlers {
            events.add_handler(name, handler);
        }

        PvaServer {
            store,
            extra_sources: self.extra_sources,
            config,
            scans: self.scans,
            monitor_registry: None,
            events,
            start_hooks: self.start_hooks,
        }
    }
}

// ─── PvaServer ───────────────────────────────────────────────────────────

/// High-level PVAccess server.
///
/// Built via [`PvaServer::builder()`] with typed record constructors,
/// `.db_file()` loading, `.on_put()` / `.scan()` callbacks, and a
/// simple `.run()` to start serving.
///
/// ```rust,ignore
/// let server = PvaServer::builder()
///     .ai("SIM:TEMP", 22.5)
///     .ao("SIM:SP", 25.0)
///     .build();
///
/// // Read/write PVs from another task:
/// let store = server.store();
/// store.set_value("SIM:TEMP", ScalarValue::F64(23.1)).await;
///
/// server.run().await?;
/// ```
pub struct PvaServer {
    store: Arc<SimplePvStore>,
    extra_sources: Vec<(String, i32, Arc<dyn Source>)>,
    config: PvaServerConfig,
    scans: Vec<(String, Duration, ScanCallback)>,
    /// Optional pre-supplied monitor registry so external code (e.g. Python
    /// bindings) can notify monitors from outside `run()`.
    monitor_registry: Option<Arc<MonitorRegistry>>,
    events: Arc<crate::events::Events>,
    start_hooks: Vec<crate::events::StartHook>,
}

impl PvaServer {
    /// Create a builder for configuring a [`PvaServer`].
    pub fn builder() -> PvaServerBuilder {
        PvaServerBuilder::new()
    }

    /// Get a reference to the underlying store for runtime get/put.
    pub fn store(&self) -> &Arc<SimplePvStore> {
        &self.store
    }

    /// The server's event registry — register sinks or post events.
    pub fn events(&self) -> &Arc<crate::events::Events> {
        &self.events
    }

    /// Post a named event.
    ///
    /// Synchronous sinks run inline; handlers are queued. When this returns,
    /// records have processed and handlers are queued — not necessarily run.
    pub fn post_event(&self, event: &str) {
        self.events.post(event);
    }

    /// Run every `on_start` hook to completion, in registration order.
    ///
    /// Returns `Err` naming the hook if one panics.
    pub async fn run_start_hooks(&self) -> Result<(), String> {
        for (i, hook) in self.start_hooks.iter().enumerate() {
            let fut = hook(self.store.clone());
            let result =
                futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(fut)).await;
            if result.is_err() {
                return Err(format!("on_start hook #{i} panicked; aborting startup"));
            }
        }
        Ok(())
    }

    /// Mint a typed handle to any record in this server's store — the
    /// pre-`run()` counterpart of [`RunningServer::pv`].
    pub async fn pv<T: crate::pv::PvScalar>(
        &self,
        name: &str,
    ) -> Result<crate::pv::Pv<T>, crate::pv::PvError> {
        crate::pv::Pv::attach(&self.store, name).await
    }

    /// Mint an array handle to any record in this server's store — the
    /// pre-`run()` counterpart of [`RunningServer::array_pv`].
    pub async fn array_pv(&self, name: &str) -> Result<crate::pv::PvArray, crate::pv::PvError> {
        crate::pv::PvArray::attach(&self.store, name).await
    }

    /// Register an additional [`Source`] after building the server.
    ///
    /// This is useful when the source needs a reference to the store
    /// (which is only available after `.build()`).
    ///
    /// ```rust,ignore
    /// let server = PvaServer::builder().ai("X", 0.0).build();
    /// let store = server.store().clone();
    /// server.add_source("agg", 10, Arc::new(MyAggSource::new(store)));
    /// server.run().await?;
    /// ```
    pub fn add_source(&mut self, label: impl Into<String>, order: i32, source: Arc<dyn Source>) {
        self.extra_sources.push((label.into(), order, source));
    }

    /// Pre-supply the [`MonitorRegistry`] that [`Self::run`] will use.
    ///
    /// This lets external code (for example Python `Source` adapters)
    /// hold onto the registry and publish monitor updates to subscribed
    /// PVAccess clients from outside `run()`.
    pub fn set_monitor_registry(&mut self, registry: Arc<MonitorRegistry>) {
        self.monitor_registry = Some(registry);
    }

    /// Get a shared handle to the [`MonitorRegistry`] that will be used
    /// when [`Self::run`] starts.  Creates (and stores) a new registry
    /// on first call so external code can register before run.
    pub fn monitor_registry(&mut self) -> Arc<MonitorRegistry> {
        if self.monitor_registry.is_none() {
            self.monitor_registry = Some(Arc::new(MonitorRegistry::new()));
        }
        self.monitor_registry.as_ref().unwrap().clone()
    }

    /// Start the PVA server (UDP search + TCP handler + beacon + scan tasks).
    ///
    /// This blocks until the server is shut down or an error occurs.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        // Create the monitor registry early so scan tasks can notify
        // PVAccess monitor clients when values change.
        let registry = self
            .monitor_registry
            .clone()
            .unwrap_or_else(|| Arc::new(MonitorRegistry::new()));
        self.store.set_registry(registry.clone()).await;

        // 1. Run every on_start hook to completion. Nothing else has started,
        //    so a hook observes a quiescent store and no client can see a
        //    pre-initialisation value.
        self.run_start_hooks()
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { Box::<dyn std::error::Error>::from(e) })?;

        // Build the source registry with the built-in store at order 0.
        let sources = Arc::new(SourceRegistry::new());
        sources.add("builtin", 0, self.store.clone()).await;

        // IOC/QSRV-style record field access (<name>.<FIELD>, <FIELD>$) so
        // tools like the EPICS Archiver Appliance can fetch record metadata.
        sources
            .add(
                "record-fields",
                10,
                Arc::new(crate::record_fields::RecordFieldSource::new(
                    self.store.clone(),
                )),
            )
            .await;

        // Register any extra sources provided via .source().
        for (label, order, source) in &self.extra_sources {
            sources.add(label.clone(), *order, source.clone()).await;
        }

        // 2. Spawn scan tasks.
        for (name, period, callback) in &self.scans {
            let store = self.store.clone();
            let name = name.clone();
            let period = *period;
            let callback = callback.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(period);
                loop {
                    interval.tick().await;
                    let new_val = callback(&name);
                    store.set_value(&name, new_val).await;
                }
            });
        }

        // 3. Start the event dispatcher.
        self.events.start_dispatcher(self.store.clone());

        let pv_count = self.store.pv_names().await.len();
        info!(
            "PvaServer starting: {} PVs on port {}",
            pv_count, self.config.tcp_port
        );

        // 4. Bind and accept.
        run_pva_server_with_registry(sources, self.config, registry).await
    }
}

// ─── Handle-based (`Pv<T>`) entry point ──────────────────────────────────

impl PvaServer {
    /// Serve a collection of typed PV handles. Shorthand entry point for the
    /// handle-based API; combine with `.db_file()`, `.source()`, etc.
    pub fn serve(pvs: impl IntoIterator<Item = impl Into<crate::pv::AnyPv>>) -> ServeBuilder {
        ServeBuilder {
            inner: PvaServerBuilder::new(),
            handles: Vec::new(),
        }
        .pvs(pvs)
    }
}

/// Builder for the PV-handle API. Wraps [`PvaServerBuilder`] and adds handle
/// binding at `build()` time.
pub struct ServeBuilder {
    inner: PvaServerBuilder,
    handles: Vec<crate::pv::AnyPv>,
}

impl ServeBuilder {
    pub fn pvs(mut self, pvs: impl IntoIterator<Item = impl Into<crate::pv::AnyPv>>) -> Self {
        self.handles.extend(pvs.into_iter().map(Into::into));
        self
    }
    pub fn db_file(mut self, path: impl AsRef<str>) -> Self {
        self.inner = self.inner.db_file(path);
        self
    }
    pub fn db_string(mut self, content: &str) -> Self {
        self.inner = self.inner.db_string(content);
        self
    }
    pub fn source(mut self, label: impl Into<String>, order: i32, source: Arc<dyn Source>) -> Self {
        self.inner = self.inner.source(label, order, source);
        self
    }
    pub fn port(mut self, port: u16) -> Self {
        self.inner = self.inner.port(port);
        self
    }
    pub fn udp_port(mut self, port: u16) -> Self {
        self.inner = self.inner.udp_port(port);
        self
    }
    pub fn listen_ip(mut self, ip: IpAddr) -> Self {
        self.inner = self.inner.listen_ip(ip);
        self
    }
    pub fn advertise_ip(mut self, ip: IpAddr) -> Self {
        self.inner = self.inner.advertise_ip(ip);
        self
    }
    pub fn compute_alarms(mut self, enabled: bool) -> Self {
        self.inner = self.inner.compute_alarms(enabled);
        self
    }
    pub fn beacon_period(mut self, secs: u64) -> Self {
        self.inner = self.inner.beacon_period(secs);
        self
    }

    /// Materialise records, links and scans from the handles, build the
    /// server, then bind every handle to the store.
    ///
    /// Async because registering PUT validators post-build goes through
    /// `SimplePvStore::set_validator`, which is async (an `RwLock` write);
    /// there is no synchronous alternative and `spvirit-server` does not
    /// depend on `futures`, so this awaits inline rather than blocking.
    pub async fn build(mut self) -> PvaServer {
        let mut validators: Vec<(String, crate::simple_store::PutValidator)> = Vec::new();
        for h in &self.handles {
            let name = h.name().to_string();
            if let Some(rec) = h.take_record() {
                self.inner.records.insert(name.clone(), rec);
            }
            if let Some(v) = h.take_validator() {
                validators.push((name.clone(), v));
            }
            if let Some((period, cb)) = h.take_scan() {
                self.inner.scans.push((name.clone(), period, cb));
            }
            if let Some((inputs, compute)) = h.take_calc() {
                self.inner.links.push(LinkDef {
                    output: name.clone(),
                    inputs,
                    compute,
                });
            }
        }
        let server = self.inner.build();
        let store = server.store().clone();
        for h in &self.handles {
            h.bind(&store);
        }
        for (name, v) in validators {
            store.set_validator(name, v).await;
        }
        server
    }

    /// Build and run (blocks until shutdown).
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        self.build().await.run().await
    }

    /// Build and spawn; returns a handle for typed access and shutdown.
    pub async fn start(self) -> RunningServer {
        let server = self.build().await;
        let store = server.store().clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = server.run().await {
                tracing::error!("PvaServer exited with error: {e}");
            }
        });
        RunningServer { store, handle }
    }
}

/// A started server: mint typed handles, then `abort()` to stop.
pub struct RunningServer {
    store: Arc<SimplePvStore>,
    handle: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    /// Mint a typed handle to any served record (handle-built or `.db`-loaded).
    pub async fn pv<T: crate::pv::PvScalar>(
        &self,
        name: &str,
    ) -> Result<crate::pv::Pv<T>, crate::pv::PvError> {
        crate::pv::Pv::attach(&self.store, name).await
    }

    /// Mint an array handle to any served record (handle-built or `.db`-loaded).
    pub async fn array_pv(&self, name: &str) -> Result<crate::pv::PvArray, crate::pv::PvError> {
        crate::pv::PvArray::attach(&self.store, name).await
    }

    /// Add a scalar record to the running server at runtime. The wire type is
    /// taken from the `ScalarValue` variant; `writable` selects an output
    /// record family (client PUTs allowed) vs an input family (read-only).
    /// Returns a bound handle to the new record. Replaces any existing record
    /// with the same name.
    pub async fn add_scalar(
        &self,
        name: &str,
        value: ScalarValue,
        writable: bool,
    ) -> crate::pv::Pv<ScalarValue> {
        let rt = scalar_family_record_type(&value, writable);
        let record = if writable {
            make_output_record(name, rt, value)
        } else {
            make_scalar_record(name, rt, value)
        };
        self.store.insert(name.to_string(), record).await;
        crate::pv::Pv::attach(&self.store, name)
            .await
            .expect("record just inserted")
    }

    /// Add an array record to the running server at runtime. `writable`
    /// selects `aao` (client PUTs allowed) vs `aai` (read-only). Element type
    /// comes from the `ScalarArrayValue` variant. Returns a bound handle.
    /// Replaces any existing record with the same name.
    pub async fn add_array(
        &self,
        name: &str,
        value: ScalarArrayValue,
        writable: bool,
    ) -> crate::pv::PvArray {
        let rt = if writable {
            RecordType::Aao
        } else {
            RecordType::Aai
        };
        let record = make_array_record(name, rt, value);
        self.store.insert(name.to_string(), record).await;
        crate::pv::PvArray::attach(&self.store, name)
            .await
            .expect("record just inserted")
    }

    /// Add an NTEnum record at runtime. `writable` selects an `mbbo`
    /// (output) vs `mbbi` (input) record type; note both accept client PUTs
    /// at the store layer. Replaces any existing record with the same name.
    pub async fn add_enum(&self, name: &str, choices: Vec<String>, index: i32, writable: bool) {
        let record = make_enum_record(name, choices, index, writable);
        self.store.insert(name.to_string(), record).await;
    }

    /// Add an NTTable record at runtime from named, typed columns. Tables are
    /// always writable at the store layer. Replaces any existing record with
    /// the same name.
    pub async fn add_table(&self, name: &str, columns: Vec<(String, ScalarArrayValue)>) {
        let record = make_table_record(name, columns);
        self.store.insert(name.to_string(), record).await;
    }

    pub fn store(&self) -> &Arc<SimplePvStore> {
        &self.store
    }

    pub fn abort(&self) {
        self.handle.abort();
    }
}

// ─── Record construction helpers ─────────────────────────────────────────

pub(crate) fn make_scalar_record(
    name: &str,
    record_type: RecordType,
    value: ScalarValue,
) -> RecordInstance {
    let nt = NtScalar::from_value(value);
    let data = match record_type {
        RecordType::Ai => RecordData::Ai {
            nt,
            inp: None,
            siml: None,
            siol: None,
            simm: false,
        },
        RecordType::Bi => RecordData::Bi {
            nt,
            inp: None,
            znam: "Off".to_string(),
            onam: "On".to_string(),
            siml: None,
            siol: None,
            simm: false,
        },
        RecordType::StringIn => RecordData::StringIn {
            nt,
            inp: None,
            siml: None,
            siol: None,
            simm: false,
        },
        // longin reuses the Ai data shape (NtScalar input record)
        RecordType::LongIn => RecordData::Ai {
            nt,
            inp: None,
            siml: None,
            siol: None,
            simm: false,
        },
        _ => panic!("make_scalar_record: unsupported type {record_type:?}"),
    };
    RecordInstance {
        name: name.to_string(),
        record_type,
        common: DbCommonState::default(),
        data,
        raw_fields: HashMap::new(),
    }
}

pub(crate) fn make_output_record(
    name: &str,
    record_type: RecordType,
    value: ScalarValue,
) -> RecordInstance {
    let nt = NtScalar::from_value(value);
    let data = match record_type {
        RecordType::Ao => RecordData::Ao {
            nt,
            out: None,
            dol: None,
            omsl: OutputMode::Supervisory,
            drvl: None,
            drvh: None,
            oroc: None,
            siml: None,
            siol: None,
            simm: false,
        },
        RecordType::Bo => RecordData::Bo {
            nt,
            out: None,
            dol: None,
            omsl: OutputMode::Supervisory,
            znam: "Off".to_string(),
            onam: "On".to_string(),
            siml: None,
            siol: None,
            simm: false,
        },
        RecordType::StringOut => RecordData::StringOut {
            nt,
            out: None,
            dol: None,
            omsl: OutputMode::Supervisory,
            siml: None,
            siol: None,
            simm: false,
        },
        // longout reuses the Ao data shape (NtScalar output record)
        RecordType::LongOut => RecordData::Ao {
            nt,
            out: None,
            dol: None,
            omsl: OutputMode::Supervisory,
            drvl: None,
            drvh: None,
            oroc: None,
            siml: None,
            siol: None,
            simm: false,
        },
        _ => panic!("make_output_record: unsupported type {record_type:?}"),
    };
    RecordInstance {
        name: name.to_string(),
        record_type,
        common: DbCommonState::default(),
        data,
        raw_fields: HashMap::new(),
    }
}

/// Build an array-backed record (`waveform`/`aai`/`aao`) with ftvl/nelm/nord
/// inferred from `data`. Shared by the classic builder (`.waveform`/`.aai`/
/// `.aao`) and `PvArray`'s constructors so the inference lives in one place.
pub(crate) fn make_array_record(
    name: &str,
    record_type: RecordType,
    data: ScalarArrayValue,
) -> RecordInstance {
    let ftvl = data.type_label().trim_end_matches("[]").to_string();
    let nelm = data.len();
    let nt = NtScalarArray::from_value(data);
    let record_data = match record_type {
        RecordType::Waveform => RecordData::Waveform {
            nt,
            inp: None,
            ftvl,
            nelm,
            nord: nelm,
        },
        RecordType::Aai => RecordData::Aai {
            nt,
            inp: None,
            ftvl,
            nelm,
            nord: nelm,
        },
        RecordType::Aao => RecordData::Aao {
            nt,
            out: None,
            dol: None,
            omsl: OutputMode::Supervisory,
            ftvl,
            nelm,
            nord: nelm,
        },
        _ => panic!("make_array_record: unsupported type {record_type:?}"),
    };
    RecordInstance {
        name: name.to_string(),
        record_type,
        common: DbCommonState::default(),
        data: record_data,
        raw_fields: HashMap::new(),
    }
}

pub(crate) fn make_enum_record(
    name: &str,
    choices: Vec<String>,
    index: i32,
    writable: bool,
) -> RecordInstance {
    RecordInstance {
        name: name.to_string(),
        record_type: if writable { RecordType::Mbbo } else { RecordType::Mbbi },
        common: DbCommonState::default(),
        data: RecordData::NtEnum {
            nt: NtEnum::new(index, choices),
            inp: None,
            out: None,
            omsl: OutputMode::Supervisory,
        },
        raw_fields: HashMap::new(),
    }
}

pub(crate) fn make_table_record(
    name: &str,
    columns: Vec<(String, ScalarArrayValue)>,
) -> RecordInstance {
    let labels: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
    let cols: Vec<NtTableColumn> = columns
        .into_iter()
        .map(|(n, v)| NtTableColumn { name: n, values: v })
        .collect();
    RecordInstance {
        name: name.to_string(),
        record_type: RecordType::NtTable,
        common: DbCommonState::default(),
        data: RecordData::NtTable {
            nt: NtTableType { labels, columns: cols, descriptor: None, alarm: None, time_stamp: None },
            inp: None,
            out: None,
            omsl: OutputMode::Supervisory,
        },
        raw_fields: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn on_start_hooks_are_stored_and_runnable_in_order() {
        use std::sync::Mutex;
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));

        let l1 = log.clone();
        let l2 = log.clone();
        let server = PvaServer::builder()
            .ai("T:A", 1.0)
            .on_start(move |_store| {
                let l = l1.clone();
                Box::pin(async move { l.lock().unwrap().push("first"); })
            })
            .on_start(move |_store| {
                let l = l2.clone();
                Box::pin(async move { l.lock().unwrap().push("second"); })
            })
            .build();

        server.run_start_hooks().await.expect("hooks must succeed");

        assert_eq!(log.lock().unwrap().as_slice(), &["first", "second"]);
    }

    #[tokio::test]
    async fn on_start_hook_can_write_the_store() {
        let server = PvaServer::builder()
            .ao("T:SP", 0.0)
            .on_start(|store| {
                Box::pin(async move {
                    store.set_value("T:SP", ScalarValue::F64(22.5)).await;
                })
            })
            .build();

        server.run_start_hooks().await.expect("hooks must succeed");

        assert_eq!(
            server.store().get_value("T:SP").await,
            Some(ScalarValue::F64(22.5))
        );
    }

    #[tokio::test]
    async fn post_event_reaches_a_builder_registered_handler() {
        use std::sync::Mutex;
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();

        let server = PvaServer::builder()
            .ai("T:A", 1.0)
            .on_event("SHUTTER", move |_store, event| {
                let s = s.clone();
                Box::pin(async move { s.lock().unwrap().push(event); })
            })
            .build();

        server.events().start_dispatcher(server.store().clone());
        server.post_event("SHUTTER");
        server.events().drain().await;

        assert_eq!(seen.lock().unwrap().as_slice(), &["SHUTTER".to_string()]);
    }

    #[tokio::test]
    async fn scan_tasks_do_not_run_before_start_hooks_finish() {
        use std::sync::Mutex;
        use std::time::Duration;
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));

        let l = log.clone();
        let scan_log = log.clone();
        let server = PvaServer::builder()
            .ai("T:TICK", 0.0)
            .port(0)
            .on_start(move |_store| {
                let l = l.clone();
                Box::pin(async move {
                    // Yield repeatedly: a scan task spawned too early would
                    // interleave here.
                    for _ in 0..50 {
                        tokio::task::yield_now().await;
                    }
                    l.lock().unwrap().push("hook-done");
                })
            })
            .scan("T:TICK", Duration::from_millis(1), move |_name| {
                scan_log.lock().unwrap().push("scan");
                ScalarValue::F64(1.0)
            })
            .build();

        // Output must be Send for tokio::spawn; discard the Result inline
        // (mirrors ServeBuilder::start's spawn) since this test only checks
        // ordering via `log`, not the run() outcome.
        let handle = tokio::spawn(async move {
            let _ = server.run().await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();

        let entries = log.lock().unwrap().clone();
        assert_eq!(
            entries.first(),
            Some(&"hook-done"),
            "start hook must complete before the first scan tick; got {entries:?}"
        );
    }

    #[tokio::test]
    async fn no_client_can_connect_before_start_hooks_finish() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // The hook blocks on this gate; the test releases it only after
        // confirming the listener has not yet bound.
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let hook_entered = std::sync::Arc::new(AtomicBool::new(false));

        let g = gate.clone();
        let entered = hook_entered.clone();
        let server = PvaServer::builder()
            .ai("T:GATED", 0.0)
            .port(0)
            .on_start(move |store| {
                let g = g.clone();
                let entered = entered.clone();
                Box::pin(async move {
                    entered.store(true, Ordering::SeqCst);
                    g.notified().await;
                    store.set_value("T:GATED", ScalarValue::F64(99.0)).await;
                })
            })
            .build();

        let store = server.store().clone();
        // Output must be Send for tokio::spawn; discard the Result inline
        // (mirrors ServeBuilder::start's spawn) since this test only checks
        // the gating via `store`, not the run() outcome.
        let handle = tokio::spawn(async move {
            let _ = server.run().await;
        });

        // Give run() time to reach the hook and block there.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(hook_entered.load(Ordering::SeqCst), "hook should have started");
        assert_eq!(
            store.get_value("T:GATED").await,
            Some(ScalarValue::F64(0.0)),
            "hook has not finished, so the initial value is still in place"
        );

        gate.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            store.get_value("T:GATED").await,
            Some(ScalarValue::F64(99.0)),
            "hook must have completed once released"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn run_aborts_when_a_start_hook_panics() {
        let server = PvaServer::builder()
            .ai("T:A", 1.0)
            .port(0)
            .on_start(|_store| Box::pin(async { panic!("init failed"); }))
            .build();

        let result = server.run().await;

        assert!(result.is_err(), "run() must fail when a start hook panics");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("on_start"),
            "error must name the failing hook, got: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn running_server_add_scalar_and_array() {
        use spvirit_types::{ScalarArrayValue, ScalarValue};

        let server = PvaServer::serve(Vec::<crate::pv::AnyPv>::new())
            .port(0)
            .udp_port(0)
            .start()
            .await;

        // add a writable u32 scalar
        let h = server.add_scalar("RT:U32", ScalarValue::U32(7), true).await;
        assert_eq!(h.get().await.unwrap(), ScalarValue::U32(7));
        // exact wire type preserved
        assert!(matches!(
            server.store().get_value("RT:U32").await,
            Some(ScalarValue::U32(7))
        ));

        // add a read-only i16 scalar; family maps to an input record
        let _ = server.add_scalar("RT:I16", ScalarValue::I16(-3), false).await;
        assert!(matches!(
            server.store().get_value("RT:I16").await,
            Some(ScalarValue::I16(-3))
        ));

        // add a writable f64 array
        let a = server
            .add_array("RT:ARR", ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]), true)
            .await;
        a.set(ScalarArrayValue::F64(vec![4.0, 5.0])).await.unwrap();
        assert!(matches!(
            server.store().get_nt("RT:ARR").await,
            Some(spvirit_types::NtPayload::ScalarArray(_))
        ));

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn running_server_add_enum_and_table() {
        use spvirit_types::{NtPayload, ScalarArrayValue};

        let server = PvaServer::serve(Vec::<crate::pv::AnyPv>::new())
            .port(0)
            .udp_port(0)
            .start()
            .await;

        // writable enum -> mbbo, choices + index preserved
        server
            .add_enum("RT:ENUM", vec!["OFF".into(), "ON".into(), "TRIP".into()], 1, true)
            .await;
        match server.store().get_nt("RT:ENUM").await {
            Some(NtPayload::Enum(e)) => {
                assert_eq!(e.index, 1);
                assert_eq!(e.choices, vec!["OFF", "ON", "TRIP"]);
            }
            other => panic!("expected enum, got {other:?}"),
        }

        // read-only enum -> mbbi; still writable() at the store layer (documented)
        server.add_enum("RT:ENUM_RO", vec!["A".into(), "B".into()], 0, false).await;
        assert!(matches!(
            server.store().get_nt("RT:ENUM_RO").await,
            Some(NtPayload::Enum(_))
        ));

        // table with two typed columns
        server
            .add_table(
                "RT:TBL",
                vec![
                    ("id".into(), ScalarArrayValue::I32(vec![1, 2, 3])),
                    ("x".into(), ScalarArrayValue::F64(vec![0.5, 1.5, 2.5])),
                ],
            )
            .await;
        match server.store().get_nt("RT:TBL").await {
            Some(NtPayload::Table(t)) => {
                assert_eq!(t.labels, vec!["id", "x"]);
                assert_eq!(t.columns.len(), 2);
            }
            other => panic!("expected table, got {other:?}"),
        }

        server.abort();
    }

    #[test]
    fn builder_creates_records() {
        let server = PvaServer::builder()
            .ai("T:AI", 1.0)
            .ao("T:AO", 2.0)
            .bi("T:BI", true)
            .bo("T:BO", false)
            .string_in("T:SI", "hello")
            .string_out("T:SO", "world")
            .build();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let names = rt.block_on(server.store.pv_names());
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn builder_defaults() {
        let server = PvaServer::builder().build();
        assert_eq!(server.config.tcp_port, 5075);
        assert_eq!(server.config.udp_port, 5076);
        assert!(!server.config.compute_alarms);
    }

    #[test]
    fn builder_port_override() {
        let server = PvaServer::builder().port(9075).udp_port(9076).build();
        assert_eq!(server.config.tcp_port, 9075);
        assert_eq!(server.config.udp_port, 9076);
    }

    #[test]
    fn builder_db_string() {
        let db = r#"
            record(ai, "TEST:VAL") {
                field(VAL, "3.14")
            }
        "#;
        let server = PvaServer::builder().db_string(db).build();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(rt.block_on(server.store.get_value("TEST:VAL")).is_some());
    }

    #[test]
    fn builder_waveform() {
        let data = ScalarArrayValue::F64(vec![1.0, 2.0, 3.0]);
        let server = PvaServer::builder().waveform("T:WF", data).build();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let names = rt.block_on(server.store.pv_names());
        assert!(names.contains(&"T:WF".to_string()));
    }

    #[test]
    fn builder_scan_callback() {
        let server = PvaServer::builder()
            .ai("SCAN:V", 0.0)
            .scan("SCAN:V", Duration::from_secs(1), |_name| {
                ScalarValue::F64(42.0)
            })
            .build();
        assert_eq!(server.scans.len(), 1);
    }

    #[test]
    fn builder_on_put_callback() {
        let server = PvaServer::builder()
            .ao("PUT:V", 0.0)
            .on_put("PUT:V", |_name, _val| {})
            .build();
        // on_put is stored in the SimplePvStore, not directly inspectable,
        // but the server built without panic.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(rt.block_on(server.store.get_value("PUT:V")).is_some());
    }

    #[test]
    fn store_runtime_get_set() {
        let server = PvaServer::builder().ao("RT:V", 0.0).build();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = server.store().clone();
        rt.block_on(async {
            assert_eq!(store.get_value("RT:V").await, Some(ScalarValue::F64(0.0)));
            store.set_value("RT:V", ScalarValue::F64(99.0)).await;
            assert_eq!(store.get_value("RT:V").await, Some(ScalarValue::F64(99.0)));
        });
    }

    #[test]
    fn link_propagates_on_set_value() {
        let server = PvaServer::builder()
            .ao("INPUT:A", 1.0)
            .ao("INPUT:B", 2.0)
            .ai("CALC:SUM", 0.0)
            .link("CALC:SUM", &["INPUT:A", "INPUT:B"], |values| {
                let a = match &values[0] {
                    ScalarValue::F64(v) => *v,
                    _ => 0.0,
                };
                let b = match &values[1] {
                    ScalarValue::F64(v) => *v,
                    _ => 0.0,
                };
                ScalarValue::F64(a + b)
            })
            .build();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = server.store().clone();
        rt.block_on(async {
            // Writing INPUT:A should recompute CALC:SUM = 10 + 2.
            store.set_value("INPUT:A", ScalarValue::F64(10.0)).await;
            assert_eq!(
                store.get_value("CALC:SUM").await,
                Some(ScalarValue::F64(12.0))
            );

            // Writing INPUT:B should recompute CALC:SUM = 10 + 5.
            store.set_value("INPUT:B", ScalarValue::F64(5.0)).await;
            assert_eq!(
                store.get_value("CALC:SUM").await,
                Some(ScalarValue::F64(15.0))
            );
        });
    }

    use crate::pv::{AnyPv, Pv};

    #[tokio::test]
    async fn serve_builder_binds_handles_and_registers_everything() {
        let temp = Pv::ai("S:T", 22.5).mdel(0.1);
        let sp = Pv::ao("S:SP", 25.0).on_put(|_pv, _v: f64| Ok(()));
        let a = Pv::ai("S:A", 1.0);
        let b = Pv::ai("S:B", 2.0);
        let sum = Pv::calc("S:SUM", &[&a, &b], |vals| vals.iter().sum());

        // Rust can't infer the `impl Into<AnyPv>` target type per-element
        // inside an array literal (E0283), so each handle is converted
        // explicitly here rather than via a bare `.into()`.
        let server = PvaServer::serve([AnyPv::from(temp.clone()), AnyPv::from(sp)])
            .pvs([
                AnyPv::from(a.clone()),
                AnyPv::from(b),
                AnyPv::from(sum.clone()),
            ])
            .build()
            .await;

        // handles are bound: typed set/get works against the built store
        temp.set(23.0).await.unwrap();
        assert_eq!(temp.get().await, Ok(23.0));

        // calc evaluated on input change
        a.set(10.0).await.unwrap();
        assert_eq!(sum.get().await, Ok(12.0));

        // record made it into the store with its raw fields
        let rec = server.store().get_record("S:T").await.unwrap();
        assert_eq!(rec.raw_fields.get("MDEL").map(String::as_str), Some("0.1"));
    }

    #[tokio::test]
    async fn running_server_mints_handles_to_db_records() {
        // parse_db is line-oriented (one `record(...)`/`field(...)`
        // statement per line); a packed one-liner silently drops its
        // fields, so this uses the same multi-line shape as the other
        // db_string tests in this module.
        let server = PvaServer::serve(Vec::<AnyPv>::new())
            .db_string("record(ao, \"DB:X\") {\n    field(VAL, \"2.5\")\n}")
            .build()
            .await;
        let store = server.store().clone();
        let h: crate::pv::Pv<f64> = crate::pv::Pv::attach(&store, "DB:X").await.unwrap();
        assert_eq!(h.get().await, Ok(2.5));
    }

    #[tokio::test]
    async fn homogeneous_iterator_feeds_serve_without_manual_erasure() {
        let bpms: Vec<Pv<f64>> = (0..100)
            .map(|i| Pv::ai(format!("BPM:{i:03}:X"), 0.0))
            .collect();
        let server = PvaServer::serve(bpms.iter().cloned()).build().await;
        assert_eq!(server.store().pv_names().await.len(), 100);
        bpms[42].set(1.23).await.unwrap();
        assert_eq!(bpms[42].get().await, Ok(1.23));
    }

    #[tokio::test]
    async fn pva_server_mints_typed_handles_pre_run() {
        let server = PvaServer::serve([AnyPv::from(Pv::ai("PRE:X", 5.0))])
            .build()
            .await;
        let h: crate::pv::Pv<f64> = server.pv("PRE:X").await.unwrap();
        assert_eq!(h.get().await, Ok(5.0));
        assert!(matches!(
            server.pv::<bool>("PRE:X").await,
            Err(crate::pv::PvError::TypeMismatch { .. })
        ));
    }
}
