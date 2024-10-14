mod wifi;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDateTime, Timelike, Utc};
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
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use shared_bus::{I2cProxy, NullMutex};
use ds323x::{Alarm2Matching, DateTimeAccess, DayAlarm2, Ds323x, Error, Hours};
use ds323x::ic::DS3231;
use ds323x::interface::I2cInterface;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::sys::nvs_flash_init;

const ADDRESS: u8 = 0x27;
static IS_INTERRUPT: AtomicBool = AtomicBool::new(false);

#[toml_cfg::toml_config]
pub struct AppConfig {
    #[default("PhuNetwork")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
}

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Start application");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;

    unsafe {
        nvs_flash_init();
        log::info!("init nvs flash");
    };

    // WIFI stuff
    let app_config: AppConfig = APP_CONFIG;

    // let _wifi = wifi::wifi(
    //     app_config.wifi_ssid,
    //     app_config.wifi_psk,
    //     peripherals.modem,
    //     sys_loop,
    // ).unwrap();

    // let _sync_ntp = thread::Builder::new()
    //     .stack_size(2000)
    //     .spawn(move || {
    //         loop {
    //             // Create Handle and Configure SNTP
    //             let ntp = EspSntp::new_default().unwrap();
    //
    //             // Synchronize NTP
    //             log::info!("Synchronizing with NTP Server");
    //             while ntp.get_sync_status() != SyncStatus::Completed {}
    //             log::info!("Time Sync Completed");
    //             FreeRtos::delay_ms(10 * 1000);
    //         }
    //     })?;

    let sda = peripherals.pins.gpio10;
    let scl = peripherals.pins.gpio11;
    let button = peripherals.pins.gpio1;
    let sqw_pin = peripherals.pins.gpio23;

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

    // Assign interrupt
    // let mut button_driver = PinDriver::input(button)?;
    // button_driver.set_interrupt_type(InterruptType::PosEdge)?;

    // Create notification
    let notification = Notification::new();
    let notifier = notification.notifier();

    // Safety: make sure the `Notification` object is not dropped while the subscription is active
    unsafe {
        sqw.subscribe(move || handle_interrupt(Arc::clone(&notifier)))?;
    }

    set_alarm_every_minute(&mut rtc);

    loop {
        display_clock(&mut lcd, rtc.datetime().unwrap())?;

        // enable_interrupt should also be called after each received notification from non-ISR context
        sqw.enable_interrupt()?;
        notification.wait(delay::BLOCK);

        FreeRtos::delay_ms(100);
        if rtc.has_alarm2_matched().unwrap() {
            set_alarm_every_minute(&mut rtc);
        }
    }
}

fn handle_interrupt(notifier: Arc<Notifier>) {
    IS_INTERRUPT.store(true, std::sync::atomic::Ordering::Relaxed);
    // Move the logic from the closure here
    unsafe { notifier.notify_and_yield(NonZeroU32::new(1).unwrap()) };
}

fn get_current_time() -> DateTime<FixedOffset> {
    let vn_offset = FixedOffset::east_opt(1 * 3600).unwrap();
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
) -> anyhow::Result<()> {
    let hour = pad_single_digit(date_time.hour());
    let minute = pad_single_digit(date_time.minute());
    let day = pad_single_digit(date_time.day());
    let month = month_to_abbreviation(date_time.month());
    let year = (date_time.year() % 100).to_string();

    // Get temperature and humidity from homeassistant mqtt

    let first_line = format!("{}:{}  {} {} {}", hour, minute, day, month, year);
    let second_line = format!("T {}C   H {}%", 27, 80);

    lcd.set_cursor_pos(0, &mut FreeRtos).unwrap();
    lcd.write_str(&first_line, &mut FreeRtos).unwrap();
    lcd.set_cursor_pos(40, &mut FreeRtos).unwrap();
    lcd.write_str(&second_line, &mut FreeRtos).unwrap();

    Ok(())
}

fn set_alarm_every_minute(rtc: &mut Ds323x<I2cInterface<I2cProxy<NullMutex<I2cDriver>>>, DS3231>) {
    let opm = Alarm2Matching::OncePerMinute;

    rtc.clear_alarm2_matched_flag().unwrap();
    rtc.set_alarm2_day(DayAlarm2 {
        day: 1,
        hour: Hours::H24(0),
        minute: 0,
    }, opm).unwrap();
}