#![no_std]
#![no_main]

use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, UART0};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::uart::{
    BufferedInterruptHandler, BufferedUart, BufferedUartRx, BufferedUartTx, Config as UartConfig,
};
use embassy_time::{Duration, Timer};
use embedded_io_async::Read;
use embedded_io_async::Write;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// Program metadata
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"Pico 2W LTE Gateway"),
    embassy_rp::binary_info::rp_program_description!(
        c"Pico 2W as WiFi AP with EC800K LTE module"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

const WIFI_SSID: &str = "Pico2W_LTE";
const WIFI_PASSWORD: &str = "12345678";

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

// Global state for web interface
static EC800K_STATUS: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    &str,
> = embassy_sync::mutex::Mutex::new("Initializing...");

static EC800K_DATA: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<2048>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

static HTTP_RESPONSE: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<2048>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

// Use signals instead of queues (available in your Embassy version)
static HTTP_REQUEST_SIGNAL: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    bool,
> = embassy_sync::signal::Signal::new();

static AT_COMMAND_SIGNAL: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<64>,
> = embassy_sync::signal::Signal::new();

static UART_TX_COUNT: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    u32,
> = embassy_sync::mutex::Mutex::new(0);

static UART_RX_COUNT: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    u32,
> = embassy_sync::mutex::Mutex::new(0);

#[embassy_executor::task]
async fn http_server_task(stack: &'static Stack<'static>) {
    info!("HTTP server task started");
    Timer::after(Duration::from_millis(500)).await;
    info!("Starting HTTP server on 192.168.4.1:80");

    let mut rx_buffer = [0; 8192];
    let mut tx_buffer = [0; 8192];
    let mut request_count = 0u32;

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        info!("Listening on TCP:80... (requests served: {})", request_count);
        if let Err(e) = socket.accept(80).await {
            warn!("Accept error: {:?}", e);
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());
        request_count += 1;

        match handle_client(&mut socket).await {
            Ok(_) => info!("Request #{} completed successfully", request_count),
            Err(e) => warn!("Request #{} failed: {:?}", request_count, e),
        }

        socket.abort();
        Timer::after(Duration::from_millis(10)).await;
    }
}

async fn handle_client(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    let mut buf = [0; 1024];

    // Quick read with short timeout
    let n = match embassy_time::with_timeout(Duration::from_secs(2), socket.read(&mut buf)).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            warn!("Read error: {:?}", e);
            return Err(e);
        }
        Err(_) => {
            warn!("Read timeout");
            return Ok(());
        }
    };

    if n == 0 {
        return Ok(());
    }

    let request = core::str::from_utf8(&buf[..n]).unwrap_or("");
    
    // Quick parsing
    if let Some(first_line) = request.lines().next() {
        let parts: heapless::Vec<&str, 3> = first_line.split_whitespace().collect();
        if parts.len() >= 2 {
            let path = parts[1];
            
            // Handle button triggers
            if path.contains("/trigger_http") {
                info!("HTTP request triggered!");
                HTTP_REQUEST_SIGNAL.signal(true);
            } else if path.contains("/trigger_at") {
                info!("AT test triggered!");
                let mut default_cmd = heapless::String::new();
                let _ = default_cmd.push_str("AT\r\n");
                AT_COMMAND_SIGNAL.signal(default_cmd);
            } else if path.contains("/at_cmd=") {
                if let Some(cmd_start) = path.find("at_cmd=") {
                    let cmd = &path[cmd_start + 7..];
                    let decoded_cmd = url_decode(cmd);
                    info!("Custom AT command: {}", decoded_cmd);
                    AT_COMMAND_SIGNAL.signal(decoded_cmd);
                }
            }

            // Get EC800K status
            let (status, tx_count, rx_count, http_resp, data) = {
                let status = EC800K_STATUS.lock().await;
                let data = EC800K_DATA.lock().await;
                let tx_count = UART_TX_COUNT.lock().await;
                let rx_count = UART_RX_COUNT.lock().await;
                let http_resp = HTTP_RESPONSE.lock().await;
                (*status, *tx_count, *rx_count, http_resp.clone(), data.clone())
            };

            // Build SIMPLE HTML response
            let html = build_simple_html(status, tx_count, rx_count, &http_resp, &data);
            
            // Send response
            socket.write_all(html.as_bytes()).await?;
            socket.flush().await?;
        }
    }

    Ok(())
}

fn build_simple_html(
    status: &str,
    tx_count: u32,
    rx_count: u32,
    http_response: &str,
    uart_log: &str
) -> heapless::String<4096> {
    let mut html = heapless::String::new();
    
    let status_color = if status.contains("ERROR") { "red" } 
        else if status.contains("OK") || status.contains("Ready") { "green" } 
        else { "orange" };
    
    let http_display = if http_response.is_empty() { "[No HTTP response yet]" } else { http_response };
    let uart_display = if uart_log.is_empty() { "[Waiting for EC800K...]" } else { uart_log };
    
    // VERY SIMPLE HTML - minimal CSS
    let _ = html.push_str("HTTP/1.1 200 OK\r\n");
    let _ = html.push_str("Content-Type: text/html; charset=utf-8\r\n");
    let _ = html.push_str("Connection: close\r\n\r\n");
    
    let _ = html.push_str("<!DOCTYPE html><html><head>");
    let _ = html.push_str("<title>Pico 2W LTE Gateway</title>");
    let _ = html.push_str("<meta name='viewport' content='width=device-width, initial-scale=1'>");
    let _ = html.push_str("<meta http-equiv='refresh' content='3'>");
    let _ = html.push_str("<style>");
    let _ = html.push_str("body{font-family:Arial;margin:20px;background:#f0f0f0}");
    let _ = html.push_str(".container{max-width:800px;margin:auto;background:white;padding:15px;border-radius:5px}");
    let _ = html.push_str("h1{color:#333;border-bottom:2px solid #4CAF50}");
    let _ = html.push_str(".btn{padding:10px 20px;margin:5px;border:none;border-radius:4px;color:white;cursor:pointer}");
    let _ = html.push_str(".btn-green{background:#4CAF50}");
    let _ = html.push_str(".btn-blue{background:#2196F3}");
    let _ = html.push_str(".btn-orange{background:#FF9800}");
    let _ = html.push_str(".btn-purple{background:#9C27B0}");
    let _ = html.push_str("pre{background:#f8f9fa;padding:10px;border-radius:4px;overflow:auto;max-height:200px;font-size:11px}");
    let _ = html.push_str("</style></head><body>");
    
    let _ = html.push_str("<div class='container'>");
    let _ = html.push_str("<h1>Pico 2W LTE Gateway</h1>");
    
    // Status
    let _ = html.push_str("<p><b>Status:</b> <span style='color:");
    let _ = html.push_str(status_color);
    let _ = html.push_str("'>");
    let _ = html.push_str(status);
    let _ = html.push_str("</span></p>");
    let _ = html.push_str("<p><b>UART:</b> TX: ");
    let _ = push_u32(&mut html, tx_count);
    let _ = html.push_str(" RX: ");
    let _ = push_u32(&mut html, rx_count);
    let _ = html.push_str("</p>");
    let _ = html.push_str("<p><b>WiFi:</b> ");
    let _ = html.push_str(WIFI_SSID);
    let _ = html.push_str(" (pass: ");
    let _ = html.push_str(WIFI_PASSWORD);
    let _ = html.push_str(")</p>");
    
    // Buttons - SIMPLE FORMS
    let _ = html.push_str("<div>");
    let _ = html.push_str("<form action='/trigger_http' method='get' style='display:inline'>");
    let _ = html.push_str("<button class='btn btn-green' type='submit'>📡 HTTP Test</button>");
    let _ = html.push_str("</form>");
    let _ = html.push_str("<form action='/trigger_at' method='get' style='display:inline'>");
    let _ = html.push_str("<button class='btn btn-blue' type='submit'>📶 AT</button>");
    let _ = html.push_str("</form>");
    let _ = html.push_str("<form action='/at_cmd=AT+CSQ' method='get' style='display:inline'>");
    let _ = html.push_str("<button class='btn btn-orange' type='submit'>AT+CSQ</button>");
    let _ = html.push_str("</form>");
    let _ = html.push_str("<form action='/at_cmd=AT+CREG?' method='get' style='display:inline'>");
    let _ = html.push_str("<button class='btn btn-purple' type='submit'>AT+CREG?</button>");
    let _ = html.push_str("</form>");
    let _ = html.push_str("</div>");
    
    // Custom command
    let _ = html.push_str("<div style='margin:10px 0'>");
    let _ = html.push_str("<form action='/' method='get'>");
    let _ = html.push_str("<input type='text' name='at_cmd' placeholder='AT command' style='padding:8px;width:200px'>");
    let _ = html.push_str("<button class='btn btn-blue' type='submit'>Send</button>");
    let _ = html.push_str("</form>");
    let _ = html.push_str("</div>");
    
    // HTTP Response
    let _ = html.push_str("<h3>HTTP Response</h3>");
    let _ = html.push_str("<pre>");
    let _ = html.push_str(http_display);
    let _ = html.push_str("</pre>");
    
    // UART Log
    let _ = html.push_str("<h3>EC800K Log</h3>");
    let _ = html.push_str("<pre>");
    let _ = html.push_str(uart_display);
    let _ = html.push_str("</pre>");
    
    // Info
    let _ = html.push_str("<p><small>Auto-refresh: 3s | GP12→EC800K_RX, GP13←EC800K_TX</small></p>");
    let _ = html.push_str("</div></body></html>");
    
    html
}

fn push_u32(s: &mut heapless::String<4096>, n: u32) {
    let mut buffer = heapless::Vec::<u8, 10>::new();
    let mut n = n;
    
    if n == 0 {
        let _ = s.push('0');
        return;
    }
    
    while n > 0 {
        let digit = (n % 10) as u8 + b'0';
        let _ = buffer.push(digit);
        n /= 10;
    }
    
    for &digit in buffer.iter().rev() {
        let _ = s.push(digit as char);
    }
}

fn url_decode(input: &str) -> heapless::String<64> {
    let mut output = heapless::String::new();
    let mut chars = input.chars();
    
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex1 = chars.next().unwrap_or('0');
            let hex2 = chars.next().unwrap_or('0');
            if let (Some(h1), Some(h2)) = (hex1.to_digit(16), hex2.to_digit(16)) {
                let byte = ((h1 << 4) | h2) as u8;
                let _ = output.push(byte as char);
            }
        } else if c == '+' {
            let _ = output.push(' ');
        } else {
            let _ = output.push(c);
        }
    }
    
    if !output.ends_with("\r\n") {
        let _ = output.push_str("\r\n");
    }
    
    output
}

#[embassy_executor::task]
async fn uart_task(mut tx: BufferedUartTx, mut rx: BufferedUartRx) {
    info!("UART task started");
    
    // Initial status
    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Ready";
    }
    
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("EC800K Ready\n");
    }
    
    // Wait for modem
    Timer::after(Duration::from_secs(2)).await;
    
    // Flag to prevent multiple simultaneous operations
    let mut operation_in_progress = false;
    
    // MAIN LOOP - YIELD FREQUENTLY
    loop {
        // Check for signals - but only if not already processing
        if !operation_in_progress {
            // Use select to wait for either signal with timeout
            use embassy_futures::select;
            
            match select::select(
                AT_COMMAND_SIGNAL.wait(),
                HTTP_REQUEST_SIGNAL.wait()
            ).await {
                select::Either::First(cmd) => {
                    operation_in_progress = true;
                    info!("Processing AT command: {}", cmd);
                    quick_at_test(&mut tx, &mut rx, cmd.as_str()).await;
                    operation_in_progress = false;
                }
                select::Either::Second(_) => {
                    operation_in_progress = true;
                    info!("Processing HTTP test");
                    quick_http_test(&mut tx, &mut rx).await;
                    operation_in_progress = false;
                }
            }
        } else {
            // If operation in progress, just yield and check for incoming data
            Timer::after(Duration::from_millis(50)).await;
        }
        
        // Check for incoming data - VERY QUICK (non-blocking)
        let mut buf = [0u8; 64];
        match rx.try_read(&mut buf) {
            Ok(n) if n > 0 => {
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;
                
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    if !s.trim().is_empty() {
                        let mut data = EC800K_DATA.lock().await;
                        let _ = data.push_str("<< ");
                        let _ = data.push_str(s);
                        
                        // Keep log manageable
                        if data.len() > 1500 {
                            let new_len = data.len() - 1000;
                            let tail = &data[new_len..];
                            let mut new_data = heapless::String::new();
                            let _ = new_data.push_str("...[truncated]...\n");
                            let _ = new_data.push_str(tail);
                            *data = new_data;
                        }
                    }
                }
            }
            _ => {}
        }
        
        // CRITICAL: Yield to WiFi task
        Timer::after(Duration::from_millis(50)).await;
    }
}

async fn quick_at_test(tx: &mut BufferedUartTx, rx: &mut BufferedUartRx, command: &str) {
    // Update status quickly
    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Sending...";
    }
    
    // Log command
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("\n>> ");
        let _ = data.push_str(command);
    }
    
    // Send command - quick
    let cmd_bytes = command.as_bytes();
    if tx.write_all(cmd_bytes).await.is_ok() {
        let mut tx_count = UART_TX_COUNT.lock().await;
        *tx_count += cmd_bytes.len() as u32;
    }
    
    // YIELD IMMEDIATELY after send
    Timer::after(Duration::from_millis(10)).await;
    
    // Quick attempt to read response - NON-BLOCKING
    let mut buf = [0u8; 128];
    let mut got_response = false;
    
    // Try reading for max 100ms total
    for _ in 0..5 {
        match rx.try_read(&mut buf) {
            Ok(n) if n > 0 => {
                got_response = true;
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;
                
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    // Update log
                    {
                        let mut data = EC800K_DATA.lock().await;
                        let _ = data.push_str("<< ");
                        let _ = data.push_str(s);
                    }
                    
                    // Update status
                    {
                        let mut status = EC800K_STATUS.lock().await;
                        if s.contains("OK") {
                            *status = "AT OK";
                        } else if s.contains("ERROR") {
                            *status = "AT ERROR";
                        } else {
                            *status = "Response received";
                        }
                    }
                }
                break;
            }
            _ => {}
        }
        
        // Yield between attempts
        Timer::after(Duration::from_millis(10)).await;
    }
    
    if !got_response {
        let mut status = EC800K_STATUS.lock().await;
        *status = "No response";
    }
}

async fn quick_http_test(tx: &mut BufferedUartTx, rx: &mut BufferedUartRx) {
    // Update status
    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Testing...";
    }
    
    // Log
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("\n=== HTTP Test ===\n");
    }
    
    // Quick AT test
    let at_cmd = b"AT\r\n";
    if tx.write_all(at_cmd).await.is_ok() {
        let mut tx_count = UART_TX_COUNT.lock().await;
        *tx_count += at_cmd.len() as u32;
    }
    
    // Yield
    Timer::after(Duration::from_millis(50)).await;
    
    // Try to read response - quick
    let mut buf = [0u8; 128];
    let mut response = heapless::String::<256>::new(); // Fixed: specify capacity
    
    match rx.try_read(&mut buf) {
        Ok(n) if n > 0 => {
            if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                let _ = response.push_str(s);
                
                {
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);
                }
            }
        }
        _ => {}
    }
    
    // Update HTTP response
    {
        let mut http_resp = HTTP_RESPONSE.lock().await;
        http_resp.clear();
        
        if response.contains("OK") {
            let _ = http_resp.push_str("EC800K responding to AT\n");
            let _ = http_resp.push_str("For full HTTP, set APN first\n");
            
            let mut status = EC800K_STATUS.lock().await;
            *status = "AT OK";
        } else {
            let _ = http_resp.push_str("No response from EC800K\n");
            let _ = http_resp.push_str("Check wiring/power\n");
            
            let mut status = EC800K_STATUS.lock().await;
            *status = "No AT response";
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    info!("=== Pico 2W LTE Gateway ===");
    
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
    control.set_power_management(cyw43::PowerManagementMode::Performance).await;
    
    // Initialize UART
    static UART_TX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 1024]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 1024]);
    
    let mut uart_config = UartConfig::default();
    uart_config.baudrate = 115200;
    
    let uart = BufferedUart::new(
        p.UART0,
        p.PIN_12,
        p.PIN_13,
        Irqs,
        uart_tx_buf,
        uart_rx_buf,
        uart_config,
    );
    
    let (uart_tx, uart_rx) = uart.split();
    spawner.spawn(uart_task(uart_tx, uart_rx).expect("Failed to spawn uart task"));
    
    // Network config
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
        gateway: Some(embassy_net::Ipv4Address::new(192, 168, 4, 1)),
        dns_servers: heapless::Vec::new(),
    });
    
    static STACK: StaticCell<Stack<'static>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<16>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<16>::new()),
        embassy_rp::clocks::RoscRng.next_u64(),
    );
    let stack = STACK.init(stack);
    
    spawner.spawn(net_task(runner).expect("Failed to spawn net task"));
    
    // Start WiFi AP
    info!("Starting AP: {}", WIFI_SSID);
    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 5).await;
    info!("AP started!");
    
    Timer::after(Duration::from_secs(2)).await;
    
    // Start HTTP server
    spawner.spawn(http_server_task(stack).expect("Failed to spawn HTTP server task"));
    
    info!("Ready! http://192.168.4.1");
    
    // Main loop - blink LED
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(1900)).await;  // Slow blink = system OK
    }
}
