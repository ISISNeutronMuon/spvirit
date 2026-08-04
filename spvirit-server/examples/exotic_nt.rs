use std::collections::HashMap;

use spvirit_server::{
    DbCommonState, OutputMode, PvaServer, RecordData, RecordInstance, RecordType,
};
use spvirit_types::{
    NdCodec, NdDimension, NtEnum, NtNdArray, NtPayload, NtTable, NtTableColumn, PvValue,
    ScalarArrayValue, ScalarValue,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Build using the high-level builder for NtEnum and Generic ────────

    // ANCHOR: enums
    let server = PvaServer::builder()
        .mbbi(
            "SIM:STATE",
            vec![
                "Idle".to_string(),
                "Running".to_string(),
                "Error".to_string(),
            ],
            0,
        )
        .mbbo(
            "SIM:MODE",
            vec![
                "Standby".to_string(),
                "Acquire".to_string(),
                "Calibrate".to_string(),
            ],
            0,
        )
        .generic(
            "SIM:POSITION",
            "demo:custom/Position:1.0",
            vec![
                ("x".to_string(), PvValue::Scalar(ScalarValue::F64(0.0))),
                ("y".to_string(), PvValue::Scalar(ScalarValue::F64(0.0))),
                (
                    "label".to_string(),
                    PvValue::Scalar(ScalarValue::Str("origin".to_string())),
                ),
            ],
        )
        .build();
    // ANCHOR_END: enums

    let store = server.store().clone();

    // ── Manually insert exotic NT types that don't have builder methods ──

    let table = make_table_record();
    let image = make_ndarray_record();

    store.insert("SIM:TBL".into(), table).await;
    store.insert("SIM:IMG".into(), image).await;

    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            // ── NtEnum — cycles through states ──────────────────────
            let mode_idx = (tick % 3) as i32;
            let mode_nt = NtEnum::new(
                mode_idx,
                vec![
                    "Standby".to_string(),
                    "Acquire".to_string(),
                    "Calibrate".to_string(),
                ],
            );
            store.put_nt("SIM:MODE", NtPayload::Enum(mode_nt)).await;

            let state_idx = (tick % 3) as i32;
            let state_nt = NtEnum::new(
                state_idx,
                vec![
                    "Idle".to_string(),
                    "Running".to_string(),
                    "Error".to_string(),
                ],
            );
            store.put_nt("SIM:STATE", NtPayload::Enum(state_nt)).await;

            // ── Generic structure — updating position ────────────────
            let x = tick as f64 * 0.1;
            let y = (tick as f64 * 0.3).sin();
            store
                .put_nt(
                    "SIM:POSITION",
                    NtPayload::Generic {
                        struct_id: "demo:custom/Position:1.0".to_string(),
                        fields: vec![
                            ("x".to_string(), PvValue::Scalar(ScalarValue::F64(x))),
                            ("y".to_string(), PvValue::Scalar(ScalarValue::F64(y))),
                            (
                                "label".to_string(),
                                PvValue::Scalar(ScalarValue::Str(format!("tick-{tick}"))),
                            ),
                        ],
                    },
                )
                .await;

            // ── NTTable — two columns that change over time ──────────
            let xs = (0..8).map(|i| i as f64).collect::<Vec<_>>();
            let ys = xs
                .iter()
                .map(|v| (v * 0.7 + tick as f64 * 0.15).sin())
                .collect::<Vec<_>>();
            // ANCHOR: table
            let table_nt = NtTable {
                labels: vec!["X".to_string(), "Y".to_string()],
                columns: vec![
                    NtTableColumn {
                        name: "x".to_string(),
                        values: ScalarArrayValue::F64(xs),
                    },
                    NtTableColumn {
                        name: "y".to_string(),
                        values: ScalarArrayValue::F64(ys),
                    },
                ],
                descriptor: Some("SIM table demo".to_string()),
                alarm: None,
                time_stamp: None,
            };
            store.put_nt("SIM:TBL", NtPayload::Table(table_nt)).await;
            // ANCHOR_END: table

            // ── NTNDArray — tiny 4x4 image ───────────────────────────
            let pixels = (0..16)
                .map(|i| (((i as i32 + tick as i32) % 16) * 16) as u8)
                .collect::<Vec<_>>();
            // ANCHOR: ndarray
            let ndarray_nt = NtNdArray {
                value: ScalarArrayValue::U8(pixels),
                codec: NdCodec {
                    name: "none".to_string(),
                    parameters: HashMap::new(),
                },
                compressed_size: 16,
                uncompressed_size: 16,
                dimension: vec![
                    NdDimension {
                        size: 4,
                        offset: 0,
                        full_size: 4,
                        binning: 1,
                        reverse: false,
                    },
                    NdDimension {
                        size: 4,
                        offset: 0,
                        full_size: 4,
                        binning: 1,
                        reverse: false,
                    },
                ],
                unique_id: tick as i32,
                data_time_stamp: Default::default(),
                attribute: vec![],
                descriptor: Some("SIM 4x4 image".to_string()),
                alarm: None,
                time_stamp: None,
                display: None,
            };
            store
                .put_nt("SIM:IMG", NtPayload::NdArray(ndarray_nt))
                .await;
            // ANCHOR_END: ndarray

            // ── Print snapshots ──────────────────────────────────────
            if let Some(snapshot) = store.get_nt("SIM:MODE").await {
                println!("SIM:MODE => {snapshot:?}");
            }
            if let Some(snapshot) = store.get_nt("SIM:STATE").await {
                println!("SIM:STATE => {snapshot:?}");
            }
            if let Some(snapshot) = store.get_nt("SIM:POSITION").await {
                println!("SIM:POSITION => {snapshot:?}");
            }

            tick += 1;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    server.run().await
}

fn make_table_record() -> RecordInstance {
    RecordInstance {
        name: "SIM:TBL".to_string(),
        record_type: RecordType::NtTable,
        common: DbCommonState::default(),
        data: RecordData::NtTable {
            nt: NtTable {
                labels: vec!["X".to_string(), "Y".to_string()],
                columns: vec![
                    NtTableColumn {
                        name: "x".to_string(),
                        values: ScalarArrayValue::F64(vec![0.0; 8]),
                    },
                    NtTableColumn {
                        name: "y".to_string(),
                        values: ScalarArrayValue::F64(vec![0.0; 8]),
                    },
                ],
                descriptor: Some("SIM table demo".to_string()),
                alarm: None,
                time_stamp: None,
            },
            inp: None,
            out: None,
            omsl: OutputMode::Supervisory,
        },
        raw_fields: HashMap::new(),
    }
}

fn make_ndarray_record() -> RecordInstance {
    RecordInstance {
        name: "SIM:IMG".to_string(),
        record_type: RecordType::NtNdArray,
        common: DbCommonState::default(),
        data: RecordData::NtNdArray {
            nt: NtNdArray {
                value: ScalarArrayValue::U8(vec![0; 16]),
                codec: NdCodec {
                    name: "none".to_string(),
                    parameters: HashMap::new(),
                },
                compressed_size: 16,
                uncompressed_size: 16,
                dimension: vec![
                    NdDimension {
                        size: 4,
                        offset: 0,
                        full_size: 4,
                        binning: 1,
                        reverse: false,
                    },
                    NdDimension {
                        size: 4,
                        offset: 0,
                        full_size: 4,
                        binning: 1,
                        reverse: false,
                    },
                ],
                unique_id: 0,
                data_time_stamp: Default::default(),
                attribute: vec![],
                descriptor: Some("SIM 4x4 image".to_string()),
                alarm: None,
                time_stamp: None,
                display: None,
            },
            inp: None,
            out: None,
            omsl: OutputMode::Supervisory,
        },
        raw_fields: HashMap::new(),
    }
}
