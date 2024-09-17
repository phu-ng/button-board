mod wifi;

use anyhow::{bail, Ok};
use chrono::{DateTime, Datelike, FixedOffset, Timelike, Utc};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay;
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_svc::hal::prelude::{Hertz, Peripherals};
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use hd44780_driver::bus::I2CBus;
use hd44780_driver::{Cursor, CursorBlink, Display, DisplayMode, HD44780};

const ADDRESS: u8 = 0x27;

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

    // WIFI stuff
    let app_config: AppConfig = APP_CONFIG;

    match wifi::wifi(
        app_config.wifi_ssid,
        app_config.wifi_psk,
        peripherals.modem,
        sys_loop,
    ) {
        Ok(inner) => {
            println!("Connected to Wi-Fi network!");
            inner
        }
        Err(err) => {
            // Red!
            bail!("Could not connect to Wi-Fi network: {:?}", err)
        }
    };

    // Create Handle and Configure SNTP
    let ntp = EspSntp::new_default()?;

    // Synchronize NTP
    log::info!("Synchronizing with NTP Server");
    while ntp.get_sync_status() != SyncStatus::Completed {}
    log::info!("Time Sync Completed");

    let sda = peripherals.pins.gpio22;
    let scl = peripherals.pins.gpio23;

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
    ).unwrap();

    loop {
        display_clock(&mut lcd, get_current_time())?;
        delay::FreeRtos::delay_ms(30 * 1000);
    }
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
