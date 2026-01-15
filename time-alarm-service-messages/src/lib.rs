#![no_std]

mod acpi_timestamp;
pub use acpi_timestamp::{AcpiDaylightSavingsTimeStatus, AcpiTimeZone, AcpiTimestamp};
use bitfield::bitfield;
use embedded_services::relay::{MessageSerializationError, SerializableMessage};

// TODO let's not have two separate error types...
#[derive(Copy, Clone, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TimeAlarmCommandError {
    UnknownCommand,
    InvalidArgument,
    InvalidAcpiTimerId,
}

impl From<embedded_mcu_hal::time::DatetimeError> for TimeAlarmCommandError {
    fn from(_error: embedded_mcu_hal::time::DatetimeError) -> Self {
        TimeAlarmCommandError::InvalidArgument
    }
}

impl From<num_enum::TryFromPrimitiveError<AcpiDaylightSavingsTimeStatus>> for TimeAlarmCommandError {
    fn from(_error: num_enum::TryFromPrimitiveError<AcpiDaylightSavingsTimeStatus>) -> Self {
        TimeAlarmCommandError::InvalidArgument
    }
}

#[derive(num_enum::IntoPrimitive, num_enum::TryFromPrimitive, Copy, Clone, Debug, PartialEq)]
#[repr(u16)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum TimeAlarmCmdCode {
    GetCapabilities = 1,
    GetRealTime = 2,
    SetRealTime = 3,
    GetWakeStatus = 4,
    ClearWakeStatus = 5,
    SetTimerValue = 6,
    GetTimerValue = 7,
    SetExpiredTimerPolicy = 8,
    GetExpiredTimerPolicy = 9,
}

/// Message types for the ACPI Time and Alarm device service.
/// These directly analogous to the ACPI Time and Alarm device methods.
/// See ACPI Specification 6.4, Section 9.18 "Time and Alarm Device" for additional details on semantics.
#[rustfmt::skip]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(PartialEq, Clone, Copy)] // TODO it's not clear to me if we should actually derive Copy - we need to to be included in the Odp messaging enum, but we're a large struct and so is it...
pub enum AcpiTimeAlarmRequest {
    GetCapabilities,                                            // 1: _GCP --> u32 (bitmask),                 failure: infallible
    GetRealTime,                                                // 2: _GRT --> AcpiTimestamp,                 failure: valid bit = 0 in returned timestamp
    SetRealTime(AcpiTimestamp),                                 // 3: _SRT --> u32 (bool),                    failure: u32::MAX
    GetWakeStatus(AcpiTimerId),                                 // 4: _GWS --> u32 (bitmask),                 failure: infallible
    ClearWakeStatus(AcpiTimerId),                               // 5: _CWS --> u32 (bool),                    failure: 1
    SetTimerValue(AcpiTimerId, AlarmTimerSeconds),              // 6: _STV --> u32 (bool),                    failure: 1,
    GetTimerValue(AcpiTimerId),                                 // 7: _TIV --> u32 (AlarmTimerSeconds),       failure: infallible, u32::MAX if disabled
    SetExpiredTimerPolicy(AcpiTimerId, AlarmExpiredWakePolicy), // 8: _STP --> u32 (bool),                    failure: 1
    GetExpiredTimerPolicy(AcpiTimerId),                         // 9: _TIP --> u32 (AlarmExpiredWakePolicy)   failure: infallible
}

// TODO convert to trait impl
// impl AcpiTimeAlarmRequest {
//     fn from_bytes(command_code: TimeAlarmCmdCode, bytes: &[u8]) -> Result<Self, TimeAlarmCommandError> {
//         match command_code {
//             TimeAlarmCmdCode::GetCapabilities => Ok(AcpiTimeAlarmRequest::GetCapabilities),
//             TimeAlarmCmdCode::GetRealTime => Ok(AcpiTimeAlarmRequest::GetRealTime),
//             TimeAlarmCmdCode::SetRealTime => {
//                 Ok(AcpiTimeAlarmRequest::SetRealTime(AcpiTimestamp::try_from_bytes(bytes)?))
//             }
//             _ => {
//                 let (timer_id, bytes) = AcpiTimerId::try_from_bytes(bytes)?;
//                 match command_code {
//                     TimeAlarmCmdCode::GetWakeStatus => Ok(AcpiTimeAlarmRequest::GetWakeStatus(timer_id)),
//                     TimeAlarmCmdCode::ClearWakeStatus => Ok(AcpiTimeAlarmRequest::ClearWakeStatus(timer_id)),
//                     TimeAlarmCmdCode::SetTimerValue => Ok(AcpiTimeAlarmRequest::SetTimerValue(
//                         timer_id,
//                         AlarmTimerSeconds(u32::from_le_bytes(bytes.try_into()?)),
//                     )),
//                     TimeAlarmCmdCode::GetTimerValue => Ok(AcpiTimeAlarmRequest::GetTimerValue(timer_id)),
//                     TimeAlarmCmdCode::SetExpiredTimerPolicy => Ok(AcpiTimeAlarmRequest::SetExpiredTimerPolicy(
//                         timer_id,
//                         AlarmExpiredWakePolicy(u32::from_le_bytes(bytes.try_into()?)),
//                     )),
//                     TimeAlarmCmdCode::GetExpiredTimerPolicy => {
//                         Ok(AcpiTimeAlarmRequest::GetExpiredTimerPolicy(timer_id))
//                     }
//                     _ => Err(TimeAlarmCommandError::UnknownCommand),
//                 }
//             }
//         }
//     }
//     // TODO do we really need to_bytes?
// }

// TODO this seems like it should be unnecessary - we only need it to respond to messages because responses require a command field, but I don't see why they would. I think this is an artifact of having the request and response types stuffed in the same enum in the comms system? See if we can get rid of it
impl From<&AcpiTimeAlarmRequest> for TimeAlarmCmdCode {
    fn from(command: &AcpiTimeAlarmRequest) -> Self {
        match command {
            AcpiTimeAlarmRequest::GetCapabilities => TimeAlarmCmdCode::GetCapabilities,
            AcpiTimeAlarmRequest::GetRealTime => TimeAlarmCmdCode::GetRealTime,
            AcpiTimeAlarmRequest::SetRealTime(_) => TimeAlarmCmdCode::SetRealTime,
            AcpiTimeAlarmRequest::GetWakeStatus(_) => TimeAlarmCmdCode::GetWakeStatus,
            AcpiTimeAlarmRequest::ClearWakeStatus(_) => TimeAlarmCmdCode::ClearWakeStatus,
            AcpiTimeAlarmRequest::SetTimerValue(_, _) => TimeAlarmCmdCode::SetTimerValue,
            AcpiTimeAlarmRequest::GetTimerValue(_) => TimeAlarmCmdCode::GetTimerValue,
            AcpiTimeAlarmRequest::SetExpiredTimerPolicy(_, _) => TimeAlarmCmdCode::SetExpiredTimerPolicy,
            AcpiTimeAlarmRequest::GetExpiredTimerPolicy(_) => TimeAlarmCmdCode::GetExpiredTimerPolicy,
        }
    }
}

// -------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct AlarmTimerSeconds(pub u32);
impl AlarmTimerSeconds {
    pub const DISABLED: Self = Self(u32::MAX);
}

impl Default for AlarmTimerSeconds {
    fn default() -> Self {
        Self::DISABLED
    }
}

// -------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct AlarmExpiredWakePolicy(pub u32);
impl AlarmExpiredWakePolicy {
    #[allow(dead_code)]
    pub const INSTANTLY: Self = Self(0);
    pub const NEVER: Self = Self(u32::MAX);
}

impl Default for AlarmExpiredWakePolicy {
    fn default() -> Self {
        Self::NEVER
    }
}

// -------------------------------------------------

// Timer ID as defined in the ACPI spec.
#[derive(Debug, Clone, Copy, PartialEq, num_enum::TryFromPrimitive, num_enum::IntoPrimitive)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u32)]
pub enum AcpiTimerId {
    AcPower = 0,
    DcPower = 1,
}

impl AcpiTimerId {
    pub fn get_other_timer_id(&self) -> Self {
        match self {
            AcpiTimerId::AcPower => AcpiTimerId::DcPower,
            AcpiTimerId::DcPower => AcpiTimerId::AcPower,
        }
    }
}

bitfield!(
    #[derive(Copy, Clone, Default, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct TimerStatus(u32);
    impl Debug;
    bool;
    pub timer_expired, set_timer_expired: 0;
    pub timer_triggered_wake, set_timer_triggered_wake: 1;
);

// -------------------------------------------------

bitfield!(
    #[derive(Copy, Clone, Default, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct TimeAlarmDeviceCapabilities(u32);
    impl Debug;
    bool;
    pub ac_wake_implemented, set_ac_wake_implemented: 0;
    pub dc_wake_implemented, set_dc_wake_implemented: 1;
    pub realtime_implemented, set_realtime_implemented: 2;
    pub realtime_accuracy_in_milliseconds, set_realtime_accuracy_in_milliseconds: 3;
    pub get_wake_status_supported, set_get_wake_status_supported: 4;
    pub ac_s4_wake_supported, set_ac_s4_wake_supported: 5;
    pub ac_s5_wake_supported, set_ac_s5_wake_supported: 6;
    pub dc_s4_wake_supported, set_dc_s4_wake_supported: 7;
    pub dc_s5_wake_supported, set_dc_s5_wake_supported: 8;
);

// -------------------------------------------------

// TODO It's not clear to me if this should be a few options for Result instead - I'm unclear on how that would interact
//      with the serialization logic, but it seems like it'd be nice if we could use Result instead?
//      something like type TimeAlarmCommandResult = Result<AcpiTimeAlarmResponse, TimeAlarmCommandError>;
//      and then remove OperationFailed?
//
//      Alternatively, maybe these shouldn't be in an enum at all - maybe they should all be distinct types and we should
//      just require that everything on the bus implement some Serializable trait for relay to the host?
//
#[derive(Copy, Clone, PartialEq)]
// TODO it's not clear to me if we should actually derive Copy - we need to to be included in the Odp messaging enum, but we're a large struct and so is it...
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum AcpiTimeAlarmResponse {
    Capabilities(TimeAlarmDeviceCapabilities),
    RealTime(AcpiTimestamp),
    TimerStatus(TimerStatus),
    WakePolicy(AlarmExpiredWakePolicy),
    TimerSeconds(AlarmTimerSeconds),

    /// Operation succeeded, but there's no data to return.
    OkNoData,
}

// TODO trait impl
impl AcpiTimeAlarmResponse {
    fn u32_to_bytes(value: u32, buffer: &mut [u8]) -> Result<usize, TimeAlarmCommandError> {
        let result = value.to_le_bytes();
        buffer
            .split_at_mut_checked(result.len())
            .ok_or(TimeAlarmCommandError::InvalidArgument)?
            .0
            .copy_from_slice(&result);
        Ok(result.len())
    }

    pub fn to_bytes(&self, buffer: &mut [u8]) -> Result<usize, TimeAlarmCommandError> {
        // TODO It's not clear to me how error reporting is meant to work here - the comms system has a facility for reporting a failure status, but I can't find any evidence that it actually gets put into the packet that we receive on the other side. We may need to make space for a return code in here if that facility is not viable for reporting errors.
        match self {
            Self::Capabilities(capabilities) => Self::u32_to_bytes(capabilities.0, buffer),
            Self::RealTime(timestamp) => {
                let result = timestamp.as_bytes();
                buffer
                    .split_at_mut_checked(result.len())
                    .ok_or(TimeAlarmCommandError::InvalidArgument)?
                    .0
                    .copy_from_slice(&result);
                Ok(result.len())
            }
            Self::TimerStatus(timer_status) => Self::u32_to_bytes(timer_status.0, buffer),
            Self::WakePolicy(wake_policy) => Self::u32_to_bytes(wake_policy.0, buffer),
            Self::TimerSeconds(timer_seconds) => Self::u32_to_bytes(timer_seconds.0, buffer),
            Self::OkNoData => Ok(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum AcpiTimeAlarmError {
    GenericFailure,
}

pub type AcpiTimeAlarmResult = Result<AcpiTimeAlarmResponse, AcpiTimeAlarmError>;

// TODO implement these, move to be with their types
impl SerializableMessage for AcpiTimeAlarmRequest {
    fn serialize(self, _buffer: &mut [u8]) -> Result<usize, MessageSerializationError> {
        todo!()
    }

    fn discriminant(&self) -> u16 {
        todo!()
    }

    fn deserialize(_discriminant: u16, _buffer: &[u8]) -> Result<Self, MessageSerializationError> {
        todo!()
    }
}

impl SerializableMessage for AcpiTimeAlarmResponse {
    fn serialize(self, _buffer: &mut [u8]) -> Result<usize, MessageSerializationError> {
        todo!()
    }

    fn discriminant(&self) -> u16 {
        todo!()
    }

    fn deserialize(_discriminant: u16, _buffer: &[u8]) -> Result<Self, MessageSerializationError> {
        todo!()
    }
}

impl SerializableMessage for AcpiTimeAlarmError {
    fn serialize(self, _buffer: &mut [u8]) -> Result<usize, MessageSerializationError> {
        todo!()
    }

    fn discriminant(&self) -> u16 {
        todo!()
    }

    fn deserialize(_discriminant: u16, _buffer: &[u8]) -> Result<Self, MessageSerializationError> {
        todo!()
    }
}
