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

#[embassy_executor::task]
async fn http_server_task(stack: &'static Stack<'static>) {
    // Wait for network to be ready
    loop {
        if stack.is_link_up() {
            info!("Network link is up!");
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Waiting for IP configuration...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("IP address: {:?}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    let mut rx_buffer = [0; 8192];
    let mut tx_buffer = [0; 8192];

    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        info!("Listening on TCP:80...");
        if let Err(e) = socket.accept(80).await {
            warn!("Accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());

        let _ = handle_client(&mut socket).await;
    }
}

async fn handle_client(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    let mut buf = [0; 1024];
    let n = socket.read(&mut buf).await?;

    if n == 0 {
        return Ok(());
    }

    let request = core::str::from_utf8(&buf[..n]).unwrap_or("");
    info!("HTTP Request received:\n{}", request);

    // Parse HTTP request
    if let Some(first_line) = request.lines().next() {
        let parts: heapless::Vec<&str, 3> = first_line.split_whitespace().collect();
        if parts.len() >= 2 {
            let method = parts[0];
            let path = parts[1];
            info!("Method: {}, Path: {}", method, path);

            // Get EC800K status
            let status = EC800K_STATUS.lock().await;
            let baud = EC800K_BAUD.lock().await;

            let mut response_buf = heapless::Vec::<u8, 512>::new();
            use core::fmt::Write;
            let _ = write!(
                &mut response_buf,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                <html><head><meta http-equiv='refresh' content='5'></head><body>\
                <h1>Pico 2W Gateway</h1>\
                <p><b>EC800K Status:</b> {}</p>\
                <p><b>Baud Rate:</b> {}</p>\
                <p><b>Request:</b> {} {}</p>\
                <p>Auto-refresh every 5 seconds</p>\
                <p><small>China Telecom APN: ctnet</small></p>\
                </body></html>\r\n",
                *status, *baud, method, path
            );

            socket.write_all(&response_buf).await?;
        }
    }

    socket.flush().await?;
    Timer::after(Duration::from_millis(100)).await;

    Ok(())
}

#[embassy_executor::task]
async fn uart_task(mut tx: BufferedUartTx, mut rx: BufferedUartRx) {
    info!("UART task started - Initializing EC800K for China Telecom");

    // Get current baud rate from config (115200 by default)
    {
        let mut baud = EC800K_BAUD.lock().await;
        *baud = 115200; // Will be updated after testing
    }

    Timer::after(Duration::from_secs(2)).await;

    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Testing connection...";
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
        Timer::after(Duration::from_secs(2)).await;

        // Read response
        let mut buf = [0u8; 512];
        let mut total_read = 0;
        let mut got_response = false;
        for _ in 0..10 {
            match rx.read(&mut buf[total_read..]).await {
                Ok(n) if n > 0 => {
                    total_read += n;
                    got_response = true;
                    if let Ok(s) = core::str::from_utf8(&buf[..total_read]) {
                        info!("Response: {}", s);
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
            *status = "ERROR: No response from EC800K";
        }
    }

    {
        let mut status = EC800K_STATUS.lock().await;
        *status = "Initialization complete";
    }

    info!("EC800K initialization complete");

    // Continue reading responses
    let mut buf = [0u8; 512];
    loop {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                    info!("EC800K: {}", s);
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

    static UART_TX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 1024]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 1024]);

    let mut uart_config = UartConfig::default();
    // Manual testing: Try 115200, 230400, 460800, 921600
    // Change this value, rebuild, and see if EC800K responds in logs
    uart_config.baudrate = 115200; // Change this to test: 230400, 460800, or 921600

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

    spawner.spawn(uart_task(uart_tx, uart_rx).unwrap());

    // Configure network stack for AP mode with static IP
    // Note: Clients must manually configure IP (192.168.4.2-254) as there's no DHCP server
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
        gateway: Some(embassy_net::Ipv4Address::new(192, 168, 4, 1)),
        dns_servers: heapless::Vec::new(),
    });

    let seed = 0x0123_4567_89ab_cdef; // Random seed for network stack

    static STACK: StaticCell<Stack<'static>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<8>::new()),
        seed,
    );
    let stack = STACK.init(stack);

    spawner.spawn(net_task(runner).unwrap());

    // Wait for stack to be ready
    Timer::after(Duration::from_millis(100)).await;

    // Start WiFi AP
    info!("Starting WiFi AP...");
    info!("SSID: {}, Password: {}", WIFI_SSID, WIFI_PASSWORD);

    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 5).await;
    info!("AP started successfully!");

    // Spawn HTTP server
    spawner.spawn(http_server_task(stack).unwrap());

    // Blink LED to indicate AP is running
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(900)).await;
    }
}
