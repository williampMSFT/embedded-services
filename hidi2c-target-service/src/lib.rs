//! This crate contains a service that behaves as a HID target/slave device over I2C.

#![no_std]
// TODO rm
#![warn(warnings)]

use core::marker::PhantomData;
use embassy_time::{Duration, with_timeout};
use embedded_mcu_hal::i2c::target::ReadStatus;
use embedded_mcu_hal::i2c::target::Request;
use embedded_mcu_hal::i2c::target::WriteStatus;
use embedded_mcu_hal::i2c::target::asynch::I2c as I2cTargetAsync;
use embedded_services::relay::hid;
use embedded_services::relay::hid::{GetHidReportType, HidError, HidReport, ReportReceiver, SetHidReport};
use embedded_services::{error, info, trace, warn};
use generic_array::ArrayLength;
use typenum::Max;
use zerocopy::IntoBytes;

mod device_descriptor;
use device_descriptor::DeviceDescriptor;
pub use device_descriptor::{HardwareVersionInfo, ProductId, VendorId, VersionId};

mod attn_pin_handler;
use attn_pin_handler::AttnPinHandler;

mod error;
use error::*;

mod sealed {
    /// Traits that derive from this one are not allowed to be implemented by 3rd party code.
    /// To have those traits implemented, you should satisfy the requirement for their blanket
    /// implementation instead.
    pub trait Sealed {}
}

/// Extension of [`hid::HidDevice`] that computes the max of feature/input and
/// feature/output report sizes as associated types so we can correctly size our
/// send/recv buffers.
///
/// Any type that implements `hid::HidDevice` will automatically implement this trait -
/// there's no type that satisfies ArrayLength that doesn't also satisfy these trait bounds.
/// However, due to some limitations in the Rust type system, we have to spell it out.
///
/// We should be able to get rid of all of this once generic_const_exprs stabilises, since
/// then we don't need any of these trait bounds and can just do the math where we declare
/// the buffers. At that point, we should also consider moving from ArrayLength to just
/// const usizes since ArraySize is just a workaround for the lack of generic const expressions.
///
pub trait ConstrainedHidDevice: hid::HidDevice + sealed::Sealed {
    /// `max(FeatureReportMaxSize, InputReportMaxSize)`.
    type MaxInputOrFeatureSize: ArrayLength;
    /// `max(FeatureReportMaxSize, OutputReportMaxSize)`.
    type MaxOutputOrFeatureSize: ArrayLength;
}

impl<T> ConstrainedHidDevice for T
where
    T: hid::HidDevice,
    T::FeatureReportMaxSize: Max<T::InputReportMaxSize>,
    T::FeatureReportMaxSize: Max<T::OutputReportMaxSize>,
    <T::FeatureReportMaxSize as Max<T::InputReportMaxSize>>::Output: ArrayLength,
    <T::FeatureReportMaxSize as Max<T::OutputReportMaxSize>>::Output: ArrayLength,
{
    type MaxInputOrFeatureSize = <T::FeatureReportMaxSize as Max<T::InputReportMaxSize>>::Output;
    type MaxOutputOrFeatureSize = <T::FeatureReportMaxSize as Max<T::OutputReportMaxSize>>::Output;
}

impl<T> sealed::Sealed for T where T: ConstrainedHidDevice {}

/// HID-I2C register addresses as specified in section 5.1 of the HID-I2C spec.
/// These specific values are our convention, not from the HID-I2C spec, but section 4.2 indicates
/// that all HID-I2C devices must have their own I2C bus address so there's no way to share a single
/// I2C address by leveraging different register addresses on the same I2C address.
///
#[repr(u16)]
#[derive(num_enum::TryFromPrimitive, num_enum::IntoPrimitive, Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum HidI2cRegister {
    /// HID descriptor register - see section 5.1
    /// NOTE: Per the HID-I2C spec, when using ACPI for enumeration, this value needs to be put in the _DSM.
    DeviceDescriptor = 0x01,

    /// HID report descriptor register - see section 5.2
    ReportDescriptor = 0x02,

    /// Input report register - see section 6.1
    Input = 0x03,

    /// Output report register - see section 6.2
    Output = 0x04,

    /// Command register - see section 7.1.1
    Command = 0x05,

    /// Data register - see section 7.1.2
    Data = 0x06,
}

/// HID-I2C Command Opcode as specified in section 7.1.1 of the HID-I2C spec
#[repr(u8)]
#[derive(num_enum::TryFromPrimitive, num_enum::IntoPrimitive, Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum Opcode {
    // Reserved: 0x00
    Reset = 0x01,
    GetReport = 0x02,
    SetReport = 0x03,

    // The following commands are in the i2c spec but are listed as optional and
    // not sent by modern hosts, and therefore we do not implement them:
    //
    // GetIdle = 0x04,
    // SetIdle = 0x05,
    // GetProtocol = 0x06,
    // SetProtocol = 0x07,
    SetPower = 0x08,
    // Reserved: 0x09 - 0x0D
    // VendorReserved = 0x0E, // TODO plumb through a vendor pipe
    // Reserved: 0x0F
}

/// I2C wire format representation for HID power states.
#[repr(u8)]
#[derive(num_enum::TryFromPrimitive, num_enum::IntoPrimitive, Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum I2cPowerState {
    On = 0x00,
    Sleep = 0x01,
}

impl From<I2cPowerState> for hid::HidDevicePowerState {
    fn from(value: I2cPowerState) -> Self {
        match value {
            I2cPowerState::On => hid::HidDevicePowerState::On,
            I2cPowerState::Sleep => hid::HidDevicePowerState::Sleep,
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum HidI2cReportType {
    Input,
    Output,
    Feature,
}

// TODO can this be a tryfrom impl?
impl HidI2cReportType {
    fn to_get_type(&self) -> Option<GetHidReportType> {
        match self {
            HidI2cReportType::Input => Some(GetHidReportType::Input),
            HidI2cReportType::Feature => Some(GetHidReportType::Feature),
            HidI2cReportType::Output => None,
        }
    }
}

struct HidI2cReportCommandHeader {
    /// The report type that this command is targeting
    report_type: HidI2cReportType,

    /// The report ID that this command is targeting, or None if another byte must be read to get the full report ID (happens if report ID is >= 0xF)
    report_id: Option<embedded_services::relay::hid::ReportId>,
}

impl HidI2cReportCommandHeader {
    fn try_from_command_byte(command_byte: u8) -> Result<Self, ProtocolError> {
        const HID_I2C_REPORT_TYPE_OFFSET: u8 = 4;
        let report_type = match command_byte >> HID_I2C_REPORT_TYPE_OFFSET {
            0x01 => HidI2cReportType::Input,
            0x02 => HidI2cReportType::Output,
            0x03 => HidI2cReportType::Feature,
            _ => return Err(ProtocolError::InvalidReportType),
        };
        let report_id = if command_byte & 0x0F == 0x0F {
            None
        } else {
            Some(embedded_services::relay::hid::ReportId(command_byte & 0x0F))
        };
        Ok(Self { report_type, report_id })
    }
}

/// Memory required for the HID-I2C target service.
pub struct Resources<Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice> {
    runner_resources: Option<RunnerResources<Bus, AttnPin, HidDevice>>,
    service_resources: Option<ServiceResources>,
}

impl<Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice> Default
    for Resources<Bus, AttnPin, HidDevice>
{
    fn default() -> Self {
        Self {
            runner_resources: None,
            service_resources: None,
        }
    }
}

/// Resources owned by the runner.  TODO should these just go in the runner itself?
struct RunnerResources<Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
{
    bus: Bus,
    attn_pin: AttnPinHandler<AttnPin>,
    hid_device: HidDevice,
    device_descriptor: DeviceDescriptor,

    // Buffer for receiving messages.
    write_buf: generic_array::GenericArray<u8, HidDevice::MaxOutputOrFeatureSize>,

    device_response_timeout: Duration,
    /// Timeout for data reads from the host.
    data_read_timeout: Duration,

    /// True if a reset has been triggered but not yet acknowledged by the host
    pending_reset: bool,
}

impl<Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
    RunnerResources<Bus, AttnPin, HidDevice>
{
    fn new(
        bus: Bus,
        attn_pin: AttnPin,
        hid_device: HidDevice,
        device_descriptor: DeviceDescriptor,
        device_response_timeout: Duration,
        data_read_timeout: Duration,
    ) -> Self {
        Self {
            bus,
            attn_pin: AttnPinHandler::new(attn_pin),
            hid_device,
            device_descriptor: device_descriptor,
            write_buf: generic_array::GenericArray::default(),
            device_response_timeout,
            data_read_timeout,
            pending_reset: false, // The host is responsible for explicitly resetting us at boot, so we start in a non-reset state
        }
    }
}

/// Resources used by the service
struct ServiceResources {
    reset_signal: embassy_sync::signal::Signal<embedded_services::GlobalRawMutex, ()>,
}

/// Service runner for the HID-I2C service. You must call run() on the runner to drive the service.
pub struct Runner<'hw, Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
{
    resources: &'hw mut RunnerResources<Bus, AttnPin, HidDevice>,
    reset_signal: &'hw embassy_sync::signal::Signal<embedded_services::GlobalRawMutex, ()>,
}

impl<
    'hw,
    Bus: I2cTargetAsync + 'hw,
    AttnPin: embedded_hal::digital::OutputPin + 'hw,
    HidDevice: ConstrainedHidDevice + 'hw,
> odp_service_common::runnable_service::ServiceRunner<'hw> for Runner<'hw, Bus, AttnPin, HidDevice>
{
    async fn run(mut self) -> embedded_services::Never {
        loop {
            let event = {
                // If we've raised the interrupt, we know it won't be dismissed again until it's serviced by the host reading
                // the input report, so we don't need to listen for another notification.
                let receiver = self.resources.hid_device.receiver();
                let input_report_ready_future = async {
                    if self.resources.attn_pin.asserted() {
                        core::future::pending().await
                    } else {
                        receiver.ready_to_receive().await
                    }
                };
                embassy_futures::select::select3(
                    self.resources.bus.listen(),
                    input_report_ready_future,
                    self.reset_signal.wait(),
                )
                .await
            };
            match event {
                embassy_futures::select::Either3::First(bus_request) => {
                    trace!("Processing request from host");
                    self.process_request(bus_request.expect("TODO handle error recovery"))
                        .await;
                }
                embassy_futures::select::Either3::Second(()) => {
                    trace!("Signalling host that we have an input report ready");
                    self.resources.attn_pin.assert_interrupt();
                }
                embassy_futures::select::Either3::Third(()) => {
                    trace!("Received reset request");
                    self.reset().await;
                }
            }
        }
    }
}

impl<
    'hw,
    Bus: I2cTargetAsync + 'hw,
    AttnPin: embedded_hal::digital::OutputPin + 'hw,
    HidDevice: ConstrainedHidDevice + 'hw,
> Runner<'hw, Bus, AttnPin, HidDevice>
{
    // TODO these are going to need to be tweaked when felipe's fix to the i2c trait goes in
    // TODO these are associated functions because `buffer` is often using a borrow on self (i.e. read_bus(self.bus, self.buffer), but this feels a bit awkward. figure out if there's a more ergonomic way to describe this pattern
    async fn read_bus(bus: &mut Bus, timeout: Duration, buffer: &mut [u8]) -> Result<usize, Error<Bus::Error>> {
        match with_timeout(timeout, bus.respond_to_write(buffer)).await {
            Err(_timeout_error) => {
                error!("Read request timeout");
                bus.recover().await.expect("TODO handle bus recovery error");
                Err(Error::Protocol(ProtocolError::Timeout))
            }
            Ok(write_status) => match write_status {
                Ok(WriteStatus::Stopped(bytes))
                | Ok(WriteStatus::Restarted(bytes))
                | Ok(WriteStatus::BufferFull(bytes)) => {
                    // TODO figure out how to not have to match this twice, I think it involves making the original write_status `format`
                    if let Ok(write_status) = write_status {
                        trace!("Host issued write command: {:?}", write_status);
                    }
                    Ok(bytes)
                }
                Err(e) => {
                    error!("Error during bus read"); // TODO figure out debug tracing bound
                    bus.recover().await.expect("TODO handle bus recovery error");
                    Err(Error::Bus(e))
                }
                _ => {
                    error!("Unexpected write status"); // TODO figure out debug tracing bound
                    bus.recover().await.expect("TODO handle bus recovery error");
                    Err(Error::Protocol(ProtocolError::InvalidData))
                }
            },
        }
    }

    /// Writes the specified bytes to the bus. If the host requests more bytes, pads with 0s until the host is satisfied.
    async fn write_bus(bus: &mut Bus, timeout: Duration, buffer: &[u8]) -> Result<(), Error<Bus::Error>> {
        let mut write_buffer = &buffer;
        const PADDING_BUFFER: &[u8] = &[0u8; 8];
        while Self::write_bus_unterminated(bus, timeout, write_buffer).await? {
            write_buffer = &PADDING_BUFFER;
            trace!("Emitting a padding byte");
        }
        Ok(())
    }

    /// Writes the specified bytes to the bus. If the host requests more bytes, returns true, otherwise false.
    async fn write_bus_unterminated(
        bus: &mut Bus,
        timeout: Duration,
        buffer: &[u8],
    ) -> Result<bool, Error<Bus::Error>> {
        match with_timeout(timeout, bus.respond_to_read(buffer)).await {
            Err(_timeout_error) => {
                error!("Write request timeout");
                bus.recover().await.expect("TODO handle bus recovery error");
                Err(Error::Protocol(ProtocolError::Timeout))
            }
            Ok(result) => result
                .map(|read_status| match read_status {
                    ReadStatus::NeedMore(_) => {
                        trace!("host requested more bytes than we provided");
                        true
                    }
                    _ => false,
                })
                .map_err(|e| Error::Bus(e)),
        }
    }

    /// Waits for the controller to command us over the bus, with timeout handling.
    async fn listen_bus(bus: &mut Bus, timeout: Duration) -> Result<Request, Error<Bus::Error>> {
        loop {
            let result = match with_timeout(timeout, bus.listen()).await {
                Err(_timeout_error) => {
                    error!("Listen request timeout");
                    bus.recover().await.expect("TODO handle bus recovery error");
                    Err(Error::Protocol(ProtocolError::Timeout))
                }
                Ok(result) => result.map_err(|_| Error::Protocol(ProtocolError::Timeout)),
            };

            if let Ok(Request::RepeatedStart(_a)) = result {
                continue;
            }

            return result;
        }
    }

    async fn process_request(&mut self, request: Request) {
        // TODO unlike the old trait where the address was fixed, this one can get multiple addresses?
        //      May need to have some way to split the bus resources across multiple hidi2c services and/or
        //      have a split between i2c and hid (but then there's some bleed with register operations?)
        //
        //      For now, assume that there's only one address on the bus and it's us. This will explode spectacularly
        //      if that's not the case - we may need some layer above this that filters on address and dispatches to
        //      different instances based on that
        //
        let result = match request {
            Request::Write(_address) => {
                trace!("HID-I2C: Processing register access");
                self.process_register_access().await
            }
            Request::Read(_address) => {
                trace!("HID-I2C: Host requested input report");
                self.reply_with_input_report().await
            }

            // TODO this is in line with what we were doing for the I2cCommand::Probe command in the old hid service, but it's not
            //      clear to me if that was correct or how the other enum variants map to that.
            //      Figure out if we need to handle any of the following:
            //          Request::RepeatedStart(prev_address) // Continue transaction - I think this is targeted at other masters on the bus?
            //          Request::Stop(address)               // End of transaction - I think this is targeted at other masters on the bus?
            //          Request::GeneralCall                 // I don't know what this is
            //          Request::SmbusAlert                  // I don't know what this is
            //
            _ => {
                info!("HID-I2C: Not handling command {:?}", request);
                return;
            }
        };

        match result {
            Ok(_) => {}
            Err(Error::Bus(bus_error)) => {
                error!("HID-I2C: Error during bus operation: {:?}", bus_error);
                // TODO what do?
            }
            Err(Error::Protocol(protocol_error)) => {
                error!("HID-I2C: Protocol error during bus operation: {:?}", protocol_error);
                // TODO what do?
            }
            Err(Error::Device(HidError::TriggerReset)) => {
                warn!("HID-I2C: HID device requested device-initiated reset");
                self.reset().await;
            }
        }
    }

    async fn process_register_access(&mut self) -> Result<(), Error<Bus::Error>> {
        let mut reg = [0u8; 2];
        Self::read_bus(&mut self.resources.bus, self.resources.data_read_timeout, &mut reg).await?;

        let register = HidI2cRegister::try_from(u16::from_le_bytes(reg))
            .map_err(|_| Error::Protocol(ProtocolError::InvalidRegisterAddress))?;

        info!("HID-I2C: Host requested to access register {:?}", register);
        match register {
            HidI2cRegister::DeviceDescriptor => {
                // TODO do we need to handle the case where the host decides to talk to someone else in the middle of talking to us?
                let request = Self::listen_bus(&mut self.resources.bus, self.resources.device_response_timeout).await?;
                match request {
                    Request::Read(_address) => {
                        trace!(
                            "Responding to request for device descriptor with {} bytes",
                            self.resources.device_descriptor.as_bytes().len()
                        );
                        Self::write_bus(
                            &mut self.resources.bus,
                            self.resources.device_response_timeout,
                            self.resources.device_descriptor.as_bytes(),
                        )
                        .await?;
                        trace!("Done responding to request for device descriptor"); // TODO rm
                        Ok(())
                    }
                    _ => {
                        error!(
                            "Expected read request after device descriptor register access: {:?}",
                            request
                        );
                        Err(Error::Protocol(ProtocolError::InvalidRegisterAddress))
                    }
                }
            }
            HidI2cRegister::ReportDescriptor => {
                match Self::listen_bus(&mut self.resources.bus, self.resources.device_response_timeout).await? {
                    // TODO do we need to handle the case where the host decides to talk to someone else in the middle of talking to us?
                    Request::Read(_address) => {
                        trace!("Responding to request for report descriptor");
                        Self::write_bus(
                            &mut self.resources.bus,
                            self.resources.device_response_timeout,
                            self.resources.hid_device.report_descriptor().as_bytes(),
                        )
                        .await
                        .expect("TODO handle write error");
                        Ok(())
                    }
                    _ => {
                        error!("Expected read request after report descriptor register access");
                        Err(Error::Protocol(ProtocolError::InvalidRegisterAddress))
                    }
                }
            }
            HidI2cRegister::Input => self.process_input_report_read().await,
            HidI2cRegister::Output => self.process_output_report_write().await,
            HidI2cRegister::Command => self.process_command().await,
            HidI2cRegister::Data => {
                error!(
                    "HID-I2C: Got read to Data register without a preceding write to the Command register; this is unexpected and may indicate a bug in the service."
                );
                Err(Error::Protocol(ProtocolError::InvalidRegisterAddress))
            }
        }
    }

    /// Process a request for an input report that we've asserted an interrupt for (i.e. not a request for a specific input report ID)
    async fn process_input_report_read(&mut self) -> Result<(), Error<Bus::Error>> {
        info!("Processing normal input report request");
        let read_request = Self::listen_bus(&mut self.resources.bus, self.resources.device_response_timeout).await?;
        if let Request::Read(_address) = read_request {
            self.reply_with_input_report().await
        } else {
            error!(
                "Expected read request after input report register access, got {:?}",
                read_request
            );
            Err(Error::Protocol(ProtocolError::InvalidCommand))
        }
    }

    // Call this after listening. TODO figure out if we can enforce this in the type system somehow, maybe take a request or something
    async fn reply_with_input_report(&mut self) -> Result<(), Error<Bus::Error>> {
        if self.resources.pending_reset {
            info!("Processing first input report read after reset");
            // We need to acknowledge that we've completed a reset by writing back 0's - see section 7.2.1 of the HID spec
            Self::write_bus(
                &mut self.resources.bus,
                self.resources.device_response_timeout,
                &[00, 00],
            )
            .await?;

            self.resources.pending_reset = false;
            self.resources.attn_pin.clear_interrupt();
            return Ok(());
        }

        let report = self.resources.hid_device.receiver().receive().await?;
        info!("Got report to return - listening to bus for read request");

        let size_bytes = report.data().len() as u16
            + device_descriptor::HID_REPORT_HEADER_SIZE_BYTES
            + if self.resources.hid_device.report_descriptor().input_id_is_implicit() {
                0
            } else {
                device_descriptor::HID_REPORT_ID_SIZE_BYTES
            };
        let [size_low, size_high] = size_bytes.to_le_bytes();
        let header = [size_low, size_high, report.id().0];

        let header_slice = if self.resources.hid_device.report_descriptor().input_id_is_implicit() {
            header
                .get(..2)
                .expect("We know header is 3 bytes because we just declared it")
        } else {
            &header
        };

        trace!(
            "Responding with input report {}: {:x} {:x}",
            report.id(),
            header_slice,
            report.data()
        );
        Self::write_bus_unterminated(
            &mut self.resources.bus,
            self.resources.device_response_timeout,
            header_slice,
        )
        .await?;

        Self::write_bus(
            &mut self.resources.bus,
            self.resources.device_response_timeout,
            report.data(),
        )
        .await?;

        if self.resources.hid_device.receiver().is_empty() {
            self.resources.attn_pin.clear_interrupt();
        }

        Ok(())
    }

    async fn process_output_report_write(&mut self) -> Result<(), Error<Bus::Error>> {
        let mut write_header_buf = [0u8; (device_descriptor::HID_REPORT_HEADER_SIZE_BYTES + device_descriptor::HID_REPORT_ID_SIZE_BYTES) as usize];
        let mut header_buf_slice = if self.resources.hid_device.report_descriptor().output_id_is_implicit() {
            // NOTE: If there is no report ID because we only have one report, we call it 0.
            write_header_buf
                .get_mut(..2)
                .expect("We know buffer is 3 bytes because we just declared it")
        } else {
            &mut write_header_buf
        };

        let header_len = header_buf_slice.len();

        Self::read_bus(
            &mut self.resources.bus,
            self.resources.data_read_timeout,
            &mut header_buf_slice,
        )
        .await?;

        let [len_low, len_high, report_id] = write_header_buf;
        let length = u16::from_le_bytes([len_low, len_high]) as usize - header_len; // Note: per HID spec, the length field needs to include its own length (2 bytes) and the report ID (1 byte)
        trace!("Reading {} bytes", length);

        let read_result = Self::read_bus(
            &mut self.resources.bus,
            self.resources.data_read_timeout,
            &mut self.resources.write_buf,
        )
        .await?;

        if read_result != length as usize {
            error!("Expected to read {} bytes but got {}", length, read_result);
            return Err(Error::Protocol(ProtocolError::InvalidSize));
        }

        // TODO this makes a copy, which feels bad - figure out if we can make this write directly into the HID report and still be typesafe . maybe some sort of builder type but need to check in compilerexplorer if something like that actually omits the copy
        let output_report = embedded_services::relay::hid::SetHidReport::Output(
            HidReport::new(
                embedded_services::relay::hid::ReportId(report_id),
                &self
                    .resources
                    .write_buf
                    .get(..length as usize)
                    .ok_or(Error::Protocol(ProtocolError::InvalidSize))?,
            )
            .map_err(|_| Error::Protocol(ProtocolError::InvalidSize))?,
        );

        self.resources.hid_device.set_report(&output_report).await?;

        Ok(())
    }

    async fn get_command_report_header(
        &mut self,
        command_byte: u8,
    ) -> Result<(HidI2cReportType, embedded_services::relay::hid::ReportId), Error<Bus::Error>> {
        let command_header = HidI2cReportCommandHeader::try_from_command_byte(command_byte)?;
        let report_id = if let Some(report_id) = command_header.report_id {
            report_id
        } else {
            let mut report_id = 0u8;
            Self::read_bus(
                &mut self.resources.bus,
                self.resources.data_read_timeout,
                core::slice::from_mut(&mut report_id),
            )
            .await?;
            embedded_services::relay::hid::ReportId(report_id)
        };

        Ok((command_header.report_type, report_id))
    }

    async fn process_command(&mut self) -> Result<(), Error<Bus::Error>> {
        let [command_byte, opcode_byte] = {
            let mut command_header_buffer = [0u8; 2];
            Self::read_bus(
                &mut self.resources.bus,
                self.resources.data_read_timeout,
                &mut command_header_buffer,
            )
            .await?;
            command_header_buffer
        };

        match Opcode::try_from(opcode_byte).map_err(|_| Error::Protocol(ProtocolError::InvalidCommand))? {
            Opcode::Reset => {
                warn!("HID-I2C: Host requested device reset");
                Err(Error::Device(HidError::TriggerReset))
            }

            Opcode::SetPower => {
                trace!("Processing set power command");
                let power_state = I2cPowerState::try_from(command_byte)
                    .map_err(|_| Error::Protocol(ProtocolError::InvalidCommand))?;
                self.resources.hid_device.set_power_state(power_state.into()).await?;
                Ok(())
            }

            Opcode::GetReport => {
                trace!("Processing get report command");

                let (report_type, report_id) = self.get_command_report_header(command_byte).await?;
                let report = self
                    .resources
                    .hid_device
                    .get_report(
                        report_type
                            .to_get_type()
                            .ok_or(Error::Protocol(ProtocolError::InvalidCommand))?,
                        report_id,
                    )
                    .await?;

                // Note: per HID spec, the length field needs to include its own length (2 bytes)
                let len_header =
                    ((report.data().len() as u16 + device_descriptor::HID_REPORT_HEADER_SIZE_BYTES)).to_le_bytes();
                Self::write_bus(
                    &mut self.resources.bus,
                    self.resources.device_response_timeout,
                    &len_header,
                )
                .await?;
                Self::write_bus(
                    &mut self.resources.bus,
                    self.resources.device_response_timeout,
                    report.data(),
                )
                .await?;

                Ok(())
            }

            Opcode::SetReport => {
                trace!("Processing set report command");
                let (report_type, report_id) = self.get_command_report_header(command_byte).await?;
                let mut len_header = [0u8; core::mem::size_of::<u16>()];
                Self::read_bus(
                    &mut self.resources.bus,
                    self.resources.data_read_timeout,
                    &mut len_header,
                )
                .await?;

                // Note: per HID spec, the length field relayed over the wire needs to include its own length (2 bytes)
                let report_size =
                    (u16::from_le_bytes(len_header) - device_descriptor::HID_REPORT_HEADER_SIZE_BYTES) as usize;
                Self::read_bus(
                    &mut self.resources.bus,
                    self.resources.data_read_timeout,
                    &mut self
                        .resources
                        .write_buf
                        .get_mut(..report_size)
                        .ok_or(Error::Protocol(ProtocolError::InvalidSize))?,
                )
                .await?;

                let set_report = match report_type {
                    HidI2cReportType::Input => {
                        error!("Host attempted to send us an input report, which is invalid");
                        return Err(Error::Protocol(ProtocolError::InvalidReportType));
                    }
                    HidI2cReportType::Output => SetHidReport::Output(HidReport::new(
                        report_id,
                        &self
                            .resources
                            .write_buf
                            .get(..report_size)
                            .ok_or(Error::Protocol(ProtocolError::InvalidSize))?,
                    )?),
                    HidI2cReportType::Feature => SetHidReport::Feature(HidReport::new(
                        report_id,
                        &self
                            .resources
                            .write_buf
                            .get(..report_size)
                            .ok_or(Error::Protocol(ProtocolError::InvalidSize))?,
                    )?),
                };

                self.resources.hid_device.set_report(&set_report).await?;

                Ok(())
            }
        }
    }

    async fn reset(&mut self) {
        warn!("HID-I2C: Executing device reset");
        self.resources.hid_device.host_reset().await;
        self.resources.pending_reset = true;
        self.resources.attn_pin.assert_interrupt();
    }
}

/// Control handle for an instance of the HID-I2C service, which presents a HID-I2C device over an (I2C bus, interrupt line) tuple
#[derive(Clone, Copy)]
pub struct Service<'hw, Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
{
    resources: &'hw ServiceResources,
    _phantom_bus: core::marker::PhantomData<Bus>,
    _phantom_attn_pin: core::marker::PhantomData<AttnPin>,
    _phantom_hid_device: core::marker::PhantomData<HidDevice>,
}

impl<
    'hw,
    Bus: I2cTargetAsync + 'hw,
    AttnPin: embedded_hal::digital::OutputPin + 'hw,
    HidDevice: ConstrainedHidDevice + 'hw,
> Service<'hw, Bus, AttnPin, HidDevice>
{
    /// Creates a new instance of the HID-I2C service and its associated runner.
    /// You must call run() on the runner to drive the service.  Consider using
    /// this in conjunction with `odp_service_common::runnable_service::spawn_service!()`
    pub async fn new(
        storage: &'hw mut Resources<Bus, AttnPin, HidDevice>,
        bus: Bus,
        attn_pin: AttnPin,
        hid_device: HidDevice,
        hwinfo: HardwareVersionInfo,
        timeout_settings: TimeoutSettings,
    ) -> Result<(Self, Runner<'hw, Bus, AttnPin, HidDevice>), core::convert::Infallible> {
        let device_descriptor = DeviceDescriptor::new(&hid_device, hwinfo);

        let service_resources: &ServiceResources = storage.service_resources.insert(ServiceResources {
            reset_signal: embassy_sync::signal::Signal::new(),
        });

        let runner_resources = storage.runner_resources.insert(RunnerResources::new(
            bus,
            attn_pin,
            hid_device,
            device_descriptor,
            timeout_settings.device_response_timeout,
            timeout_settings.data_read_timeout,
        ));

        Ok((
            Service {
                resources: service_resources,
                _phantom_bus: PhantomData,
                _phantom_attn_pin: PhantomData,
                _phantom_hid_device: PhantomData,
            },
            Runner {
                resources: runner_resources,
                reset_signal: &service_resources.reset_signal,
            },
        ))
    }

    /// Causes the HID service to perform a device-initiated reset.
    pub fn reset(&mut self) {
        self.resources.reset_signal.signal(());
    }
}

impl<
    'hw,
    Bus: I2cTargetAsync + 'hw,
    AttnPin: embedded_hal::digital::OutputPin + 'hw,
    HidDevice: ConstrainedHidDevice + 'hw,
> odp_service_common::runnable_service::Service<'hw> for Service<'hw, Bus, AttnPin, HidDevice>
{
    type Runner = Runner<'hw, Bus, AttnPin, HidDevice>;
    type Resources = Resources<Bus, AttnPin, HidDevice>;
}

/// Timeout configuration for I2C operations
pub struct TimeoutSettings {
    /// Timeout for device response reads
    pub device_response_timeout: Duration,
    /// Timeout for data reads from the host.
    pub data_read_timeout: Duration,
}

impl Default for TimeoutSettings {
    fn default() -> Self {
        Self {
            device_response_timeout: Duration::from_secs(1),
            data_read_timeout: Duration::from_secs(1),
        }
    }
}
