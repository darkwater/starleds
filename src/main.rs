#![no_std]
#![no_main]
#![feature(generic_const_exprs)]
#![expect(incomplete_features)]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use core::fmt::Write;
use core::future::pending;
use core::sync::atomic::AtomicU32;

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::rmt::{PulseCode, Rmt};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal_smartled::buffer_size;
use esp_radio::Controller;
use esp_radio::wifi::{
    ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
};
use log::info;
use mcutie::{McutieBuilder, MqttMessage, Publishable, Topic};
use smart_leds::hsv::{Hsv, hsv2rgb};
use smart_leds::{RGB8, SmartLedsWrite};
use tanuki_common::capabilities::light::{Color, LightState};

use self::patterns::Pattern as _;

mod patterns;

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");
const LEDS: usize = 102;
type SmartLedsAdapter<'a> = esp_hal_smartled::SmartLedsAdapter<'a, { buffer_size(LEDS) }, RGB8>;

pub static COLOR: AtomicColor = AtomicColor::new();
pub struct AtomicColor(AtomicU32);
impl AtomicColor {
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    pub fn get(&self) -> RGB8 {
        let v = self.0.load(core::sync::atomic::Ordering::Relaxed);
        RGB8 {
            r: ((v >> 16) & 0xFF) as u8,
            g: ((v >> 8) & 0xFF) as u8,
            b: (v & 0xFF) as u8,
        }
    }

    pub fn set(&self, color: RGB8) {
        let v = ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32);
        self.0.store(v, core::sync::atomic::Ordering::Relaxed);
    }
}

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.2.0

    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 66320);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("Embassy initialized!");

    let data_pin = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    // .into_async();
    let channel = rmt.channel0;

    let buf = mk_static!(
        [PulseCode; buffer_size(LEDS)],
        [PulseCode::default(); buffer_size(LEDS)]
    );
    let ws = SmartLedsAdapter::new_with_color(channel, data_pin, buf);

    spawner.spawn(led_task(ws)).unwrap();

    if SSID == "PLACEHOLDER" {
        log::error!("WIFI_SSID and WIFI_PASSWORD must be set");
        pending().await
    }

    let radio_init = &*mk_static!(
        Controller<'static>,
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller")
    );

    let (controller, interfaces) =
        esp_radio::wifi::new(radio_init, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    let wifi_interface = interfaces.sta;

    let config = embassy_net::Config::dhcpv4(Default::default());

    let rng = esp_hal::rng::Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner
        .spawn(connection(controller))
        .expect("failed to spawn wifi task");
    spawner
        .spawn(net_task(runner))
        .expect("failed to spawn net task");

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    log::info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            log::info!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let (rx, task) = McutieBuilder::new(stack, "starleds", "mqtt.fbk.red")
        .with_subscriptions([Topic::General(
            "tanuki/entities/south_lamp/tanuki.light/state",
        )])
        .with_last_will(LastWill)
        .build();

    spawner.spawn(mcutie_task(task)).unwrap();

    loop {
        match rx.receive().await {
            MqttMessage::Connected => {
                log::info!("MQTT connected");
            }
            MqttMessage::Disconnected => {
                panic!("MQTT disconnected");
            }
            MqttMessage::Publish(Topic::General(topic), payload) => {
                log::info!("MQTT publish on topic '{}'", topic);
                log::info!(
                    "Payload: {}",
                    core::str::from_utf8(payload.as_ref()).unwrap_or("<invalid utf8>")
                );

                let color = match serde_json::from_slice::<LightState>(payload.as_ref()) {
                    Ok(state) => {
                        log::info!("Parsed light state: {:#?}", state);

                        match state.color {
                            Some(Color::Xy { x, y }) => {
                                let (r, g, b) = hue_xy_to_rgb8(
                                    x,
                                    y,
                                    ((state.brightness.unwrap_or(1.) * 254.) as u8).min(254),
                                );

                                Some(RGB8 { r, g, b })
                            }
                            Some(Color::Hs { h, s }) => Some(hsv2rgb(Hsv {
                                hue: (h / 360. * 255.) as u8,
                                sat: (s * 255.) as u8,
                                val: (state.brightness.unwrap_or(1.) * 255.) as u8,
                            })),
                            None => {
                                let brightness = (state.brightness.unwrap_or(0.) * 255.) as u8;
                                Some(RGB8 {
                                    r: brightness,
                                    g: brightness,
                                    b: brightness,
                                })
                            }
                            _ => None,
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to parse light state: {}", e);
                        None
                    }
                };

                if let Some(color) = color {
                    log::info!(
                        "Setting color to: R={} G={} B={}",
                        color.r,
                        color.g,
                        color.b
                    );

                    COLOR.set(color);
                }
            }
            MqttMessage::Publish(_, _) => {
                log::info!("MQTT publish on unknown topic");
            }
        }
    }

    // pending().await
}

#[embassy_executor::task]
#[allow(clippy::large_stack_frames)]
async fn mcutie_task(task: mcutie::McutieTask<'static, &'static str, LastWill, 1>) {
    task.run().await
}

struct LastWill;
impl Publishable for LastWill {
    fn write_topic(&self, buffer: &mut mcutie::TopicString) -> Result<(), mcutie::Error> {
        buffer
            .write_str("tanuki/entities/starleds/$meta/status")
            .map_err(|_| mcutie::Error::PacketError)
    }

    fn write_payload(&self, buffer: &mut mcutie::Payload) -> Result<(), mcutie::Error> {
        buffer
            .write_str("offline")
            .map_err(|_| mcutie::Error::PacketError)
    }
}

#[expect(non_snake_case)]
pub fn hue_xy_to_rgb8(x: f32, y: f32, bri: u8) -> (u8, u8, u8) {
    if y <= 0.0 {
        return (0, 0, 0);
    }

    let Y = bri as f32 / 254.0;
    let X = (Y / y) * x;
    let Z = (Y / y) * (1.0 - x - y);

    // XYZ -> linear RGB
    let mut r = 3.2406 * X - 1.5372 * Y - 0.4986 * Z;
    let mut g = -0.9689 * X + 1.8758 * Y + 0.0415 * Z;
    let mut b = 0.0557 * X - 0.2040 * Y + 1.0570 * Z;

    r = r.max(0.0);
    g = g.max(0.0);
    b = b.max(0.0);

    let m = r.max(g).max(b);
    if m > 1.0 {
        r /= m;
        g /= m;
        b /= m;
    }

    (
        GAMMA[(r * 255.0 + 0.5) as u8 as usize],
        GAMMA[(g * 255.0 + 0.5) as u8 as usize],
        GAMMA[(b * 255.0 + 0.5) as u8 as usize],
    )
}

pub static GAMMA: [u8; 256] = [
    0, 13, 22, 28, 34, 38, 42, 46, 50, 53, 56, 59, 61, 64, 66, 69, 71, 73, 75, 77, 79, 81, 83, 85,
    86, 88, 90, 92, 93, 95, 96, 98, 99, 101, 102, 104, 105, 106, 108, 109, 110, 112, 113, 114, 115,
    117, 118, 119, 120, 121, 122, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136,
    137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 148, 149, 150, 151, 152, 153, 154,
    155, 155, 156, 157, 158, 159, 159, 160, 161, 162, 163, 163, 164, 165, 166, 167, 167, 168, 169,
    170, 170, 171, 172, 173, 173, 174, 175, 175, 176, 177, 178, 178, 179, 180, 180, 181, 182, 182,
    183, 184, 185, 185, 186, 187, 187, 188, 189, 189, 190, 190, 191, 192, 192, 193, 194, 194, 195,
    196, 196, 197, 197, 198, 199, 199, 200, 200, 201, 202, 202, 203, 203, 204, 205, 205, 206, 206,
    207, 208, 208, 209, 209, 210, 210, 211, 212, 212, 213, 213, 214, 214, 215, 215, 216, 216, 217,
    218, 218, 219, 219, 220, 220, 221, 221, 222, 222, 223, 223, 224, 224, 225, 226, 226, 227, 227,
    228, 228, 229, 229, 230, 230, 231, 231, 232, 232, 233, 233, 234, 234, 235, 235, 236, 236, 237,
    237, 238, 238, 238, 239, 239, 240, 240, 241, 241, 242, 242, 243, 243, 244, 244, 245, 245, 246,
    246, 246, 247, 247, 248, 248, 249, 249, 250, 250, 251, 251, 251, 252, 252, 253, 253, 254, 254,
    255, 255,
];

#[embassy_executor::task]
#[allow(clippy::large_stack_frames)]
async fn connection(mut controller: WifiController<'static>) {
    log::info!("start connection task");
    log::info!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if let WifiStaState::Connected = esp_radio::wifi::sta_state() {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            log::info!("Starting wifi");
            controller.start_async().await.unwrap();
            log::info!("Wifi started!");

            log::info!("Scan");
            let scan_config = ScanConfig::default().with_max(10);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            for ap in result {
                log::info!("{:?}", ap);
            }
        }

        log::info!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => log::info!("Wifi connected!"),
            Err(e) => {
                log::info!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn led_task(mut ws: SmartLedsAdapter<'static>) -> ! {
    let mut colors = [RGB8::default(); LEDS];

    let mut pattern = patterns::stars::Stars::default();

    loop {
        pattern.update(&mut colors);

        critical_section::with(|_| {
            ws.write(colors.iter().copied()).unwrap();
        });

        Timer::after(Duration::from_millis(pattern.update_rate())).await;
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log::error!("Panic: {info}");

    embassy_time::block_for(Duration::from_secs(1));

    esp_hal::system::software_reset();
}
