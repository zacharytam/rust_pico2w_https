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
    heapless::String<64>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

static EC800K_DATA: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<2048>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

static HTTP_RESPONSE: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<2048>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

// Signals for button actions
static HTTP_REQUEST_TRIGGER: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    bool,
> = embassy_sync::signal::Signal::new();

static AT_TEST_TRIGGER: embassy_sync::signal::Signal<
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

static ACTION_IN_PROGRESS: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<64>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

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
        socket.set_timeout(Some(Duration::from_secs(30)));

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
        Timer::after(Duration::from_millis(50)).await;
    }
}

async fn handle_client(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    let mut buf = [0; 2048];

    // Read request with timeout
    let n = match embassy_time::with_timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
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
        info!("Empty request, closing");
        return Ok(());
    }

    let request = core::str::from_utf8(&buf[..n]).unwrap_or("");
    info!("HTTP Request ({} bytes)", n);

    // Parse HTTP request
    if let Some(first_line) = request.lines().next() {
        let parts: heapless::Vec<&str, 3> = first_line.split_whitespace().collect();
        if parts.len() >= 2 {
            let method = parts[0];
            let path = parts[1];
            info!("Method: {}, Path: {}", method, path);

            // Handle button triggers - IMMEDIATE REDIRECT
            let mut should_redirect = false;
            let mut trigger_type = "";
            
            if path.contains("/trigger_http") {
                info!("HTTP request triggered!");
                trigger_type = "http";
                should_redirect = true;
            } else if path.contains("/trigger_at") {
                info!("AT test triggered!");
                trigger_type = "at";
                should_redirect = true;
            } else if path.contains("/at_cmd=") {
                if let Some(cmd_start) = path.find("at_cmd=") {
                    let cmd = &path[cmd_start + 7..];
                    let decoded_cmd = url_decode(cmd);
                    info!("Custom AT command: {}", decoded_cmd);
                    trigger_type = "at_custom";
                    should_redirect = true;
                }
            }

            if should_redirect {
                // Send immediate redirect first
                let response = "HTTP/1.1 302 Found\r\nLocation: /\r\n\r\n";
                socket.write_all(response.as_bytes()).await?;
                socket.flush().await?;
                
                // Then trigger the action (after response is sent)
                match trigger_type {
                    "http" => {
                        {
                            let mut progress = ACTION_IN_PROGRESS.lock().await;
                            progress.clear();
                            let _ = progress.push_str("🔄 Fetching httpbin.org...");
                        }
                        HTTP_REQUEST_TRIGGER.signal(true);
                    }
                    "at" => {
                        {
                            let mut progress = ACTION_IN_PROGRESS.lock().await;
                            progress.clear();
                            let _ = progress.push_str("🔄 Sending AT...");
                        }
                        let mut default_cmd = heapless::String::new();
                        let _ = default_cmd.push_str("AT\r\n");
                        AT_TEST_TRIGGER.signal(default_cmd);
                    }
                    "at_custom" => {
                        if let Some(cmd_start) = path.find("at_cmd=") {
                            let cmd = &path[cmd_start + 7..];
                            let decoded_cmd = url_decode(cmd);
                            {
                                let mut progress = ACTION_IN_PROGRESS.lock().await;
                                progress.clear();
                                let _ = progress.push_str("🔄 Sending ");
                                let _ = progress.push_str(cmd);
                                if progress.len() > 40 {
                                    progress.truncate(40);
                                    let _ = progress.push_str("...");
                                }
                            }
                            AT_TEST_TRIGGER.signal(decoded_cmd);
                        }
                    }
                    _ => {}
                }
                return Ok(());
            }

            // Get status for normal page view
            let status = EC800K_STATUS.lock().await;
            let data = EC800K_DATA.lock().await;
            let tx_count = UART_TX_COUNT.lock().await;
            let rx_count = UART_RX_COUNT.lock().await;
            let http_resp = HTTP_RESPONSE.lock().await;
            let progress = ACTION_IN_PROGRESS.lock().await;

            let html = format_html_response(
                status.as_str(),
                *tx_count,
                *rx_count,
                http_resp.as_str(),
                data.as_str(),
                progress.as_str()
            );
            
            match socket.write_all(html.as_bytes()).await {
                Ok(_) => info!("Response sent ({} bytes)", html.len()),
                Err(e) => warn!("Write error: {:?}", e),
            }
            
            socket.flush().await?;
        }
    }

    Ok(())
}

fn format_html_response(
    status: &str,
    tx_count: u32,
    rx_count: u32,
    http_response: &str,
    uart_log: &str,
    progress: &str
) -> heapless::String<4096> {
    let mut html = heapless::String::new();
    
    let status_display = if status.is_empty() { "Initializing EC800K..." } else { status };
    let status_color = if status_display.contains("ERROR") { "red" } 
        else if status_display.contains("Ready") || status_display.contains("OK") || status_display.contains("complete") { "green" } 
        else { "orange" };
    
    let http_display = if http_response.is_empty() { "[No HTTP response yet]" } else { http_response };
    let uart_display = if uart_log.is_empty() { "[Waiting for EC800K communication...]" } else { uart_log };
    
    // Build HTML
    let _ = html.push_str("HTTP/1.1 200 OK\r\n");
    let _ = html.push_str("Content-Type: text/html; charset=utf-8\r\n");
    let _ = html.push_str("Connection: close\r\n\r\n");
    
    let _ = html.push_str("<!DOCTYPE html><html><head>");
    let _ = html.push_str("<title>Pico 2W LTE Gateway</title>");
    let _ = html.push_str("<meta name='viewport' content='width=device-width, initial-scale=1'>");
    let _ = html.push_str("<meta http-equiv='refresh' content='2'>");  // 2 seconds refresh
    let _ = html.push_str("<style>");
    let _ = html.push_str("body { font-family: Arial, sans-serif; margin: 20px; background: #f5f5f5; }");
    let _ = html.push_str(".container { max-width: 800px; margin: auto; background: white; padding: 20px; border-radius: 10px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }");
    let _ = html.push_str("h1 { color: #333; border-bottom: 2px solid #4CAF50; padding-bottom: 10px; }");
    let _ = html.push_str(".status { padding: 15px; margin: 10px 0; border-radius: 5px; background: #f0f8ff; }");
    let _ = html.push_str(".progress { padding: 15px; margin: 10px 0; border-radius: 5px; background: #fff3cd; border: 1px solid #ffc107; }");
    let _ = html.push_str(".button-group { display: flex; gap: 10px; margin: 20px 0; flex-wrap: wrap; }");
    let _ = html.push_str("button { padding: 12px 24px; font-size: 16px; border: none; border-radius: 5px; cursor: pointer; transition: opacity 0.3s; }");
    let _ = html.push_str("button:hover { opacity: 0.8; }");
    let _ = html.push_str(".btn-http { background: #4CAF50; color: white; }");
    let _ = html.push_str(".btn-at { background: #2196F3; color: white; }");
    let _ = html.push_str(".btn-at2 { background: #FF9800; color: white; }");
    let _ = html.push_str(".btn-at3 { background: #9C27B0; color: white; }");
    let _ = html.push_str("pre { background: #f8f9fa; padding: 15px; border-radius: 5px; overflow: auto; max-height: 300px; font-size: 12px; border: 1px solid #ddd; white-space: pre-wrap; word-wrap: break-word; }");
    let _ = html.push_str(".section { margin: 20px 0; padding: 15px; background: #fafafa; border-radius: 5px; }");
    let _ = html.push_str(".form-group { margin: 10px 0; }");
    let _ = html.push_str("input[type='text'] { width: 300px; padding: 8px; margin-right: 10px; border: 1px solid #ccc; border-radius: 4px; }");
    let _ = html.push_str("</style>");
    let _ = html.push_str("<script>");
    let _ = html.push_str("window.onload = function() {");
    let _ = html.push_str("  // Auto-scroll to bottom of UART log");
    let _ = html.push_str("  var preElements = document.getElementsByTagName('pre');");
    let _ = html.push_str("  for(var i = 0; i < preElements.length; i++) {");
    let _ = html.push_str("    preElements[i].scrollTop = preElements[i].scrollHeight;");
    let _ = html.push_str("  }");
    let _ = html.push_str("}");
    let _ = html.push_str("</script>");
    let _ = html.push_str("</head><body>");
    
    let _ = html.push_str("<div class='container'>");
    let _ = html.push_str("<h1>Pico 2W LTE Gateway</h1>");
    
    // Progress indicator
    if !progress.is_empty() {
        let _ = html.push_str("<div class='progress'>");
        let _ = html.push_str("<strong>⏳ ");
        let _ = html.push_str(progress);
        let _ = html.push_str("</strong><br>");
        let _ = html.push_str("<small>Page refreshes every 2 seconds...</small>");
        let _ = html.push_str("</div>");
    }
    
    // Status section
    let _ = html.push_str("<div class='status'>");
    let _ = html.push_str("<p><b>EC800K Status:</b> <span style='color:");
    let _ = html.push_str(status_color);
    let _ = html.push_str("'>");
    let _ = html.push_str(status_display);
    let _ = html.push_str("</span></p>");
    let _ = html.push_str("<p><b>UART Stats:</b> TX: ");
    let _ = write_u32(&mut html, tx_count);
    let _ = html.push_str(" bytes | RX: ");
    let _ = write_u32(&mut html, rx_count);
    let _ = html.push_str(" bytes</p>");
    let _ = html.push_str("<p><b>WiFi AP:</b> ");
    let _ = html.push_str(WIFI_SSID);
    let _ = html.push_str(" (password: ");
    let _ = html.push_str(WIFI_PASSWORD);
    let _ = html.push_str(")</p>");
    let _ = html.push_str("<p><b>Your IP:</b> 192.168.4.2 (set manually)</p>");
    let _ = html.push_str("<p><i>Page auto-refreshes every 2 seconds</i></p>");
    let _ = html.push_str("</div>");
    
    // Button section
    let _ = html.push_str("<div class='section'>");
    let _ = html.push_str("<h2>Quick Actions</h2>");
    let _ = html.push_str("<div class='button-group'>");
    let _ = html.push_str("<a href='/trigger_http'><button class='btn-http'>📡 Fetch httpbin.org/get</button></a>");
    let _ = html.push_str("<a href='/trigger_at'><button class='btn-at'>📶 Test AT Command</button></a>");
    let _ = html.push_str("<a href='/at_cmd=AT+CSQ'><button class='btn-at2'>📶 AT+CSQ (Signal)</button></a>");
    let _ = html.push_str("<a href='/at_cmd=AT+CREG?'><button class='btn-at3'>📶 AT+CREG? (Network)</button></a>");
    let _ = html.push_str("</div>");
    
    // Custom command form
    let _ = html.push_str("<div class='form-group'>");
    let _ = html.push_str("<form action='/' method='get'>");
    let _ = html.push_str("<input type='text' name='at_cmd' placeholder='Enter custom AT command (e.g., AT+CGMI)'>");
    let _ = html.push_str("<button class='btn-at' type='submit'>Send AT Command</button>");
    let _ = html.push_str("</form>");
    let _ = html.push_str("</div>");
    let _ = html.push_str("</div>");
    
    // HTTP Response section
    let _ = html.push_str("<div class='section'>");
    let _ = html.push_str("<h2>HTTP Response (httpbin.org/get)</h2>");
    let _ = html.push_str("<pre>");
    let _ = html.push_str(http_display);
    let _ = html.push_str("</pre>");
    let _ = html.push_str("</div>");
    
    // UART Log section
    let _ = html.push_str("<div class='section'>");
    let _ = html.push_str("<h2>EC800K Communication Log</h2>");
    let _ = html.push_str("<pre>");
    let _ = html.push_str(uart_display);
    let _ = html.push_str("</pre>");
    let _ = html.push_str("</div>");
    
    // Connection info
    let _ = html.push_str("<div class='section'>");
    let _ = html.push_str("<h3>Connection Info</h3>");
    let _ = html.push_str("<ul>");
    let _ = html.push_str("<li><b>EC800K TX</b> → GP13 (UART0 RX)</li>");
    let _ = html.push_str("<li><b>EC800K RX</b> → GP12 (UART0 TX)</li>");
    let _ = html.push_str("<li><b>EC800K GND</b> → GND</li>");
    let _ = html.push_str("<li><b>EC800K VCC</b> → 3.3V (check module)</li>");
    let _ = html.push_str("<li><b>Refresh:</b> Auto-updates every 2 seconds</li>");
    let _ = html.push_str("</ul>");
    let _ = html.push_str("</div>");
    
    let _ = html.push_str("</div></body></html>");
    
    html
}

fn write_u32(s: &mut heapless::String<4096>, n: u32) -> Result<(), ()> {
    let mut buffer = heapless::Vec::<u8, 10>::new();
    let mut n = n;
    
    if n == 0 {
        let _ = s.push_str("0");
        return Ok(());
    }
    
    while n > 0 {
        let digit = (n % 10) as u8 + b'0';
        let _ = buffer.push(digit);
        n /= 10;
    }
    
    for &digit in buffer.iter().rev() {
        let _ = s.push(digit as char);
    }
    
    Ok(())
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
    info!("UART task started with GP12(TX) -> EC800K RX, GP13(RX) <- EC800K TX");
    
    {
        let mut status = EC800K_STATUS.lock().await;
        status.clear();
        let _ = status.push_str("Initializing EC800K...");
    }
    
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("=== EC800K LTE Modem ===\n");
        let _ = data.push_str("Pins: GP12(TX)→EC800K_RX, GP13(RX)←EC800K_TX\n");
        let _ = data.push_str("Baud: 921600 (default)\n");
        let _ = data.push_str("Waiting for modem...\n");
    }
    
    Timer::after(Duration::from_secs(3)).await;
    
    // Clear any initial garbage
    let mut buf = [0u8; 512];
    for _ in 0..5 {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("Boot messages: {}", s);
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str("Boot: ");
                    let _ = data.push_str(s);
                    
                    if s.contains("RDY") || s.contains("READY") {
                        let mut status = EC800K_STATUS.lock().await;
                        status.clear();
                        let _ = status.push_str("Boot complete - Ready");
                    }
                }
            }
            _ => break,
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    
    {
        let mut status = EC800K_STATUS.lock().await;
        if status.is_empty() {
            let _ = status.push_str("Ready - click buttons to test");
        }
    }
    
    // Main loop
    loop {
        use embassy_futures::select::{select, Either};
        
        // Wait for either trigger with timeout
        match select(
            select(HTTP_REQUEST_TRIGGER.wait(), AT_TEST_TRIGGER.wait()),
            Timer::after(Duration::from_millis(100))
        ).await {
            Either::First(trigger_result) => {
                match trigger_result {
                    Either::First(_) => {
                        info!("HTTP test triggered!");
                        run_http_test(&mut tx, &mut rx).await;
                    }
                    Either::Second(cmd) => {
                        info!("AT command triggered: {}", cmd);
                        run_at_test(&mut tx, &mut rx, cmd.as_str()).await;
                    }
                }
            }
            Either::Second(_) => {
                // Timeout, just check for incoming data
                check_for_incoming_data(&mut rx).await;
            }
        }
        
        Timer::after(Duration::from_millis(10)).await;
    }
}

async fn check_for_incoming_data(rx: &mut BufferedUartRx) {
    let mut buf = [0u8; 256];
    match rx.read(&mut buf).await {
        Ok(n) if n > 0 => {
            let mut rx_count = UART_RX_COUNT.lock().await;
            *rx_count += n as u32;
            
            if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                if !s.trim().is_empty() {
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);
                    
                    if data.len() > 1500 {
                        let start = data.len() - 1200;
                        let mut tail = heapless::String::<1200>::new();
                        let _ = tail.push_str(&data[start..]);
                        data.clear();
                        let _ = data.push_str("...[truncated]...\n");
                        let _ = data.push_str(tail.as_str());
                    }
                }
            }
        }
        _ => {}
    }
}

async fn run_at_test(tx: &mut BufferedUartTx, rx: &mut BufferedUartRx, command: &str) {
    info!("Running AT test: {}", command);
    
    {
        let mut status = EC800K_STATUS.lock().await;
        status.clear();
        let _ = status.push_str("Sending AT command...");
    }
    
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("\n=== AT TEST ===\n");
        let _ = data.push_str(">> ");
        let _ = data.push_str(command);
    }
    
    let cmd_bytes = command.as_bytes();
    match tx.write_all(cmd_bytes).await {
        Ok(_) => {
            let mut tx_count = UART_TX_COUNT.lock().await;
            *tx_count += cmd_bytes.len() as u32;
            info!("Sent AT command: {}", command);
        }
        Err(e) => {
            warn!("Failed to send AT command: {:?}", e);
            let mut status = EC800K_STATUS.lock().await;
            status.clear();
            let _ = status.push_str("ERROR: Send failed");
            {
                let mut progress = ACTION_IN_PROGRESS.lock().await;
                progress.clear();
            }
            return;
        }
    }
    
    tx.flush().await.ok();
    Timer::after(Duration::from_millis(100)).await;
    
    let mut response = heapless::String::<512>::new();
    let mut response_received = false;
    
    for attempt in 0..5 {
        let mut buf = [0u8; 256];
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                response_received = true;
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;
                
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("AT Response (attempt {}): {}", attempt + 1, s);
                    let _ = response.push_str(s);
                    
                    {
                        let mut data = EC800K_DATA.lock().await;
                        let _ = data.push_str("<< ");
                        let _ = data.push_str(s);
                    }
                    
                    if s.contains("OK") || s.contains("ERROR") || s.contains("+CSQ") || s.contains("+CREG") {
                        break;
                    }
                }
            }
            _ => {}
        }
        
        Timer::after(Duration::from_millis(100)).await;
    }
    
    {
        let mut status = EC800K_STATUS.lock().await;
        status.clear();
        
        if response.contains("OK") {
            let _ = status.push_str("✅ AT OK");
        } else if response.contains("ERROR") {
            let _ = status.push_str("❌ AT ERROR");
        } else if response.contains("+CSQ") {
            let _ = status.push_str("📶 Signal: ");
            if let Some(csq_start) = response.find("+CSQ:") {
                let csq_value = &response[csq_start + 5..];
                if let Some(end) = csq_value.find(',') {
                    let _ = status.push_str(&csq_value[..end]);
                }
            }
        } else if response.contains("+CREG") {
            let _ = status.push_str("📡 Network reg: ");
            if let Some(creg_start) = response.find("+CREG:") {
                let creg_value = &response[creg_start + 6..];
                if let Some(end) = creg_value.find(',') {
                    let _ = status.push_str(&creg_value[..end]);
                }
            }
        } else if response_received {
            let _ = status.push_str("📨 Response received");
        } else {
            let _ = status.push_str("⚠️ No response - check wiring");
            let mut data = EC800K_DATA.lock().await;
            let _ = data.push_str("<< NO RESPONSE - Check wiring/baud rate\n");
        }
        
        // Clear progress indicator
        let mut progress = ACTION_IN_PROGRESS.lock().await;
        progress.clear();
    }
    
    info!("AT test completed");
}

async fn run_http_test(tx: &mut BufferedUartTx, rx: &mut BufferedUartRx) {
    info!("Running HTTP test");
    
    {
        let mut status = EC800K_STATUS.lock().await;
        status.clear();
        let _ = status.push_str("Testing EC800K...");
    }
    
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("\n=== HTTP TEST ===\n");
    }
    
    // Clear any pending data
    let mut buf = [0u8; 256];
    let _ = rx.read(&mut buf).await;
    
    // Test AT command
    info!("Testing AT command");
    let test_cmd = b"AT\r\n";
    if tx.write_all(test_cmd).await.is_err() {
        warn!("Failed to send AT command");
        {
            let mut progress = ACTION_IN_PROGRESS.lock().await;
            progress.clear();
        }
        return;
    }
    
    {
        let mut tx_count = UART_TX_COUNT.lock().await;
        *tx_count += test_cmd.len() as u32;
    }
    
    tx.flush().await.ok();
    Timer::after(Duration::from_millis(500)).await;
    
    let mut at_response = heapless::String::<256>::new();
    match rx.read(&mut buf).await {
        Ok(n) if n > 0 => {
            if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                info!("AT Response: {}", s);
                at_response.push_str(s).ok();
                
                {
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str(">> AT\\r\\n\n<< ");
                    let _ = data.push_str(s);
                }
            }
        }
        _ => {}
    }
    
    {
        let mut http_resp = HTTP_RESPONSE.lock().await;
        http_resp.clear();
        
        if at_response.contains("OK") {
            let _ = http_resp.push_str("✅ EC800K is responding to AT commands!\n\n");
            let _ = http_resp.push_str("AT response:\n");
            let _ = http_resp.push_str(&at_response);
            let _ = http_resp.push_str("\n\nTo fetch httpbin.org, send:\n");
            let _ = http_resp.push_str("1. AT+CGDCONT=1,\"IP\",\"your_apn\"\n");
            let _ = http_resp.push_str("2. AT+QIACT=1\n");
            let _ = http_resp.push_str("3. AT+QIOPEN=1,0,\"TCP\",\"httpbin.org\",80\n");
            let _ = http_resp.push_str("4. AT+QISEND=0,<length>\n");
            let _ = http_resp.push_str("5. GET /get HTTP/1.1\\r\\nHost: httpbin.org\\r\\n\\r\\n");
            
            {
                let mut status = EC800K_STATUS.lock().await;
                status.clear();
                let _ = status.push_str("✅ EC800K Ready");
            }
        } else {
            let _ = http_resp.push_str("❌ EC800K not responding\n");
            let _ = http_resp.push_str("Check wiring and power\n");
            let _ = http_resp.push_str("GP12 → EC800K RX\n");
            let _ = http_resp.push_str("GP13 ← EC800K TX\n");
            let _ = http_resp.push_str("GND → GND\n");
            let _ = http_resp.push_str("3.3V → VCC\n");
            
            {
                let mut status = EC800K_STATUS.lock().await;
                status.clear();
                let _ = status.push_str("❌ No AT response");
            }
        }
        
        // Clear progress indicator
        let mut progress = ACTION_IN_PROGRESS.lock().await;
        progress.clear();
    }
    
    info!("HTTP test completed");
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

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
    control
        .set_power_management(cyw43::PowerManagementMode::Performance)
        .await;

    static UART_TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 2048]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 2048]);

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = 921600;

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

    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
        gateway: Some(embassy_net::Ipv4Address::new(192, 168, 4, 1)),
        dns_servers: heapless::Vec::new(),
    });

    let seed = 0x0123_4567_89ab_cdef;

    static STACK: StaticCell<Stack<'static>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<16>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<16>::new()),
        seed,
    );
    let stack = STACK.init(stack);

    spawner.spawn(net_task(runner).expect("Failed to spawn net task"));

    info!("Starting WiFi AP...");
    info!("SSID: {}, Password: {}", WIFI_SSID, WIFI_PASSWORD);

    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 5).await;
    info!("AP started successfully!");

    Timer::after(Duration::from_secs(3)).await;
    info!("Network stack ready");

    info!("Starting HTTP server on port 80...");
    spawner.spawn(http_server_task(stack).expect("Failed to spawn HTTP server task"));
    info!("HTTP server task spawned");

    info!("==========================================");
    info!("Pico 2W LTE Gateway Ready!");
    info!("Connect to WiFi: {}", WIFI_SSID);
    info!("Password: {}", WIFI_PASSWORD);
    info!("Visit: http://192.168.4.1");
    info!("EC800K UART: GP12(TX)→EC800K_RX, GP13(RX)←EC800K_TX");
    info!("==========================================");

    let mut blink_count = 0;
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(900)).await;
        
        blink_count += 1;
        if blink_count % 20 == 0 {
            info!("System alive...");
        }
    }
}
