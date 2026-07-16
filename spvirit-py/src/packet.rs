//! Raw-frame `Packet` wrapper exposed via `Channel.read_packet` /
//! `Channel.read_until`.

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use spvirit_codec::epics_decode::{PvaPacket, command_name};

/// Owned PVA frame with header accessors and on-demand detail decoding.
#[pyclass(name = "Packet", module = "spvirit.lowlevel", frozen)]
pub struct PyPacket {
    bytes: Vec<u8>,
    magic: u8,
    version: u8,
    command: u8,
    flags_raw: u8,
    is_application: bool,
    is_control: bool,
    is_segmented: u8,
    is_client: bool,
    is_server: bool,
    is_msb: bool,
    payload_length: u32,
}

impl PyPacket {
    pub(crate) fn raw(&self) -> &[u8] {
        &self.bytes
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        if bytes.len() >= 8 {
            let pkt = PvaPacket::new(&bytes);
            return Self {
                magic: pkt.header.magic,
                version: pkt.header.version,
                command: pkt.header.command,
                flags_raw: pkt.header.flags.raw,
                is_application: pkt.header.flags.is_application,
                is_control: pkt.header.flags.is_control,
                is_segmented: pkt.header.flags.is_segmented,
                is_client: pkt.header.flags.is_client,
                is_server: pkt.header.flags.is_server,
                is_msb: pkt.header.flags.is_msb,
                payload_length: pkt.header.payload_length,
                bytes,
            };
        }
        Self {
            magic: 0,
            version: 0,
            command: 0,
            flags_raw: 0,
            is_application: false,
            is_control: false,
            is_segmented: 0,
            is_client: false,
            is_server: false,
            is_msb: false,
            payload_length: 0,
            bytes,
        }
    }
}

#[pymethods]
impl PyPacket {
    /// Header magic byte (0xCA for a valid PVA frame).
    #[getter]
    fn magic(&self) -> u8 {
        self.magic
    }
    /// Protocol version byte from the header.
    #[getter]
    fn version(&self) -> u8 {
        self.version
    }
    /// Command byte from the header.
    #[getter]
    fn command(&self) -> u8 {
        self.command
    }
    /// Human-readable name of the command byte.
    #[getter]
    fn command_name(&self) -> &'static str {
        command_name(self.command)
    }
    /// Raw header flags byte.
    #[getter]
    fn flags(&self) -> u8 {
        self.flags_raw
    }
    /// True if this is an application message.
    #[getter]
    fn is_application(&self) -> bool {
        self.is_application
    }
    /// True if this is a control message.
    #[getter]
    fn is_control(&self) -> bool {
        self.is_control
    }
    /// Segmentation flag bits as an int; 0 means unsegmented.
    #[getter]
    fn is_segmented(&self) -> u8 {
        self.is_segmented
    }
    /// True if the message was sent by a client.
    #[getter]
    fn is_client(&self) -> bool {
        self.is_client
    }
    /// True if the message was sent by a server.
    #[getter]
    fn is_server(&self) -> bool {
        self.is_server
    }
    /// True if the payload is big-endian (MSB flag set).
    #[getter]
    fn is_msb(&self) -> bool {
        self.is_msb
    }
    /// Payload length in bytes as declared in the header.
    #[getter]
    fn payload_length(&self) -> u32 {
        self.payload_length
    }

    /// Complete frame bytes (header + payload).
    #[getter]
    fn bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.bytes)
    }

    /// Payload bytes (everything after the 8-byte header).
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        let start = 8.min(self.bytes.len());
        PyBytes::new(py, &self.bytes[start..])
    }

    fn __len__(&self) -> usize {
        self.bytes.len()
    }

    /// Decode command-specific payload details.  Delegates to the
    /// standalone codec module.
    fn details<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let mut pkt = PvaPacket::new(&self.bytes);
        let d = PyDict::new(py);
        if let Some(cmd) = pkt.decode_payload() {
            crate::codec::fill_command_details(py, &cmd, &d)?;
        }
        Ok(d)
    }

    /// Alias for the free `spvirit.codec.decode_packet` function
    /// applied to this packet's bytes.
    fn decode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        crate::codec::decode_packet(py, &self.bytes)
    }

    fn __repr__(&self) -> String {
        format!(
            "Packet(command={} ({}), flags=0x{:02x}, payload_length={})",
            self.command,
            self.command_name(),
            self.flags_raw,
            self.payload_length
        )
    }
}
