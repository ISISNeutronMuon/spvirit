use spvirit_client::{MonitorOptions, MonitorUpdate, PvaClient};
use std::ops::ControlFlow;

/// Minimal `pvmonitor` example.
///
/// Usage:
///
/// ```text
/// cargo run --example pvmonitor -- MY:PV
/// cargo run --example pvmonitor -- MY:PV --fields value,alarm.severity
/// cargo run --example pvmonitor -- MY:PV --pipeline 4
/// ```
// ANCHOR: main
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut pv: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();
    let mut pipeline: Option<u32> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "-F" | "--fields" => {
                let raw = args.next().ok_or("--fields requires a value")?;
                fields.extend(
                    raw.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
            "--pipeline" => {
                let raw = args.next().ok_or("--pipeline requires a queue size")?;
                pipeline = Some(raw.parse()?);
            }
            _ if pv.is_none() => pv = Some(a),
            other => return Err(format!("unexpected argument: {other}").into()),
        }
    }

    let pv = pv.unwrap_or_else(|| "MY:PV:NAME".into());
    // ANCHOR: core
    let client = PvaClient::builder().build();
    let cb = |update: &MonitorUpdate| {
        println!("{}", update.value);
        ControlFlow::Continue(())
    };
    let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
    if let Some(q) = pipeline {
        client
            .pvmonitor_with_options(&pv, &refs, MonitorOptions::pipelined(q), cb)
            .await?;
    } else if fields.is_empty() {
        client.pvmonitor(&pv, cb).await?;
    } else {
        client.pvmonitor_fields(&pv, &refs, cb).await?;
    }
    // ANCHOR_END: core
    Ok(())
}
// ANCHOR_END: main
