#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use spvirit_codec::epics_decode::{DecodeMode, PvaPacketCommand, PvaStatus};
use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};
use spvirit_tools::spvirit_client::put_encode::encode_put_payload;

use crate::protocol::frame_harness::{TestServer, TestSession};

const PV_REQUEST_EMPTY: [u8; 6] = [0xfd, 0x02, 0x00, 0x80, 0x00, 0x00];

pub struct ScenarioSession {
    pub session: TestSession,
    sid_by_pv: HashMap<String, u32>,
    next_cid: u32,
    next_ioid: u32,
}

impl ScenarioSession {
    pub async fn connect(server: &TestServer) -> Result<Self, String> {
        let mut session = server.connect().await?;
        session.handshake().await?;
        Ok(Self {
            session,
            sid_by_pv: HashMap::new(),
            next_cid: 100,
            next_ioid: 10,
        })
    }

    pub async fn ensure_channel(&mut self, pv: &str) -> Result<u32, String> {
        if let Some(sid) = self.sid_by_pv.get(pv).copied() {
            return Ok(sid);
        }
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        let sid = self.session.create_channel(cid, pv).await?;
        self.sid_by_pv.insert(pv.to_string(), sid);
        Ok(sid)
    }

    fn next_ioid(&mut self) -> u32 {
        let ioid = self.next_ioid;
        self.next_ioid = self.next_ioid.wrapping_add(1);
        ioid
    }

    pub async fn get_scalar_value(&mut self, pv: &str) -> Result<DecodedValue, String> {
        let sid = self.ensure_channel(pv).await?;
        let ioid = self.next_ioid();

        self.session
            .send_client_op(10, sid, ioid, 0x08, &PV_REQUEST_EMPTY)
            .await?;
        let init_cmd = self
            .session
            .read_until(Duration::from_secs(2), |cmd| {
                matches!(cmd, PvaPacketCommand::Op(op) if op.command == 10 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
            })
            .await?;

        let desc = match init_cmd {
            PvaPacketCommand::Op(op) => op
                .introspection
                .ok_or_else(|| "GET init missing introspection".to_string())?,
            _ => return Err("unexpected GET init response".to_string()),
        };

        self.session
            .send_client_op(10, sid, ioid, 0x00, &[])
            .await?;
        let data_cmd = self
            .session
            .read_until(Duration::from_secs(2), |cmd| {
                matches!(cmd, PvaPacketCommand::Op(op) if op.command == 10 && op.ioid == ioid && op.subcmd == 0x00)
            })
            .await?;

        match data_cmd {
            PvaPacketCommand::Op(mut op) => {
                op.decode_with_field_desc(&desc, self.session.is_be, DecodeMode::Strict)
                    .map_err(|e| format!("GET data decode failed: {e}"))?;
                op.decoded_value
                    .ok_or_else(|| "GET data decode produced no value".to_string())
            }
            _ => Err("unexpected GET data response".to_string()),
        }
    }

    pub async fn put_scalar_number(&mut self, pv: &str, value: f64) -> Result<(), String> {
        let sid = self.ensure_channel(pv).await?;
        let ioid = self.next_ioid();

        self.session
            .send_client_op(11, sid, ioid, 0x08, &PV_REQUEST_EMPTY)
            .await?;
        let init_cmd = self
            .session
            .read_until(Duration::from_secs(2), |cmd| {
                matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
            })
            .await?;

        let desc = extract_desc_or_status(init_cmd)?;
        let payload = encode_put_payload(&desc, &json!(value), self.session.is_be)?;
        self.session
            .send_client_op(11, sid, ioid, 0x00, &payload)
            .await?;

        let put_cmd = self
            .session
            .read_until(Duration::from_secs(2), |cmd| {
                matches!(cmd, PvaPacketCommand::Op(op) if op.command == 11 && op.ioid == ioid)
            })
            .await?;

        match put_cmd {
            PvaPacketCommand::Op(op) => {
                if let Some(status) = op.status {
                    if status.code != 0xFF {
                        return Err(format_status("PUT failed", &status));
                    }
                }
                Ok(())
            }
            _ => Err("unexpected PUT response".to_string()),
        }
    }

    pub async fn monitor_init_start_stop(&mut self, pv: &str) -> Result<(), String> {
        let sid = self.ensure_channel(pv).await?;
        let ioid = self.next_ioid();

        self.session
            .send_client_op(13, sid, ioid, 0x08, &PV_REQUEST_EMPTY)
            .await?;
        let _init = self
            .session
            .read_until(Duration::from_secs(2), |cmd| {
                matches!(cmd, PvaPacketCommand::Op(op) if op.command == 13 && op.ioid == ioid && (op.subcmd & 0x08) != 0)
            })
            .await?;

        self.session
            .send_client_op(13, sid, ioid, 0x44, &[])
            .await?;
        let _first_data = self
            .session
            .read_until(Duration::from_secs(2), |cmd| {
                matches!(cmd, PvaPacketCommand::Op(op) if op.command == 13 && op.ioid == ioid && op.subcmd == 0x00)
            })
            .await?;

        self.session
            .send_client_op(13, sid, ioid, 0x04, &[])
            .await?;
        self.session
            .send_client_op(13, sid, ioid, 0x10, &[])
            .await?;
        let _end = self
            .session
            .read_until(Duration::from_secs(2), |cmd| {
                matches!(cmd, PvaPacketCommand::Op(op) if op.command == 13 && op.ioid == ioid && (op.subcmd & 0x10) != 0)
            })
            .await?;

        Ok(())
    }
}

fn extract_desc_or_status(init_cmd: PvaPacketCommand) -> Result<StructureDesc, String> {
    match init_cmd {
        PvaPacketCommand::Op(op) => {
            if let Some(status) = op.status {
                if status.code != 0xFF {
                    return Err(format_status("PUT init failed", &status));
                }
            }
            op.introspection
                .ok_or_else(|| "PUT init missing introspection".to_string())
        }
        _ => Err("unexpected PUT init response".to_string()),
    }
}

fn format_status(prefix: &str, status: &PvaStatus) -> String {
    format!(
        "{} code={} message={}",
        prefix,
        status.code,
        status.message.clone().unwrap_or_default()
    )
}
