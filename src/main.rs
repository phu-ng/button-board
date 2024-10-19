mod mqtt;
mod wifi;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};
use ds323x::ic::DS3231;
use ds323x::interface::I2cInterface;
use ds323x::{Alarm2Matching, DateTimeAccess, DayAlarm2, Ds323x, Hours};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::{InterruptType, PinDriver};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::prelude::{Hertz, Peripherals};
use esp_idf_svc::hal::task::notification::{Notification, Notifier};
use esp_idf_svc::mqtt::client::EventPayload;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use esp_idf_svc::sys::nvs_flash_init;
use hd44780_driver::bus::I2CBus;
use hd44780_driver::{Cursor, CursorBlink, Display, DisplayMode, HD44780};
use log::{error, info};
use serde::Deserialize;
use shared_bus::{I2cProxy, NullMutex};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;

const ADDRESS: u8 = 0x27;
static THREAD_SIZE: usize = 6000;
static CURRENT_DISPLAY_STATE: AtomicU8 = AtomicU8::new(0);
static BUTTON_A_NOTICE: AtomicBool = AtomicBool::new(false);
static BUTTON_B_NOTICE: AtomicBool = AtomicBool::new(false);
static BUTTON_C_NOTICE: AtomicBool = AtomicBool::new(false);
static BUTTON_D_NOTICE: AtomicBool = AtomicBool::new(false);
static BUTTON_E_NOTICE: AtomicBool = AtomicBool::new(false);
static BUTTON_F_NOTICE: AtomicBool = AtomicBool::new(false);
static BUTTON_G_NOTICE: AtomicBool = AtomicBool::new(false);
static BUTTON_H_NOTICE: AtomicBool = AtomicBool::new(false);
static TEMP: AtomicU32 = AtomicU32::new(0);
static HUMID: AtomicU32 = AtomicU32::new(0);
static PM2_5: AtomicU32 = AtomicU32::new(0);
static PM10: AtomicU32 = AtomicU32::new(0);

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
    mqtt_room_topic: &'static str,
    #[default("")]
    mqtt_command_topic: &'static str,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct EnvironmentalInfo {
    temp: f32,
    humid: f32,
    #[serde(rename = "pm2.5")]
    pm2_5: f32,
    pm10: f32,
}

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Start application");

    // Load config
    let app_config: AppConfig = APP_CONFIG;

    unsafe {
        nvs_flash_init();
        log::info!("init nvs flash");
    };

    let peripherals = Peripherals::take()?;
    // Needed for wifi
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Create notification
    let notification = Notification::new();
    let notifier = notification.notifier();

    let mut a = PinDriver::input(peripherals.pins.gpio18)?;
    let mut b = PinDriver::input(peripherals.pins.gpio19)?;
    let mut c = PinDriver::input(peripherals.pins.gpio20)?;
    let mut d = PinDriver::input(peripherals.pins.gpio21)?;
    let mut e = PinDriver::input(peripherals.pins.gpio22)?;
    let mut f = PinDriver::input(peripherals.pins.gpio23)?;
    let mut g = PinDriver::input(peripherals.pins.gpio2)?;
    let mut h = PinDriver::input(peripherals.pins.gpio3)?;

    // Assign interrupt button
    a.set_interrupt_type(InterruptType::PosEdge)?;
    b.set_interrupt_type(InterruptType::PosEdge)?;
    c.set_interrupt_type(InterruptType::PosEdge)?;
    d.set_interrupt_type(InterruptType::PosEdge)?;
    e.set_interrupt_type(InterruptType::PosEdge)?;
    f.set_interrupt_type(InterruptType::PosEdge)?;
    g.set_interrupt_type(InterruptType::PosEdge)?;
    h.set_interrupt_type(InterruptType::PosEdge)?;

    // Create notifiers for each button
    let notifier_a = Arc::clone(&notifier);
    let notifier_b = Arc::clone(&notifier);
    let notifier_c = Arc::clone(&notifier);
    let notifier_d = Arc::clone(&notifier);
    let notifier_e = Arc::clone(&notifier);
    let notifier_f = Arc::clone(&notifier);
    let notifier_g = Arc::clone(&notifier);
    let notifier_h = Arc::clone(&notifier);

    // Safety: make sure the `Notification` object is not dropped while the subscription is active
    unsafe {
        a.subscribe(move || handle_button_default(&notifier_a, "a"))?;
        b.subscribe(move || handle_button_default(&notifier_b, "b"))?;
        c.subscribe(move || handle_button_default(&notifier_c, "c"))?;
        d.subscribe(move || handle_button_default(&notifier_d, "d"))?;
        e.subscribe(move || handle_button_default(&notifier_e, "e"))?;
        f.subscribe(move || handle_button_default(&notifier_f, "f"))?;
        g.subscribe(move || handle_button_default(&notifier_g, "g"))?;
        h.subscribe(move || handle_button_default(&notifier_h, "h"))?;
    }

    // Init I2C
    let sda = peripherals.pins.gpio6;
    let scl = peripherals.pins.gpio7;

    let mut i2c_config = I2cConfig::new();
    i2c_config.baudrate = Hertz(100 * 1000); // 100kHz
    let i2c_driver = I2cDriver::new(peripherals.i2c0, sda, scl, &i2c_config)?;
    let bus = shared_bus::BusManagerSimple::new(i2c_driver);

    // Init RTC module
    let mut rtc = Ds323x::new_ds3231(bus.acquire_i2c());
    rtc.use_int_sqw_output_as_interrupt().unwrap();
    rtc.enable_alarm2_interrupts().unwrap();
    // Init sqw input for ds3231
    let mut sqw = PinDriver::input(peripherals.pins.gpio10)?;
    sqw.set_interrupt_type(InterruptType::NegEdge)?;
    let sqw_notifier = Arc::clone(&notifier);
    unsafe {
        sqw.subscribe(move || handle_sqw_notice(&sqw_notifier))?;
    }

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
    display_message(&mut lcd, "CONNECT TO WIFI", "")?;

    // Init wifi
    let mut wifi = wifi::wifi(
        app_config.wifi_ssid,
        app_config.wifi_psk,
        peripherals.modem,
        sys_loop.clone(),
        nvs,
    )?;

    display_message(&mut lcd, "SYNCHRONIZE NTP", "")?;
    // Create Handle and Configure SNTP
    let ntp = EspSntp::new_default()?;
    for _i in 0..5 {
        match ntp.get_sync_status() {
            SyncStatus::Reset => {
                info!("reset");
                FreeRtos::delay_ms(2000);
                continue;
            }
            SyncStatus::Completed => {
                info!("complete");
                let now = get_current_time();
                let dt = NaiveDate::from_ymd_opt(now.year(), now.month(), now.day())
                    .unwrap()
                    .and_hms_opt(now.hour(), now.minute(), now.second())
                    .unwrap();
                rtc.set_datetime(&dt).unwrap();
                break;
            }
            SyncStatus::InProgress => {
                info!("In progress");
                FreeRtos::delay_ms(1000);
                continue;
            }
        }
    }

    // Subcribe to Mqtt
    let (mut mqtt_client, mut conn) = mqtt::init(
        app_config.mqtt_url,
        "bb",
        app_config.mqtt_user,
        app_config.mqtt_password,
    )
    .unwrap();
    // when set this code in another separated function, it will get delete after fn exit, so
    // need to keep it in main fn at the moment
    // TODO: move this logic to other module. Maybe async can help
    thread::Builder::new()
        .stack_size(THREAD_SIZE)
        .spawn(move || {
            info!("MQTT Listening for messages");
            while let Ok(event) = conn.next() {
                match event.payload() {
                    EventPayload::Received { data, .. } => {
                        let info = convert_event_data(data);
                        if info.temp == 0.0 {
                        } else {
                            TEMP.store(info.temp as u32, Ordering::SeqCst);
                        }
                        if info.humid == 0.0 {
                        } else {
                            HUMID.store(info.humid as u32, Ordering::SeqCst);
                        }
                        PM2_5.store(info.pm2_5 as u32, Ordering::SeqCst);
                        PM10.store(info.pm10 as u32, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
            info!("Connection closed");
        })?;

    // This fn also block. Maybe async will help
    mqtt::subscribes(&mut mqtt_client, app_config.mqtt_room_topic);

    handle_alarm_every_minute(&mut rtc);
    // handle_alarm_ntp_sync(&mut rtc, &ntp);

    // FreeRtos::delay_ms(5000);
    // sys_loop.subscribe::<WifiEvent, _>(move |wifi_event| {
    //     log::info!("some kind of wifi event {:?}", wifi_event)
    // })?;

    loop {
        // enable_interrupt should also be called after each received notification from non-ISR context
        sqw.enable_interrupt()?;
        a.enable_interrupt()?;
        b.enable_interrupt()?;
        c.enable_interrupt()?;
        d.enable_interrupt()?;
        e.enable_interrupt()?;
        f.enable_interrupt()?;
        g.enable_interrupt()?;
        h.enable_interrupt()?;

        // Re-draw display after every minute
        if CURRENT_DISPLAY_STATE.load(Ordering::SeqCst) == 0 {
            display_clock(
                &mut lcd,
                rtc.datetime().unwrap(),
                TEMP.load(Ordering::SeqCst),
                HUMID.load(Ordering::SeqCst),
            )?;
        } else if CURRENT_DISPLAY_STATE.load(Ordering::SeqCst) == 1 {
            display_aqi(
                &mut lcd,
                PM2_5.load(Ordering::SeqCst),
                PM10.load(Ordering::SeqCst),
            )?
        }

        notification.wait(delay::BLOCK);

        FreeRtos::delay_ms(100);

        if rtc.has_alarm2_matched().unwrap() {
            handle_alarm_every_minute(&mut rtc);
        }
        if BUTTON_A_NOTICE.load(Ordering::SeqCst) && a.is_low() {
            // TODO: display function should have full line message so don't have to clear everytime
            lcd.clear(&mut FreeRtos).unwrap();
            let state = CURRENT_DISPLAY_STATE.load(Ordering::SeqCst);
            if state == 0 {
                CURRENT_DISPLAY_STATE.store(1, Ordering::SeqCst);
            } else {
                CURRENT_DISPLAY_STATE.store(0, Ordering::SeqCst);
            }
            BUTTON_A_NOTICE.store(false, Ordering::SeqCst);
        }
        if BUTTON_B_NOTICE.load(Ordering::SeqCst) && b.is_low() {
            display_message(&mut lcd, "TURN ON/OFF AC", "")?;
            let result = mqtt::send_payload(&mut mqtt_client, app_config.mqtt_command_topic, "b");
            match result {
                Ok(_) => {}
                Err(_) => {
                    error!("cannot send command")
                }
            }
            FreeRtos::delay_ms(1000);
            BUTTON_B_NOTICE.store(false, Ordering::SeqCst);
        }
        if BUTTON_C_NOTICE.load(Ordering::SeqCst) && c.is_low() {
            display_message(&mut lcd, "TURN ON/OFF", "   AIR FILTER")?;
            let result = mqtt::send_payload(&mut mqtt_client, app_config.mqtt_command_topic, "c");
            match result {
                Ok(_) => {}
                Err(_) => {
                    error!("cannot send command")
                }
            }
            FreeRtos::delay_ms(1000);
            BUTTON_C_NOTICE.store(false, Ordering::SeqCst);
        }
        if BUTTON_D_NOTICE.load(Ordering::SeqCst) && d.is_low() {
            display_message(&mut lcd, "LIGHT MODE", "   DAY")?;
            let result = mqtt::send_payload(&mut mqtt_client, app_config.mqtt_command_topic, "d");
            match result {
                Ok(_) => {}
                Err(_) => {
                    error!("cannot send command")
                }
            }
            FreeRtos::delay_ms(1000);
            BUTTON_D_NOTICE.store(false, Ordering::SeqCst);
        }
        if BUTTON_E_NOTICE.load(Ordering::SeqCst) && e.is_low() {
            display_message(&mut lcd, "LIGHT MODE", "  NIGHT")?;
            let result = mqtt::send_payload(&mut mqtt_client, app_config.mqtt_command_topic, "e");
            match result {
                Ok(_) => {}
                Err(_) => {
                    error!("cannot send command")
                }
            }
            FreeRtos::delay_ms(1000);
            BUTTON_E_NOTICE.store(false, Ordering::SeqCst);
        }
        if BUTTON_F_NOTICE.load(Ordering::SeqCst) && f.is_low() {
            display_message(&mut lcd, "TURN ON/OFF LIGHT", "")?;
            let result = mqtt::send_payload(&mut mqtt_client, app_config.mqtt_command_topic, "f");
            match result {
                Ok(_) => {}
                Err(_) => {
                    error!("cannot send command")
                }
            }
            FreeRtos::delay_ms(1000);
            BUTTON_F_NOTICE.store(false, Ordering::SeqCst);
        }
        if BUTTON_G_NOTICE.load(Ordering::SeqCst) && g.is_low() {
            display_message(&mut lcd, "EMPTY FUNCTION", "")?;
            let result = mqtt::send_payload(&mut mqtt_client, app_config.mqtt_command_topic, "g");
            match result {
                Ok(_) => {}
                Err(_) => {
                    error!("cannot send command")
                }
            }
            FreeRtos::delay_ms(1000);
            BUTTON_G_NOTICE.store(false, Ordering::SeqCst);
        }
        if BUTTON_H_NOTICE.load(Ordering::SeqCst) && h.is_low() {
            display_message(&mut lcd, "EMPTY FUNCTION", "")?;
            let result = mqtt::send_payload(&mut mqtt_client, app_config.mqtt_command_topic, "h");
            match result {
                Ok(_) => {}
                Err(_) => {
                    error!("cannot send command")
                }
            }
            FreeRtos::delay_ms(1000);
            BUTTON_H_NOTICE.store(false, Ordering::SeqCst);
        }

        if !wifi.is_connected()? {
            info!("wifi is down. reconnecting");
            wifi.connect()?;
            FreeRtos::delay_ms(5000);
        }
    }
}

// fn handle_button_a(notifier: &Arc<Notifier>) {
//     let state = BUTTON_A_NOTICE.load(Ordering::SeqCst);
//     if state == 0 {
//         BUTTON_A_NOTICE.store(1, Ordering::SeqCst);
//     } else {
//         BUTTON_A_NOTICE.store(0, Ordering::SeqCst);
//     }
//     // Move the logic from the closure here
//     unsafe { notifier.notify_and_yield(NonZeroU32::new(1).unwrap()) };
// }

fn handle_button_default(notifier: &Arc<Notifier>, button: &str) {
    if button == "a" {
        BUTTON_A_NOTICE.store(true, Ordering::SeqCst);
    } else if button == "b" {
        BUTTON_B_NOTICE.store(true, Ordering::SeqCst);
    } else if button == "c" {
        BUTTON_C_NOTICE.store(true, Ordering::SeqCst);
    } else if button == "d" {
        BUTTON_D_NOTICE.store(true, Ordering::SeqCst);
    } else if button == "e" {
        BUTTON_E_NOTICE.store(true, Ordering::SeqCst);
    } else if button == "f" {
        BUTTON_F_NOTICE.store(true, Ordering::SeqCst);
    } else if button == "g" {
        BUTTON_G_NOTICE.store(true, Ordering::SeqCst);
    } else if button == "h" {
        BUTTON_H_NOTICE.store(true, Ordering::SeqCst);
    }
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
    temp: u32,
    humid: u32,
) -> anyhow::Result<()> {
    let hour = pad_single_digit(date_time.hour());
    let minute = pad_single_digit(date_time.minute());
    let day = pad_single_digit(date_time.day());
    let month = month_to_abbreviation(date_time.month());
    let year = (date_time.year() % 100).to_string();
    let temp = pad_single_digit(temp);
    let humid = pad_single_digit(humid);

    let first_line = format!("{}:{}  {} {} {}", hour, minute, day, month, year);
    let second_line = format!("  T {}C  H {}%", temp, humid);

    lcd.set_cursor_pos(0, &mut FreeRtos).unwrap();
    lcd.write_str(&first_line, &mut FreeRtos).unwrap();
    lcd.set_cursor_pos(40, &mut FreeRtos).unwrap();
    lcd.write_str(&second_line, &mut FreeRtos).unwrap();

    Ok(())
}

fn display_aqi(
    lcd: &mut HD44780<I2CBus<I2cProxy<NullMutex<I2cDriver>>>>,
    pm2_5: u32,
    pm10: u32,
) -> anyhow::Result<()> {
    let first_line = format!("PM2.5: {}", pm2_5);
    let second_line = format!("PM10: {}", pm10);

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

fn handle_alarm_every_minute(
    rtc: &mut Ds323x<I2cInterface<I2cProxy<NullMutex<I2cDriver>>>, DS3231>,
) {
    let opm = Alarm2Matching::OncePerMinute;

    rtc.clear_alarm2_matched_flag().unwrap();
    rtc.set_alarm2_day(
        DayAlarm2 {
            day: 1,
            hour: Hours::H24(0),
            minute: 0,
        },
        opm,
    )
    .unwrap();
}

fn convert_event_data(raw: &[u8]) -> EnvironmentalInfo {
    match String::from_utf8(Vec::from(raw)) {
        Ok(as_str) => {
            let as_struct: Result<EnvironmentalInfo, _> = serde_json::from_str(&as_str);
            as_struct.unwrap_or_else(|_| EnvironmentalInfo {
                temp: 0.0,
                humid: 0.0,
                pm2_5: 0.0,
                pm10: 0.0,
            })
        }
        Err(_) => EnvironmentalInfo {
            temp: 0.0,
            humid: 0.0,
            pm2_5: 0.0,
            pm10: 0.0,
        },
    }
}
