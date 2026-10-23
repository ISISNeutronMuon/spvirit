use color_eyre::owo_colors::OwoColorize;
use spvirit_tools::scry::common::stats::IfStats;
use spvirit_tools::scry::utils::common_version_print::version_header;
use spvirit_tools::scry::utils::influxdb_utils::InfluxDB;
use spvirit_tools::scry::utils::loki_utils::LokiLogger;
use spvirit_tools::scry::utils::packet_processor::process_packets;

use argparse::{ArgumentParser, Store, StoreFalse, StoreTrue};
use std::sync::{Arc, Mutex};
use std::thread;
// use colored::Colorize;
use colored::*;
use tracing::{debug, error, info, Level};

// toml parsing
use serde::Deserialize;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut verbose = true;
    let mut show_payload = false;
    let mut device = String::from("bond10gbe");
    let mut integration_period = 1;
    let mut tui_mode = false;
    let mut debug_mode = false;
    let mut resolve = false;
    let mut buffer_size: usize = 5000;
    let mut influxdb_config_file = String::from("influxdb_config.toml");
    let mut version = false;
    let mut fp_enabled = false;
    let mut fp_window_secs: u64 = 900;
    let mut fp_min_samples: u64 = 30;
    let mut fp_reservoir: usize = 200;
    let mut fp_slots: usize = 1;
    let mut fp_max_channels: usize = 5000;
    let mut fp_d1 = false;
    let mut fp_suppress_secs: u64 = 0;

    #[derive(Deserialize, Debug)]
    struct Config {
        url: String,
        bucket: String,
        org: String,
        token: String,
        loki_url: String,
    }

    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Packet capture tool");
        ap.refer(&mut verbose)
            .add_option(&["-q", "--quiet"], StoreFalse, "Be quiet");
        ap.refer(&mut device)
            .add_option(&["-i", "--device"], Store, "Device to capture from");
        ap.refer(&mut integration_period).add_option(
            &["-t", "--integration-time"],
            Store,
            "Integration period in seconds",
        );
        ap.refer(&mut show_payload).add_option(
            &["-p", "--show-payload"],
            StoreTrue,
            "Show packet payload",
        );
        ap.refer(&mut tui_mode)
            .add_option(&["-t", "--tui"], StoreTrue, "Show TUI");
        ap.refer(&mut debug_mode)
            .add_option(&["-d", "--debug"], StoreTrue, "Show debug messages");
        ap.refer(&mut resolve)
            .add_option(&["-r", "--resolve"], StoreTrue, "Resolve IP addresses");
        ap.refer(&mut buffer_size).add_option(
            &["-b", "--buffer-size"],
            Store,
            "Buffer size for packet capture 100 - 50000",
        );

        ap.refer(&mut influxdb_config_file).add_option(
            &["-c", "--config-file"],
            Store,
            "InfluxDB config file",
        );
        ap.refer(&mut version)
            .add_option(&["-v", "--version"], StoreTrue, "Show version and exit");

        ap.refer(&mut fp_enabled).add_option(
            &["--fingerprint"],
            StoreTrue,
            "Enable passive source fingerprinting (off by default)",
        );
        ap.refer(&mut fp_window_secs).add_option(
            &["--fp-window"],
            Store,
            "Fingerprint window in seconds (default: 900)",
        );
        ap.refer(&mut fp_min_samples).add_option(
            &["--fp-min-samples"],
            Store,
            "Minimum samples before a fingerprint is emitted (default: 30)",
        );
        ap.refer(&mut fp_reservoir).add_option(
            &["--fp-reservoir"],
            Store,
            "Per-channel reservoir size per window (default: 200)",
        );
        ap.refer(&mut fp_slots).add_option(
            &["--fp-round-robin-slots"],
            Store,
            "Round-robin channel slots; N>1 extends detection latency N-fold (default: 1)",
        );
        ap.refer(&mut fp_max_channels).add_option(
            &["--fp-max-channels"],
            Store,
            "Maximum channels fingerprinted concurrently (default: 5000)",
        );
        ap.refer(&mut fp_d1).add_option(
            &["--fp-d1"],
            StoreTrue,
            "Enable D1 source-change detection. Off by default: planned failover is              indistinguishable from silent re-sourcing (spec 6.2)",
        );
        ap.refer(&mut fp_suppress_secs).add_option(
            &["--fp-suppress-secs"],
            Store,
            "Suppress D1 for this many seconds from startup (maintenance window)",
        );
        ap.parse_args_or_exit();
    }
    if version {
        version_header();
        std::process::exit(0);
    }

    let if_stats = Arc::new(Mutex::new(IfStats::new()));
    if resolve {
        let mut if_stats = if_stats.lock().unwrap();
        if_stats.enable_resolve();
    }
    if !(100..=50_000).contains(&buffer_size) {
        eprintln!("Buffer size must be between 100 and 50000");
        std::process::exit(1);
    } else {
        let mut if_stats = if_stats.lock().unwrap();
        if_stats.set_buffer_size(buffer_size);
    }

    let config: Config = toml::from_str(&std::fs::read_to_string(influxdb_config_file)?)?;

    let loki_enabled = false;

    let loki_logger = LokiLogger::new(config.loki_url);

    let influxdb_client: InfluxDB =
        InfluxDB::new(config.url, config.bucket, config.org, config.token);

    let influxdb_ping = influxdb_client.ping();
    let loki_ping = loki_logger.ping();

    if verbose {
        if debug_mode {
            tracing_subscriber::fmt::fmt()
                .with_max_level(Level::DEBUG)
                .init();
        } else {
            tracing_subscriber::fmt::fmt()
                .with_max_level(Level::INFO)
                .init();
        }
        if !tui_mode {
            // The name was env!("CARGO_PKG_NAME") before the move into spvirit-tools.
            info!("Starting spvirit-scry v{} ({}) [{}]",
                env!("VERGEN_GIT_DESCRIBE"),
                env!("VERGEN_GIT_BRANCH"),
                env!("VERGEN_GIT_SHA"),
            );
            info!("{}", format!("Using device {}", device).green());
        }
        info!("InfluxDB ping: {:?}", influxdb_ping.await.unwrap());
        info!(
            "InfluxDB bucket check: {:?}",
            influxdb_client.check_bucket_exists().await.unwrap()
        );
    }

    let mut handles_vec = vec![];

    let mut if_stats_clone = Arc::clone(&if_stats);
    debug!("Starting packet processing thread");
    let fingerprint_options = spvirit_tools::scry::common::fingerprint::options::FingerprintOptions {
        enabled: fp_enabled,
        config: spvirit_tools::scry::common::fingerprint::engine::FingerprintConfig {
            window_secs: fp_window_secs,
            min_samples: fp_min_samples,
            reservoir_cap: fp_reservoir,
            round_robin_slots: fp_slots.max(1),
            max_channels: fp_max_channels,
            ..Default::default()
        },
        // Spec §6.2 is unresolved: a planned failover and a silent
        // re-sourcing are the same signature on the wire, so D1 only runs
        // when the operator asks for it, and `--fp-suppress-secs` gives the
        // maintenance-window option the spec lists.
        detector: spvirit_tools::scry::common::fingerprint::detect::DetectorConfig {
            d1_enabled: fp_d1,
            suppress_until: (fp_suppress_secs > 0).then(|| {
                std::time::SystemTime::now() + std::time::Duration::from_secs(fp_suppress_secs)
            }),
            ..Default::default()
        },
    };

    let handle_scan = thread::spawn(move || {
        process_packets(
            verbose,
            show_payload,
            &device,
            integration_period,
            &mut if_stats_clone,
            Vec::new(),
            86_400, // pva_ttl_secs: a day, so passive accumulation has time to work
            40_000, // pva_max_channels: 40k default
            1024,   // pva_max_memory_mb: what bounds the long TTL
            false,  // tracked_only: disabled by default in collector
            10_000, // pva_max_update_rate: 10k default
            None,
            fingerprint_options,
        );
    });
    handles_vec.push(handle_scan);
    debug!("Packet processing thread started");

    // if tui_mode {
    //     debug!("Starting TUI thread");
    //     let mut if_stats_clone_tui = Arc::clone(&if_stats);
    //     let handle_tui = thread::spawn(move || {
    //         let _res = main_funcs(&mut if_stats_clone_tui);
    //     });
    //     handles_vec.push(handle_tui);
    //     debug!("TUI thread started");
    // }

    // if verbose && !tui_mode {
    loop {
        let data = {
            let stats = if_stats.lock().unwrap();
            stats.to_line_protocol().concat()
        };
        let loki_logs = {
            let stats = if_stats.lock().unwrap();
            stats.to_loki_stream()
        };
        influxdb_client.write(data).await.unwrap();
        loki_logger
            .log(loki_logs)
            .await
            .unwrap_or_else(|e| error!("Failed to send logs to Loki: {}", e));
        {
            let mut stats = if_stats.lock().unwrap();
            stats.clear_events();
            stats.clear_fingerprint_exports();
            stats.rotate_stats();
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        if !show_payload {
            let if_stats = if_stats.lock().unwrap();
            let rate = if_stats.data_rate;
            let packet_rate = if_stats.packet_rate;
            let _timestamp = if_stats.timestamp.elapsed().as_secs();

            let str_rate = if rate > 1_000_000.0 {
                format!("{:.2} Mb/s", rate / 1_000_000.0)
            } else if rate > 1_000.0 {
                format!("{:.2} Kb/s", rate / 1_000.0)
            } else {
                format!("{:.2} b/s", rate)
            };
            let str_packet_rate = format!("{:.2} pps", packet_rate);
            info!(
                "{}",
                format!("Data rate: {}, Packet rate: {}", str_rate, str_packet_rate).green()
            );

            for (k, v) in if_stats.source_ips.iter() {
                info!("\tsource_ip: {} count: {}", k, v);
            }
            for (k, v) in if_stats.destination_ips.iter() {
                info!("\tdestination_ip: {} count: {}", k, v);
            }
            for (k, v) in if_stats.request_types.iter() {
                info!("\trequest_type:\t {:?} count: {:?}", k, v);
            }
        }
    }
    // }

    for handle in handles_vec {
        handle.join().unwrap();
        debug!("Thread closed");
    }
    debug!("All threads closed");

    Ok(())
}
