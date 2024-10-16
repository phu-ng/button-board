mod wifi;
mod mqtt;

use std::ffi::CString;
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{InterruptType, PinDriver};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver, I2cError};
use esp_idf_svc::hal::prelude::{Hertz, Peripherals};
use esp_idf_svc::hal::task::notification::{Notification, Notifier};
use esp_idf_svc::hal::delay;
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use hd44780_driver::bus::I2CBus;
use hd44780_driver::{Cursor, CursorBlink, Display, DisplayMode, HD44780};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use shared_bus::{I2cProxy, NullMutex};
use ds323x::{Alarm1Matching, Alarm2Matching, DateTimeAccess, DayAlarm1, DayAlarm2, Ds323x, Error, Hours, Rtcc};
use ds323x::ic::DS3231;
use ds323x::interface::I2cInterface;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::mqtt::client::{EspMqttClient, EspMqttConnection, EventPayload, MqttClientConfiguration};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{nvs_flash_init, EspError};
use esp_idf_svc::tls::X509;
use esp_idf_svc::wifi::WifiEvent;
use log::{error, info};

const ADDRESS: u8 = 0x27;
static THREAD_SIZE: usize = 2000;
static BUTTON_A_NOTICE: AtomicBool = AtomicBool::new(false);
static TEMP: AtomicU8 = AtomicU8::new(30);
static HUMID: AtomicU8 = AtomicU8::new(70);

#[toml_cfg::toml_config]
pub struct AppConfig {
    #[default("PhuNetwork")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
    #[default("")]
    mqtt_url: &'static str,
    #[default("")]
    mqtt_user: &'static str,
    #[default("")]
    mqtt_password: &'static str,
    #[default("")]
    mqtt_temp_topic: &'static str,
    #[default("")]
    mqtt_humid_topic: &'static str,
}

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Start application");

    // Load config
    let app_config: AppConfig = APP_CONFIG;

    unsafe {
        nvs_flash_init();
        log::info!("init nvs flash");
    };

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let sda = peripherals.pins.gpio10;
    let scl = peripherals.pins.gpio11;
    let button_a = peripherals.pins.gpio23;
    let sqw_pin = peripherals.pins.gpio7;

    // Init sqw input for ds3231
    let mut sqw = PinDriver::input(sqw_pin)?;
    sqw.set_interrupt_type(InterruptType::NegEdge)?;

    let mut i2c_config = I2cConfig::new();
    i2c_config.baudrate = Hertz(100 * 1000); // 100kHz
    let i2c_driver = I2cDriver::new(peripherals.i2c0, sda, scl, &i2c_config)?;
    let bus = shared_bus::BusManagerSimple::new(i2c_driver);

    // Init RTC module
    let mut rtc = Ds323x::new_ds3231(bus.acquire_i2c());
    rtc.use_int_sqw_output_as_interrupt().unwrap();
    rtc.enable_alarm2_interrupts().unwrap();

    // Init LCD module
    let mut lcd = HD44780::new_i2c(bus.acquire_i2c(), ADDRESS, &mut FreeRtos).unwrap();
    lcd.reset(&mut FreeRtos).unwrap();
    lcd.clear(&mut FreeRtos).unwrap();
    lcd.set_display_mode(
        DisplayMode {
            display: Display::On,
            cursor_visibility: Cursor::Invisible,
            cursor_blink: CursorBlink::Off,
        },
        &mut FreeRtos,
    )
        .unwrap();
    display_clock(&mut lcd, rtc.datetime().unwrap(), get_temp(), get_temp())?;

    // Init wifi
    let mut wifi = wifi::wifi(
        app_config.wifi_ssid,
        app_config.wifi_psk,
        peripherals.modem,
        sys_loop.clone(),
        nvs,
    )?;

    // Create Handle and Configure SNTP
    let ntp = EspSntp::new_default()?;
    for _i in 0..5 {
        match ntp.get_sync_status() {
            SyncStatus::Reset => {
                log::info!("reset");
                FreeRtos::delay_ms(2000);
                continue;
            }
            SyncStatus::Completed => {
                log::info!("complete");
                let now = get_current_time();
                let dt = NaiveDate::from_ymd_opt(now.year(), now.month(), now.day()).unwrap()
                    .and_hms_opt(now.hour(), now.minute(), now.second()).unwrap();
                rtc.set_datetime(&dt).unwrap();
                break;
            }
            SyncStatus::InProgress => {
                log::info!("In progress");
                FreeRtos::delay_ms(1000);
                continue;
            }
        }
    }

    // Assign interrupt button
    let mut button_a_driver = PinDriver::input(button_a)?;
    button_a_driver.set_interrupt_type(InterruptType::PosEdge)?;

    // Create notification
    let notification = Notification::new();
    let notifier = notification.notifier();
    let button_a_notifier = Arc::clone(&notifier);
    let sqw_notifier = Arc::clone(&notifier);

    // Safety: make sure the `Notification` object is not dropped while the subscription is active
    unsafe {
        sqw.subscribe(move || handle_sqw_notice(&sqw_notifier))?;
        button_a_driver.subscribe(move || handle_button_a(&button_a_notifier))?;
    }

    handle_alarm_every_minute(&mut rtc);
    // handle_alarm_ntp_sync(&mut rtc, &ntp);

    // FreeRtos::delay_ms(5000);
    // sys_loop.subscribe::<WifiEvent, _>(move |wifi_event| {
    //     log::info!("some kind of wifi event {:?}", wifi_event)
    // })?;

    // Subcribe to Mqtt
    let (mut mqtt_client, mut conn) = mqtt::init(app_config.mqtt_url,
                                         "bb",
                                         app_config.mqtt_user,
                                         app_config.mqtt_password).unwrap();
    thread::Builder::new()
        .stack_size(6000)
        .spawn(move || {
            info!("MQTT Listening for messages");
            while let Ok(event) = conn.next() {
                match event.payload() {
                    EventPayload::Received { topic, data, .. } => {
                        if topic.is_none() {
                            info!("ignore unknown topic")
                        }

                        if topic.unwrap() == app_config.mqtt_temp_topic {
                            info!("updated temp is {}", convert_event_data(data).unwrap());
                            TEMP.store(convert_event_data(data).unwrap(), Ordering::SeqCst);
                            info!("Atomic temp is {}", TEMP.load(Ordering::SeqCst));
                        }
                        if topic.unwrap() == app_config.mqtt_humid_topic {
                            HUMID.store(convert_event_data(data).unwrap(), Ordering::SeqCst);
                        }
                        // info!("event has data {data}")
                    }
                    _ => {}
                }
                // info!("[Queue] Event: {}", event.payload());
            }
            info!("Connection closed");
        }).unwrap();

    mqtt::subscribes(&mut mqtt_client, app_config.mqtt_temp_topic, app_config.mqtt_humid_topic);

    info!("after");
    loop {
        // enable_interrupt should also be called after each received notification from non-ISR context
        sqw.enable_interrupt()?;
        button_a_driver.enable_interrupt()?;
        notification.wait(delay::BLOCK);

        FreeRtos::delay_ms(100);

        if rtc.has_alarm2_matched().unwrap() {
            handle_alarm_every_minute(&mut rtc);
        }
        // if rtc.has_alarm1_matched().unwrap() {
        //     handle_alarm_ntp_sync(&mut rtc, &ntp);
        // }
        if BUTTON_A_NOTICE.load(Ordering::SeqCst) {
            info!("button a is pressed");
            display_message(&mut lcd, "TURN ON/OFF AC", "")?;
            FreeRtos::delay_ms(2000);
            BUTTON_A_NOTICE.store(false, Ordering::SeqCst);
        }
        if !wifi.is_connected()? {
            info!("wifi is down. reconnecting");
            wifi.connect()?;
            FreeRtos::delay_ms(5000);
        }

        info!("temp is {}", get_temp());
        display_clock(&mut lcd, rtc.datetime().unwrap(), get_temp(), get_temp())?;
    }
}

fn handle_button_a(notifier: &Arc<Notifier>) {
    BUTTON_A_NOTICE.store(true, Ordering::SeqCst);
    // Move the logic from the closure here
    unsafe { notifier.notify_and_yield(NonZeroU32::new(1).unwrap()) };
}

fn handle_sqw_notice(notifier: &Arc<Notifier>) {
    unsafe { notifier.notify_and_yield(NonZeroU32::new(1).unwrap()) };
}

fn get_current_time() -> DateTime<FixedOffset> {
    let vn_offset = FixedOffset::east_opt(7 * 3600).unwrap();
    // Obtain System Time
    let now = Utc::now().with_timezone(&vn_offset);
    // Print Time
    now
}

fn pad_single_digit(num: u32) -> String {
    if num < 10 {
        format!("0{}", num)
    } else {
        num.to_string()
    }
}

fn month_to_abbreviation(month: u32) -> &'static str {
    match month {
        1 => "JAN",
        2 => "FEB",
        3 => "MAR",
        4 => "APR",
        5 => "MAY",
        6 => "JUN",
        7 => "JUL",
        8 => "AUG",
        9 => "SEP",
        10 => "OCT",
        11 => "NOV",
        12 => "DEC",
        _ => "Invalid", // Handle invalid month numbers
    }
}

fn display_clock(
    lcd: &mut HD44780<I2CBus<I2cProxy<NullMutex<I2cDriver>>>>,
    date_time: NaiveDateTime,
    temp: u8,
    humid: u8
) -> anyhow::Result<()> {
    let hour = pad_single_digit(date_time.hour());
    let minute = pad_single_digit(date_time.minute());
    let day = pad_single_digit(date_time.day());
    let month = month_to_abbreviation(date_time.month());
    let year = (date_time.year() % 100).to_string();

    let first_line = format!("{}:{}  {} {} {}", hour, minute, day, month, year);
    let second_line = format!("T {}C   H {}%", pad_single_digit(temp as u32), pad_single_digit(humid as u32));

    lcd.set_cursor_pos(0, &mut FreeRtos).unwrap();
    lcd.write_str(&first_line, &mut FreeRtos).unwrap();
    lcd.set_cursor_pos(40, &mut FreeRtos).unwrap();
    lcd.write_str(&second_line, &mut FreeRtos).unwrap();

    Ok(())
}

fn display_message(
    lcd: &mut HD44780<I2CBus<I2cProxy<NullMutex<I2cDriver>>>>,
    line_1: &str,
    line_2: &str,
) -> anyhow::Result<()> {
    lcd.clear(&mut FreeRtos).unwrap();
    lcd.set_cursor_pos(0, &mut FreeRtos).unwrap();
    lcd.write_str(line_1, &mut FreeRtos).unwrap();
    lcd.set_cursor_pos(40, &mut FreeRtos).unwrap();
    lcd.write_str(line_2, &mut FreeRtos).unwrap();

    Ok(())
}

fn handle_alarm_every_minute(rtc: &mut Ds323x<I2cInterface<I2cProxy<NullMutex<I2cDriver>>>, DS3231>) {
    let opm = Alarm2Matching::OncePerMinute;

    rtc.clear_alarm2_matched_flag().unwrap();
    rtc.set_alarm2_day(DayAlarm2 {
        day: 1,
        hour: Hours::H24(0),
        minute: 0,
    }, opm).unwrap();
}

fn convert_event_data(raw: &[u8]) -> Option<(u8)> {
    match String::from_utf8(Vec::from(raw)) {
        Ok(as_str) => {
            let as_num = as_str.parse::<f32>();
            return match as_num {
                Ok(t) => {
                    Some(t as u8)
                }
                Err(_) => {
                    None
                }
            };
        }
        Err(e) => {
            error!("cannot convert data {:?}", e);
        }
    };

    None
}

pub fn get_temp() -> u8 {
    TEMP.load(Ordering::SeqCst)
}