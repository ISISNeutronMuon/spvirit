use spvirit_tools::scry::common::interfaces::{default_interface_index, find_interface_index, InterfaceInfo, InterfaceSelection};
use spvirit_tools::scry::common::stats::IfStats;
use spvirit_tools::scry::ui::tui::main_funcs;
use spvirit_tools::scry::npcap;
use spvirit_tools::scry::utils::common_version_print::version_header;
use spvirit_tools::scry::utils::packet_processor::process_packets;

use argparse::{ArgumentParser, Store, StoreFalse, StoreTrue};
use std::sync::{Arc, Mutex};
use std::thread;
// use colored::Colorize;
use colored::*;
use tracing::{debug, info, Level};

/// Where TUI-mode logs go, since stdout belongs to ratatui.
const TUI_LOG_FILE: &str = "spvirit-scry.log";

fn list_interfaces() -> Vec<InterfaceInfo> {
    match pcap::Device::list() {
        Ok(devices) => devices
            .into_iter()
            .map(|dev| InterfaceInfo {
                name: dev.name,
                description: dev.desc.unwrap_or_default(),
            })
            .collect(),
        Err(err) => {
            eprintln!("Failed to list interfaces: {}", err);
            Vec::new()
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut verbose = true;
    let mut show_payload = false;
    let mut exclude_cmds_str = String::new();
    let mut exclude_cmds: Vec<String> = Vec::new();
    let mut device = String::new();
    let mut integration_period = 1;
    let mut tui_mode = false;
    let mut debug_mode = false;
    let mut resolve = false;
    let mut buffer_size: usize = 5000;
    let mut version = false;
    // A day, not five minutes. Introspection is only learned by witnessing a
    // MONITOR INIT, so the tracker earns its coverage by being left running;
    // a short TTL discards channels faster than they can be re-witnessed.
    // What used to make a long TTL unsafe -- unbounded growth -- is now
    // bounded by --pva-max-memory-mb instead.
    let mut pva_ttl_secs: u64 = 86_400;
    let mut pva_max_channels: usize = 40_000;
    let mut pva_max_memory_mb: usize = 1024;
    let mut tracked_only = false;
    let mut tui_tick_ms: u64 = 100;
    let mut pva_max_update_rate: usize = 10_000;
    let mut fp_enabled = false;
    let mut fp_window_secs: u64 = 900;
    let mut fp_min_samples: u64 = 30;
    let mut fp_reservoir: usize = 200;
    let mut fp_slots: usize = 1;
    let mut fp_max_channels: usize = 5000;
    let mut fp_d1 = false;
    let mut fp_suppress_secs: u64 = 0;

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
        ap.refer(&mut exclude_cmds_str).add_option(
            &["-x", "--exclude-cmds"],
            Store,
            "Comma-separated PVA commands to exclude from display (e.g. PUT,MONITOR)",
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
        ap.refer(&mut version)
            .add_option(&["-v", "--version"], StoreTrue, "Show version and exit");
        ap.refer(&mut pva_ttl_secs).add_option(
            &["--pva-ttl"],
            Store,
            "PVA channel state TTL in seconds (default: 86400)",
        );
        ap.refer(&mut pva_max_memory_mb).add_option(
            &["--pva-max-memory-mb"],
            Store,
            "Ceiling on PVA tracker state in MiB, 0 to disable (default: 1024)",
        );
        ap.refer(&mut pva_max_channels).add_option(
            &["--pva-max-channels"],
            Store,
            "Maximum PVA channels to track per connection (default: 40000)",
        );
        ap.refer(&mut tracked_only).add_option(
            &["--tracked-only"],
            StoreTrue,
            "Only show packets from tracked connections (where PV names are known)",
        );
        ap.refer(&mut tui_tick_ms).add_option(
            &["--tui-tick-ms"],
            Store,
            "TUI refresh interval in milliseconds (default: 100)",
        );
        ap.refer(&mut pva_max_update_rate).add_option(
            &["--pva-max-update-rate"],
            Store,
            "Maximum update timestamps kept for rate calculation (default: 10000)",
        );
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

    // parse exclude list into uppercased tokens
    if !exclude_cmds_str.trim().is_empty() {
        exclude_cmds = exclude_cmds_str
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
    }

    if version {
        version_header();
        std::process::exit(0);
    }
    // Before any pcap call: explains a missing Npcap/libpcap instead of
    // failing in the loader (wpcap.dll is delay-loaded on Windows).
    npcap::exit_unless_capture_runtime("spvirit-scry");

    let if_stats = Arc::new(Mutex::new(IfStats::new()));
    if resolve {
        let mut if_stats = if_stats.lock().unwrap();
        if_stats.enable_resolve();
    }
    if pva_max_memory_mb != 0 && pva_max_memory_mb < 16 {
        eprintln!("PVA memory ceiling must be 0 (disabled) or at least 16 MiB");
        std::process::exit(1);
    }
    if !(100..=50_000).contains(&buffer_size) {
        eprintln!("Buffer size must be between 100 and 50000");
        std::process::exit(1);
    } else {
        let mut if_stats = if_stats.lock().unwrap();
        if_stats.set_buffer_size(buffer_size);
    }

    let interfaces = list_interfaces();
    if interfaces.is_empty() {
        eprintln!("No capture interfaces found");
        std::process::exit(1);
    }

    let requested_device = device.trim().to_string();
    let selected_index = if requested_device.is_empty() {
        default_interface_index(&interfaces)
    } else {
        find_interface_index(&interfaces, &requested_device)
            .unwrap_or_else(|| default_interface_index(&interfaces))
    };
    let selected_name = interfaces
        .get(selected_index)
        .map(|iface| iface.name.clone())
        .unwrap_or_else(|| "".to_string());

    if !requested_device.is_empty() && requested_device != selected_name {
        eprintln!(
            "Requested interface '{}' not found, using '{}' instead",
            requested_device, selected_name
        );
    }

    let interface_selection = Arc::new(InterfaceSelection::new(interfaces, selected_index));
    let capture_device = selected_name;

    if verbose {
        let level = if debug_mode { Level::DEBUG } else { Level::INFO };
        if tui_mode {
            // The TUI owns the terminal. Anything written to stdout or stderr
            // while it is up scrolls the alternate screen out from under
            // ratatui, and its diff renderer then only repaints the cells it
            // believes changed - leaving the rest of the previous frame on
            // screen as interleaved garbage. Log to a file instead so
            // --verbose and --debug stay useful without corrupting the display.
            match std::fs::File::create(TUI_LOG_FILE) {
                Ok(file) => {
                    tracing_subscriber::fmt::fmt()
                        .with_max_level(level)
                        .with_ansi(false)
                        .with_writer(move || -> Box<dyn std::io::Write> {
                            match file.try_clone() {
                                Ok(handle) => Box::new(handle),
                                Err(_) => Box::new(std::io::sink()),
                            }
                        })
                        .init();
                }
                Err(err) => {
                    // Nothing is on screen yet, so this is safe to print.
                    eprintln!("Could not open {}, logging disabled: {}", TUI_LOG_FILE, err);
                }
            }
        } else {
            tracing_subscriber::fmt::fmt().with_max_level(level).init();
            info!("Starting packet capture");
            info!("{}", format!("Using device {}", capture_device).green());
        }
    }

    let mut handles_vec = vec![];

    let mut if_stats_clone = Arc::clone(&if_stats);
    let interface_selection_capture = Arc::clone(&interface_selection);
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
            &capture_device,
            integration_period,
            &mut if_stats_clone,
            exclude_cmds,
            pva_ttl_secs,
            pva_max_channels,
            pva_max_memory_mb,
            tracked_only,
            pva_max_update_rate,
            Some(interface_selection_capture),
            fingerprint_options,
        );
    });
    handles_vec.push(handle_scan);
    debug!("Packet processing thread started");

    if tui_mode {
        debug!("Starting TUI thread");
        let mut if_stats_clone_tui = Arc::clone(&if_stats);
        let interface_selection_tui = Arc::clone(&interface_selection);
        let handle_tui = thread::spawn(move || {
            let _res = main_funcs(&mut if_stats_clone_tui, tui_tick_ms, Some(interface_selection_tui));
        });
        handles_vec.push(handle_tui);
        debug!("TUI thread started");
    }

    if verbose && !tui_mode {
        loop {
            thread::sleep(std::time::Duration::from_secs(1));

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
    }

    for handle in handles_vec {
        handle.join().unwrap();
        debug!("Thread closed");
    }
    debug!("All threads closed");

    Ok(())
}
