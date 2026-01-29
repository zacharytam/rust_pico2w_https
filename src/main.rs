#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources, tcp::TcpSocket};
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
async fn http_server_task(stack: embassy_net::Stack<'static>) {
    info!("HTTP server starting...");
    
    // Wait for network
    loop {
        if stack.is_link_up() {
            info!("Network link is UP!");
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    
    loop {
        if stack.is_config_up() {
            info!("Network config is UP!");
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    
    info!("HTTP Server Ready on 192.168.4.1:80");
    
    loop {
        // Create fresh buffers for each connection
        let mut rx_buffer = [0; 512];
        let mut tx_buffer = [0; 512];
        
        // Pass stack by value
        let mut socket = TcpSocket::new(stack.clone(), &mut rx_buffer, &mut tx_buffer);
        
        if let Err(e) = socket.accept(80).await {
            warn!("Accept error: {:?}", e);
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }
        
        info!("Client connected!");
        
        // Simple HTTP response
        let response = "HTTP/1.1 200 OK\r\n\
                       Content-Type: text/html\r\n\
                       Connection: close\r\n\
                       \r\n\
                       <html><body><h1>Hello from Pico!</h1></body></html>\r\n";
        
        if socket.write_all(response.as_bytes()).await.is_ok() {
            info!("Response sent");
        }
        
        socket.close();
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    
    info!("=== Pico 2W HTTP Test ===");
    
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
    
    // Unwrap the Result from the task macro
    spawner.spawn(cyw43_task(runner).expect("Failed to spawn cyw43 task"));
    
    control.init(clm).await;
    
    // Start AP with WPA2 (more reliable than open)
    Timer::after(Duration::from_secs(1)).await;
    info!("Starting AP: {}", WIFI_SSID);
    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 11).await;
    info!("AP Started!");
    
    Timer::after(Duration::from_secs(2)).await;
    
    // Network config with static IP
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
    
    // Spawn tasks and unwrap the Results
    spawner.spawn(net_task(runner).expect("Failed to spawn net task"));
    spawner.spawn(http_server_task(stack).expect("Failed to spawn HTTP server task"));
    
    info!("=== System Ready ===");
    info!("Connect to WiFi: {}", WIFI_SSID);
    info!("Password: {}", WIFI_PASSWORD);
    info!("Visit: http://192.168.4.1");
    
    // Simple LED blink
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(500)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(500)).await;
    }
}
