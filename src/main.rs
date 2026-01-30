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
    heapless::String<1024>,
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
    heapless::String<64>,  // Changed from &'static str to owned String
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

    let mut rx_buffer = [0; 16384];
    let mut tx_buffer = [0; 16384];
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

            // Handle button triggers
            if path.contains("/trigger_http") {
                info!("HTTP request triggered!");
                HTTP_REQUEST_TRIGGER.signal(true);
            } else if path.contains("/trigger_at") {
                info!("AT test triggered!");
                let mut default_cmd = heapless::String::new();
                let _ = default_cmd.push_str("AT\r\n");
                AT_TEST_TRIGGER.signal(default_cmd);  // Pass owned String
            } else if path.contains("/at_cmd=") {
                // Extract custom AT command from URL
                if let Some(cmd_start) = path.find("at_cmd=") {
                    let cmd = &path[cmd_start + 7..];
                    let decoded_cmd = url_decode(cmd);
                    info!("Custom AT command: {}", decoded_cmd);
                    AT_TEST_TRIGGER.signal(decoded_cmd);  // Pass owned String
                }
            }

            // Get EC800K status
            let status = EC800K_STATUS.lock().await;
            let data = EC800K_DATA.lock().await;
            let tx_count = UART_TX_COUNT.lock().await;
            let rx_count = UART_RX_COUNT.lock().await;
            let http_resp = HTTP_RESPONSE.lock().await;

            // Build response with two buttons
            let mut response_str = heapless::String::<4096>::new();
            use core::fmt::Write as _;
            let _ = core::write!(
                &mut response_str,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: ",
            );

            let body = {
                let mut body_str = heapless::String::<3500>::new();
                let _ = core::write!(
                    &mut body_str,
                    "<html><head><title>Pico 2W LTE Gateway</title>\
                    <meta name='viewport' content='width=device-width, initial-scale=1'>\
                    <style>\
                    body {{ font-family: Arial, sans-serif; margin: 20px; background: #f5f5f5; }}\
                    .container {{ max-width: 800px; margin: auto; background: white; padding: 20px; border-radius: 10px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }}\
                    h1 {{ color: #333; border-bottom: 2px solid #4CAF50; padding-bottom: 10px; }}\
                    .status {{ padding: 15px; margin: 10px 0; border-radius: 5px; background: #f0f8ff; }}\
                    .button-group {{ display: flex; gap: 10px; margin: 20px 0; flex-wrap: wrap; }}\
                    button {{ padding: 12px 24px; font-size: 16px; border: none; border-radius: 5px; cursor: pointer; }}\
                    .btn-http {{ background: #4CAF50; color: white; }}\
                    .btn-at {{ background: #2196F3; color: white; }}\
                    .btn-at2 {{ background: #FF9800; color: white; }}\
                    .btn-at3 {{ background: #9C27B0; color: white; }}\
                    pre {{ background: #f8f9fa; padding: 15px; border-radius: 5px; overflow: auto; max-height: 300px; font-size: 12px; border: 1px solid #ddd; }}\
                    .section {{ margin: 20px 0; padding: 15px; background: #fafafa; border-radius: 5px; }}\
                    .form-group {{ margin: 10px 0; }}\
                    input[type='text'] {{ width: 70%; padding: 8px; margin-right: 10px; border: 1px solid #ccc; border-radius: 4px; }}\
                    </style>\
                    </head><body>\
                    <div class='container'>\
                    <h1>Pico 2W LTE Gateway</h1>\
                    <div class='status'>\
                    <p><b>EC800K Status:</b> <span style='color:{}'>{}</span></p>\
                    <p><b>UART Stats:</b> TX: {} bytes | RX: {} bytes</p>\
                    <p><b>WiFi AP:</b> {} (password: {})</p>\
                    <p><b>Your IP:</b> 192.168.4.2 (set manually)</p>\
                    </div>\
                    <div class='section'>\
                    <h2>Quick Actions</h2>\
                    <div class='button-group'>\
                    <form action='/trigger_http' method='get'>\
                    <button class='btn-http' type='submit'>📡 Fetch httpbin.org/get</button>\
                    </form>\
                    <form action='/trigger_at' method='get'>\
                    <button class='btn-at' type='submit'>📶 Test AT Command</button>\
                    </form>\
                    <form action='/at_cmd=AT+CSQ' method='get'>\
                    <button class='btn-at2' type='submit'>📶 AT+CSQ (Signal)</button>\
                    </form>\
                    <form action='/at_cmd=AT+CREG?' method='get'>\
                    <button class='btn-at3' type='submit'>📶 AT+CREG? (Network)</button>\
                    </form>\
                    </div>\
                    <div class='form-group'>\
                    <form action='/' method='get'>\
                    <input type='text' name='at_cmd' placeholder='Enter custom AT command (e.g., AT+CGMI)'>\
                    <button class='btn-at' type='submit'>Send AT Command</button>\
                    </form>\
                    </div>\
                    </div>\
                    <div class='section'>\
                    <h2>HTTP Response (httpbin.org/get)</h2>\
                    <pre>{}</pre>\
                    </div>\
                    <div class='section'>\
                    <h2>EC800K Communication Log</h2>\
                    <pre>{}</pre>\
                    </div>\
                    <div class='section'>\
                    <h3>Pin Configuration (Pico 2W)</h3>\
                    <ul>\
                    <li><b>EC800K TX</b> → GP13 (UART0 RX)</li>\
                    <li><b>EC800K RX</b> → GP12 (UART0 TX)</li>\
                    <li><b>EC800K GND</b> → GND</li>\
                    <li><b>EC800K VCC</b> → 5V or 3.3V (check module)</li>\
                    </ul>\
                    <p><small>Note: You must manually set your IP to 192.168.4.2/24</small></p>\
                    </div>\
                    </div>\
                    </body></html>",
                    if status.contains("ERROR") {
                        "red"
                    } else if status.contains("Ready") || status.contains("complete") {
                        "green"
                    } else {
                        "orange"
                    },
                    *status,
                    *tx_count,
                    *rx_count,
                    WIFI_SSID,
                    WIFI_PASSWORD,
                    if http_resp.is_empty() {
                        "[No HTTP response yet - click 'Fetch httpbin.org/get' to test]"
                    } else {
                        http_resp.as_str()
                    },
                    if data.is_empty() {
                        "[Waiting for EC800K communication...]"
                    } else {
                        data.as_str()
                    }
                );
                body_str
            };

            let _ = core::write!(&mut response_str, "{}\r\n\r\n{}", body.len(), body.as_str());

            // Write response
            info!("Sending response ({} bytes)", response_str.len());
            socket.write_all(response_str.as_bytes()).await?;
            socket.flush().await?;
            info!("Response sent successfully");
        }
    }

    Timer::after(Duration::from_millis(100)).await;
    Ok(())
}

// Helper function to URL decode AT commands
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
    
    // Add \r\n if not present
    if !output.ends_with("\r\n") {
        let _ = output.push_str("\r\n");
    }
    
    output
}

#[embassy_executor::task]
async fn uart_task(mut tx: BufferedUartTx, mut rx: BufferedUartRx) {
    info!("UART task started with GP12(TX) -> EC800K RX, GP13(RX) <- EC800K TX");
    
    // Initial status update
    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Initializing EC800K...";
    }
    
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("=== EC800K LTE Modem ===\n");
        let _ = data.push_str("Pins: GP12(TX)→EC800K_RX, GP13(RX)←EC800K_TX\n");
        let _ = data.push_str("Baud: 921600 (default)\n");
    }
    
    // Wait for modem to stabilize
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
                }
            }
            _ => break,
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    
    // Main loop to handle triggers
    loop {
        // Wait for either HTTP trigger or AT trigger
        info!("Waiting for trigger...");
        
        // Use embassy_futures::select to wait for either signal
        use embassy_futures::select;
        
        match select::select(
            HTTP_REQUEST_TRIGGER.wait(),
            AT_TEST_TRIGGER.wait()
        ).await {
            select::Either::First(_) => {
                info!("HTTP test triggered!");
                run_http_test(&mut tx, &mut rx).await;
            }
            select::Either::Second(command) => {
                info!("AT command triggered: {}", command);
                run_at_test(&mut tx, &mut rx, command.as_str()).await;
            }
        }
    }
}

async fn read_uart_data(rx: &mut BufferedUartRx) {
    let mut buf = [0u8; 256];
    // FIXED: Use read() instead of try_read()
    match rx.read(&mut buf).await {
        Ok(n) if n > 0 => {
            let mut rx_count = UART_RX_COUNT.lock().await;
            *rx_count += n as u32;
            
            if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                // Log unexpected data
                if !s.trim().is_empty() {
                    let mut data = EC800K_DATA.lock().await;
                    // Keep buffer manageable
                    if data.len() > 800 {
                        let start = data.len() - 600;
                        let mut tail = heapless::String::<600>::new();
                        let _ = tail.push_str(&data[start..]);
                        data.clear();
                        let _ = data.push_str("...[truncated]...\n");
                        let _ = data.push_str(tail.as_str());
                    }
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);
                }
            }
        }
        Ok(_) => {} // n == 0
        Err(e) => warn!("UART read error: {:?}", e),
    }
}

async fn run_at_test(tx: &mut BufferedUartTx, rx: &mut BufferedUartRx, command: &str) {
    let mut status = EC800K_STATUS.lock().await;
    *status = "Sending AT command...";
    drop(status);
    
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("\n=== AT TEST ===\n");
        let _ = data.push_str(">> ");
        let _ = data.push_str(command);
    }
    
    // Send the AT command
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
            *status = "ERROR: Send failed";
            return;
        }
    }
    
    // Wait for response
    Timer::after(Duration::from_millis(500)).await;
    
    let mut response_received = false;
    for _ in 0..10 {
        let mut buf = [0u8; 512];
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                response_received = true;
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;
                
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("AT Response: {}", s);
                    
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);
                    
                    let mut status = EC800K_STATUS.lock().await;
                    if s.contains("OK") {
                        *status = "AT OK";
                    } else if s.contains("ERROR") {
                        *status = "AT ERROR";
                    } else {
                        *status = "AT Response received";
                    }
                }
                break;
            }
            _ => {}
        }
        Timer::after(Duration::from_millis(200)).await;
    }
    
    if !response_received {
        let mut status = EC800K_STATUS.lock().await;
        *status = "ERROR: No response to AT command";
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("<< NO RESPONSE\n");
    }
}

async fn run_http_test(tx: &mut BufferedUartTx, rx: &mut BufferedUartRx) {
    // Simplified HTTP test
    let mut status = EC800K_STATUS.lock().await;
    *status = "Starting HTTP test...";
    drop(status);
    
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("\n=== HTTP TEST ===\n");
    }
    
    // Send AT command to check modem
    let at_cmd = b"AT\r\n";
    tx.write_all(at_cmd).await.ok();
    Timer::after(Duration::from_millis(500)).await;
    
    // Clear any response
    let mut buf = [0u8; 256];
    let _ = rx.read(&mut buf).await;
    
    // For now, just update status
    let mut status = EC800K_STATUS.lock().await;
    *status = "HTTP test would connect to httpbin.org";
    
    {
        let mut http_resp = HTTP_RESPONSE.lock().await;
        http_resp.clear();
        let _ = http_resp.push_str("HTTP test triggered but not fully implemented in this version.\n");
        let _ = http_resp.push_str("To fully implement, you would need to:\n");
        let _ = http_resp.push_str("1. Initialize EC800K with APN (AT+CGDCONT=1,\"IP\",\"your_apn\")\n");
        let _ = http_resp.push_str("2. Activate PDP context (AT+QIACT=1)\n");
        let _ = http_resp.push_str("3. Open TCP connection (AT+QIOPEN=...)\n");
        let _ = http_resp.push_str("4. Send HTTP request and read response\n");
    }
    
    info!("HTTP test triggered (simplified version)");
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Initialize firmware blobs
    let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");

    // Initialize CYW43 WiFi chip
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
    
    // FIXED: Use expect() to unwrap the Result and pass SpawnToken to spawn()
    spawner.spawn(cyw43_task(runner).expect("Failed to spawn cyw43 task"));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::Performance)
        .await;

    // Initialize UART for EC800K - USING GP12 and GP13
    // GP12 = TX (to EC800K RX)
    // GP13 = RX (from EC800K TX)
    
    static UART_TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 2048]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 2048]);

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = 921600; // EC800K default baud rate

    let uart = BufferedUart::new(
        p.UART0,
        p.PIN_12,  // Changed from PIN_0 to PIN_12 (TX to EC800K RX)
        p.PIN_13,  // Changed from PIN_1 to PIN_13 (RX from EC800K TX)
        Irqs,
        uart_tx_buf,
        uart_rx_buf,
        uart_config,
    );

    let (uart_tx, uart_rx) = uart.split();

    // FIXED: Use expect() to unwrap the Result
    spawner.spawn(uart_task(uart_tx, uart_rx).expect("Failed to spawn uart task"));

    // Configure network stack for AP mode with static IP
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

    // FIXED: Use expect() to unwrap the Result
    spawner.spawn(net_task(runner).expect("Failed to spawn net task"));

    // Start WiFi AP
    info!("Starting WiFi AP...");
    info!("SSID: {}, Password: {}", WIFI_SSID, WIFI_PASSWORD);

    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 5).await;
    info!("AP started successfully!");

    // Wait for network stack to be fully ready
    Timer::after(Duration::from_secs(3)).await;
    info!("Network stack ready");

    // Spawn HTTP server
    info!("Starting HTTP server on port 80...");
    // FIXED: Use expect() to unwrap the Result
    spawner.spawn(http_server_task(stack).expect("Failed to spawn HTTP server task"));
    info!("HTTP server task spawned");

    info!("==========================================");
    info!("Pico 2W LTE Gateway Ready!");
    info!("Connect to WiFi: {}", WIFI_SSID);
    info!("Password: {}", WIFI_PASSWORD);
    info!("Visit: http://192.168.4.1");
    info!("EC800K UART: GP12(TX)→EC800K_RX, GP13(RX)←EC800K_TX");
    info!("==========================================");

    // Blink LED to indicate AP is running
    let mut blink_count = 0;
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(900)).await;
        
        blink_count += 1;
        if blink_count % 10 == 0 {
            info!("System alive...");
        }
    }
}
