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
    embassy_rp::binary_info::rp_program_name!(c"EC800K AT Tester"),
    embassy_rp::binary_info::rp_program_description!(
        c"Web-based AT command tester for EC800K"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

const WIFI_SSID: &str = "Pico2W_AT";
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

// Global state
static AT_RESULT: embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<512>,
> = embassy_sync::mutex::Mutex::new(heapless::String::new());

static AT_COMMAND_SIGNAL: embassy_sync::signal::Signal<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    heapless::String<64>,
> = embassy_sync::signal::Signal::new();

#[embassy_executor::task]
async fn http_server_task(stack: &'static Stack<'static>) {
    info!("HTTP server task started");
    
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        if let Err(e) = socket.accept(80).await {
            warn!("Accept error: {:?}", e);
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }

        // 读取请求
        let mut buf = [0; 512];
        let n = match socket.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => continue,
        };

        if n == 0 {
            continue;
        }

        let request = core::str::from_utf8(&buf[..n]).unwrap_or("");
        
        // 解析请求路径
        let mut cmd_to_send = heapless::String::<64>::new();
        let mut immediate_refresh = false;
        
        if request.starts_with("GET /at?cmd=") {
            immediate_refresh = true;
            if let Some(start) = request.find("cmd=") {
                let query = &request[start+4..];
                if let Some(end) = query.find(' ') {
                    let cmd = &query[..end];
                    let decoded = decode_url(cmd);
                    cmd_to_send = decoded;
                } else if let Some(end) = query.find('\n') {
                    let cmd = &query[..end];
                    let decoded = decode_url(cmd);
                    cmd_to_send = decoded;
                } else if !query.is_empty() {
                    let decoded = decode_url(query);
                    cmd_to_send = decoded;
                }
            }
        }

        // 获取当前结果
        let result = AT_RESULT.lock().await;
        
        // 构建响应
        let html = format_response(result.as_str(), immediate_refresh);
        
        // 发送响应
        let _ = socket.write_all(html.as_bytes()).await;
        let _ = socket.flush().await;
        
        // 如果有命令要发送，在响应后发送信号
        if !cmd_to_send.is_empty() {
            info!("Sending AT command signal: {}", cmd_to_send);
            AT_COMMAND_SIGNAL.signal(cmd_to_send);
        }
    }
}

fn format_response(result: &str, immediate_refresh: bool) -> heapless::String<4096> {
    let mut html = heapless::String::new();
    
    let _ = html.push_str("HTTP/1.1 200 OK\r\n");
    let _ = html.push_str("Content-Type: text/html; charset=utf-8\r\n");
    let _ = html.push_str("Connection: close\r\n\r\n");
    
    let _ = html.push_str("<!DOCTYPE html><html><head>");
    let _ = html.push_str("<title>EC800K AT Tester</title>");
    let _ = html.push_str("<meta name='viewport' content='width=device-width, initial-scale=1'>");
    
    if !immediate_refresh {
        // 正常页面：5秒刷新一次
        let _ = html.push_str("<meta http-equiv='refresh' content='5'>");
    }
    
    let _ = html.push_str("<style>");
    let _ = html.push_str("body { font-family: Arial, sans-serif; margin: 20px; background: #f0f2f5; }");
    let _ = html.push_str(".container { max-width: 900px; margin: auto; background: white; padding: 25px; border-radius: 10px; box-shadow: 0 2px 15px rgba(0,0,0,0.1); }");
    let _ = html.push_str("h1 { color: #2c3e50; border-bottom: 3px solid #3498db; padding-bottom: 15px; }");
    let _ = html.push_str("input[type='text'] { width: 350px; padding: 12px; font-size: 16px; border: 2px solid #ddd; border-radius: 6px; margin-right: 10px; }");
    let _ = html.push_str("button { padding: 12px 25px; font-size: 16px; background: linear-gradient(135deg, #3498db, #2980b9); color: white; border: none; border-radius: 6px; cursor: pointer; font-weight: bold; }");
    let _ = html.push_str("button:hover { background: linear-gradient(135deg, #2980b9, #1c5a7d); transform: translateY(-2px); box-shadow: 0 4px 8px rgba(0,0,0,0.1); }");
    let _ = html.push_str("pre { background: #2c3e50; color: #ecf0f1; padding: 20px; border-radius: 8px; overflow: auto; white-space: pre-wrap; font-family: 'Courier New', monospace; font-size: 14px; line-height: 1.4; border-left: 5px solid #3498db; }");
    let _ = html.push_str(".info-box { background: #e8f4fd; border-left: 5px solid #3498db; padding: 15px; margin: 20px 0; border-radius: 5px; }");
    let _ = html.push_str(".success { color: #2ecc71; font-weight: bold; }");
    let _ = html.push_str(".error { color: #e74c3c; font-weight: bold; }");
    let _ = html.push_str(".cmd-example { background: #f8f9fa; padding: 10px; border-radius: 5px; margin: 10px 0; font-family: monospace; }");
    let _ = html.push_str("</style>");
    
    // JavaScript用于立即刷新页面
    if immediate_refresh {
        let _ = html.push_str("<script>");
        let _ = html.push_str("window.onload = function() {");
        let _ = html.push_str("  // 立即刷新页面以显示结果");
        let _ = html.push_str("  setTimeout(function() { location.reload(); }, 800);");
        let _ = html.push_str("};");
        let _ = html.push_str("</script>");
    }
    
    let _ = html.push_str("</head><body>");
    
    let _ = html.push_str("<div class='container'>");
    let _ = html.push_str("<h1>📡 EC800K AT Command Tester</h1>");
    
    let _ = html.push_str("<div class='info-box'>");
    let _ = html.push_str("<strong>ℹ️ Connection Info:</strong><br>");
    let _ = html.push_str("WiFi: <strong>");
    let _ = html.push_str(WIFI_SSID);
    let _ = html.push_str("</strong> | Password: <strong>");
    let _ = html.push_str(WIFI_PASSWORD);
    let _ = html.push_str("</strong> | IP: <strong>192.168.4.1</strong><br>");
    let _ = html.push_str("UART: Pico GP12(TX) → EC800K RX | Pico GP13(RX) ← EC800K TX<br>");
    let _ = html.push_str("Baudrate: <strong>921600</strong> (tested working in CircuitPython)");
    let _ = html.push_str("</div>");
    
    let _ = html.push_str("<h3>📝 Send AT Command</h3>");
    let _ = html.push_str("<form action='/at' method='get'>");
    let _ = html.push_str("<input type='text' name='cmd' value='AT' placeholder='Enter AT command'>");
    let _ = html.push_str("<button type='submit'>🚀 Send Command</button>");
    let _ = html.push_str("</form>");
    
    let _ = html.push_str("<h3>💡 Example Commands</h3>");
    let _ = html.push_str("<div class='cmd-example'>AT+CGMI</div>");
    let _ = html.push_str("<div class='cmd-example'>AT+CSQ</div>");
    let _ = html.push_str("<div class='cmd-example'>AT+CREG?</div>");
    
    let _ = html.push_str("<h3>🌐 HTTP via LTE (Advanced)</h3>");
    let _ = html.push_str("<div class='cmd-example'>AT+HTTPGET=\"http://httpbin.org/get\"</div>");
    let _ = html.push_str("<small>Note: Your EC800K might not support HTTP AT commands directly. Use TCP method below.</small>");
    
    let _ = html.push_str("<h3>📊 Test Results:</h3>");
    let _ = html.push_str("<pre>");
    let _ = html.push_str(result);
    let _ = html.push_str("</pre>");
    
    if immediate_refresh {
        let _ = html.push_str("<p class='success'>🔄 Page will refresh in 1 second to show results...</p>");
    } else {
        let _ = html.push_str("<p><em>Page auto-refreshes every 5 seconds</em></p>");
    }
    
    let _ = html.push_str("</div></body></html>");
    
    html
}

fn decode_url(input: &str) -> heapless::String<64> {
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
    
    // 确保命令以回车换行结束
    if !output.ends_with("\r\n") {
        let _ = output.push_str("\r\n");
    }
    
    output
}

#[embassy_executor::task]
async fn uart_task(mut tx: BufferedUartTx, mut rx: BufferedUartRx) {
    info!("UART task started (921600 baud)");
    
    // 初始测试
    {
        info!("Sending initial AT command...");
        let test_cmd = b"AT\r\n";
        if let Err(e) = tx.write_all(test_cmd).await {
            error!("Failed to send initial AT command: {:?}", e);
        } else {
            info!("Initial AT command sent");
            tx.flush().await.ok();
            
            Timer::after(Duration::from_millis(200)).await;
            
            let mut buf = [0u8; 256];
            let mut response_received = false;
            
            for _ in 0..5 {
                match rx.read(&mut buf).await {
                    Ok(n) if n > 0 => {
                        if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                            info!("Initial response: {}", s);
                            response_received = true;
                            
                            let mut result = AT_RESULT.lock().await;
                            result.clear();
                            let _ = result.push_str("✅ Initial test successful!\n");
                            let _ = result.push_str("EC800K is responding to AT commands.\n\n");
                            let _ = result.push_str("Response:\n");
                            let _ = result.push_str(s);
                            break;
                        }
                    }
                    _ => {}
                }
                Timer::after(Duration::from_millis(100)).await;
            }
            
            if !response_received {
                let mut result = AT_RESULT.lock().await;
                result.clear();
                let _ = result.push_str("⚠️ Initial test: No response from EC800K\n");
            }
        }
    }
    
    // 主循环
    loop {
        // 等待AT命令信号
        let cmd = AT_COMMAND_SIGNAL.wait().await;
        info!("Processing AT command: {:?}", cmd);
        
        // 更新状态为发送中
        {
            let mut result = AT_RESULT.lock().await;
            result.clear();
            let _ = result.push_str("🔄 Sending command:\n");
            let _ = result.push_str(cmd.trim());
            let _ = result.push_str("\n\n⏳ Waiting for response...\n");
        }
        
        // 发送AT命令
        let cmd_bytes = cmd.as_bytes();
        match tx.write_all(cmd_bytes).await {
            Ok(_) => {
                info!("AT command sent successfully");
                tx.flush().await.ok();
                
                // 等待响应
                Timer::after(Duration::from_millis(200)).await;
                
                // 读取响应
                let mut response = heapless::String::<1024>::new();
                let mut received = false;
                let mut total_bytes = 0;
                
                for attempt in 0..15 {
                    let mut buf = [0u8; 256];
                    match rx.read(&mut buf).await {
                        Ok(n) if n > 0 => {
                            received = true;
                            total_bytes += n;
                            if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                                info!("Response chunk {}: {}", attempt + 1, s);
                                let _ = response.push_str(s);
                                
                                if s.contains("OK") || s.contains("ERROR") || s.contains("+HTTPGET") {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                    
                    Timer::after(Duration::from_millis(100)).await;
                }
                
                // 更新结果
                {
                    let mut result = AT_RESULT.lock().await;
                    result.clear();
                    
                    if received {
                        let _ = result.push_str("📤 Command sent:\n");
                        let _ = result.push_str(cmd.trim());
                        let _ = result.push_str("\n\n📥 Response (");
                        let mut bytes_str = heapless::String::<10>::new();
                        let _ = write_u32(&mut bytes_str, total_bytes as u32);
                        let _ = result.push_str(bytes_str.as_str());
                        let _ = result.push_str(" bytes):\n");
                        let _ = result.push_str(&response);
                        
                        if response.contains("OK") {
                            let _ = result.push_str("\n\n✅ Command successful!");
                        } else if response.contains("ERROR") {
                            let _ = result.push_str("\n\n❌ Command failed");
                        } else if response.trim().is_empty() {
                            let _ = result.push_str("\n\n⚠️ Empty response");
                        }
                    } else {
                        let _ = result.push_str("📤 Command sent:\n");
                        let _ = result.push_str(cmd.trim());
                        let _ = result.push_str("\n\n❌ No response received\n");
                    }
                }
            }
            Err(e) => {
                error!("Failed to send AT command: {:?}", e);
                let mut result = AT_RESULT.lock().await;
                result.clear();
                let _ = result.push_str("❌ Failed to send AT command\n");
            }
        }
        
        info!("AT command processing complete");
    }
}

fn write_u32(s: &mut heapless::String<10>, n: u32) -> Result<(), ()> {
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

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("=========================================");
    info!("EC800K AT Tester Starting...");
    info!("=========================================");
    
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
    control.set_power_management(cyw43::PowerManagementMode::Performance).await;

    static UART_TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 2048]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 2048]);

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = 921600;

    info!("Configuring UART at 921600 baud...");
    
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
    static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<8>::new()),
        seed,
    );
    let stack = STACK.init(stack);

    spawner.spawn(net_task(runner).expect("Failed to spawn net task"));

    info!("Starting WiFi AP: {}", WIFI_SSID);
    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 5).await;
    info!("AP started!");

    Timer::after(Duration::from_secs(2)).await;

    spawner.spawn(http_server_task(stack).expect("Failed to spawn HTTP server"));
    info!("HTTP server started on port 80");

    info!("=========================================");
    info!("✅ EC800K AT Tester Ready!");
    info!("Connect to WiFi: {}", WIFI_SSID);
    info!("Password: {}", WIFI_PASSWORD);
    info!("Visit: http://192.168.4.1");
    info!("=========================================");

    let mut counter = 0;
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(50)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(950)).await;
        
        counter += 1;
        if counter % 20 == 0 {
            info!("System alive...");
        }
    }
}
