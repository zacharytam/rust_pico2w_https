#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use picoserve::{response::Html, Router};

const WIFI_SSID: &str = "PicoTest";
const WIFI_PASSWORD: &str = "test1234";

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn http_server_task(stack: &'static embassy_net::Stack<'static>) {
    info!("[HTTP] Task starting...");
    
    // Simple wait for network
    Timer::after(Duration::from_secs(5)).await;
    
    info!("[HTTP] Checking network status...");
    info!("[HTTP] Link up: {}", stack.is_link_up());
    info!("[HTTP] Config up: {}", stack.is_config_up());
    
    let router = Router::new().route("/", |_| async { Html("<h1>Hello from Pico with picoserve!</h1>") });
    let config = picoserve::Config::new(picoserve::Timeouts {
        start_read_request: Some(Duration::from_secs(5)),
        read_request: Some(Duration::from_secs(1)),
        write: Some(Duration::from_secs(1)),
    })
    .keep_connection_alive();
    
    let server = picoserve::Server::new(stack, config, router);
    
    info!("HTTP Server Ready on 192.168.4.1:80");
    
    loop {
        match server.run().await {
            Ok(_) => info!("Server completed (shouldn't happen)"),
            Err(e) => {
                warn!("Server error: {:?}", e);
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    
    info!("=== Pico 2W HTTP Test (with picoserve) ===");
    
    // Initialize WiFi
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");
    
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );
    
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    
    spawner.spawn(cyw43_task(runner).expect("Failed to spawn cyw43 task"));
    
    control.init(clm).await;
    
    Timer::after(Duration::from_secs(1)).await;
    info!("[WiFi] Starting AP...");
    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 11).await;
    info!("✅ [WiFi] AP Started!");
    
    // Configure network
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
        dns_servers: heapless::Vec::new(),
        gateway: None,
    });
    
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());
    
    let (stack, runner) = embassy_net::new(
        net_device, 
        config, 
        resources, 
        embassy_rp::clocks::RoscRng.next_u64()
    );
    
    static STACK: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();
    let stack_ref = STACK.init(stack);
    
    spawner.spawn(net_task(runner).expect("Failed to spawn net task"));
    spawner.spawn(http_server_task(stack_ref).expect("Failed to spawn HTTP server task"));
    
    info!("=== System Ready ===");
    info!("WiFi: {} / {}", WIFI_SSID, WIFI_PASSWORD);
    info!("Test URL: http://192.168.4.1");
    
    // Blink LED to show status
    let mut status = 0;
    loop {
        control.gpio_set(0, status % 2 == 0).await;
        Timer::after(Duration::from_secs(1)).await;
        status += 1;
    }
}
