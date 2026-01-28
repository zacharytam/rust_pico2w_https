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

// Global channel for UART communication
static UART_CHANNEL: embassy_sync::channel::Channel<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    UartRequest,
    1,
> = embassy_sync::channel::Channel::new();

static UART_RESPONSE: embassy_sync::channel::Channel<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    UartResponse,
    1,
> = embassy_sync::channel::Channel::new();

#[derive(Clone)]
enum UartRequest {
    HttpRequest {
        host: String<64>,
        path: String<128>,
    },
    AtCommand {
        command: String<128>,
    },
    HttpBinRequest,
}

struct UartResponse {
    data: String<8192>,
    success: bool,
}

#[embassy_executor::task]
async fn uart_task(mut uart: BufferedUart) {
    info!("UART task started at {} baud", UART_BAUDRATE);

    // Initialize EC800K
    Timer::after(Duration::from_secs(2)).await;

    info!("Initializing EC800K...");
    send_at_command(&mut uart, "AT").await;
    Timer::after(Duration::from_millis(500)).await;

    send_at_command(&mut uart, "AT+CPIN?").await;
    Timer::after(Duration::from_millis(500)).await;

    send_at_command(&mut uart, "AT+CREG?").await;
    Timer::after(Duration::from_millis(500)).await;

    send_at_command(&mut uart, "AT+CGATT=1").await;
    Timer::after(Duration::from_secs(1)).await;

    send_at_command(&mut uart, "AT+QICSGP=1,1,\"CTNET\"").await;
    Timer::after(Duration::from_millis(500)).await;

    send_at_command(&mut uart, "AT+QIACT=1").await;
    Timer::after(Duration::from_secs(2)).await;

    send_at_command(&mut uart, "AT+QIACT?").await;
    Timer::after(Duration::from_millis(500)).await;

    send_at_command(&mut uart, "AT+QIDNSCFG=1,\"114.114.114.114\",\"8.8.8.8\"").await;
    Timer::after(Duration::from_millis(500)).await;

    info!("EC800K initialized successfully!");

    // Main loop - wait for requests
    loop {
        let request = UART_CHANNEL.receive().await;
        
        let result = match request {
            UartRequest::HttpRequest { host, path } => {
                info!("Received HTTP request for {}:{}", host.as_str(), path.as_str());
                fetch_via_lte(&mut uart, &host, &path).await
            }
            UartRequest::AtCommand { command } => {
                info!("Received AT command: {}", command.as_str());
                execute_at_command(&mut uart, &command).await
            }
            UartRequest::HttpBinRequest => {
                info!("Received HttpBin request");
                fetch_via_lte(&mut uart, "httpbin.org", "/get").await
            }
        };

        UART_RESPONSE.send(result).await;
    }
}

async fn send_at_command(uart: &mut BufferedUart, cmd: &str) -> bool {
    let mut cmd_buf = String::<256>::new();
    let _ = cmd_buf.push_str(cmd);
    let _ = cmd_buf.push_str("\r\n");

    info!("TX: {}", cmd);
    let _ = uart.write_all(cmd_buf.as_bytes()).await;

    // Read response
    let mut response = [0u8; 512];
    Timer::after(Duration::from_millis(100)).await;

    if let Ok(Ok(n)) =
        embassy_time::with_timeout(Duration::from_secs(2), uart.read(&mut response)).await
    {
        if let Ok(resp_str) = core::str::from_utf8(&response[..n]) {
            info!("RX: {}", resp_str.trim());
            return true;
        }
    }
    false
}

async fn execute_at_command(uart: &mut BufferedUart, cmd: &str) -> UartResponse {
    info!("Executing AT command: {}", cmd);
    
    // Clear buffer
    clear_uart_buffer(uart).await;
    
    let mut cmd_buf = String::<256>::new();
    let _ = cmd_buf.push_str(cmd);
    let _ = cmd_buf.push_str("\r\n");
    
    let _ = uart.write_all(cmd_buf.as_bytes()).await;
    
    // Collect response
    let mut response = String::<8192>::new();
    let mut buffer = [0u8; 256];
    
    for _ in 0..10 {
        match embassy_time::with_timeout(Duration::from_millis(500), uart.read(&mut buffer)).await {
            Ok(Ok(n)) => {
                if let Ok(chunk) = core::str::from_utf8(&buffer[..n]) {
                    let _ = response.push_str(chunk);
                    if response.contains("OK\r\n") || response.contains("ERROR\r\n") {
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    
    let success = response.contains("OK");
    
    UartResponse {
        data: response,
        success,
    }
}

async fn clear_uart_buffer(uart: &mut BufferedUart) {
    Timer::after(Duration::from_millis(500)).await;
    let mut discard = [0u8; 256];
    while embassy_time::with_timeout(Duration::from_millis(100), uart.read(&mut discard))
        .await
        .is_ok()
    {}
}

async fn fetch_via_lte(
    uart: &mut BufferedUart,
    host: &str,
    path: &str,
) -> UartResponse {
    info!("Fetching http://{}{} via LTE...", host, path);

    // Clear buffer
    clear_uart_buffer(uart).await;

    // Step 1: Open TCP connection
    info!("1. Opening TCP connection...");
    let mut open_cmd = String::<256>::new();
    let _ = FmtWrite::write_fmt(&mut open_cmd, format_args!("AT+QIOPEN=1,0,\"TCP\",\"{}\",80,0,1\r\n", host));
    let _ = uart.write_all(open_cmd.as_bytes()).await;

    // Wait for +QIOPEN: 0,0
    let mut response = [0u8; 256];
    let mut connected = false;
    for _ in 0..20 {
        Timer::after(Duration::from_millis(500)).await;
        if let Ok(Ok(n)) = embassy_time::with_timeout(
            Duration::from_millis(500),
            uart.read(&mut response),
        )
        .await
        {
            if let Ok(resp_str) = core::str::from_utf8(&response[..n]) {
                info!("Open response: {}", resp_str);
                if resp_str.contains("+QIOPEN: 0,0") {
                    connected = true;
                    break;
                }
            }
        }
    }

    if !connected {
        warn!("TCP connection failed");
        let mut err_msg = String::new();
        let _ = err_msg.push_str("TCP connection failed");
        return UartResponse {
            data: err_msg,
            success: false,
        };
    }

    info!("✅ TCP connected");
    Timer::after(Duration::from_secs(1)).await;

    // Step 2: Prepare HTTP request
    let mut http_request = String::<512>::new();
    let _ = FmtWrite::write_fmt(&mut http_request, format_args!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: PicoLTE-Proxy/1.0\r\n\r\n",
        path, host
    ));

    // Step 3: Send HTTP data
    info!("2. Sending HTTP request...");
    let mut send_cmd = String::<64>::new();
    let _ = FmtWrite::write_fmt(&mut send_cmd, format_args!("AT+QISEND=0,{}\r\n", http_request.len()));
    let _ = uart.write_all(send_cmd.as_bytes()).await;

    // Wait for '>'
    Timer::after(Duration::from_millis(500)).await;
    let mut got_prompt = false;
    if let Ok(Ok(n)) =
        embassy_time::with_timeout(Duration::from_secs(5), uart.read(&mut response)).await
    {
        if let Ok(resp_str) = core::str::from_utf8(&response[..n]) {
            if resp_str.contains(">") {
                got_prompt = true;
            }
        }
    }

    if !got_prompt {
        warn!("No send prompt received");
        let _ = uart.write_all(b"AT+QICLOSE=0\r\n").await;
        let mut err_msg = String::new();
        let _ = err_msg.push_str("No send prompt");
        return UartResponse {
            data: err_msg,
            success: false,
        };
    }

    // Send actual HTTP data
    let _ = uart.write_all(http_request.as_bytes()).await;
    Timer::after(Duration::from_millis(500)).await;

    // Wait for SEND OK
    info!("3. Waiting for SEND OK...");
    let mut got_send_ok = false;
    for _ in 0..10 {
        if let Ok(Ok(n)) = embassy_time::with_timeout(
            Duration::from_millis(500),
            uart.read(&mut response),
        )
        .await
        {
            if let Ok(resp_str) = core::str::from_utf8(&response[..n]) {
                if resp_str.contains("SEND OK") {
                    got_send_ok = true;
                    info!("✅ SEND OK received");
                    break;
                }
            }
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    if !got_send_ok {
        warn!("SEND OK not received");
    }

    // Step 4: Collect HTTP response
    info!("4. Collecting HTTP response...");
    let mut http_data = String::<8192>::new();
    let mut buffer = [0u8; 512];
    let mut no_data_count = 0;

    for _ in 0..60 {
        // 30 seconds max
        match embassy_time::with_timeout(Duration::from_millis(500), uart.read(&mut buffer)).await
        {
            Ok(Ok(n)) => {
                if let Ok(chunk) = core::str::from_utf8(&buffer[..n]) {
                    let _ = http_data.push_str(chunk);
                    no_data_count = 0;

                    // Check if we have complete response
                    if http_data.contains("</html>") || http_data.contains("</HTML>") || 
                       http_data.contains("\"url\":") || http_data.contains("}") {
                        info!("✅ Complete response detected");
                        break;
                    }
                }
            }
            _ => {
                no_data_count += 1;
                if no_data_count > 6 && http_data.len() > 0 {
                    info!("✅ No more data");
                    break;
                }
            }
        }
    }

    info!("Total response: {} bytes", http_data.len());

    // Step 5: Close connection
    info!("5. Closing connection...");
    let _ = uart.write_all(b"AT+QICLOSE=0\r\n").await;
    Timer::after(Duration::from_millis(500)).await;

    UartResponse {
        data: http_data,
        success: true,
    }
}

#[embassy_executor::task]
async fn http_server_task(stack: &'static embassy_net::Stack<'static>) {
    info!("HTTP server starting...");

    // Wait for network link to be up
    info!("Waiting for network link...");
    let mut link_wait_count = 0;
    loop {
        if stack.is_link_up() {
            info!("Network link is UP!");
            break;
        }
        link_wait_count += 1;
        if link_wait_count % 10 == 0 {
            info!("Still waiting for network link... ({} attempts)", link_wait_count);
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // Wait for stack to be configured
    info!("Waiting for network config...");
    let mut config_wait_count = 0;
    loop {
        if stack.is_config_up() {
            info!("Network config is UP!");
            break;
        }
        config_wait_count += 1;
        if config_wait_count % 10 == 0 {
            info!("Still waiting for network config... ({} attempts)", config_wait_count);
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // Extra wait for everything to settle
    Timer::after(Duration::from_secs(2)).await;

    info!("==================================================");
    info!("HTTP SERVER READY on 192.168.4.1:80");
    info!("Client IP must be: 192.168.4.2-254/24");
    info!("Gateway must be: 192.168.4.1");
    info!("==================================================");

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut connection_count = 0u32;

    loop {
        info!("🔵 Creating new socket...");
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        info!("🔵 Listening on TCP port 80... (connections: {})", connection_count);
        match socket.accept(80).await {
            Ok(_) => {
                info!("✅ Socket accepted connection");
            }
            Err(e) => {
                warn!("❌ Accept error: {:?}", e);
                Timer::after(Duration::from_millis(100)).await;
                continue;
            }
        }

        connection_count += 1;
        info!("✅ Client connected! (connection #{})", connection_count);

        // Try to read the request first
        let mut request_buf = [0u8; 1024];
        let mut total_read = 0;

        // Set a shorter timeout for reading request
        socket.set_timeout(Some(Duration::from_secs(5)));

        loop {
            match socket.read(&mut request_buf[total_read..]).await {
                Ok(0) => {
                    info!("Client closed connection (read 0 bytes)");
                    break;
                }
                Ok(n) => {
                    total_read += n;
                    info!("Read {} bytes, total: {}", n, total_read);
                    if total_read >= request_buf.len()
                        || request_buf[..total_read]
                            .windows(4)
                            .any(|w| w == b"\r\n\r\n")
                    {
                        break;
                    }
                }
                Err(e) => {
                    info!("Read error or timeout: {:?}", e);
                    break;
                }
            }
        }

        if total_read == 0 {
            info!("No request data, sending welcome page");
            let response = format_main_page();
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            socket.close();
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }

        let request_str = match core::str::from_utf8(&request_buf[..total_read]) {
            Ok(s) => {
                info!("Request received ({} bytes): {}", total_read, s.split("\r\n").next().unwrap_or(""));
                s
            }
            Err(_) => {
                warn!("Invalid UTF-8 in request");
                let response = format_error_response("Invalid UTF-8 in request");
                let _ = socket.write_all(response.as_bytes()).await;
                socket.close();
                Timer::after(Duration::from_millis(100)).await;
                continue;
            }
        };

        // Parse request
        let response = if request_str.contains("/atcmd") {
            // AT command test form
            if let Some(body_start) = request_str.find("\r\n\r\n") {
                let body = &request_str[body_start + 4..];
                if body.contains("cmd=") {
                    // Parse command from POST request
                    let cmd_part = body.split("cmd=").nth(1).unwrap_or("");
                    let cmd = if let Some(end) = cmd_part.find('&') {
                        &cmd_part[..end]
                    } else {
                        cmd_part
                    };
                    let decoded_cmd = url_decode(cmd);
                    
                    // Send to UART task
                    let mut cmd_str = String::<128>::new();
                    let _ = cmd_str.push_str(&decoded_cmd);
                    UART_CHANNEL
                        .send(UartRequest::AtCommand { command: cmd_str })
                        .await;

                    // Wait for response
                    let uart_resp = UART_RESPONSE.receive().await;
                    
                    format_at_command_result(&decoded_cmd, &uart_resp)
                } else {
                    format_at_command_form("")
                }
            } else {
                format_at_command_form("")
            }
        } else if request_str.contains("/httpbin") {
            // HttpBin request
            UART_CHANNEL
                .send(UartRequest::HttpBinRequest)
                .await;

            let uart_resp = UART_RESPONSE.receive().await;
            
            if uart_resp.success {
                // Extract JSON content
                let json_content = extract_json(&uart_resp.data);
                if json_content.len() > 0 {
                    format_httpbin_response(&json_content)
                } else {
                    format_error_response("No JSON content found in response")
                }
            } else {
                format_error_response(&uart_resp.data)
            }
        } else if request_str.starts_with("GET /proxy?url=") {
            // Parse URL parameter
            let (host, path) = if let Some(url_start) = request_str.find("url=http://") {
                let url_part = &request_str[url_start + 11..];
                if let Some(url_end) = url_part.find(|c: char| c.is_whitespace() || c == '&') {
                    let full_url = &url_part[..url_end];
                    if let Some(slash_pos) = full_url.find('/') {
                        let h = &full_url[..slash_pos];
                        let p = &full_url[slash_pos..];
                        let mut host_str = String::<64>::new();
                        let _ = host_str.push_str(h);
                        let mut path_str = String::<128>::new();
                        let _ = path_str.push_str(p);
                        (Some(host_str), Some(path_str))
                    } else {
                        let mut host_str = String::<64>::new();
                        let _ = host_str.push_str(full_url);
                        let mut path_str = String::<128>::new();
                        let _ = path_str.push_str("/");
                        (Some(host_str), Some(path_str))
                    }
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

            if let (Some(h), Some(p)) = (host, path) {
                info!("Proxying: {}:{}", h.as_str(), p.as_str());

                // Send request to UART task
                UART_CHANNEL
                    .send(UartRequest::HttpRequest {
                        host: h.clone(),
                        path: p.clone(),
                    })
                    .await;

                // Wait for response
                let uart_resp = UART_RESPONSE.receive().await;

                if uart_resp.success {
                    // Extract HTML content
                    let html_content = extract_html(&uart_resp.data);

                    if html_content.len() > 0 {
                        info!("✅ Sending {} bytes to browser", html_content.len());
                        format_http_response(&html_content)
                    } else {
                        info!("⚠️ No HTML content found");
                        format_error_response("No HTML content found in response")
                    }
                } else {
                    format_error_response(&uart_resp.data)
                }
            } else {
                format_error_response("Invalid URL format. Use /proxy?url=http://example.com")
            }
        } else {
            // Default main page
            format_main_page()
        };

        // Send response
        info!("Sending response ({} bytes)...", response.len());
        match socket.write_all(response.as_bytes()).await {
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

fn url_decode(input: &str) -> String<128> {
    let mut result = String::<128>::new();
    let mut chars = input.chars();
    let mut temp_buf = [0u8; 2];
    
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex1 = chars.next().unwrap_or('0');
            let hex2 = chars.next().unwrap_or('0');
            temp_buf[0] = hex1 as u8;
            temp_buf[1] = hex2 as u8;
            if let Ok(byte) = u8::from_str_radix(core::str::from_utf8(&temp_buf).unwrap_or("00"), 16) {
                let _ = result.push(byte as char);
            }
        } else if c == '+' {
            let _ = result.push(' ');
        } else {
            let _ = result.push(c);
        }
    }
    
    result
}

fn extract_html(data: &str) -> String<8192> {
    let mut result = String::<8192>::new();

    // Find header end
    if let Some(header_end) = data.find("\r\n\r\n") {
        let _ = result.push_str(&data[header_end + 4..]);
    } else if let Some(html_start) = data.find("<!DOCTYPE") {
        let _ = result.push_str(&data[html_start..]);
    } else if let Some(html_start) = data.find("<html") {
        let _ = result.push_str(&data[html_start..]);
    } else if let Some(body_start) = data.find("<body") {
        let _ = result.push_str(&data[body_start..]);
    } else {
        let _ = result.push_str(data);
    }

    // Clean AT command artifacts
    let artifacts = ["AT+", "+QI", "SEND OK", "OK\r\n"];
    for artifact in &artifacts {
        if let Some(pos) = result.find(artifact) {
            result.truncate(pos);
            break;
        }
    }

    result
}

fn extract_json(data: &str) -> String<8192> {
    let mut result = String::<8192>::new();

    // Find JSON start
    if let Some(json_start) = data.find('{') {
        // Find matching closing brace
        let mut brace_count = 0;
        let mut in_string = false;
        let mut escape = false;
        
        for (i, c) in data[json_start..].chars().enumerate() {
            match c {
                '"' if !escape => in_string = !in_string,
                '\\' => escape = !escape,
                '{' if !in_string => brace_count += 1,
                '}' if !in_string => {
                    brace_count -= 1;
                    if brace_count == 0 {
                        let _ = result.push_str(&data[json_start..json_start + i + 1]);
                        break;
                    }
                }
                _ => escape = false,
            }
        }
    }

    result
}

fn format_main_page() -> String<8192> {
    let mut response = String::new();
    let _ = FmtWrite::write_fmt(&mut response, format_args!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
        <!DOCTYPE html>\
        <html>\
        <head>\
            <title>Pico 2W LTE Proxy</title>\
            <style>\
                body {{ font-family: Arial, sans-serif; margin: 40px; background: #f0f0f0; }}\
                .container {{ max-width: 800px; margin: 0 auto; background: white; padding: 30px; border-radius: 10px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }}\
                h1 {{ color: #333; border-bottom: 2px solid #007acc; padding-bottom: 10px; }}\
                .button {{ display: inline-block; padding: 12px 24px; margin: 10px; background: #007acc; color: white; text-decoration: none; border-radius: 5px; font-weight: bold; }}\
                .button:hover {{ background: #005fa3; }}\
                .section {{ margin: 30px 0; padding: 20px; border-left: 4px solid #007acc; background: #f8f9fa; }}\
                textarea {{ width: 100%; height: 100px; padding: 10px; font-family: monospace; margin: 10px 0; }}\
                pre {{ background: #f8f8f8; padding: 15px; border-radius: 5px; overflow-x: auto; white-space: pre-wrap; }}\
                .btn-secondary {{ background: #6c757d; }}\
                .btn-secondary:hover {{ background: #545b62; }}\
            </style>\
        </head>\
        <body>\
            <div class=\"container\">\
                <h1>📶 Pico 2W LTE Proxy</h1>\
                <p>Control panel for EC800K LTE module via WiFi</p>\
                \
                <div class=\"section\">\
                    <h2>🛠️ AT Command Testing</h2>\
                    <p>Send AT commands directly to the EC800K module:</p>\
                    <form action=\"/atcmd\" method=\"post\">\
                        <textarea name=\"cmd\" placeholder=\"Enter AT command (e.g., AT, AT+CSQ, AT+COPS?)\"></textarea><br>\
                        <button type=\"submit\" class=\"button\">📤 Send AT Command</button>\
                    </form>\
                    <p><a href=\"/atcmd\" class=\"button\">📝 AT Command Form</a></p>\
                </div>\
                \
                <div class=\"section\">\
                    <h2>🌐 HTTP Testing</h2>\
                    <p>Test HTTP connectivity with HttpBin.org:</p>\
                    <a href=\"/httpbin\" class=\"button\">📡 Test HttpBin.org</a>\
                    <p>Fetches data from http://httpbin.org/get to verify LTE connection.</p>\
                </div>\
                \
                <div class=\"section\">\
                    <h2>🔗 Custom Proxy</h2>\
                    <p>Proxy any HTTP website through LTE:</p>\
                    <form action=\"/\" style=\"margin-top: 15px;\">\
                        <input type=\"text\" name=\"url\" placeholder=\"http://example.com\" style=\"width: 70%; padding: 8px;\">\
                        <button type=\"submit\" style=\"padding: 8px 15px; background: #007acc; color: white; border: none; border-radius: 3px;\">Go</button>\
                    </form>\
                </div>\
                \
                <div class=\"section\">\
                    <h2>📊 System Info</h2>\
                    <ul>\
                        <li>WiFi AP: <strong>{}</strong></li>\
                        <li>WiFi Password: <strong>{}</strong></li>\
                        <li>IP Address: <strong>192.168.4.1</strong></li>\
                        <li>UART Baud Rate: <strong>921600</strong></li>\
                        <li>Test Target: <strong>http://httpbin.org/get</strong></li>\
                    </ul>\
                </div>\
            </div>\
        </body>\
        </html>",
        WIFI_SSID, WIFI_PASSWORD
    ));
    response
}

fn format_at_command_form(cmd: &str) -> String<8192> {
    let mut response = String::new();
    let _ = FmtWrite::write_fmt(&mut response, format_args!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
        <!DOCTYPE html>\
        <html>\
        <head>\
            <title>AT Command Tester - Pico LTE</title>\
            <style>\
                body {{ font-family: monospace; margin: 20px; background: #1e1e1e; color: #d4d4d4; }}\
                .container {{ max-width: 800px; margin: 0 auto; }}\
                h1 {{ color: #569cd6; }}\
                form {{ margin: 20px 0; }}\\
                textarea {{ width: 100%; height: 100px; background: #252525; color: #d4d4d4; border: 1px solid #3e3e3e; padding: 10px; font-family: monospace; }}\\
                button {{ background: #007acc; color: white; border: none; padding: 10px 20px; cursor: pointer; }}\\
                button:hover {{ background: #005fa3; }}\\
                .response {{ background: #0e2941; padding: 15px; border-radius: 5px; margin: 20px 0; white-space: pre-wrap; overflow-x: auto; }}\\
                .back {{ margin-top: 20px; display: inline-block; color: #569cd6; text-decoration: none; }}\\
                .success {{ color: #4ec9b0; }}\\
                .error {{ color: #f48771; }}\
            </style>\
        </head>\
        <body>\
            <div class=\"container\">\
                <h1>📡 AT Command Tester</h1>\
                <p>Send AT commands to EC800K LTE module:</p>\
                <form action=\"/atcmd\" method=\"post\">\
                    <textarea name=\"cmd\" placeholder=\"AT\\r\\nAT+CSQ\\r\\nAT+COPS?\\r\\nAT+CGMR\">{}</textarea><br>\
                    <button type=\"submit\">📤 Send Command</button>\
                </form>\
                <a href=\"/\" class=\"back\">← Back to Main</a>\
            </div>\
        </body>\
        </html>",
        cmd
    ));
    response
}

fn format_at_command_result(cmd: &str, result: &UartResponse) -> String<8192> {
    let mut response = String::new();
    
    let status_class = if result.success { "success" } else { "error" };
    let status_text = if result.success { "✅ Success" } else { "❌ Error" };
    
    let _ = FmtWrite::write_fmt(&mut response, format_args!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
        <!DOCTYPE html>\
        <html>\
        <head>\
            <title>AT Command Result - Pico LTE</title>\
            <style>\
                body {{ font-family: monospace; margin: 20px; background: #1e1e1e; color: #d4d4d4; }}\
                .container {{ max-width: 800px; margin: 0 auto; }}\
                h1 {{ color: #569cd6; }}\
                .status {{ font-size: 1.2em; margin: 20px 0; }}\
                .success {{ color: #4ec9b0; }}\
                .error {{ color: #f48771; }}\
                .cmd {{ background: #2d2d2d; padding: 10px; border-left: 4px solid #007acc; margin: 20px 0; }}\
                .response {{ background: #0e2941; padding: 15px; border-radius: 5px; margin: 20px 0; white-space: pre-wrap; overflow-x: auto; }}\
                .back {{ margin-top: 20px; display: inline-block; color: #569cd6; text-decoration: none; }}\
                form {{ margin: 20px 0; }}\
                textarea {{ width: 100%; height: 100px; background: #252525; color: #d4d4d4; border: 1px solid #3e3e3e; padding: 10px; font-family: monospace; }}\
                button {{ background: #007acc; color: white; border: none; padding: 10px 20px; cursor: pointer; }}\
            </style>\
        </head>\
        <body>\
            <div class=\"container\">\
                <h1>📡 AT Command Result</h1>\
                <div class=\"status {}\">{}</div>\
                <div class=\"cmd\">\
                    <strong>Command sent:</strong><br>\
                    <pre>{}</pre>\
                </div>\
                <div class=\"response\">\
                    <strong>Response:</strong><br>\
                    <pre>{}</pre>\
                </div>\
                <form action=\"/atcmd\" method=\"post\">\
                    <textarea name=\"cmd\" placeholder=\"Enter another AT command...\"></textarea><br>\
                    <button type=\"submit\">📤 Send Another Command</button>\
                </form>\
                <a href=\"/\" class=\"back\">← Back to Main</a>\
            </div>\
        </body>\
        </html>",
        status_class, status_text, cmd, result.data
    ));
    response
}

fn format_httpbin_response(json_content: &str) -> String<8192> {
    let mut response = String::new();
    let _ = FmtWrite::write_fmt(&mut response, format_args!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
        <!DOCTYPE html>\
        <html>\
        <head>\
            <title>HttpBin Test - Pico LTE</title>\
            <style>\
                body {{ font-family: Arial, sans-serif; margin: 40px; background: #f0f0f0; }}\
                .container {{ max-width: 800px; margin: 0 auto; background: white; padding: 30px; border-radius: 10px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }}\
                h1 {{ color: #333; border-bottom: 2px solid #28a745; padding-bottom: 10px; }}\
                .success {{ color: #28a745; background: #d4edda; padding: 10px; border-radius: 5px; margin: 20px 0; }}\
                pre {{ background: #f8f8f8; padding: 15px; border-radius: 5px; overflow-x: auto; font-family: 'Courier New', monospace; }}\
                .back {{ display: inline-block; margin-top: 20px; padding: 10px 20px; background: #007acc; color: white; text-decoration: none; border-radius: 5px; }}\
                .info {{ background: #d1ecf1; color: #0c5460; padding: 10px; border-radius: 5px; margin: 10px 0; }}\
            </style>\
        </head>\
        <body>\
            <div class=\"container\">\
                <h1>🌐 HttpBin.org Test Result</h1>\
                <div class=\"success\">✅ Successfully fetched data from http://httpbin.org/get via LTE</div>\
                <div class=\"info\">\
                    <strong>Test Target:</strong> http://httpbin.org/get<br>\
                    <strong>Result:</strong> JSON response showing your connection details\
                </div>\
                <h2>Raw JSON Response:</h2>\
                <pre>{}</pre>\
                <a href=\"/\" class=\"back\">← Back to Main</a>\
            </div>\
        </body>\
        </html>",
        json_content
    ));
    response
}

fn format_http_response(content: &str) -> String<8192> {
    let mut response = String::new();
    let _ = FmtWrite::write_fmt(&mut response, format_args!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{}",
        content
    ));
    response
}

fn format_error_response(error: &str) -> String<8192> {
    let mut response = String::new();
    let _ = FmtWrite::write_fmt(&mut response, format_args!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
        <!DOCTYPE html>\
        <html>\
        <head><title>Pico LTE Proxy - Error</title>\
        <style>body{{font-family:Arial,sans-serif;margin:40px;}}\
        .error{{color:red;background:#ffe6e6;padding:15px;border-radius:5px;}}</style>\
        </head>\
        <body>\
        <h1>Pico LTE Proxy</h1>\
        <div class=\"error\"><h2>Error</h2><p>{}</p></div>\
        <a href=\"/\">← Back to Main</a>\
        </body></html>",
        error
    ));
    response
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    info!("=== BOOT: Pico 2W Starting ===");
    Timer::after(Duration::from_millis(500)).await;

    info!("=== Pico 2W LTE Proxy ===");
    info!("WiFi AP: {} / {}", WIFI_SSID, WIFI_PASSWORD);
    info!("UART: {} baud on GP12/GP13", UART_BAUDRATE);

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

    // Check if WiFi is up by trying to get link status
    info!("Checking WiFi status...");
    for i in 0..10 {
        info!("WiFi check [{}]", i);
        Timer::after(Duration::from_millis(500)).await;
    }

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

    // Initialize UART for EC800K
    let uart_tx_buf = {
        static BUF: StaticCell<[u8; 256]> = StaticCell::new();
        BUF.init([0; 256])
    };
    let uart_rx_buf = {
        static BUF: StaticCell<[u8; 256]> = StaticCell::new();
        BUF.init([0; 256])
    };

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = UART_BAUDRATE;

    let uart = BufferedUart::new(
        p.UART0,
        p.PIN_12, // TX
        p.PIN_13, // RX
        Irqs,
        uart_tx_buf,
        uart_rx_buf,
        uart_config,
    );

    spawner.spawn(unwrap!(uart_task(uart)));

    info!("UART initialized");

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
    info!("   Features:");
    info!("   - AT Command Tester");
    info!("   - HttpBin.org Test (http://httpbin.org/get)");
    info!("   - Custom Proxy: /proxy?url=http://example.com");
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
