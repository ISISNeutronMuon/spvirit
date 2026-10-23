use std::f64::consts::PI;

use argparse::{ArgumentParser, Store};
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::time::{Instant, interval};

use spvirit_client::client_from_opts;
use spvirit_tools::spvirit_client::cli::{CommonClientArgs, require_pv_name};
use spvirit_tools::spvirit_client::types::PvGetError;

async fn pvsine(
    opts: &spvirit_client::PvOptions,
    freq_hz: f64,
    rate_hz: f64,
    amp: f64,
    offset: f64,
    phase: f64,
    duration: Option<f64>,
) -> Result<(), PvGetError> {
    if rate_hz <= 0.0 {
        return Err(PvGetError::Protocol("rate must be > 0".to_string()));
    }

    let client = client_from_opts(opts);
    let mut channel = client.open_put_channel(&opts.pv_name).await?;

    let mut tick = interval(std::time::Duration::from_secs_f64(1.0 / rate_hz));
    let start = Instant::now();

    loop {
        tick.tick().await;
        let t = start.elapsed().as_secs_f64();
        if let Some(dur) = duration
            && t >= dur
        {
            break;
        }
        let value = offset + amp * (2.0 * PI * freq_hz * t + phase).sin();
        let number = serde_json::Number::from_f64(value).ok_or_else(|| {
            PvGetError::Protocol(format!(
                "computed value is not finite ({value}); check --amp/--offset/--phase"
            ))
        })?;
        let input = Value::Number(number);
        channel.put(input).await?;
    }

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pv_name = String::new();
    let mut freq_hz: f64 = 1.0;
    let mut rate_hz: f64 = 10.0;
    let mut amp: f64 = 1.0;
    let mut offset: f64 = 0.0;
    let mut phase: f64 = 0.0;
    let mut duration_secs: f64 = 0.0;
    let mut common = CommonClientArgs::new();

    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Generate a sine wave and pvput it at a fixed rate");
        ap.refer(&mut pv_name)
            .add_argument("pv", Store, "PV name to write");
        ap.refer(&mut freq_hz)
            .add_option(&["--freq"], Store, "Sine frequency (Hz)");
        ap.refer(&mut rate_hz)
            .add_option(&["--rate"], Store, "Update rate (samples/sec)");
        ap.refer(&mut amp)
            .add_option(&["--amp"], Store, "Amplitude");
        ap.refer(&mut offset)
            .add_option(&["--offset"], Store, "Offset");
        ap.refer(&mut phase)
            .add_option(&["--phase"], Store, "Phase (radians)");
        ap.refer(&mut duration_secs).add_option(
            &["--duration"],
            Store,
            "Duration (seconds, 0=run forever)",
        );
        common.add_to_parser(&mut ap);
        ap.parse_args_or_exit();
    }

    require_pv_name(&pv_name)?;
    common.init_tracing();
    let opts = common.into_pv_get_options(pv_name.clone())?;

    let duration = if duration_secs > 0.0 {
        Some(duration_secs)
    } else {
        None
    };

    let rt = Runtime::new()?;
    let result =
        rt.block_on(
            async move { pvsine(&opts, freq_hz, rate_hz, amp, offset, phase, duration).await },
        );
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("{} ERROR {}", pv_name, e);
            Err(e.into())
        }
    }
}
