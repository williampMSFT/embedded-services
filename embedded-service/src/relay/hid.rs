//! HID relay code

use generic_array::{ArrayLength, GenericArray};
use num_enum::TryFromPrimitive;

// TODO read over comments and make sure they're still true when we go to check in
// TODO some of this may belong in some sort of external "HID support" library (e.g. stuff to manipulate HID descriptors)

/// Errors that a HID device operation can fail with.
///
/// Reporting failure triggers a device-initiated reset, so callers must handle these errors explicitly
/// rather than swallowing them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum HidError {
    /// The operation has failed and a device-initiated reset should be triggered.
    TriggerReset,
}

/// Power states that the host can command a HID device to be put into.
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))] // TODO add default impls to all the other types in here
pub enum HidDevicePowerState {
    /// Normal operation
    On,

    /// Reduced power state, but a device that sends a report in this state can wake the host - quiesce messages if you don't want to do that
    Sleep,

    /// The device is not allowed to wake the host. This is not supported on all transports - in particular, I2C will never command a device into the off state.
    Off,
}

/// A HID report of no more than X bytes
pub struct HidReport<MaxSize: ArrayLength> {
    id: ReportId,

    data: GenericArray<u8, MaxSize>,
    valid_bytes: usize,
}

impl<MaxSize: ArrayLength> HidReport<MaxSize> {
    /// Create a new HID report from the provided data slice.
    pub fn new(id: ReportId, data: &[u8]) -> Result<Self, generic_array::LengthError> {
        Ok(Self {
            id,
            data: {
                let mut result = GenericArray::default();
                result
                    .get_mut(..data.len())
                    .ok_or(generic_array::LengthError)?
                    .copy_from_slice(data);
                result
            },
            valid_bytes: data.len(),
        })
    }

    /// The report ID for this report
    pub fn id(&self) -> ReportId {
        self.id
    }

    /// The data for this report. This will be no more than `MaxSize` bytes, but may be less if the report is smaller than the maximum size.
    pub fn data(&self) -> &[u8] {
        &self.data.as_slice().get(..self.valid_bytes).unwrap_or(&[])
    }
}

/// HID report types supported by the SetReport operation.
pub enum SetHidReport<OutputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> {
    /// An output report
    Output(HidReport<OutputMaxSize>),

    /// A feature report
    Feature(HidReport<FeatureMaxSize>),
}

impl<OutputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> SetHidReport<OutputMaxSize, FeatureMaxSize> {
    /// The data for this report, whatever its type.
    pub fn data(&self) -> &[u8] {
        match self {
            SetHidReport::Output(report) => report.data(),
            SetHidReport::Feature(report) => report.data(),
        }
    }
}

/// A type of report that can be requested by the host
pub enum GetHidReportType {
    /// The host has requested an input report
    Input,

    /// The host has requested a feature report
    Feature,
}

/// HID report types supported by the GetReport operation.
pub enum GetHidReport<InputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> {
    /// An input report
    Input(HidReport<InputMaxSize>),

    /// A feature report
    Feature(HidReport<FeatureMaxSize>),
}

impl<InputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> GetHidReport<InputMaxSize, FeatureMaxSize> {
    /// The data for this report, whatever its type.
    pub fn data(&self) -> &[u8] {
        match self {
            GetHidReport::Input(report) => report.data(),
            GetHidReport::Feature(report) => report.data(),
        }
    }
}

/// HID report ID
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct ReportId(pub u8);

/// An object capable of listening for unsolicited reports from a HID device.
pub trait ReportReceiver<MaxSize: ArrayLength> {
    /// Blocks until the receiver is ready to yield a report.
    fn ready_to_receive(&self) -> impl core::future::Future<Output = ()>;

    /// Blocks until a report is available and returns it. If the receiver is empty, this will block until a report is available.
    fn receive(&self) -> impl core::future::Future<Output = Result<HidReport<MaxSize>, HidError>>;

    /// Returns true if the receiver is empty and there are no reports available to receive.
    fn is_empty(&self) -> bool;
}

impl<'ch, M: embassy_sync::blocking_mutex::raw::RawMutex, MaxSize: ArrayLength, const N: usize> ReportReceiver<MaxSize>
    for embassy_sync::channel::Receiver<'ch, M, Result<HidReport<MaxSize>, HidError>, N>
{
    fn ready_to_receive(&self) -> impl Future<Output = ()> {
        self.ready_to_receive()
    }
    fn receive(&self) -> impl Future<Output = Result<HidReport<MaxSize>, HidError>> {
        self.receive()
    }
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

/// A single HID device that we want to present to the host.
/// This is a transport-agnostic trait that abstracts over the details of how we get reports to/from the host,
/// so that we can implement it once and then use it for both HID-I2C and HID-I3C (and potentially HID-SPI in the
/// future if we want to add support for that).
///
/// A note on error handling - the HID spec only really has one way for a device to communicate failure, and that's
/// by doing a device-initiated reset.  If any of these functions fail, a device-initiated reset will be signalled.
///
/// If you're part of an aggregate device created by impl_odp_hid_aggregate_device!, ***this will reset your peers too***.
/// Therefore, you should be very certain this is the behavior you want before you return an error from any of these functions.
///
/// The normal pattern in HID seems to be to either embed an error code in an input report or to drop the message entirely.
///
pub trait HidDevice {
    /// The maximum size of an input report (device -> host) that this device can use, expressed in bytes.
    /// This must agree with the descriptor returned by `report_descriptor()`.
    type InputReportMaxSize: ArrayLength;

    /// The maximum size of an output report (host -> device) that this device can use, expressed in bytes.
    /// This must agree with the descriptor returned by `report_descriptor()`.
    type OutputReportMaxSize: ArrayLength;

    /// The maximum size of a feature report (bidirectional) that this device can use, expressed in bytes.
    /// This must agree with the descriptor returned by `report_descriptor()`.
    type FeatureReportMaxSize: ArrayLength;

    /// The type that will surface HID reports as they become available.
    /// In general, default to using `embassy_sync::channel::Receiver<'a, M, Result<HidReport<Self::InputReportMaxSize>, HidError>, N>`
    /// where `N` is some reasonable upper bound on the number of pending reports unless you have a specific reason to do something
    /// else.
    type ReportReceiver<'a>: ReportReceiver<Self::InputReportMaxSize>
    where
        Self: 'a;

    /// The maximum number of individual reports that the device will have.  In most cases, this should be exactly the number of
    /// reports that the device has, but in the passthrough case where that knowledge isn't available at compile time, this will
    /// be an upper bound.  This must agree with the descriptor returned by `report_descriptor()`.
    ///
    const MAX_REPORT_COUNT: u8;

    /// Returns the HID descriptor for this device. This isn't allowed to change, but the passthrough case means that
    /// we can't require that it be known at compile time.
    /// If the descriptor disagrees with the sizes implied by `InputReport` / `FeatureReport` / `OutputReport` / `MAX_REPORT_COUNT`, callers should not use the object. // TODO figure out if we can write a wrapper that verifies this in the type system, maybe a ValidatedHidDevice or something
    ///
    // TODO it may be valuable to have a ConstHidDevice trait that has the same API but with the descriptor as an associated
    //      const, and then have a blanket implementation of HidDevice for ConstHidDevice that derives these or something? Might
    //      make usage more ergonomic in the non-passthrough case, which is likely to be more common.
    //
    fn report_descriptor(&self) -> &HidReportDescriptor;

    /// Respond to an explicit request for a particular report from the host. You must fill `out` with the report data.
    fn get_report(
        &mut self,
        report_type: GetHidReportType,
        report_id: ReportId,
    ) -> impl core::future::Future<
        Output = Result<GetHidReport<Self::InputReportMaxSize, Self::FeatureReportMaxSize>, HidError>,
    >;

    /// Respond to a command from the host to handle a particular output/feature report.
    fn set_report(
        &mut self,
        report: &SetHidReport<Self::OutputReportMaxSize, Self::FeatureReportMaxSize>,
    ) -> impl core::future::Future<Output = Result<(), HidError>>;

    /// This is for 'unsolicited' reports - user is responsible for polling this and sending it up.
    fn receiver(&mut self) -> Self::ReportReceiver<'_>;

    /// Called when the host commands a particular power state.
    fn set_power_state(
        &mut self,
        state: HidDevicePowerState,
    ) -> impl core::future::Future<Output = Result<(), HidError>>;

    /// Called when the device should reset its state.  The semantics of reset are device-specific, but
    /// should generally result in clearing any pending reports and returning to a known-good state.
    /// This can be called under the following circumstances:
    ///   1. The host commands a reset, which happens once at startup and can happen again at any time
    ///   2. The implementor of this trait returned HidError::TriggerReset from one of its functions, thereby requesting a reset
    ///   3. A peer HidDevice in an aggregate device triggers a device-initiated reset (see impl_odp_hid_aggregate_device! for details)
    ///
    fn reset(&mut self) -> impl core::future::Future<Output = ()>;
}

// TODO we need to expand on how we're going to present HidReportDescriptors. Initially, it might be a [u8;N] that's just the binary
// representation of a HID report descriptor or something, but I think we need the ability to parse a set of descriptors, "add"
// their TLCs together, and then yield a new one.  This is probably going to require writing a new HID library in Rust.
//
// Additionally, not pictured here but desirable for pretty much all non-passthrough use cases is the ability to have a Rust
// struct that represents a HID report.  I think we may want this HID support library that we need to write to have a macro
// that takes as input the shape of a HID report and generates a repr(C) struct of that report with appropriate padding and
// whatnot, along with potentially emitting a partial HID descriptor that describes that report struct, and break the build if
// the report isn't byte-aligned.  Passthrough can't use this, but I think literally every other use case wants it.
//
// Note - all reports must be byte-aligned, so our support library may need to either emit padding or break the build if padding
// isn't added manually by the user.

/// A HID report descriptor
pub struct HidReportDescriptor {
    bytes: &'static [u8], // TODO this probably doesn't work in the passthrough case; may need to be generic over a size/lifetime or have some sort of buffer type or lifetime annotation or something

    /// Whether or not the input report ID is implicit in the report descriptor. If true, the report ID is not sent to the host as part of the report header.
    /// This is only possible on devices that have no more than one input report.
    input_id_is_implicit: bool,

    /// Whether or not the output report ID is implicit in the report descriptor. If true, the report ID is not sent to the host as part of the report header.
    /// This is only possible on devices that have no more than one output report.
    output_id_is_implicit: bool,

    /// Whether or not the feature report ID is implicit in the report descriptor. If true, the report ID is not sent to the host as part of the report header.
    /// This is only possible on devices that have no more than one feature report.
    feature_id_is_implicit: bool,
}

struct HidReportDescriptorElementHeader(u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, num_enum::IntoPrimitive, num_enum::TryFromPrimitive)]
#[repr(u8)]
enum HidItemType {
    Main = 0,
    Global = 1,
    Local = 2,
    Reserved = 3,
}

impl HidReportDescriptorElementHeader {
    /// The size of this item in bytes.
    fn item_size(&self) -> usize {
        if self.0 == 0b11111110 {
            panic!("Long items are not yet supported"); // TODO implement - see 6.2.2.3 of https://www.usb.org/sites/default/files/hid1_11.pdf
        }

        match self.0 & 0b11 {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4, // per hid spec, size=3 means 4 bytes, not 3 bytes. See section 6.2.2.2 of https://www.usb.org/sites/default/files/hid1_11.pdf
            _ => unreachable!(),
        }
    }

    /// The type of the item, which is one of Main, Global, Local, or Reserved.
    fn item_type(&self) -> HidItemType {
        HidItemType::try_from_primitive((self.0 >> 2) & 0b11)
            .expect("HidItemType::try_from_primitive should never fail because we mask to 2 bits")
    }

    /// The tag of this item, which is a 4-bit value that identifies the specific item within its type (e.g. start collection, end collection, input, output, etc)
    fn item_tag(&self) -> u8 {
        self.0 >> 4
    }
}

impl HidReportDescriptor {
    /// Constructs a HID descriptor from a statically computed and allocated byte slice.
    /// TODO this is a hack for bootstrap until we have a HID support library that can codegen one of these from a bunch of annotated structs or something.
    pub fn new_static(bytes: &'static [u8]) -> Self {
        // TODO validate that this is a well-formed HID report descriptor and that all reports are byte-aligned and whatnot - that'll be part of the hid support library.
        //      alternatively, could just get rid of this when we have the HID support library and have this codegenned or something

        // TODO this is a hack until we get the hid report descriptor library implemented that assumes that either all or no report IDs are implicit.
        let mut iter = bytes.iter();
        let mut implicit = true;
        while let Some(header_bytes) = iter.next() {
            const REPORT_ID_ITEM_TAG: u8 = 0b1000; // per section 6.2.2.7
            let header = HidReportDescriptorElementHeader(*header_bytes);
            if header.item_type() == HidItemType::Global && header.item_tag() == REPORT_ID_ITEM_TAG {
                implicit = false;
                break;
            }

            if header.item_size() != 0 {
                iter.nth(header.item_size() - 1); // skip over the data bytes for this item
            }
        }

        Self {
            bytes,
            input_id_is_implicit: implicit,
            output_id_is_implicit: implicit,
            feature_id_is_implicit: implicit,
        }
    }

    /// Returns the raw bytes of the HID report descriptor. This is what will be sent to the host when it requests the HID descriptor.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes
    }

    /// Whether or not the input report ID is implicit in the report descriptor. If true, the report ID is not sent to the host as part of the report header.
    /// This is only possible on devices that have no more than one input report.
    pub fn input_id_is_implicit(&self) -> bool {
        self.input_id_is_implicit
    }

    /// Whether or not the output report ID is implicit in the report descriptor. If true, the report ID is not sent to the host as part of the report header.
    /// This is only possible on devices that have no more than one output report.
    pub fn output_id_is_implicit(&self) -> bool {
        self.output_id_is_implicit
    }

    /// Whether or not the feature report ID is implicit in the report descriptor. If true, the report ID is not sent to the host as part of the report header.
    /// This is only possible on devices that have no more than one feature report.
    pub fn feature_id_is_implicit(&self) -> bool {
        self.feature_id_is_implicit
    }
}

// /// Generates an implementation of HidDevice that has the following properties:
// /// - Has its own HID descriptor that it derived from the collection of HidDevices it wraps - all top-level collections are concatenated in the order the child devices are provided.
// ///   NOTE: This means we're going to have to parse HID descriptors at runtime, which is going to suck, but I don't see a way around it if we want to support passthrough.
// /// - Remaps report IDs from the devices it wraps to be globally unique across the entire device.
// ///   NOTE: This means that devices won't be able to choose a specific report ID that the host will see - it'll be assigned by this macro. If you want that level of fine control, you
// ///         may need to roll your own.  If this is a dealbreaker, please flag it and we can consider alternatives.
// /// - Routes incoming get_report/set_report/etc calls to the appropriate HidTopLevelCollection based on report ID
// /// - On power state / reset commands, dispatches the command to all child devices
// /// - On error from any child device, performs a device-initiated reset, which includes resetting all child devices.
// ///   NOTE: This implies that one misbehaving child device can take down the entire aggregate device.
// ///   DISCUSSION: Do we want to have a rate limit after which we mark a child device as "bad" and stop routing to it in order to avoid this?
// ///
// /// This is intended to behave very similarly to the impl_odp_mctp_relay_handler!() macro for MCTP.
// ///
// macro_rules! impl_odp_hid_aggregate_device {
//     (
//         $aggregate_type_name:ident; // The name of a new struct to be generate that implements HidDevice
//         $(
//             $hid_device_type:ty, // A list of concrete types that implement the HidDevice trait, which we will wrap
//         )+
//     ) => {
//         todo!()
//     }
// }
