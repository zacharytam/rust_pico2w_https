#![no_std]
#![no_main]

use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use defmt::*;
use core::fmt::Write as FmtWrite;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};

use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, UART0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::uart::{BufferedInterruptHandler, BufferedUart, Config as UartConfig};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read;
use embedded_io_async::Write;
use heapless::String;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// Program metadata for `picotool info`
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"Pico2W LTE Proxy"),
    embassy_rp::binary_info::rp_program_description!(
        c"WiFi AP + LTE HTTP Proxy via EC800K module"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

const WIFI_SSID: &str = "PicoLTE";
const WIFI_PASSWORD: &str = "12345678";
const UART_BAUDRATE: u32 = 921600;

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(runner: &'static mut embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn http_server_task(stack: &'static embassy_net::Stack<'static>) {
    info!("HTTP server starting...");

    // Wait for network link to be up
    info!("Waiting for network link...");
    loop {
        if stack.is_link_up() {
            info!("Network link is UP!");
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // Wait for stack to be configured
    info!("Waiting for network config...");
    loop {
        if stack.is_config_up() {
            info!("Network config is UP!");
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    info!("==================================================");
    info!("HTTP SERVER READY on 192.168.4.1:80");
    info!("Client IP must be: 192.168.4.2-254/24");
    info!("Gateway must be: 192.168.4.1");
    info!("==================================================");

    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];
    let mut connection_count = 0u32;

    loop {
        info!("Creating new socket...");
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        
        // Listen on port 80
        info!("Listening on TCP port 80...");
        if let Err(e) = socket.accept(80).await {
            warn!("Accept error: {:?}", e);
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }

        connection_count += 1;
        info!("✅ Client connected! (connection #{})", connection_count);

        // Send a simple HTTP response
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 42\r\nConnection: close\r\n\r\n<h1>Hello from Pico 2W LTE Proxy!</h1>\r\n";
        
        match socket.write_all(response).await {
            Ok(_) => {
                info!("✅ Response written");
            }
            Err(e) => {
                warn!("❌ Write error: {:?}", e);
            }
        }

        match socket.flush().await {
            Ok(_) => {
                info!("✅ Response flushed");
            }
            Err(e) => {
                warn!("❌ Flush error: {:?}", e);
            }
        }

        socket.close();
        info!("✅ Connection closed");
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    info!("=== BOOT: Pico 2W Starting ===");
    Timer::after(Duration::from_millis(500)).await;

    info!("=== Pico 2W LTE Proxy ===");
    info!("WiFi AP: {} / {}", WIFI_SSID, WIFI_PASSWORD);

    // Initialize WiFi
    info!("Loading WiFi firmware...");
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");
    info!("Firmware loaded: {} bytes, CLM: {} bytes", fw.len(), clm.len());

    info!("Initializing CYW43 pins...");
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    info!("PIO initialized");
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        // Use RM2_CLOCK_DIVIDER for reliable SPI communication on Pico 2W
        // DEFAULT_CLOCK_DIVIDER is too fast and causes issues
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    info!("Creating CYW43 state...");
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());

    info!("Initializing CYW43 driver...");
    Timer::after(Duration::from_secs(1)).await;
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    info!("CYW43 driver initialized");

    spawner.spawn(unwrap!(cyw43_task(runner)));
    info!("CYW43 task spawned");

    // Start WiFi AP
    info!("Initializing WiFi with CLM data...");
    control.init(clm).await;
    info!("CLM initialized");

    // Set power management mode
    control.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    Timer::after(Duration::from_secs(2)).await;
    info!("Starting AP mode: SSID={}", WIFI_SSID);
    control.start_ap_open(WIFI_SSID, 5).await;
    info!("✅ WiFi AP started successfully!");

    Timer::after(Duration::from_secs(3)).await;

    // Configure network stack with static IP
    info!("Configuring network stack...");
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
        dns_servers: heapless::Vec::new(),
        gateway: None,
    });
    info!("Network config: 192.168.4.1/24");

    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());

    let (stack, runner) = embassy_net::new(net_device, config, resources, embassy_rp::clocks::RoscRng.next_u64());

    static STACK_STORAGE: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();
    let stack = STACK_STORAGE.init(stack);

    static RUNNER_STORAGE: StaticCell<embassy_net::Runner<'static, cyw43::NetDriver<'static>>> = StaticCell::new();
    let runner = RUNNER_STORAGE.init(runner);

    spawner.spawn(unwrap!(net_task(runner)));

    let stack = stack;

    info!("Network stack initialized at 192.168.4.1");

    // Start HTTP server
    info!("Spawning HTTP server task...");
    spawner.spawn(unwrap!(http_server_task(stack)));
    info!("✅ HTTP server task spawned successfully");

    info!("==================================================");
    info!("🚀 LTE Proxy Ready!");
    info!("==================================================");
    info!("1. Connect to WiFi SSID: {}", WIFI_SSID);
    info!("   Password: {}", WIFI_PASSWORD);
    info!("");
    info!("2. MANUALLY configure your device:");
    info!("   IP Address: 192.168.4.2 (or .3, .4, etc.)");
    info!("   Subnet Mask: 255.255.255.0");
    info!("   Gateway: 192.168.4.1");
    info!("   DNS: 192.168.4.1 (optional)");
    info!("");
    info!("3. Open browser to: http://192.168.4.1");
    info!("==================================================");
    info!("NOTE: No DHCP server - manual IP required!");
    info!("==================================================");

    // Keep LED blinking to show alive
    info!("Starting LED blink loop...");
    let mut blink_count = 0u32;
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(500)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(500)).await;
        blink_count += 1;
        if blink_count % 10 == 0 {
            info!("LED blink count: {}", blink_count);
        }
    }
}
