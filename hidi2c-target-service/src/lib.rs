//! This crate contains a service that behaves as a HID target/slave device over I2C.

#![no_std]
// TODO rm
#![warn(warnings)]
#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_imports)]

use core::marker::PhantomData;
use embassy_time::{Duration, with_timeout};
use embedded_mcu_hal::i2c::target::Request;
use embedded_mcu_hal::i2c::target::WriteStatus;
use embedded_mcu_hal::i2c::target::asynch::I2c as I2cTargetAsync;
use embedded_services::relay::hid;
use embedded_services::relay::hid::{GetHidReport, HidReport, HidResult, ReportReceiver, SetHidReport};
use embedded_services::{error, info, trace, warn};
use generic_array::ArrayLength;
use typenum::Max;
use zerocopy::IntoBytes;

mod device_descriptor;
use device_descriptor::DeviceDescriptor;
pub use device_descriptor::{ProductId, VendorId, VersionId};

//  HID errors
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum HidError {
    // TODO prune for usage
    /// Invalid data
    InvalidData,
    /// Invalid size
    InvalidSize,
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

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum Error<BusError> {
    /// Error from the underlying bus
    Bus(BusError),
    // HID error
    Hid(HidError),
}

impl<BusError> From<HidError> for Error<BusError> {
    fn from(err: HidError) -> Self {
        Error::Hid(err)
    }
}

mod sealed {
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

impl<T> sealed::Sealed for T where T: ConstrainedHidDevice{}

// These are our convention, not from the HID-I2C spec. TODO figure out if a device with a single I2C bus address is allowed to expose more than one register file? if it is we may need to rework this to a struct. that would also sort-of solve our reset domain problem (although we'd still need a separate interrupt line per device)...
#[repr(u16)]
#[derive(num_enum::TryFromPrimitive, num_enum::IntoPrimitive, Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum HidI2cRegister {
    DeviceDescriptor = 0x01, // NOTE: Per the HID-I2C spec, when using ACPI for enumeration, this value needs to be put in the _DSM. The others are discovered by reading this one.
    ReportDescriptor = 0x02,
    Input = 0x03,
    Output = 0x04,
    Command = 0x05,
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

/// I2C wire format representation for HID power states
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

#[repr(u8)]
#[derive(num_enum::TryFromPrimitive, num_enum::IntoPrimitive, Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum HidI2cReportType {
    Input,
    Output,
    Feature,
}

struct HidI2cReportCommandHeader {
    /// The report type that this command is targeting
    report_type: HidI2cReportType,

    /// The report ID that this command is targeting, or None if another byte must be read to get the full report ID (happens if report ID is >= 0xF)
    report_id: Option<embedded_services::relay::hid::ReportId>,
}

impl HidI2cReportCommandHeader {
    fn try_from_command_byte(command_byte: u8) -> Result<Self, HidError> {
        const HID_I2C_REPORT_TYPE_OFFSET: u8 = 4;
        let report_type = match command_byte >> HID_I2C_REPORT_TYPE_OFFSET {
            0x01 => HidI2cReportType::Input,
            0x02 => HidI2cReportType::Output,
            0x03 => HidI2cReportType::Feature,
            _ => return Err(HidError::InvalidReportType),
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
    service_resources: Option<ServiceResources<Bus, AttnPin, HidDevice>>,
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
struct RunnerResources<Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
{
    bus: Bus,
    attn_pin: AttnPinHandler<AttnPin>,
    hid_device: HidDevice, // TODO some sort of channel for talking to runner?
    device_descriptor: DeviceDescriptor,

    // Read/write buffers.
    read_buf: generic_array::GenericArray<u8, HidDevice::MaxInputOrFeatureSize>,
    write_buf: generic_array::GenericArray<u8, HidDevice::MaxOutputOrFeatureSize>,

    device_response_timeout: Duration,
    /// Timeout for data reads from the host.
    data_read_timeout: Duration,

    /// True if a reset has been triggered but not yet acknowledged by the host
    pending_reset: bool
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
            read_buf: generic_array::GenericArray::default(),
            write_buf: generic_array::GenericArray::default(),
            device_response_timeout,
            data_read_timeout,
            pending_reset: false // The host is responsible for explicitly resetting us at boot, so we start in a non-reset state
        }
    }
}


// TODO should this be a trait or something, or do we want to require open-drain active low?
struct AttnPinHandler<AttnPin: embedded_hal::digital::OutputPin> {
    attn_pin: AttnPin,
    asserted: bool
}

impl<AttnPin: embedded_hal::digital::OutputPin> AttnPinHandler<AttnPin> {
    fn new(attn_pin: AttnPin) -> Self {
        let mut result = Self { attn_pin, asserted: false};
        result.clear_interrupt();
        result
    }

    fn clear_interrupt(&mut self) -> Result<(), AttnPin::Error> {
        trace!("ATTN: clear interrupt");
        self.asserted = false;
        self.attn_pin.set_high()
    }

    fn assert_interrupt(&mut self) -> Result<(), AttnPin::Error> {
        trace!("ATTN: assert interrupt");
        self.asserted = true;
        self.attn_pin.set_low()
    }

    fn asserted(&self) -> bool {
        self.asserted
    }
}

struct ServiceResources<Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
{
    // TODO figure out if we even need a control handle - maybe we want to allow manual device-initiated reset or something?
    // TODO I think having all these phantomdatas may be an indication that the RunnableService trait is too constrained, figure out if we need to make changes there?
    _bus: PhantomData<Bus>,
    _attn_pin: PhantomData<AttnPin>,
    _hid_device: PhantomData<HidDevice>,
}

pub struct Runner<'hw, Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
{
    resources: &'hw mut RunnerResources<Bus, AttnPin, HidDevice>,
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
            // TODO this thing is going to fire a bunch if ready_to_receive is triggered,
            //      we may need some sort of way to not wait for ready_to_receive after pin is set high until it goes low again
            //

            // TODO this feels a little weird? we have to construct a receiver every time to avoid running afoul of the borrow checker when
            //      we try to use another method on the HidDevice. Receiver is cheap to construct (it's really just a pointer-to-channel),
            //      but it seems a bit odd and if someone wanted to use another non-embassy_sync::Channel type to implement this it might be
            //      expensive, not sure. we may want to consider doing some sort of 'split(&mut self)' function that returns a receiver and
            //      a control handle or something so we can borrow both pieces of the underlying struct at the same time?
            let event = {
                let receiver = self.resources.hid_device.receiver();
                let listen_future = self.resources.bus.listen();
                // If we've raised the interrupt, we know it won't go down again until it's serviced, so we don't need to
                // wait for it
                if self.resources.attn_pin.asserted() {
                    embassy_futures::select::Either::First(listen_future.await)
                }
                else {
                    embassy_futures::select::select(listen_future, receiver.ready_to_receive()).await
                }
            };
            match event {
                embassy_futures::select::Either::First(bus_request) => {
                    trace!("Processing request from host");
                    self.process_request(bus_request.expect("TODO handle error recovery")).await;
                    trace!("Done processing request from host"); // TODO rm
                }
                embassy_futures::select::Either::Second(()) => {
                    trace!("Signalling host that we have an input report ready");
                    self.resources.attn_pin.assert_interrupt().expect("TODO handle attn pin error");
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
    // TODO these are associated functions because `buffer` is often using a borrow on self (i.e. read_bus(self.bus, self.buffer), but this feels a bit awkward. figure out if there's a more ergonomic way to describe this pattern
    async fn read_bus(bus: &mut Bus, timeout: Duration, buffer: &mut [u8]) -> Result<usize, Error<Bus::Error>> {
        match with_timeout(timeout, bus.respond_to_write(buffer)).await {
            Err(_timeout_error) => {
                error!("Read request timeout");
                bus.recover().await.expect("TODO handle bus recovery error");
                Err(Error::Hid(HidError::Timeout))
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
                    Err(Error::Hid(HidError::InvalidData))
                }
            },
        }
    }

    async fn write_bus(bus: &mut Bus, timeout: Duration, buffer: &[u8]) -> Result<(), Error<Bus::Error>> {
        match with_timeout(timeout, bus.respond_to_read(buffer)).await {
            Err(_timeout_error) => {
                error!("Write request timeout");
                bus.recover().await.expect("TODO handle bus recovery error");
                Err(Error::Hid(HidError::Timeout))
            }
            Ok(result) => {
                // TODO this is aligned with what soc-embedded-controller was doing for embassy-imxrt, with hid-service, but I'm not convinced this is totally correct - revisit
                result.map(|_| ()).map_err(|e| Error::Bus(e)) // TODO map needmore to failure?
            }
        }
    }

    async fn listen_bus(bus: &mut Bus, timeout: Duration) -> Result<Request, Error<Bus::Error>> {
        loop {
            let result = match with_timeout(timeout, bus.listen()).await {
                Err(_timeout_error) => {
                    error!("Listen request timeout");
                    bus.recover().await.expect("TODO handle bus recovery error");
                    Err(Error::Hid(HidError::Timeout))
                }
                Ok(result) => result.map_err(|_| Error::Hid(HidError::Timeout)),
            };

            // TODO is this the right thing to do?
            if let Ok(Request::RepeatedStart(_a)) = result {
                info!("Received repeated start; ignoring");
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
        match request {
            Request::Write(_address) => {
                self.process_register_access().await.expect("TODO handle error correctly");
                // if let Err(e) = result {
                //     error!("Error processing register access");
                // }
            }
            Request::Read(_address) => {
                // TODO the old impl did an input report read here, but it's not clear to me that that's correct?
                //      I think an unsolicited read should never happen - they need to go through a register - right?
                //      Per the spec 6.1:
                //               When the HOST receives the Interrupt, it is responsible for reading the data of the DEVICE via the Input Register (field: wInputRegister) as defined in the HID Descriptor. The HOST does this by issuing an I2C read request to the DEVICE.
                //
                //      But also the sequence diagram in section 6.1.3 doesn't have it issuing a write to the "register to read" field at all, so seems ambiguous. Need to verify with an in-market device, I guess
                //
                // todo!("figure out if we're supposed to handle this case - I think this is an invalid message? request {:?}", request)
                //
                // To match the old behavior we'd do:
                warn!("Treating naked read from host as a request for an input report, unclear if this is correct");
                self.process_input_report_read().await.expect("TODO handle error correctly");
            }

            // TODO this is in line with what we were doing for the I2cCommand::Probe command in the old hid service, but it's not
            //      clear to me if that was correct or how the other enum variants map to that.
            //      Figure out if we need to handle any of the following:
            //          Request::RepeatedStart(prev_address) // Continue transaction - I think this is targeted at other masters on the bus?
            //          Request::Stop(address)               // End of transaction - I think this is targeted at other masters on the bus?
            //          Request::GeneralCall                 // I don't know what this is
            //          Request::SmbusAlert                  // I don't know what this is
            //
            _ => return,
        }
    }

    async fn process_register_access(&mut self) -> Result<(), Error<Bus::Error>> {
        let mut reg = [0u8; 2];
        Self::read_bus(&mut self.resources.bus, self.resources.data_read_timeout, &mut reg).await?;

        let register = HidI2cRegister::try_from(u16::from_le_bytes(reg))
            .map_err(|_| Error::Hid(HidError::InvalidRegisterAddress))?;

        info!("Host requested to access register {:?}", register);
        match register {
            HidI2cRegister::DeviceDescriptor => {
                // TODO do we need to handle the case where the host decides to talk to someone else in the middle of talking to us?
                let request = Self::listen_bus(&mut self.resources.bus, self.resources.device_response_timeout).await?;
                match request {
                    Request::Read(_address) => {
                        trace!("Responding to request for device descriptor");
                        Self::write_bus(
                            &mut self.resources.bus,
                            self.resources.device_response_timeout,
                            self.resources.device_descriptor.as_bytes(),
                        )
                        .await?;
                        Ok(())
                    }
                    _ => {
                        error!("Expected read request after device descriptor register access: {:?}", request);
                        Err(Error::Hid(HidError::InvalidRegisterAddress))
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
                        Err(Error::Hid(HidError::InvalidRegisterAddress))
                    }
                }
            }
            HidI2cRegister::Input => self.process_input_report_read().await,
            HidI2cRegister::Output => {
                // TODO I don't understand why both this and the "output" command exist. Why would a host send one or the other? They seem equivalent? Should probably share some parts of impl
                self.process_output_report_write().await
            }
            HidI2cRegister::Command => self.process_command().await,
            HidI2cRegister::Data => {
                // TODO clean up logging here
                error!(
                    "Got a data read when we weren't expecting one, those should only come in when we're in the middle of handling a Command register invocation"
                );
                Err(Error::Hid(HidError::InvalidRegisterAddress))
            }
        }
    }

    /// Process a request for an input report that we've asserted an interrupt for (i.e. not a request for a specific input report ID)
    async fn process_input_report_read(&mut self) -> Result<(), Error<Bus::Error>> {
        if self.resources.pending_reset {
            info!("Processing first input report read after reset");
            // Upon reset, the next input report read is supposed to return a length of 0x0000 to ack the reset
            Self::write_bus(
                &mut self.resources.bus,
                self.resources.device_response_timeout,
                &[0x00, 0x00], // Length of 0 to acknowledge reset
            )
            .await?;

            self.resources.pending_reset = false;
            return Ok(());
        }
        let report = self.resources.hid_device.receiver().receive().await; // TODO should we timeout?
        match report {
            HidResult::Ok(report) => {
                if let Request::Read(_address) =
                    Self::listen_bus(&mut self.resources.bus, self.resources.device_response_timeout).await?
                {
                    let [size_low, size_high] = (report.data().len() as u16).to_le_bytes();
                    let header = [size_low, size_high, report.id().0];

                    // TODO make sure this is legal - these shouldn't be split across two transactions but I don't want to have to copy everything to a buffer just to copy it out again?
                    Self::write_bus(&mut self.resources.bus, self.resources.device_response_timeout, &header).await?;
                    Self::write_bus(
                        &mut self.resources.bus,
                        self.resources.device_response_timeout,
                        report.data(),
                    )
                    .await?;

                    if self.resources.hid_device.receiver().is_empty() {
                        self.resources.attn_pin.clear_interrupt().expect("TODO handle attn pin error");
                    }
                    Ok(())
                } else {
                    error!("Expected read request after input report register access");
                    Err(Error::Hid(HidError::InvalidCommand))
                }
            }
            HidResult::TriggerReset => {
                self.reset().await;
                Err(Error::Hid(HidError::InvalidCommand)) // TODO do we want to aggregate the reset path into one place? Maybe we should just propagate the reset and have the top-level fn do the reset or something
            }
        }
    }

    async fn process_output_report_write(&mut self) -> Result<(), Error<Bus::Error>> {
        let mut write_header_buf = [0u8; 3];
        Self::read_bus(
            &mut self.resources.bus,
            self.resources.data_read_timeout,
            &mut write_header_buf,
        )
        .await?;

        let [len_low, len_high, report_id] = write_header_buf;
        let length = u16::from_le_bytes([len_low, len_high]);
        trace!("Reading {} bytes", length);

        let read_result = Self::read_bus(
            &mut self.resources.bus,
            self.resources.data_read_timeout,
            &mut self.resources.write_buf,
        )
        .await?;
        if read_result != length as usize {
            error!("Expected to read {} bytes but got {}", length, read_result);
            return Err(Error::Hid(HidError::InvalidSize));
        }

        // TODO this makes a copy, which feels bad - figure out if we can make this write directly into the HID report and still be typesafe . maybe some sort of builder type but need to check in compilerexplorer if something like that actually omits the copy
        let output_report = embedded_services::relay::hid::SetHidReport::Output(
            HidReport::new(
            embedded_services::relay::hid::ReportId(report_id),
            &self.resources.write_buf.get(..length as usize).ok_or(Error::Hid(HidError::InvalidSize))?).map_err(|_| Error::Hid(HidError::InvalidSize) /* TODO figure out if this should just be the err type for hidreport::new */)?,
        );

        match self.resources.hid_device.set_report(&output_report).await {
            HidResult::Ok(_) => Ok(()), // No response to host in success case
            HidResult::TriggerReset => {
                self.reset().await;
                Err(Error::Hid(HidError::InvalidCommand)) // TODO do we want to aggregate the reset path into one place? Maybe we should just propagate the reset and have the top-level fn do the reset or something
            }
        }
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
        // TODO emperically it looks like I had this backward but verify in the spec that I'm not missing something here about the order
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

        let opcode = Opcode::try_from(opcode_byte).map_err(|_| Error::Hid(HidError::InvalidCommand));
        if opcode.is_err() {
            error!("Received invalid opcode: {:#x} (command {:#x})", opcode_byte, command_byte);
        }

        match opcode? {
            Opcode::Reset => {
                trace!("Processing reset command");
                self.reset().await;
                Ok(())
            }
            Opcode::SetPower => {
                trace!("Processing set power command");
                let power_state =
                    I2cPowerState::try_from(command_byte).map_err(|_| Error::Hid(HidError::InvalidCommand))?;
                self.resources.hid_device.set_power_state(power_state.into()).await;
                Ok(())
            }

            Opcode::GetReport => {
                trace!("Processing get report command");

                let (report_type, report_id) = self.get_command_report_header(command_byte).await?;
                match self.resources.hid_device.get_report(report_id).await {
                    HidResult::TriggerReset => {
                        trace!("Triggering reset due to GetReport failure");
                        self.reset().await;
                        Err(Error::Hid(HidError::Timeout)) // TODO do we want to aggregate the reset path into one place? Maybe we should just propagate the reset and have the top-level fn do the reset or something
                    }

                    HidResult::Ok(report) => {
                        // Note: per HID spec, the length field needs to include its own length (2 bytes)
                        let len_header = (report.data().len() as u16 + 2).to_le_bytes();

                        // TODO make sure this is legal - these shouldn't be split across two transactions but I don't want to have to copy everything to a buffer just to copy it out again?
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
                        .await
                        .expect("TODO handle write error");

                        Ok(())
                    }
                }
            }

            Opcode::SetReport => {
                trace!("Processing set report command");
                let (report_type, report_id) = self.get_command_report_header(command_byte).await?;
                let mut len_header = [0u8; 2];
                Self::read_bus(
                    &mut self.resources.bus,
                    self.resources.data_read_timeout,
                    &mut len_header,
                )
                .await?;
                let report_size = u16::from_le_bytes(len_header) - 2; // Note: per HID spec, the length field needs to include its own length (2 bytes)
                Self::read_bus(
                    &mut self.resources.bus,
                    self.resources.data_read_timeout,
                    &mut self
                        .resources
                        .write_buf
                        .get_mut(..report_size as usize)
                        .ok_or(Error::Hid(HidError::InvalidSize))?,
                )
                .await?;

                let set_report = match report_type {
                    HidI2cReportType::Input => {
                        error!("Host attempted to send us an input report, which is invalid");
                        return Err(Error::Hid(HidError::InvalidReportType));
                    }
                    HidI2cReportType::Output => SetHidReport::Output(
                        HidReport::new(
                            report_id,
                            &self.resources.write_buf.get(..report_size as usize).ok_or(Error::Hid(HidError::InvalidSize))?,
                        )
                        .map_err(|_| Error::Hid(HidError::InvalidSize) /* TODO figure out if this should just be the err type for hidreport::new */)?,
                    ),
                    HidI2cReportType::Feature => SetHidReport::Feature(
                        HidReport::new(
                            report_id,
                            &self.resources.write_buf.get(..report_size as usize).ok_or(Error::Hid(HidError::InvalidSize))?,
                        )
                        .map_err(|_| Error::Hid(HidError::InvalidSize) /* TODO figure out if this should just be the err type for hidreport::new */)?,
                    ),
                };

                match self.resources.hid_device.set_report(&set_report).await {
                    HidResult::TriggerReset => {
                        trace!("Triggering reset due to HID result timeout");
                        self.reset().await;
                        Err(Error::Hid(HidError::InvalidCommand)) // TODO do we want to aggregate the reset path into one place? Maybe we should just propagate the reset and have the top-level fn do the reset or something
                    }
                    HidResult::Ok(_) => Ok(()), // No response to host in success case
                }
            }
        }
    }

    async fn reset(&mut self) {
        trace!("Executing reset");
        self.resources.hid_device.host_reset().await;
        self.resources.pending_reset = true;
        self.resources.attn_pin.assert_interrupt().expect("TODO handle attn pin error");
    }
}

pub struct Service<'hw, Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice>
{
    resources: &'hw ServiceResources<Bus, AttnPin, HidDevice>,
}

impl<
    'hw,
    Bus: I2cTargetAsync + 'hw,
    AttnPin: embedded_hal::digital::OutputPin + 'hw,
    HidDevice: ConstrainedHidDevice + 'hw,
> Service<'hw, Bus, AttnPin, HidDevice>
{
    pub async fn new(
        storage: &'hw mut Resources<Bus, AttnPin, HidDevice>,
        params: InitParams<Bus, AttnPin, HidDevice>,
        // TODO this is probably not supposed to be infallible
    ) -> Result<(Self, Runner<'hw, Bus, AttnPin, HidDevice>), core::convert::Infallible> {
        let device_descriptor = DeviceDescriptor::new(
            &params.hid_device,
            params.vendor_id,
            params.product_id,
            params.version_id,
        );

        let service_resources = storage.service_resources.insert(ServiceResources {
            _bus: PhantomData,
            _attn_pin: PhantomData,
            _hid_device: PhantomData,
        });

        let runner_resources = storage.runner_resources.insert(RunnerResources::new(
            params.bus,
            params.attn_pin,
            params.hid_device,
            device_descriptor,
            params.device_response_timeout,
            params.data_read_timeout,
        ));

        Ok((
            Service {
                resources: service_resources,
            },
            Runner {
                resources: runner_resources,
            },
        ))
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

// TODO probably get rid of this and just use params directly, maybe struct some of these that are defaultable
pub struct InitParams<Bus: I2cTargetAsync, AttnPin: embedded_hal::digital::OutputPin, HidDevice: ConstrainedHidDevice> {
    pub bus: Bus,
    pub attn_pin: AttnPin,
    pub hid_device: HidDevice,

    pub vendor_id: VendorId,
    pub product_id: ProductId,
    pub version_id: VersionId,

    // TODO figure out why these were different on the prior impl
    // TODO figure out if we should have these in a sub-struct so they're easier to default or something
    // TODO maybe we can do something like std::bind in the spawn_service macro to not require an InitParams struct? would make using multiple constructors more flexible, I think
    /// Timeout for device response reads
    pub device_response_timeout: Duration,
    /// Timeout for data reads from the host.
    pub data_read_timeout: Duration,
}
