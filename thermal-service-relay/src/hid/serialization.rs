use embedded_services::relay::hid::ReportId;

const REPORT_VALUE_MASK: u32 = 0x7FFF_FFFF;
const REPORT_NULL_VALUE: u32 = REPORT_VALUE_MASK;
const REPORT_LOGICAL_MAX: u32 = REPORT_NULL_VALUE - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(crate) enum ReportError {
    InvalidLength,
    ValueOutOfRange,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, num_enum::IntoPrimitive, num_enum::TryFromPrimitive)]
pub(crate) enum InputReportId {
    FanSpeed = 1,
    SensorTemperature = 2,
}

impl TryFrom<ReportId> for InputReportId {
    type Error = num_enum::TryFromPrimitiveError<InputReportId>;

    fn try_from(value: ReportId) -> Result<Self, Self::Error> {
        Self::try_from(value.0)
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, num_enum::IntoPrimitive, num_enum::TryFromPrimitive)]
pub(crate) enum OutputReportId {
    FanSpeed = 1,
}

impl TryFrom<ReportId> for OutputReportId {
    type Error = num_enum::TryFromPrimitiveError<OutputReportId>;

    fn try_from(value: ReportId) -> Result<Self, Self::Error> {
        Self::try_from(value.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(crate) enum FanSpeed {
    Automatic,
    Rpm(u16),
}

pub(crate) fn serialize_fan_speed_report(rpm: u16) -> [u8; 4] {
    u32::from(rpm).to_le_bytes()
}

pub(crate) fn serialize_sensor_temperature_report(temperature_deci_kelvin: u32) -> [u8; 4] {
    temperature_deci_kelvin.min(REPORT_LOGICAL_MAX).to_le_bytes()
}

pub(crate) fn deserialize_fan_speed_report(data: &[u8]) -> Result<FanSpeed, ReportError> {
    let bytes = data
        .get(..4)
        .and_then(|data| data.try_into().ok())
        .ok_or(ReportError::InvalidLength)?;
    let value = u32::from_le_bytes(bytes) & REPORT_VALUE_MASK;

    if value == REPORT_NULL_VALUE {
        Ok(FanSpeed::Automatic)
    } else {
        u16::try_from(value)
            .map(FanSpeed::Rpm)
            .map_err(|_| ReportError::ValueOutOfRange)
    }
}

// Generated from Waratah - don't hand-edit this. Instead, modify the .wara file and rerun Waratah.
// See https://github.com/microsoft/hidtools for more information.
#[rustfmt::skip]
pub(crate) const THERMAL_HID_DESCRIPTOR: &[u8] = &[
    0x06, 0xED, 0xFF,                // UsagePage(Vendor Thermal Device[0xFFED])
    0x09, 0x01,                      // UsageId(Thermal Management Device[0x0001])
    0xA1, 0x01,                      // Collection(Application)
    0x85, 0x01,                      //     ReportId(1)
    0x09, 0x20,                      //     UsageId(Fan Speed[0x0020])
    0x15, 0x00,                      //     LogicalMinimum(0)
    0x27, 0xFF, 0xFF, 0xFF, 0x7F,  //     LogicalMaximum(2,147,483,647)
    0x95, 0x01,                      //     ReportCount(1)
    0x75, 0x1F,                      //     ReportSize(31)
    0x91, 0x42,                      //     Output(Data, Variable, Absolute, NoWrap, Linear, PreferredState, NullState, NonVolatile, BitField)
    0x75, 0x01,                      //     ReportSize(1)
    0x91, 0x03,                      //     Output(Constant, Variable, Absolute, NoWrap, Linear, PreferredState, NoNullPosition, NonVolatile, BitField)
    0x09, 0x20,                      //     UsageId(Fan Speed[0x0020])
    0x75, 0x1F,                      //     ReportSize(31)
    0x81, 0x42,                      //     Input(Data, Variable, Absolute, NoWrap, Linear, PreferredState, NullState, BitField)
    0x75, 0x01,                      //     ReportSize(1)
    0x81, 0x03,                      //     Input(Constant, Variable, Absolute, NoWrap, Linear, PreferredState, NoNullPosition, BitField)
    0x85, 0x02,                      //     ReportId(2)
    0x09, 0x21,                      //     UsageId(Sensor Temperature[0x0021])
    0x75, 0x1F,                      //     ReportSize(31)
    0x81, 0x42,                      //     Input(Data, Variable, Absolute, NoWrap, Linear, PreferredState, NullState, BitField)
    0x75, 0x01,                      //     ReportSize(1)
    0x81, 0x03,                      //     Input(Constant, Variable, Absolute, NoWrap, Linear, PreferredState, NoNullPosition, BitField)
    0xC0,                            // EndCollection()
];

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn reports_use_little_endian_31_bit_values() {
        assert_eq!(serialize_fan_speed_report(0x1234), [0x34, 0x12, 0, 0]);
        assert_eq!(
            serialize_sensor_temperature_report(REPORT_NULL_VALUE),
            REPORT_LOGICAL_MAX.to_le_bytes()
        );
    }

    #[test]
    fn fan_speed_report_supports_rpm_and_automatic_control() {
        assert_eq!(
            deserialize_fan_speed_report(&42_u32.to_le_bytes()).unwrap(),
            FanSpeed::Rpm(42)
        );
        assert_eq!(
            deserialize_fan_speed_report(&REPORT_NULL_VALUE.to_le_bytes()).unwrap(),
            FanSpeed::Automatic
        );
    }

    #[test]
    fn fan_speed_report_rejects_invalid_values_and_lengths() {
        assert_eq!(
            deserialize_fan_speed_report(&65_536_u32.to_le_bytes()),
            Err(ReportError::ValueOutOfRange)
        );
        assert_eq!(deserialize_fan_speed_report(&[0; 3]), Err(ReportError::InvalidLength));
    }
}
