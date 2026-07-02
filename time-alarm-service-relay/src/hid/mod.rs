// TODO rm before checkin
#![warn(warnings)]

use embedded_services::relay::hid::*; // TODO avoid wildcard?

type MaxReportSize = typenum::U20; // TODO figure out the actual max report size we need to support and set this accordingly

const MAX_PENDING_MESSAGES: usize = 10; // TODO maybe we should take this as a const generic parameter?

pub struct TimeAlarmHidRelay<
    T: time_alarm_service_interface::TimeAlarmService,
    M: embassy_sync::blocking_mutex::raw::RawMutex,
> {
    _service: T,
    channel: embassy_sync::channel::Channel<M, Result<HidReport<MaxReportSize>, HidError>, MAX_PENDING_MESSAGES>, // TODO figure out the right size for this buffer
    report_descriptor: HidReportDescriptor,
}

impl<T: time_alarm_service_interface::TimeAlarmService, M: embassy_sync::blocking_mutex::raw::RawMutex>
    TimeAlarmHidRelay<T, M>
{
    pub fn new(_service: T) -> Self {
        // Self {
        //     service,
        //     channel: embassy_sync::channel::Channel::new(),
        //     report_descriptor: todo!()
        // }
        todo!()
    }
}

impl<T: time_alarm_service_interface::TimeAlarmService, M: embassy_sync::blocking_mutex::raw::RawMutex> HidDevice
    for TimeAlarmHidRelay<T, M>
{
    // TODO for the static descriptor case, these should all be inferrable from the report descriptor.
    //      When we have the HID report support types implemented, see if we can have a 'ConstHidDevice'
    //      trait or something and then blanket implement 'HidDevice' for 'ConstHidDevice' that does this
    //      inference at compile time so we don't have to duplicate state in the 95% case, and then for the
    //      5% (passthrough) case we can implement 'HidDevice' directly and have these be maxima.
    type InputReportMaxSize = MaxReportSize; // TODO split per report once we figure out what the reports actually look like
    type OutputReportMaxSize = MaxReportSize;
    type FeatureReportMaxSize = MaxReportSize;
    const MAX_REPORT_COUNT: u8 = 10; // TODO figure out how many reports we actually need to support and set this accordingly

    type ReportReceiver<'a>
        = embassy_sync::channel::Receiver<
        'a,
        M,
        Result<HidReport<Self::InputReportMaxSize>, HidError>,
        MAX_PENDING_MESSAGES,
    >
    where
        Self: 'a; // TODO figure out the right size for this buffer

    fn report_descriptor(&self) -> &HidReportDescriptor {
        &self.report_descriptor
    }

    async fn get_report(
        &mut self,
        _report_type: GetHidReportType,
        _report_id: ReportId,
    ) -> Result<GetHidReport<Self::InputReportMaxSize, Self::FeatureReportMaxSize>, HidError> {
        todo!()
    }

    async fn set_report(
        &mut self,
        _report: &SetHidReport<Self::OutputReportMaxSize, Self::FeatureReportMaxSize>,
    ) -> Result<(), HidError> {
        todo!()
    }

    fn receiver(&mut self) -> Self::ReportReceiver<'_> {
        self.channel.receiver()
    }

    async fn set_power_state(&mut self, _state: HidDevicePowerState) -> Result<(), HidError> {
        Ok(())
    }

    async fn reset(&mut self) {
        // TODO Empty out tx queue
        todo!()
    }
}
