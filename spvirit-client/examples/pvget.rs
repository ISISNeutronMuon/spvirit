use spvirit_client::PvaClient;

/// Minimal `pvget` example.
///
/// Usage:
///
/// ```text
/// cargo run --example pvget -- MY:PV
/// cargo run --example pvget -- MY:PV --fields value,alarm.severity
/// ```
// ANCHOR: main
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut pv: Option<String> = None;
    let mut fields: Vec<String> = Vec::new();

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
            _ if pv.is_none() => pv = Some(a),
            other => return Err(format!("unexpected argument: {other}").into()),
        }
    }

    let pv = pv.unwrap_or_else(|| "MY:PV:NAME".into());
    // ANCHOR: core
    let client = PvaClient::builder().build();
    let result = if fields.is_empty() {
        client.pvget(&pv).await?
    } else {
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        client.pvget_fields(&pv, &refs).await?
    };
    println!("{}: {}", result.pv_name, result.value);
    // ANCHOR_END: core
    Ok(())
}
// ANCHOR_END: main
