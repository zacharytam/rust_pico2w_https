#![no_std]
#![no_main]

use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::{Config, StackResources};

use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_time::{Duration, Timer};
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
});

const WIFI_SSID: &str = "PicoLTE";
const WIFI_PASSWORD: &str = "12345678";

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
    info!("HTTP server task started");
    
    // 给网络栈一些时间初始化
    Timer::after(Duration::from_secs(5)).await;
    
    info!("Checking network status...");
    info!("Link up: {}", stack.is_link_up());
    info!("Config up: {}", stack.is_config_up());
    
    // 等待网络连接
    let mut attempts = 0;
    while !stack.is_link_up() || !stack.is_config_up() {
        Timer::after(Duration::from_millis(500)).await;
        attempts += 1;
        if attempts % 10 == 0 {
            info!("Waiting for network... (attempt {})", attempts);
            info!("Link up: {}, Config up: {}", stack.is_link_up(), stack.is_config_up());
        }
        if attempts > 100 {
            error!("Network never came up!");
            return;
        }
    }
    
    info!("✅ Network is ready!");
    info!("IP: {:?}", stack.config_v4());
    
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];
    
    loop {
        info!("Creating socket...");
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        
        // 设置更长的超时时间
        socket.set_timeout(Some(Duration::from_secs(30)));
        
        info!("Binding to port 80...");
        match socket.bind(80) {
            Ok(_) => info!("✅ Socket bound to port 80"),
            Err(e) => {
                error!("❌ Bind error: {:?}", e);
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        }
        
        info!("Listening for connections...");
        match socket.listen(1).await {
            Ok(_) => info!("✅ Listening on port 80"),
            Err(e) => {
                error!("❌ Listen error: {:?}", e);
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        }
        
        info!("Waiting for connection...");
        match socket.accept().await {
            Ok(_) => {
                info!("✅ Client connected!");
                
                // 读取请求（非阻塞）
                let mut request_buf = [0u8; 512];
                let mut total_read = 0;
                
                for _ in 0..10 {
                    match embassy_time::with_timeout(
                        Duration::from_millis(100),
                        socket.read(&mut request_buf[total_read..])
                    ).await {
                        Ok(Ok(n)) if n > 0 => {
                            total_read += n;
                            info!("Read {} bytes, total: {}", n, total_read);
                            if total_read >= request_buf.len() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                
                if total_read > 0 {
                    if let Ok(request_str) = core::str::from_utf8(&request_buf[..total_read]) {
                        info!("Request: {}", request_str.lines().next().unwrap_or(""));
                    }
                }
                
                // 发送响应
                let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 60\r\nConnection: close\r\n\r\n<html><body><h1>Pico 2W WiFi AP Works!</h1></body></html>\r\n";
                
                info!("Sending response...");
                match socket.write_all(response.as_bytes()).await {
                    Ok(_) => info!("✅ Response sent"),
                    Err(e) => error!("❌ Write error: {:?}", e),
                }
                
                match socket.flush().await {
                    Ok(_) => info!("✅ Response flushed"),
                    Err(e) => error!("❌ Flush error: {:?}", e),
                }
            }
            Err(e) => {
                error!("❌ Accept error: {:?}", e);
            }
        }
        
        info!("Closing socket...");
        socket.close();
        
        // 短暂的延迟
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("=== BOOT: Pico 2W Starting ===");
    Timer::after(Duration::from_millis(1000)).await;

    info!("=== Pico 2W WiFi AP Test ===");
    info!("WiFi AP: {} / {}", WIFI_SSID, WIFI_PASSWORD);

    let p = embassy_rp::init(Default::default());

    // Initialize WiFi
    info!("Loading WiFi firmware...");
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");
    info!("Firmware: {} bytes, CLM: {} bytes", fw.len(), clm.len());

    info!("Initializing CYW43 pins...");
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    info!("PIO initialized");
    
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

    info!("Creating CYW43 state...");
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());

    info!("Initializing CYW43 driver...");
    Timer::after(Duration::from_secs(2)).await;
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    info!("CYW43 driver initialized");

    spawner.spawn(unwrap!(cyw43_task(runner)));
    info!("CYW43 task spawned");

    // Start WiFi AP
    info!("Initializing WiFi with CLM data...");
    control.init(clm).await;
    info!("CLM initialized");

    // 等待CLM加载完成
    Timer::after(Duration::from_secs(2)).await;

    info!("Starting AP mode: SSID={}", WIFI_SSID);
    control.start_ap_open(WIFI_SSID, 5).await;
    info!("✅ WiFi AP started successfully!");

    // 等待AP稳定
    Timer::after(Duration::from_secs(5)).await;

    // Configure network stack
    info!("Configuring network stack...");
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(
            embassy_net::Ipv4Address::new(192, 168, 4, 1), 
            24
        ),
        dns_servers: heapless::Vec::new(),
        gateway: None,
    });
    info!("Network config: 192.168.4.1/24");

    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());

    // 生成随机种子
    let seed = embassy_rp::clocks::RoscRng.next_u64();
    info!("Random seed: {}", seed);

    let (stack, runner) = embassy_net::new(
        net_device, 
        config, 
        resources, 
        seed
    );

    static STACK_STORAGE: StaticCell<embassy_net::Stack<'static>> = StaticCell::new();
    let stack = STACK_STORAGE.init(stack);

    static RUNNER_STORAGE: StaticCell<embassy_net::Runner<'static, cyw43::NetDriver<'static>>> = StaticCell::new();
    let runner = RUNNER_STORAGE.init(runner);

    spawner.spawn(unwrap!(net_task(runner)));
    info!("Network task spawned");

    // 等待网络栈启动
    Timer::after(Duration::from_secs(3)).await;

    // Start HTTP server
    info!("Spawning HTTP server task...");
    spawner.spawn(unwrap!(http_server_task(stack)));
    info!("✅ HTTP server task spawned");

    info!("==================================================");
    info!("🚀 Pico 2W WiFi AP Ready!");
    info!("==================================================");
    info!("1. Connect to WiFi: {}", WIFI_SSID);
    info!("   Password: {}", WIFI_PASSWORD);
    info!("");
    info!("2. MANUAL IP Configuration:");
    info!("   IP Address: 192.168.4.2 (or .3, .4, etc.)");
    info!("   Subnet Mask: 255.255.255.0");
    info!("   Gateway: 192.168.4.1");
    info!("");
    info!("3. Open browser to: http://192.168.4.1");
    info!("==================================================");

    // LED心跳
    info!("Starting LED heartbeat...");
    let mut count = 0u32;
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(1900)).await;
        
        count += 1;
        if count % 5 == 0 {
            info!("System alive (heartbeat #{})", count);
        }
    }
}
