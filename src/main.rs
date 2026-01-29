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
    embassy_rp::binary_info::rp_program_name!(c"WiFi AP + EC800K Gateway"),
    embassy_rp::binary_info::rp_program_description!(
        c"Raspberry Pi Pico 2 W as WiFi AP routing through EC800K 4G module"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

const WIFI_SSID: &str = "Pico2W_Gateway";
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

static EC800K_STATUS: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    &str,
> = embassy_sync::mutex::Mutex::new("Initializing...");

static EC800K_BAUD: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    u32,
> = embassy_sync::mutex::Mutex::new(115200);

static EC800K_DATA: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<1024>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

static HTTP_RESPONSE: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<2048>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

static HTTP_REQUEST_TRIGGER: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    bool,
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
    // Static IP is already configured, just wait a bit for stack initialization
    info!("HTTP server task started");
    Timer::after(Duration::from_millis(500)).await;
    info!("Starting HTTP server on 192.168.4.1:80");

    let mut rx_buffer = [0; 16384];
    let mut tx_buffer = [0; 16384];
    let mut request_count = 0u32;

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(30)));

        info!(
            "Listening on TCP:80... (requests served: {})",
            request_count
        );
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

        // Ensure socket is fully closed
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

            // Check if trigger button was pressed
            if path.contains("/trigger") {
                info!("HTTP request triggered!");
                HTTP_REQUEST_TRIGGER.signal(true);
            }

            // Get EC800K status
            let status = EC800K_STATUS.lock().await;
            let baud = EC800K_BAUD.lock().await;
            let data = EC800K_DATA.lock().await;
            let tx_count = UART_TX_COUNT.lock().await;
            let rx_count = UART_RX_COUNT.lock().await;
            let http_resp = HTTP_RESPONSE.lock().await;

            // Build response string
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
                    "<html><head><meta http-equiv='refresh' content='5'><title>Pico 2W Gateway</title></head><body>\
                    <h1>Pico 2W Gateway Status</h1>\
                    <p><b>EC800K Status:</b> <span style='color:{}'>{}</span></p>\
                    <p><b>Baud Rate:</b> {} baud</p>\
                    <p><b>UART TX:</b> {} bytes | <b>RX:</b> {} bytes</p>\
                    <p><b>Request:</b> {} {}</p>\
                    <p><b>Network:</b> AP Mode - 192.168.4.1</p>\
                    <form action='/trigger' method='get'><button type='submit' style='padding:10px 20px;font-size:16px;background:#4CAF50;color:white;border:none;cursor:pointer'>Fetch httpbin.org/get</button></form>\
                    <hr>\
                    <h2>HTTP Test (httpbin.org/get):</h2>\
                    <pre style='background:#e8f4f8;padding:10px;overflow:auto;max-height:300px;font-size:12px'>{}</pre>\
                    <hr>\
                    <h2>EC800K Data Log:</h2>\
                    <pre style='background:#f0f0f0;padding:10px;overflow:auto;max-height:400px;font-size:12px'>{}</pre>\
                    <p><small>Auto-refresh: 5s | China Telecom APN: ctnet</small></p>\
                    <p style='color:#666'><small>Debug: If RX=0, check UART wiring (GP0→EC800K_RX, GP1→EC800K_TX, GND)</small></p>\
                    </body></html>",
                    if status.contains("ERROR") {
                        "red"
                    } else if status.contains("complete") {
                        "green"
                    } else {
                        "orange"
                    },
                    *status,
                    *baud,
                    *tx_count,
                    *rx_count,
                    method,
                    path,
                    if http_resp.is_empty() {
                        "[No HTTP response yet - waiting for EC800K to fetch data...]"
                    } else {
                        http_resp.as_str()
                    },
                    if data.is_empty() {
                        "[No data received - Check UART connection]"
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

#[embassy_executor::task]
async fn uart_task(mut tx: BufferedUartTx, mut rx: BufferedUartRx, baud_rate: u32) {
    info!("UART task started - Testing EC800K connection");

    // Update baud rate status
    {
        let mut baud = EC800K_BAUD.lock().await;
        *baud = baud_rate;
    }

    // Add diagnostic data immediately
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("=== UART Task Started ===\n");
        let _ = core::fmt::Write::write_fmt(
            &mut *data,
            format_args!("Baud: {} | Pins: GP0(TX), GP1(RX)\n", baud_rate),
        );
        let _ = data.push_str("Waiting for modem to stabilize...\n");
    }

    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Waiting for modem...";
    }

    // Wait for modem to boot and clear RDY messages
    Timer::after(Duration::from_secs(3)).await;

    // Clear any pending RDY messages
    let mut buf = [0u8; 512];
    for _ in 0..10 {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("Clearing boot messages: {}", s);
                }
            }
            _ => break,
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Testing AT command...";
    }

    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("Modem ready, starting init...\n");
    }

    // Simple AT test first
    info!("Sending test AT command");
    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str(">> AT\\r\\n\n");
    }

    let test_at = b"AT\r\n";
    let _ = tx.write_all(test_at).await;
    {
        let mut tx_count = UART_TX_COUNT.lock().await;
        *tx_count += test_at.len() as u32;
    }
    info!("AT command sent ({} bytes)", test_at.len());

    Timer::after(Duration::from_secs(1)).await;

    // Check for response
    let mut buf = [0u8; 256];
    let mut got_response = false;
    for attempt in 0..5 {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                got_response = true;
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;

                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("GOT RESPONSE: {}", s);
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);
                    let _ = data.push_str("\n");
                }
                break;
            }
            _ => {
                info!("Read attempt {}: no data", attempt + 1);
            }
        }
        Timer::after(Duration::from_millis(200)).await;
    }

    if !got_response {
        warn!("NO RESPONSE from EC800K after AT command!");
        let mut status = EC800K_STATUS.lock().await;
        *status = "ERROR: No response (check wiring)";
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("!! NO RESPONSE - Check:\n");
        let _ = data.push_str("  1. EC800K powered on?\n");
        let _ = data.push_str("  2. GP0 -> EC800K RX\n");
        let _ = data.push_str("  3. GP1 -> EC800K TX\n");
        let _ = data.push_str("  4. GND connected\n");
        let _ = data.push_str("  5. Try 115200 baud\n");

        // Keep trying to read
        loop {
            match rx.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                        info!("Late response: {}", s);
                        let mut data = EC800K_DATA.lock().await;
                        let _ = data.push_str("<< LATE: ");
                        let _ = data.push_str(s);
                    }
                }
                _ => {}
            }
            Timer::after(Duration::from_secs(1)).await;
        }
    }

    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "AT OK - Initializing modem...";
    }

    // Initialize EC800K modem for China Telecom
    let init_commands: &[&[u8]] = &[
        b"AT\r\n",                            // Test AT
        b"ATE0\r\n",                          // Disable echo
        b"AT+CPIN?\r\n",                      // Check SIM
        b"AT+CREG?\r\n",                      // Check network registration
        b"AT+CGATT=1\r\n",                    // Attach to GPRS
        b"AT+CGDCONT=1,\"IP\",\"ctnet\"\r\n", // China Telecom APN
        b"AT+QIACT=1\r\n",                    // Activate PDP context
        b"AT+QIACT?\r\n",                     // Query IP address
    ];

    for cmd in init_commands {
        info!("Sending: {}", core::str::from_utf8(*cmd).unwrap_or(""));
        let _ = tx.write_all(*cmd).await;

        {
            let mut tx_count = UART_TX_COUNT.lock().await;
            *tx_count += cmd.len() as u32;
        }

        Timer::after(Duration::from_millis(500)).await;

        // Read response
        let mut buf = [0u8; 512];
        let mut total_read = 0;
        let mut got_response = false;
        for _ in 0..20 {
            match rx.read(&mut buf[total_read..]).await {
                Ok(n) if n > 0 => {
                    total_read += n;
                    got_response = true;

                    let mut rx_count = UART_RX_COUNT.lock().await;
                    *rx_count += n as u32;

                    if let Ok(s) = core::str::from_utf8(&buf[..total_read]) {
                        info!("Response: {}", s);

                        // Log to web interface
                        let mut data = EC800K_DATA.lock().await;
                        let _ = data.push_str("<< ");
                        let _ = data.push_str(s);

                        if s.contains("OK") || s.contains("ERROR") {
                            break;
                        }
                    }
                }
                _ => break,
            }
            Timer::after(Duration::from_millis(100)).await;
        }

        if !got_response {
            let mut status = EC800K_STATUS.lock().await;
            *status = "ERROR: No response during init";
        }
    }

    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Ready - Click button to test";
    }

    info!("EC800K initialization complete - Waiting for button press");

    // Wait for user to trigger HTTP request
    info!("Waiting for HTTP request trigger...");
    HTTP_REQUEST_TRIGGER.wait().await;
    info!("HTTP request triggered by user!");

    Timer::after(Duration::from_millis(500)).await;

    {
        let mut data = EC800K_DATA.lock().await;
        let _ = data.push_str("\n=== TCP HTTP TEST ===\n");
    }

    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Opening TCP connection...";
    }

    // Open TCP connection to httpbin.org:80
    info!("Opening TCP connection to httpbin.org");
    let tcp_open = b"AT+QIOPEN=1,0,\"TCP\",\"httpbin.org\",80,0,1\r\n";
    let _ = tx.write_all(tcp_open).await;
    {
        let mut tx_count = UART_TX_COUNT.lock().await;
        *tx_count += tcp_open.len() as u32;
    }

    // Read initial OK response
    let mut buf = [0u8; 512];
    Timer::after(Duration::from_millis(500)).await;

    for _ in 0..5 {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;
                drop(rx_count);
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("Initial response: {}", s);
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);
                }
                break;
            }
            _ => {}
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // Now wait for +QIOPEN URC (can take several seconds)
    info!("Waiting for +QIOPEN connection result...");
    Timer::after(Duration::from_secs(3)).await;

    let mut connected = false;
    for _ in 0..100 {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;
                drop(rx_count);
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("TCP connection status: {}", s);
                    let mut data = EC800K_DATA.lock().await;
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);

                    // +QIOPEN: 0,0 means context 0, error 0 (success)
                    if s.contains("+QIOPEN: 0,0") {
                        connected = true;
                        info!("TCP connection established!");
                        break;
                    }
                    // Check for error codes
                    if s.contains("+QIOPEN:") && !s.contains(",0") {
                        info!("TCP connection failed");
                        break;
                    }
                }
            }
            _ => {}
        }
        Timer::after(Duration::from_millis(200)).await;
    }

    if !connected {
        let mut status = EC800K_STATUS.lock().await;
        *status = "TCP connection failed";
        info!("TCP connection failed");
    } else {
        info!("TCP connected, sending HTTP request");

        {
            let mut status = EC800K_STATUS.lock().await;
            *status = "TCP connected, sending request...";
        }

        // Send HTTP GET request via TCP
        let http_request = b"GET /get HTTP/1.1\r\nHost: httpbin.org\r\nConnection: close\r\n\r\n";

        let mut len_str = heapless::String::<8>::new();
        use core::fmt::Write as _;
        let _ = core::write!(&mut len_str, "{}", http_request.len());

        let send_cmd = b"AT+QISEND=0,";
        let _ = tx.write_all(send_cmd).await;
        let _ = tx.write_all(len_str.as_bytes()).await;
        let _ = tx.write_all(b"\r\n").await;

        {
            let mut tx_count = UART_TX_COUNT.lock().await;
            *tx_count += send_cmd.len() as u32 + len_str.len() as u32 + 2;
        }

        Timer::after(Duration::from_millis(500)).await;

        // Wait for '>'
        for _ in 0..10 {
            match rx.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    let mut rx_count = UART_RX_COUNT.lock().await;
                    *rx_count += n as u32;
                    drop(rx_count);
                    if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                        let mut data = EC800K_DATA.lock().await;
                        let _ = data.push_str("<< ");
                        let _ = data.push_str(s);
                        if s.contains(">") {
                            break;
                        }
                    }
                }
                _ => {}
            }
            Timer::after(Duration::from_millis(50)).await;
        }

        // Send actual HTTP request
        let _ = tx.write_all(http_request).await;
        {
            let mut tx_count = UART_TX_COUNT.lock().await;
            *tx_count += http_request.len() as u32;
        }

        Timer::after(Duration::from_secs(2)).await;

        // Read HTTP response
        {
            let mut status = EC800K_STATUS.lock().await;
            *status = "Receiving HTTP response...";
        }

        for _ in 0..200 {
            match rx.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    let mut rx_count = UART_RX_COUNT.lock().await;
                    *rx_count += n as u32;
                    drop(rx_count);

                    if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                        info!("HTTP response chunk: {}", s);

                        let mut http_resp = HTTP_RESPONSE.lock().await;
                        let _ = http_resp.push_str(s);

                        let mut data = EC800K_DATA.lock().await;
                        let _ = data.push_str("<< ");
                        let _ = data.push_str(s);
                    }
                }
                _ => {}
            }
            Timer::after(Duration::from_millis(100)).await;
        }

        // Close connection
        let close_cmd = b"AT+QICLOSE=0\r\n";
        let _ = tx.write_all(close_cmd).await;
        {
            let mut tx_count = UART_TX_COUNT.lock().await;
            *tx_count += close_cmd.len() as u32;
        }

        let mut status = EC800K_STATUS.lock().await;
        *status = "HTTP test complete!";
    }

    // Continue reading responses and log to web interface
    let mut buf = [0u8; 512];
    loop {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                let mut rx_count = UART_RX_COUNT.lock().await;
                *rx_count += n as u32;

                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("EC800K: {}", s);

                    // Update the data log for web display
                    let mut data = EC800K_DATA.lock().await;
                    // Keep last 800 chars to prevent overflow
                    if data.len() > 800 {
                        let start = data.len() - 600;
                        let mut tail_buf = heapless::String::<600>::new();
                        let _ = tail_buf.push_str(&data[start..]);
                        data.clear();
                        let _ = data.push_str("...[truncated]...\n");
                        let _ = data.push_str(tail_buf.as_str());
                    }
                    let _ = data.push_str("<< ");
                    let _ = data.push_str(s);
                }
            }
            _ => {}
        }
        Timer::after(Duration::from_millis(100)).await;
    }
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
    spawner.spawn(cyw43_task(runner).unwrap());

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::Performance)
        .await;

    // Initialize UART for EC800K
    // GP0 = TX (to EC800K RX)
    // GP1 = RX (from EC800K TX)

    static UART_TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 2048]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 2048]);

    let mut uart_config = UartConfig::default();
    // Manual testing: Try 115200, 230400, 460800, 921600
    // Change this value, rebuild, and see if EC800K responds in logs
    uart_config.baudrate = 115200; // Lowered to 115200 for stability

    let uart = BufferedUart::new(
        p.UART0,
        p.PIN_0,
        p.PIN_1,
        Irqs,
        uart_tx_buf,
        uart_rx_buf,
        uart_config,
    );

    let (uart_tx, uart_rx) = uart.split();

    spawner.spawn(uart_task(uart_tx, uart_rx, uart_config.baudrate).unwrap());

    // Configure network stack for AP mode with static IP
    // Note: Clients must manually configure IP (192.168.4.2-254) as there's no DHCP server
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
        gateway: Some(embassy_net::Ipv4Address::new(192, 168, 4, 1)),
        dns_servers: heapless::Vec::new(),
    });

    let seed = 0x0123_4567_89ab_cdef; // Random seed for network stack

    static STACK: StaticCell<Stack<'static>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<16>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<16>::new()),
        seed,
    );
    let stack = STACK.init(stack);

    spawner.spawn(net_task(runner).unwrap());

    // Start WiFi AP first
    info!("Starting WiFi AP...");
    info!("SSID: {}, Password: {}", WIFI_SSID, WIFI_PASSWORD);

    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 5).await;
    info!("AP started successfully!");

    // Wait for network stack to be fully ready
    Timer::after(Duration::from_secs(3)).await;
    info!("Network stack ready");

    // Spawn HTTP server
    info!("Starting HTTP server on port 80...");
    spawner.spawn(http_server_task(stack).unwrap());
    info!("HTTP server task spawned");

    // Blink LED to indicate AP is running
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(900)).await;
    }
}
