mod wifi;

use anyhow::{bail, Ok};
use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{InterruptType, PinDriver, Pull};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::prelude::{Hertz, Peripherals};
use esp_idf_svc::hal::task::notification::{Notification, Notifier};
use esp_idf_svc::hal::{delay, peripherals};
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use hd44780_driver::bus::I2CBus;
use hd44780_driver::{Cursor, CursorBlink, Display, DisplayMode, HD44780};
use std::num::NonZeroU32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const ADDRESS: u8 = 0x27;
const PLAYLIST_1: [u8; 4] = [249, 123, 27, 3];
const PLAYLIST_2: [u8; 4] = [122, 146, 1, 1];
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
    // let sys_loop = EspSystemEventLoop::take()?;
    //
    // // WIFI stuff
    // let app_config: AppConfig = APP_CONFIG;
    //
    // match wifi::wifi(
    //     app_config.wifi_ssid,
    //     app_config.wifi_psk,
    //     peripherals.modem,
    //     sys_loop,
    // ) {
    //     Ok(inner) => {
    //         println!("Connected to Wi-Fi network!");
    //         inner
    //     }
    //     Err(err) => {
    //         // Red!
    //         bail!("Could not connect to Wi-Fi network: {:?}", err)
    //     }
    // };
    //
    // // Create Handle and Configure SNTP
    // let ntp = EspSntp::new_default()?;
    //
    // // Synchronize NTP
    // log::info!("Synchronizing with NTP Server");
    // while ntp.get_sync_status() != SyncStatus::Completed {}
    // log::info!("Time Sync Completed");

    let sda = peripherals.pins.gpio22;
    let scl = peripherals.pins.gpio23;
    let button = peripherals.pins.gpio1;

    let mut i2c_config = I2cConfig::new();
    i2c_config.baudrate = Hertz(100 * 1000); // 100kHz
    let i2c_driver = I2cDriver::new(peripherals.i2c0, sda, scl, &i2c_config)?;
    let mut lcd = HD44780::new_i2c(i2c_driver, ADDRESS, &mut delay::FreeRtos).unwrap();

    lcd.reset(&mut delay::FreeRtos).unwrap();

    lcd.clear(&mut delay::FreeRtos).unwrap();

    lcd.set_display_mode(
        DisplayMode {
            display: Display::On,
            cursor_visibility: Cursor::Invisible,
            cursor_blink: CursorBlink::Off,
        },
        &mut delay::FreeRtos,
    )
    .unwrap();

    // Assign interrupt
    let mut button_driver = PinDriver::input(button)?;
    button_driver.set_interrupt_type(InterruptType::PosEdge)?;

    // Create notification
    let notification = Notification::new();
    let notifier = notification.notifier();

    // Safety: make sure the `Notification` object is not dropped while the subscription is active
    unsafe {
        button_driver.subscribe(move || handle_interrupt(Arc::clone(&notifier)))?;
    }

    loop {
        display_clock(&mut lcd, get_current_time())?;

        // enable_interrupt should also be called after each received notification from non-ISR context
        button_driver.enable_interrupt()?;
        notification.wait(30 * 1000);

        if IS_INTERRUPT.load(std::sync::atomic::Ordering::Relaxed) {
            lcd.clear(&mut delay::FreeRtos).unwrap();
            lcd.write_str("show some config", &mut delay::FreeRtos).unwrap();
            delay::FreeRtos::delay_ms(5 * 1000);
            IS_INTERRUPT.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn handle_interrupt(notifier: Arc<Notifier>) {
    delay::FreeRtos::delay_ms(100);
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
    lcd: &mut HD44780<I2CBus<I2cDriver>>,
    date_time: DateTime<FixedOffset>,
) -> anyhow::Result<()> {
    let hour = pad_single_digit(date_time.hour());
    let minute = pad_single_digit(date_time.minute());
    let day = pad_single_digit(date_time.day());
    let month = month_to_abbreviation(date_time.month());
    let year = (date_time.year() % 100).to_string();

    // Get temperature and humidity from homeassistant mqtt

    let first_line = format!("{}:{}  {} {} {}", hour, minute, day, month, year);
    let second_line = format!("T {}C   H {}%", 27, 80);

    lcd.set_cursor_pos(0, &mut delay::FreeRtos).unwrap();
    lcd.write_str(&first_line, &mut delay::FreeRtos).unwrap();
    lcd.set_cursor_pos(40, &mut delay::FreeRtos).unwrap();
    lcd.write_str(&second_line, &mut delay::FreeRtos).unwrap();

    Ok(())
}
