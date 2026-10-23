use reqwest::Client;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use tracing::{debug, error, info, warn};

/// Maximum payload size per Loki push request (512KB, well under typical nginx 1MB limit)
const MAX_BATCH_BYTES: usize = 512 * 1024;

#[derive(Serialize)]
struct Stream {
    stream: HashMap<String, String>,
    values: Vec<(String, String, String)>,
}

#[derive(Serialize)]
struct PushRequest {
    streams: Vec<Stream>,
}

pub struct LokiLogger {
    client: Client,
    loki_url: String,
}

impl LokiLogger {
    pub fn new(loki_url: String) -> Self {
        let client = Client::new();
        LokiLogger { client, loki_url }
    }

    pub async fn log(
        &self,
        logs: Vec<(String, String, HashMap<String, String>)>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if logs.is_empty() {
            return Ok(());
        }

        let mut grouped_streams: HashMap<BTreeMap<String, String>, Vec<Vec<serde_json::Value>>> =
            HashMap::new();

        for (timestamp, log_line, mut metadata) in logs {
            let request_type = metadata
                .remove("request_type")
                .unwrap_or_else(|| "unknown".to_string());
            let source_ip = metadata
                .remove("source_ip")
                .unwrap_or_else(|| "unknown".to_string());
            let destination_ip = metadata
                .remove("destination_ip")
                .unwrap_or_else(|| "unknown".to_string());
            let hostname = metadata
                .remove("host_name")
                .unwrap_or_else(|| "unknown".to_string());

            let mut labels = BTreeMap::new();
            labels.insert("job".to_string(), "epics-event-logger-rust".to_string());
            labels.insert("request_type".to_string(), request_type);
            labels.insert("source_ip".to_string(), source_ip);
            labels.insert("destination_ip".to_string(), destination_ip);
            labels.insert("host_name".to_string(), hostname);

            metadata.remove("context");

            let metadata_json = serde_json::Value::Object(
                metadata
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::String(v)))
                    .collect(),
            );

            grouped_streams.entry(labels).or_default().push(vec![
                serde_json::Value::String(timestamp),
                serde_json::Value::String(log_line),
                metadata_json,
            ]);
        }

        // Split into batches that fit under the size limit
        let all_streams: Vec<(BTreeMap<String, String>, Vec<Vec<serde_json::Value>>)> =
            grouped_streams.into_iter().collect();

        let mut batches: Vec<Vec<serde_json::Value>> = Vec::new();
        let mut current_batch: Vec<serde_json::Value> = Vec::new();
        let mut current_size: usize = 16; // base JSON overhead: {"streams":[]}

        for (labels, values) in all_streams {
            // Estimate label overhead once per stream
            let label_json = serde_json::to_string(&labels).unwrap_or_default();
            let stream_overhead = label_json.len() + 32; // {"stream":...,"values":[]}

            // If all values fit in a single stream entry within budget, add them
            let values_json_size: usize = values
                .iter()
                .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(128))
                .sum();

            if current_size + stream_overhead + values_json_size <= MAX_BATCH_BYTES {
                // Whole stream fits in current batch
                current_size += stream_overhead + values_json_size;
                current_batch.push(serde_json::json!({
                    "stream": labels,
                    "values": values
                }));
            } else {
                // Need to split this stream's values across batches
                let mut remaining = values;
                while !remaining.is_empty() {
                    // How much space is left in the current batch?
                    let available = if current_size + stream_overhead >= MAX_BATCH_BYTES {
                        // Current batch is full, flush it
                        if !current_batch.is_empty() {
                            batches.push(current_batch);
                            current_batch = Vec::new();
                            current_size = 16;
                        }
                        MAX_BATCH_BYTES - 16 - stream_overhead
                    } else {
                        MAX_BATCH_BYTES - current_size - stream_overhead
                    };

                    // Take as many values as fit
                    let mut chunk: Vec<Vec<serde_json::Value>> = Vec::new();
                    let mut chunk_size: usize = 0;

                    while !remaining.is_empty() {
                        let entry_size = serde_json::to_string(&remaining[0])
                            .map(|s| s.len())
                            .unwrap_or(128);
                        if chunk_size + entry_size > available && !chunk.is_empty() {
                            break;
                        }
                        chunk_size += entry_size;
                        chunk.push(remaining.remove(0));
                    }

                    if !chunk.is_empty() {
                        current_size += stream_overhead + chunk_size;
                        current_batch.push(serde_json::json!({
                            "stream": labels,
                            "values": chunk
                        }));
                    }
                }
            }
        }

        // Flush remaining batch
        if !current_batch.is_empty() {
            batches.push(current_batch);
        }

        // Send all batches
        let push_url = format!("{}/loki/api/v1/push", self.loki_url.trim_end_matches('/'));
        let num_batches = batches.len();

        if num_batches > 1 {
            debug!("Splitting Loki push into {} batches", num_batches);
        }

        for (i, batch) in batches.into_iter().enumerate() {
            let payload = serde_json::json!({ "streams": batch });
            let res = self.client.post(&push_url).json(&payload).send().await?;

            if res.status().is_success() {
                debug!("Loki batch {}/{} sent successfully.", i + 1, num_batches);
            } else {
                let status = res.status();
                let body = res
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                warn!(
                    "Failed to send Loki batch {}/{}: HTTP {} - {}",
                    i + 1,
                    num_batches,
                    status,
                    body
                );
            }
        }

        Ok(())
    }

    pub async fn ping(&self) -> Result<(), Box<dyn std::error::Error>> {
        let ready_url = format!("{}/ready", self.loki_url.trim_end_matches('/'));

        match self.client.get(&ready_url).send().await {
            Ok(res) if res.status().is_success() => {
                info!("Loki is healthy and ready.");
                Ok(())
            }
            Ok(res) => {
                let status = res.status();
                let body = res
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                warn!("Loki is not ready: HTTP {} - {}", status, body);
                Ok(())
            }
            Err(err) => {
                error!("Error while checking Loki readiness: {}", err);
                Ok(())
            }
        }
    }
}
