#![no_std]

// TODO clean these up before checkin
use core::cell::RefCell;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use embassy_sync::once_lock::OnceLock;
use embassy_sync::signal::Signal;
use embedded_mcu_hal::NvramStorage;
use embedded_mcu_hal::time::{Datetime, DatetimeClock, DatetimeClockError};
use embedded_services::ec_type::message::OdpCommand;
use embedded_services::ec_type::message::{StdHostMsg, StdHostPayload, StdHostRequest};
use embedded_services::ec_type::protocols::mctp::Odp::TimeAlarmCommand;
use embedded_services::{GlobalRawMutex, comms::MailboxDelegateError};
use embedded_services::{comms, error, info, warn};
use time_alarm_service_messages::*;

mod timer;
use timer::Timer;

// -------------------------------------------------

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TimeAlarmError {
    UnknownCommand,
    DoubleInitError,
    MailboxFullError,
    ClockError(DatetimeClockError),
}

impl From<TimeAlarmError> for MailboxDelegateError {
    fn from(error: TimeAlarmError) -> Self {
        match error {
            TimeAlarmError::UnknownCommand => MailboxDelegateError::InvalidData,
            TimeAlarmError::DoubleInitError => {
                panic!("Should never attempt intitialization as a response to receiving a mailbox message")
            }
            TimeAlarmError::MailboxFullError => MailboxDelegateError::BufferFull,
            TimeAlarmError::ClockError(_) => MailboxDelegateError::Other,
        }
    }
}

impl From<DatetimeClockError> for TimeAlarmError {
    fn from(e: DatetimeClockError) -> Self {
        TimeAlarmError::ClockError(e)
    }
}

impl From<embedded_services::intrusive_list::Error> for TimeAlarmError {
    fn from(_error: embedded_services::intrusive_list::Error) -> Self {
        TimeAlarmError::DoubleInitError
    }
}

// -------------------------------------------------

mod time_zone_data {
    use crate::AcpiDaylightSavingsTimeStatus;
    use crate::AcpiTimeZone;
    use crate::NvramStorage;

    pub struct TimeZoneData {
        // Storage used to back the timezone and DST settings.
        storage: &'static mut dyn NvramStorage<'static, u32>,
    }

    #[repr(C)]
    #[derive(bytemuck::Pod, bytemuck::Zeroable, Copy, Clone, Debug)]
    struct RawTimeZoneData {
        tz: i16,
        dst: u8,
        _padding: u8, // padding to make the struct 4 bytes
    }

    impl TimeZoneData {
        pub fn new(storage: &'static mut dyn NvramStorage<'static, u32>) -> Self {
            Self { storage }
        }

        /// Writes the given time zone and daylight savings time status to NVRAM.
        ///
        pub fn set_data(&mut self, tz: AcpiTimeZone, dst: AcpiDaylightSavingsTimeStatus) {
            let representation = RawTimeZoneData {
                tz: tz.into(),
                dst: dst.into(),
                _padding: 0,
            };

            self.storage.write(bytemuck::cast(representation));
        }

        /// Retreives the current time zone / daylight savings time.
        /// If the stored data is invalid, implying that the NVRAM has never been initialized, defaults to
        /// (AcpiTimeZone::Unknown, AcpiDaylightSavingsTimeStatus::NotObserved).
        ///
        pub fn get_data(&self) -> (AcpiTimeZone, AcpiDaylightSavingsTimeStatus) {
            let representation: RawTimeZoneData = bytemuck::cast(self.storage.read());
            (|| -> Result<(AcpiTimeZone, AcpiDaylightSavingsTimeStatus), time_alarm_service_messages::TimeAlarmCommandError> {
                Ok((representation.tz.try_into()?, representation.dst.try_into()?))
            })()
            .unwrap_or_else(|_| (AcpiTimeZone::Unknown, AcpiDaylightSavingsTimeStatus::NotObserved))
        }
    }
}
use time_zone_data::TimeZoneData;

// -------------------------------------------------

struct ClockState {
    datetime_clock: &'static mut dyn DatetimeClock,
    tz_data: TimeZoneData,
}

// -------------------------------------------------

struct Timers {
    ac_timer: Timer,
    dc_timer: Timer,
}

impl Timers {
    fn get_timer(&self, timer: AcpiTimerId) -> &Timer {
        match timer {
            AcpiTimerId::AcPower => &self.ac_timer,
            AcpiTimerId::DcPower => &self.dc_timer,
        }
    }

    fn new(
        ac_expiration_storage: &'static mut dyn NvramStorage<'static, u32>,
        ac_policy_storage: &'static mut dyn NvramStorage<'static, u32>,
        dc_expiration_storage: &'static mut dyn NvramStorage<'static, u32>,
        dc_policy_storage: &'static mut dyn NvramStorage<'static, u32>,
    ) -> Self {
        Self {
            ac_timer: Timer::new(ac_expiration_storage, ac_policy_storage),
            dc_timer: Timer::new(dc_expiration_storage, dc_policy_storage),
        }
    }
}

// -------------------------------------------------

pub struct Service {
    endpoint: comms::Endpoint,

    // ACPI messages from the host are sent through this channel.
    acpi_channel: Channel<GlobalRawMutex, (comms::EndpointID, AcpiTimeAlarmDeviceCommand), 10>,

    clock_state: Mutex<GlobalRawMutex, RefCell<ClockState>>,

    // TODO [POWER_SOURCE] signal this whenever the power source changes
    power_source_signal: Signal<GlobalRawMutex, AcpiTimerId>,

    timers: Timers,

    capabilities: TimeAlarmDeviceCapabilities,
}

impl Service {
    // TODO [DYN] if we want to allow taking the HAL traits as concrete types rather than as dyn references, we'll likely need to make this a macro
    //      in order to accommodate the restriction that embassy tasks can't have generic parameters. When we do that, it may be worthwhile to
    //      also investigate ways to take the backing storage as a slice rather than as a bunch of individual references - currently, we can't
    //      take a slice of the array because that would be a slice of trait impls and we need dyn references here to accommodate the constraints
    //      on embassy task implementation.
    //
    pub async fn init(
        service_storage: &'static OnceLock<Service>,
        spawner: &embassy_executor::Spawner,
        backing_clock: &'static mut impl DatetimeClock,
        tz_storage: &'static mut dyn NvramStorage<'static, u32>,
        ac_expiration_storage: &'static mut dyn NvramStorage<'static, u32>,
        ac_policy_storage: &'static mut dyn NvramStorage<'static, u32>,
        dc_expiration_storage: &'static mut dyn NvramStorage<'static, u32>,
        dc_policy_storage: &'static mut dyn NvramStorage<'static, u32>,
    ) -> Result<(), TimeAlarmError> {
        info!("Starting time-alarm service task");

        let service = service_storage.get_or_init(|| Service {
            endpoint: comms::Endpoint::uninit(comms::EndpointID::Internal(comms::Internal::TimeAlarm)),
            acpi_channel: Channel::new(),
            clock_state: Mutex::new(RefCell::new(ClockState {
                datetime_clock: backing_clock,
                tz_data: TimeZoneData::new(tz_storage),
            })),
            power_source_signal: Signal::new(),
            timers: Timers::new(
                ac_expiration_storage,
                ac_policy_storage,
                dc_expiration_storage,
                dc_policy_storage,
            ),
            capabilities: {
                // TODO [CONFIG] We could consider making some of these user-configurable, e.g. if we want to support devices that don't have a battery
                let mut caps = TimeAlarmDeviceCapabilities(0);
                caps.set_ac_wake_implemented(true);
                caps.set_dc_wake_implemented(true);
                caps.set_realtime_implemented(true);
                caps.set_realtime_accuracy_in_milliseconds(false);
                caps.set_get_wake_status_supported(true);
                caps.set_ac_s4_wake_supported(true);
                caps.set_ac_s5_wake_supported(true);
                caps.set_dc_s4_wake_supported(true);
                caps.set_dc_s5_wake_supported(true);
                caps
            },
        });

        // TODO [POWER_SOURCE] we need to subscribe to messages that tell us if we're on AC or DC power so we can decide which alarms to trigger - how do we do that?
        // TODO [POWER_SOURCE] if it's possible to learn which power source is active at init time, we should set that one active rather than defaulting to the AC timer.
        service.timers.ac_timer.start(&service.clock_state, true);
        service.timers.dc_timer.start(&service.clock_state, false);

        comms::register_endpoint(service, &service.endpoint).await?;

        spawner.must_spawn(command_handler_task(service));
        spawner.must_spawn(timer_task(service, AcpiTimerId::AcPower));
        spawner.must_spawn(timer_task(service, AcpiTimerId::DcPower));

        Ok(())
    }

    pub async fn handle_requests(&'static self) {
        loop {
            let acpi_command = self.acpi_channel.receive();
            let power_source_change = self.power_source_signal.wait();

            match select(acpi_command, power_source_change).await {
                Either::First((respond_to_endpoint, acpi_command)) => {
                    match self.handle_acpi_command(acpi_command).await {
                        Ok(response) => {
                            // TODO [COMMS] it seems like we're sort of conflating wire representation with message representation here -
                            //      is this really how we want to pass messages through the comms system? It seems like it makes it
                            //      harder for other services to send messages to us - we're obligated to serialize/deserialize messages
                            //      whenever we send them to another subsystem on the MCU rather than just passing around strongly-typed
                            //      objects.  We may want to consider changing the comms system to allow passing strongly-typed objects and
                            //      perhaps a trait that indicates if it's serializable for an off-system transport like eSPI?
                            //
                            const STATUS_SUCCEEDED: u8 = 0;
                            let request = StdHostRequest {
                                command: OdpCommand::TimeAlarm((&acpi_command).into()), // TODO is this right?
                                status: STATUS_SUCCEEDED,
                                payload: StdHostPayload::TimeAlarmResponse(response), // TODO it's weird to me that we have a status and an 'error response' message type - is this the right shape for the comm system?
                            };
                            self.endpoint
                                .send(respond_to_endpoint, &StdHostMsg::Response(request))
                                .await
                                .expect("send returns Infallible");
                        }
                        Err(e) => {
                            error!("Error handling ACPI command: {:?}", e);
                            const STATUS_FAILED: u8 = 1;
                            let request = StdHostRequest {
                                command: OdpCommand::TimeAlarm((&acpi_command).into()), // TODO is this right? It seems odd to me that we'd need the request type in the header - either we should enforce FIFO ordering for requests or we should have a unique request ID rather than just a "kind of request" tag
                                status: STATUS_FAILED,
                                payload: StdHostPayload::ErrorResponse {}, // TODO it's weird to me that we have a status and an 'error response' message type - is this the right shape for the comm system?
                            };
                            self.endpoint
                                .send(respond_to_endpoint, &StdHostMsg::Response(request))
                                .await
                                .expect("send returns Infallible");
                        }
                    }
                }
                Either::Second(new_power_source) => {
                    info!("Power source changed to {:?}", new_power_source);

                    self.timers
                        .get_timer(new_power_source.get_other_timer_id())
                        .set_active(&self.clock_state, false);
                    self.timers
                        .get_timer(new_power_source)
                        .set_active(&self.clock_state, true);
                }
            }
        }
    }

    pub async fn handle_timer(&'static self, timer_id: AcpiTimerId) {
        let timer = self.timers.get_timer(timer_id);
        loop {
            timer.wait_until_wake(&self.clock_state).await;
            // TODO [SPEC] section 9.18.7 indicates that when a timer expires, both timers have their wake policies reset,
            //      but I can't find any similar rule for the actual timer value - that seems odd to me, verify that's actually how
            //      it's supposed to work
            self.timers
                .get_timer(timer_id.get_other_timer_id())
                .set_timer_wake_policy(&self.clock_state, AlarmExpiredWakePolicy::NEVER);

            warn!(
                "Timer {:?} expired and would trigger a wake now, but the power service is not yet implemented so will currently do nothing",
                timer_id
            );
            // TODO [COMMS] Figure out how to signal a wake event to the host and do that here
        }
    }

    async fn handle_acpi_command(
        &'static self,
        command: AcpiTimeAlarmDeviceCommand,
    ) -> Result<AcpiTimeAlarmCommandResponse, TimeAlarmError> {
        info!("Received Time-Alarm Device command: {:?}", command);
        match command {
            AcpiTimeAlarmDeviceCommand::GetCapabilities => {
                Ok(AcpiTimeAlarmCommandResponse::Capabilities(self.capabilities))
            }
            AcpiTimeAlarmDeviceCommand::GetRealTime => self.clock_state.lock(|clock_state| {
                let clock_state = clock_state.borrow();
                let datetime = clock_state.datetime_clock.get_current_datetime()?;
                let (time_zone, dst_status) = clock_state.tz_data.get_data();
                Ok(AcpiTimeAlarmCommandResponse::RealTime(AcpiTimestamp {
                    datetime,
                    time_zone,
                    dst_status,
                }))
            }),
            AcpiTimeAlarmDeviceCommand::SetRealTime(timestamp) => {
                self.clock_state.lock(|clock_state| {
                    let mut clock_state = clock_state.borrow_mut();
                    clock_state.datetime_clock.set_current_datetime(&timestamp.datetime)?;
                    clock_state.tz_data.set_data(timestamp.time_zone, timestamp.dst_status);

                    // TODO [SPEC] the spec is ambiguous on whether or not we should adjust any outstanding timers based on the new time - see if we can find an answer elsewhere
                    Ok(AcpiTimeAlarmCommandResponse::OkNoData)
                })
            }
            AcpiTimeAlarmDeviceCommand::GetWakeStatus(timer_id) => {
                let status = self.timers.get_timer(timer_id).get_wake_status();
                Ok(AcpiTimeAlarmCommandResponse::TimerStatus(status))
            }
            AcpiTimeAlarmDeviceCommand::ClearWakeStatus(timer_id) => {
                self.timers.get_timer(timer_id).clear_wake_status();
                Ok(AcpiTimeAlarmCommandResponse::OkNoData)
            }
            AcpiTimeAlarmDeviceCommand::SetExpiredTimerPolicy(timer_id, timer_policy) => {
                self.timers
                    .get_timer(timer_id)
                    .set_timer_wake_policy(&self.clock_state, timer_policy);
                Ok(AcpiTimeAlarmCommandResponse::OkNoData)
            }
            AcpiTimeAlarmDeviceCommand::SetTimerValue(timer_id, timer_value) => {
                let new_expiration_time = match timer_value {
                    AlarmTimerSeconds::DISABLED => None,
                    AlarmTimerSeconds(secs) => {
                        let current_time = self
                            .clock_state
                            .lock(|clock_state| clock_state.borrow().datetime_clock.get_current_datetime())?;

                        Some(Datetime::from_unix_time_seconds(
                            current_time.to_unix_time_seconds() + u64::from(secs),
                        ))
                    }
                };

                self.timers
                    .get_timer(timer_id)
                    .set_expiration_time(&self.clock_state, new_expiration_time);
                Ok(AcpiTimeAlarmCommandResponse::OkNoData)
            }
            AcpiTimeAlarmDeviceCommand::GetExpiredTimerPolicy(timer_id) => Ok(
                AcpiTimeAlarmCommandResponse::WakePolicy(self.timers.get_timer(timer_id).get_timer_wake_policy()),
            ),
            AcpiTimeAlarmDeviceCommand::GetTimerValue(timer_id) => {
                let expiration_time = self.timers.get_timer(timer_id).get_expiration_time();

                let timer_wire_format = match expiration_time {
                    Some(expiration_time) => {
                        let current_time = self
                            .clock_state
                            .lock(|clock_state| clock_state.borrow().datetime_clock.get_current_datetime())?;

                        AlarmTimerSeconds(expiration_time.to_unix_time_seconds().saturating_sub(current_time.to_unix_time_seconds()).try_into().expect("Per the ACPI spec, timers are communicated in u32 seconds, so this shouldn't be able to overflow"))
                    }
                    None => AlarmTimerSeconds::DISABLED,
                };

                Ok(AcpiTimeAlarmCommandResponse::TimerSeconds(timer_wire_format))
            }
        }
    }
}

impl comms::MailboxDelegate for Service {
    fn receive(&self, message: &comms::Message) -> Result<(), comms::MailboxDelegateError> {
        info!("Received message at time-alarm-service");

        if let Some(acpi_cmd) = message.data.get::<StdHostRequest>()
            && let TimeAlarmCommand(command) = acpi_cmd.payload
        {
            self.acpi_channel
                .try_send((message.from, command))
                .map_err(|_| MailboxDelegateError::BufferFull)?;
            Ok(())
        } else {
            // TODO [COMMS] right now, if pushing the message to the channel fails, the error that we return this gets
            //              discarded by our caller and we have no opportunity to raise a failure. Fixing that probably
            //              requires changes in the mailbox system, so we're ignoring it for now.
            Err(comms::MailboxDelegateError::InvalidData)
        }
    }
}

#[embassy_executor::task]
async fn command_handler_task(service: &'static Service) {
    info!("Starting time-alarm service task");
    service.handle_requests().await;
}

#[embassy_executor::task(pool_size = 2)]
async fn timer_task(service: &'static Service, timer_id: AcpiTimerId) {
    info!("Starting time-alarm timer task");
    service.handle_timer(timer_id).await;
}
