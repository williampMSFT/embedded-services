//! HID sevices
//! See spec at <http://msdn.microsoft.com/en-us/library/windows/hardware/hh852380.aspx>
use core::convert::Infallible;

use embassy_sync::signal::Signal;

use crate::buffer::SharedRef;
use crate::comms::{self, Endpoint, EndpointID, External, Internal, MailboxDelegate};
use crate::{GlobalRawMutex, IntrusiveList, Node, NodeContainer, error, intrusive_list};

mod command;
pub use command::*;

// TODO williamp revert changes to this file before checkin. A lot of this stuff is HID-I2C specific and doesn't apply to other transports, so we'll need to
//               break those pieces out into transport-specific modules rather than having them be in embedded-services.  Embedded-services should only contain
//               transport-agnostic code, and that's going to go in the relay module.

/// HID descriptor length
pub const DESCRIPTOR_LEN: usize = 30;

/// Data for [`Error::InvalidSize`]
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct InvalidSizeError {
    /// Expected size
    pub expected: usize,
    /// Actual size
    pub actual: usize,
}

/// HID errors
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// Invalid data
    InvalidData,
    /// Invalid size: expected and actual sizes
    InvalidSize(InvalidSizeError),
    /// Invalid register address
    InvalidRegisterAddress,
    /// Invalid device
    InvalidDevice,
    /// Invalid command
    InvalidCommand,
    /// Command requires a report ID
    RequiresReportId,
    /// Command requires data
    RequiresData,
    /// Invalid report type for command
    InvalidReportType,
    /// Invalid report frequency
    InvalidReportFreq,
    /// Error from transport service
    Transport,
    /// Timeout
    Timeout,
    /// Errors from serialization/deserialization
    Serialize,
}


impl DeviceContainer for Device {
    fn get_hid_device(&self) -> &Device {
        self
    }
}

impl MailboxDelegate for Device {
    fn receive(&self, message: &comms::Message) -> Result<(), comms::MailboxDelegateError> {
        let message = message
            .data
            .get::<Message>()
            .ok_or(comms::MailboxDelegateError::MessageNotFound)?;

        match message.data {
            MessageData::Request(ref request) => {
                self.request.signal(request.clone());
                Ok(())
            }
            _ if message.id != self.id => Err(comms::MailboxDelegateError::InvalidId),
            _ => Err(comms::MailboxDelegateError::InvalidData),
        }
    }
}

/// HID device ID
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct DeviceId(pub u8);

/// HID report ID
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct ReportId(pub u8);

/// Host to device messages
#[derive(Clone)]
pub enum Request<'a> {
    /// HID descriptor request
    Descriptor,
    /// Report descriptor request
    ReportDescriptor,
    /// Input report request
    InputReport,
    /// Output report request
    OutputReport(Option<ReportId>, SharedRef<'a, u8>),
    /// Command
    Command(Command<'a>),
}

/// Device to host messages
#[derive(Clone)]
pub enum Response<'a> {
    /// HID descriptor response
    Descriptor(SharedRef<'a, u8>),
    /// Report descriptor response
    ReportDescriptor(SharedRef<'a, u8>),
    /// Input report
    InputReport(SharedRef<'a, u8>),
    /// Feature report
    FeatureReport(SharedRef<'a, u8>),
    /// General command responses
    Command(CommandResponse),
}

/// HID message data
#[derive(Clone)]
pub enum MessageData<'a> {
    /// HID read/write request to register
    Request(Request<'a>),
    /// HID response, some commands may not produce a response
    Response(Option<Response<'a>>),
}

/// Top-level struct for HID communication
#[derive(Clone)]
pub struct Message<'a> {
    /// Target/originating device ID
    pub id: DeviceId,
    /// Message contents
    pub data: MessageData<'a>,
}


/// Convenience function to send a request to a HID device
pub async fn send_request(tp: &Endpoint, to: DeviceId, request: Request<'static>) -> Result<(), Infallible> {
    let message = Message {
        id: to,
        data: MessageData::Request(request),
    };
    tp.send(EndpointID::Internal(Internal::Hid), &message).await
}

