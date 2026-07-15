//! PVAccess server library for EPICS.
//!
//! Provides reusable server-side types, .db parsing, connection state, and the
//! [`Source`] trait that abstracts over a PV data source.  Consumers implement
//! `Source` and register it with the [`SourceRegistry`] to serve PVs over PVAccess.
//!
//! # High-level API
//!
//! ```rust,ignore
//! use spvirit_server::{AnyPv, Pv, PvaServer};
//!
//! let temp = Pv::ai("SIM:TEMP", 22.5).units("C").prec(2);
//! let sp = Pv::ao("SIM:SETPOINT", 25.0)
//!     .on_put(|_pv, v: f64| if v.is_finite() { Ok(()) } else { Err("NaN".into()) });
//!
//! let server = PvaServer::serve([AnyPv::from(temp.clone()), AnyPv::from(sp)])
//!     .start()
//!     .await;
//! temp.set(23.1).await?;
//!
//! // Classic builder (still supported):
//! let server = PvaServer::builder()
//!     .ai("SIM:TEMP", 22.5)
//!     .ao("SIM:SETPOINT", 25.0)
//!     .bo("SIM:ENABLE", false)
//!     .build();
//!
//! server.run().await?;
//! ```

pub mod apply;
pub mod beacon;
pub mod convert;
pub mod db;
pub mod decode;
pub mod group;
pub mod handler;
pub mod monitor;
pub mod pv;
pub mod pva_server;
pub mod pvstore;
pub mod record_fields;
pub mod server;
pub mod simple_store;
pub mod state;
pub mod types;

// Convenience re-exports.
pub use group::{
    FieldMapping, GroupMember, GroupPvDef, GroupSource, TriggerDef, merge_group_defs,
    parse_group_config, parse_info_group,
};
pub use handler::PvListMode;
pub use pv::{AnyPv, Pv, PvError, PvScalar};
pub use pva_server::{PvaServer, PvaServerBuilder, RunningServer, ServeBuilder};
pub use pvstore::{PvInfo, Source, SourceRegistry};
pub use server::{PvaServerConfig, PvaServerState, run_pva_server, run_pva_server_with_registry};
pub use simple_store::SimplePvStore;
pub use types::{
    DbCommonState, LinkExpr, OutputMode, RecordData, RecordInstance, RecordType, ScanMode,
};
