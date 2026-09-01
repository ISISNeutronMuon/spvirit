/// # Custom Source Backend Example
///
/// This example demonstrates how to implement a custom [`Source`] backend
/// to integrate spvirit-server with any data source.
///
/// The example implements a simulated sensor backend that:
/// - Manages synthetic sensor PVs (temperature, pressure, etc.)
/// - Generates realistic sine-wave data for each sensor
/// - Supports subscribing to value changes
/// - Permits PUT operations on setpoint PVs
///
/// This pattern can be adapted to:
/// - Connect to external hardware via I/O libraries
/// - Read/write from a database
/// - Integrate with REST APIs
/// - Proxy to other control systems
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use spvirit_codec::spvd_decode::{DecodedValue, FieldDesc, FieldType, StructureDesc, TypeCode};
use spvirit_server::monitor::MonitorRegistry;
use spvirit_server::pvstore::{PvInfo, Source, SourceRegistry};
use spvirit_types::{NtPayload, NtScalar, ScalarValue};
use tokio::sync::RwLock;
use tokio::sync::mpsc;

/// A simulated sensor backend that generates synthetic PV data.
/// In a real system, this would connect to actual hardware or a database.
struct SensorBackend {
    /// Stores the current state of each simulated sensor
    sensors: Arc<RwLock<HashMap<String, SensorState>>>,
    /// Subscribers waiting for value updates
    subscribers: Arc<RwLock<HashMap<String, Vec<mpsc::Sender<NtPayload>>>>>,
    /// Registry for notifying PVAccess protocol clients
    monitor_registry: Arc<RwLock<Option<Arc<MonitorRegistry>>>>,
}

/// The internal state of a single simulated sensor
#[derive(Clone)]
struct SensorState {
    /// Current value (real sensors might read this from hardware)
    value: f64,
    /// Amplitude for sine-wave simulation
    amplitude: f64,
    /// Frequency in Hz for sine-wave simulation
    frequency: f64,
    /// Whether this sensor accepts writes
    writable: bool,
}

impl SensorState {
    fn new(initial_value: f64, amplitude: f64, frequency: f64, writable: bool) -> Self {
        Self {
            value: initial_value,
            amplitude,
            frequency,
            writable,
        }
    }

    /// Update the simulated value based on elapsed time.
    /// Returns true if the value changed (i.e. this is an active sensor).
    fn update(&mut self) -> bool {
        // Skip static/writable PVs — their value is set externally
        if self.amplitude == 0.0 {
            return false;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let phase = 2.0 * std::f64::consts::PI * self.frequency * now as f64;
        self.value = self.amplitude * phase.sin();
        true
    }

    /// Create an NtPayload snapshot of the current state
    fn to_payload(&self) -> NtPayload {
        NtPayload::Scalar(NtScalar::from_value(ScalarValue::F64(self.value)))
    }
}

impl SensorBackend {
    /// Create a new sensor backend with some example sensors
    fn new() -> Self {
        let mut sensors = HashMap::new();

        // Temperature sensor: ±2.5°C oscillation, 0.1 Hz frequency (10s period)
        sensors.insert(
            "SENSOR:TEMP".to_string(),
            SensorState::new(22.5, 2.5, 0.1, false),
        );

        // Pressure sensor: ±50 hPa oscillation, 0.15 Hz frequency
        sensors.insert(
            "SENSOR:PRESSURE".to_string(),
            SensorState::new(1050.0, 50.0, 0.15, false),
        );

        // Humidity sensor: ±15% oscillation, 0.2 Hz frequency
        sensors.insert(
            "SENSOR:HUMIDITY".to_string(),
            SensorState::new(55.0, 15.0, 0.2, false),
        );

        // Setpoint: writable target for temperature control
        sensors.insert(
            "CONTROL:TEMP_SETPOINT".to_string(),
            SensorState::new(23.0, 0.0, 0.0, true),
        );

        // Feedback: read-only representation of control system output
        sensors.insert(
            "CONTROL:FEEDBACK".to_string(),
            SensorState::new(0.0, 0.0, 0.0, false),
        );

        Self {
            sensors: Arc::new(RwLock::new(sensors)),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            monitor_registry: Arc::new(RwLock::new(None)),
        }
    }

    /// Set the MonitorRegistry so PVAccess monitor clients are notified of updates.
    /// This is called automatically by the server after startup.
    pub async fn set_registry(&self, registry: Arc<MonitorRegistry>) {
        *self.monitor_registry.write().await = Some(registry);
    }

    /// Background task to simulate sensor updates and notify subscribers
    async fn run_updates(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            interval.tick().await;

            // Update all sensor values, collecting names that changed
            let changed_names: Vec<String> = {
                let mut sensors = self.sensors.write().await;
                sensors
                    .iter_mut()
                    .filter_map(|(name, sensor)| sensor.update().then(|| name.clone()))
                    .collect()
            };

            // Build payloads only for sensors that changed
            let updates: Vec<(String, NtPayload)> = {
                let sensors = self.sensors.read().await;
                changed_names
                    .iter()
                    .filter_map(|name| sensors.get(name).map(|s| (name.clone(), s.to_payload())))
                    .collect()
            };

            // Notify PVAccess protocol clients through the monitor registry
            let registry = self.monitor_registry.read().await;
            if let Some(registry) = registry.as_ref() {
                for (pv_name, payload) in &updates {
                    println!("Notifying PVAccess clients of update to '{}'", pv_name);
                    registry.notify_monitors(pv_name, payload).await;
                }
            }

            // Also notify internal subscribers via mpsc channels.
            //
            // NOTE for anyone copying this template: distinguish the two
            // `try_send` failures. `Full` means the receiver is alive and
            // merely behind — drop the *update*, never the sender. `Closed`
            // means the receiver is genuinely gone, so the sender can never
            // deliver again and is pruned. Dropping a sender on `Full` would
            // close the stream, and for a source the server pumps a closed
            // stream MEANS "this PV is dead": one episode of ordinary
            // backpressure would send DESTROY_CHANNEL to every subscriber of
            // the PV. This store self-notifies (`pushes_own_updates() == true`
            // below), so the server never pumps it and never sees these
            // channels — but copy the pattern, because the moment a template
            // leaves that flag at its `false` default the hazard is real. See
            // `wildcard_source.rs` for the pumped case.
            let mut subscribers = self.subscribers.write().await;
            for (pv_name, payload) in updates {
                if let Some(senders) = subscribers.get_mut(&pv_name) {
                    senders.retain(|tx| match tx.try_send(payload.clone()) {
                        Ok(()) => true,
                        Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    });
                }
            }
        }
    }
}

impl Source for SensorBackend {
    fn claim(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<PvInfo>> + Send + '_>> {
        let sensors = self.sensors.clone();
        let name = name.to_string();
        Box::pin(async move {
            let sensors = sensors.read().await;
            let sensor = sensors.get(&name)?;
            println!("[claim] '{}' -> Some", name);
            Some(PvInfo {
                descriptor: StructureDesc {
                    struct_id: Some("epics:nt/NTScalar:1.0".to_string()),
                    fields: vec![FieldDesc {
                        name: "value".to_string(),
                        field_type: FieldType::Scalar(TypeCode::Float64),
                    }],
                },
                writable: sensor.writable,
            })
        })
    }

    fn get(&self, name: &str) -> Pin<Box<dyn Future<Output = Option<NtPayload>> + Send + '_>> {
        let sensors = self.sensors.clone();
        let name = name.to_string();
        Box::pin(async move {
            let result = sensors.read().await.get(&name).map(|s| s.to_payload());
            println!(
                "[get] '{}' -> {}",
                name,
                if result.is_some() { "Some" } else { "None" }
            );
            result
        })
    }

    fn put(
        &self,
        name: &str,
        value: &DecodedValue,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(String, NtPayload)>, String>> + Send + '_>> {
        let sensors = self.sensors.clone();
        let name = name.to_string();
        let value = value.clone();
        Box::pin(async move {
            println!("[put] '{}' called with {:?}", name, value);
            let mut sensors = sensors.write().await;
            let sensor = sensors.get_mut(&name).ok_or_else(|| {
                println!("[put] '{}' -> ERROR: PV not found", name);
                format!("PV '{}' not found", name)
            })?;

            if !sensor.writable {
                println!("[put] '{}' -> ERROR: not writable", name);
                return Err(format!("PV '{}' is not writable", name));
            }

            // Extract numeric value from PUT request
            let new_value = match &value {
                DecodedValue::Float64(v) => *v,
                DecodedValue::Int32(v) => *v as f64,
                DecodedValue::Int64(v) => *v as f64,
                DecodedValue::Structure(fields) => fields
                    .iter()
                    .find(|(k, _)| k == "value")
                    .and_then(|(_, v)| match v {
                        DecodedValue::Float64(f) => Some(*f),
                        DecodedValue::Int32(i) => Some(*i as f64),
                        DecodedValue::Int64(i) => Some(*i as f64),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        println!(
                            "[put] '{}' -> ERROR: no numeric 'value' field in structure",
                            name
                        );
                        "Invalid value format".to_string()
                    })?,
                other => {
                    println!(
                        "[put] '{}' -> ERROR: unsupported DecodedValue variant: {:?}",
                        name, other
                    );
                    return Err("Unsupported value type".to_string());
                }
            };

            println!("[put] '{}' = {} -> OK", name, new_value);
            sensor.value = new_value;
            let payload = sensor.to_payload();

            Ok(vec![(name.clone(), payload)])
        })
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = Option<mpsc::Receiver<NtPayload>>> + Send + '_>> {
        let sensors = self.sensors.clone();
        let subscribers = self.subscribers.clone();
        let name = name.to_string();
        Box::pin(async move {
            if !sensors.read().await.contains_key(&name) {
                println!("[subscribe] '{}' -> None (PV not found)", name);
                return None;
            }

            let (tx, rx) = mpsc::channel(64);
            subscribers
                .write()
                .await
                .entry(name.clone())
                .or_default()
                .push(tx);

            println!("[subscribe] '{}' -> subscribed", name);
            Some(rx)
        })
    }

    /// This backend pushes its own updates: `run_updates` calls
    /// `MonitorRegistry::notify_monitors` for every change. Saying so is what
    /// stops the server *also* pumping `subscribe` into the registry, which
    /// would deliver every update to a PVA client twice. The `subscribe`
    /// stream above stays available for in-process consumers.
    fn pushes_own_updates(&self) -> bool {
        true
    }

    fn names(&self) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        let sensors = self.sensors.clone();
        Box::pin(async move {
            let names: Vec<String> = sensors.read().await.keys().cloned().collect();
            println!("[names] -> {} PVs", names.len());
            names
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = Arc::new(SensorBackend::new());

    // Create the monitor registry for notifying PVAccess clients
    let registry = Arc::new(MonitorRegistry::new());
    backend.set_registry(registry.clone()).await;

    // Start the background update task
    let backend_clone = backend.clone();
    tokio::spawn(async move {
        backend_clone.run_updates().await;
    });

    // Print available PVs with their writability status
    println!("Custom Source backend server starting");
    println!("Available PVs:");
    for pv in backend.names().await {
        let writable = backend
            .claim(&pv)
            .await
            .map(|info| info.writable)
            .unwrap_or(false);
        println!("  - {} (writable: {})", pv, writable);
    }
    println!();
    println!("Try: spvirit-monitor SENSOR:TEMP SENSOR:PRESSURE SENSOR:HUMIDITY");
    println!("Try: spvirit-put CONTROL:TEMP_SETPOINT 25.0 (should be writable)");

    // Build a SourceRegistry and register the custom backend
    let sources = Arc::new(SourceRegistry::new());
    sources.add("sensors", 0, backend).await;

    // Create the default server config
    let config = spvirit_server::PvaServerConfig::default();

    // Run the PVA server with the source registry
    spvirit_server::run_pva_server_with_registry(sources, config, registry).await
}
