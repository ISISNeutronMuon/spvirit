use spvirit_client::PvaClient;

/// Minimal `pvput` example.
///
/// Usage:
///
/// ```text
/// cargo run --example pvput -- MY:PV 42.0
/// cargo run --example pvput -- MY:PV 42.0 --fields value
/// ```
// ANCHOR: main
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut positional: Vec<String> = Vec::new();
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
            _ => positional.push(a),
        }
    }

    let pv = positional
        .first()
        .cloned()
        .unwrap_or_else(|| "MY:PV:NAME".into());
    let value: f64 = positional
        .get(1)
        .cloned()
        .unwrap_or_else(|| "42.0".into())
        .parse()?;

    // ANCHOR: core
    let client = PvaClient::builder().build();
    if fields.is_empty() {
        client.pvput(&pv, value).await?;
    } else {
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        client.pvput_fields(&pv, value, &refs).await?;
    }
    println!("OK");
    // ANCHOR_END: core
    Ok(())
}
// ANCHOR_END: main
