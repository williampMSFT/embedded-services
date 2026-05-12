//! HID relay code

// TODO remove before checkin
#![allow(unused_doc_comments)]
#![allow(dead_code)]
#![allow(missing_docs)]
#![allow(async_fn_in_trait)]

use generic_array::{ArrayLength, GenericArray};

// TODO read over comments and make sure they're still true when we go to check in
// TODO some of this may belong in some sort of external "HID support" library (e.g. stuff to manipulate HID descriptors)

/// A result-like type for HID operations. Reporting failure triggers a device-initiated reset, so we force anyone returning an error
/// to be explicit about it (i.e. not use ? operator).
pub enum HidResult<T> {
    /// The operation has completed successfully with the given result.
    Ok(T),
    /// The operation has failed and a device-initiated reset should be triggered.
    TriggerReset,
}

/// Power states that the host can command a HID device to be put into.
pub enum HidDevicePowerState {
    On,    // Normal operation
    Sleep, // Reduced power state, but a device that sends a report in this state can wake the host - quiesce messages if you don't want to do that
    Off,   // The device is not allowed to wake the host. I2C does not support this state - I3C and SPI do.
}

/// A HID report of no more than X bytes
pub struct HidReport<MaxSize: ArrayLength> {
    id: ReportId,

    data: GenericArray<u8, MaxSize>,
    valid_bytes: usize,
}

impl<MaxSize: ArrayLength> HidReport<MaxSize> {
    // TODO come up with a better error type for failure here
    pub fn new(id: ReportId, data: &[u8]) -> Result<Self, ()> {
        Ok(Self {
            id,
            data: GenericArray::try_from_slice(data).map_err(|_| ())?.clone(),
            valid_bytes: data.len(),
        })
    }

    pub fn id(&self) -> ReportId {
        self.id
    }

    pub fn data(&self) -> &[u8] {
        &self.data.as_slice().get(..self.valid_bytes).unwrap_or(&[])
    }
}

/// HID report types supported by the SetReport operation.
pub enum SetHidReport<OutputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> {
    Output(HidReport<OutputMaxSize>),
    Feature(HidReport<FeatureMaxSize>),
}

impl<OutputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> SetHidReport<OutputMaxSize, FeatureMaxSize> {
    pub fn data(&self) -> &[u8] {
        match self {
            SetHidReport::Output(report) => report.data(),
            SetHidReport::Feature(report) => report.data(),
        }
    }
}

/// HID report types supported by the GetReport operation.
pub enum GetHidReport<InputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> {
    Input(HidReport<InputMaxSize>),
    Feature(HidReport<FeatureMaxSize>),
}

impl<InputMaxSize: ArrayLength, FeatureMaxSize: ArrayLength> GetHidReport<InputMaxSize, FeatureMaxSize> {
    pub fn data(&self) -> &[u8] {
        match self {
            GetHidReport::Input(report) => report.data(),
            GetHidReport::Feature(report) => report.data(),
        }
    }
}

/// HID report ID
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// #[cfg_attr(feature = "defmt", derive(defmt::Format))]// TODO do this
pub struct ReportId(pub u8);

pub trait ReportReceiver<MaxSize: ArrayLength> {
    async fn ready_to_receive(&self);
    async fn receive(&self) -> HidResult<HidReport<MaxSize>>;
    fn is_empty(&self) -> bool;
}

impl<'ch, M: embassy_sync::blocking_mutex::raw::RawMutex, MaxSize: ArrayLength, const N: usize> ReportReceiver<MaxSize>
    for embassy_sync::channel::Receiver<'ch, M, HidResult<HidReport<MaxSize>>, N>
{
    async fn ready_to_receive(&self) -> () {
        self.ready_to_receive().await
    }
    async fn receive(&self) -> HidResult<HidReport<MaxSize>> {
        self.receive().await // TODO figure out if there's a way to do this without an .await since I think you end up double-awaiting
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
    /// The concrete type used for input reports (device -> host).
    /// Implementors typically set this to `HidReport<N>` for some `N`.
    type InputReportMaxSize: ArrayLength;

    /// The concrete type used for output reports (host -> device). // TODO update comments
    type OutputReportMaxSize: ArrayLength;

    /// The concrete type used for feature reports (bidirectional).
    type FeatureReportMaxSize: ArrayLength;

    /// The type that will surface HID reports as they become available.
    type ReportReceiver<'a>: ReportReceiver<Self::InputReportMaxSize>
    where
        Self: 'a;

    /// The maximum number of individual reports that the device will have.  In most cases, this should be exactly the number of
    /// reports that the device has, but in the passthrough case where that knowledge isn't available at compile time, this will
    /// be an upper bound.
    ///
    const MAX_REPORT_COUNT: u8;

    /// Returns the HID descriptor for this device. This isn't allowed to change, but the passthrough case means that
    /// we can't require that it be known at compile time.
    /// If the descriptor disagrees with the sizes implied by `InputReport` / `FeatureReport` / `OutputReport` / `MAX_REPORT_COUNT`, callers should not use the object. // TODO figure out if we can write a wrapper that verifies this in the type system, maybe a ValidatedHidDevice or something
    ///
    /// TODO it may be interesting to have a ConstHidDevice trait that has the same API but with the descriptor as an associated
    ///      const, and then have a blanket implementation of HidDevice for ConstHidDevice that derives these or something? Might
    ///      make usage more ergonomic in the non-passthrough case, which is likely to be more common. Can punt on that for now though.
    ///
    fn report_descriptor(&self) -> &HidReportDescriptor;

    /// Respond to an explicit request for a particular report from the host. You must fill `out` with the report data.
    async fn get_report(
        &mut self,
        report_id: ReportId,
    ) -> HidResult<GetHidReport<Self::InputReportMaxSize, Self::FeatureReportMaxSize>>; // TODO: I believe the Rust compiler will do RVO for this, but verify in compiler explorer

    /// Respond to a command from the host to handle a particular output/feature report.
    async fn set_report(
        &mut self,
        report: &SetHidReport<Self::OutputReportMaxSize, Self::FeatureReportMaxSize>,
    ) -> HidResult<()>;

    /// This is for 'unsolicited' reports - user is responsible for polling this thing and sending it up.
    /// This function blocks until a report is ready.
    // TODO either remove this or switch back to it
    // async fn next_report(&mut self) -> HidResult<Self::InputReport>; // TODO what if this trait just returned an embassy_sync::channel::Receiver? that would let us wait on it directly instead of needing two queues? Alternatively,  split into try_receive and ready_to_receive and have receive return an error if empty

    /// This is for 'unsolicited' reports - user is responsible for polling this and sending it up.
    fn receiver(&mut self) -> Self::ReportReceiver<'_>;

    /// Called when the host commands a particular power state.
    async fn set_power_state(&mut self, state: HidDevicePowerState) -> HidResult<()>;

    /// Called when the host commands a reset, or when a peer HidDevice in an aggregate triggers a device-initiated reset.
    async fn host_reset(&mut self);
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
pub struct HidReportDescriptor {
    bytes: &'static [u8] // TODO this probably doesn't work in the passthrough case; may need to be generic over a size or have some sort of buffer type or lifetime annotation or something
}

impl HidReportDescriptor {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes
    }

    // TODO this is a hack for bootstrap until we have a HID support library that can codegen one of these from a bunch of annotated structs or something.
    pub fn new_static(bytes: &'static [u8]) -> Self {
        // TODO validate that this is a well-formed HID report descriptor and that all reports are byte-aligned and whatnot - that'll be part of the hid support library.
        //      alternatively, could just get rid of this when we have the HID support library and have this codegenned or something
        Self { bytes }
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
