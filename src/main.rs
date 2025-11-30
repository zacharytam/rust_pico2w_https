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
use embassy_rp::uart::{BufferedInterruptHandler, BufferedUart, BufferedUartRx, Config as UartConfig};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// Program metadata
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"WiFi AP + EC800K Gateway"),
    embassy_rp::binary_info::rp_program_description!(c"Raspberry Pi Pico 2 W as WiFi AP routing through EC800K 4G module"),
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
async fn cyw43_task(runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<'static>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn http_server_task(
    stack: &'static Stack<'static>,
    _uart_rx: &'static embassy_sync::mutex::Mutex<embassy_sync::blocking_mutex::raw::NoopRawMutex, BufferedUartRx>,
) {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
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

async fn handle_client(
    socket: &mut TcpSocket<'_>,
) -> Result<(), embassy_net::tcp::Error> {
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

            // TODO: Forward to EC800K via AT commands
            // For now, send a simple response
            let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body><h1>Pico 2W Gateway</h1><p>Connected via EC800K</p></body></html>\r\n";
            socket.write_all(response.as_bytes()).await?;
        }
    }

    socket.flush().await?;
    Timer::after(Duration::from_millis(100)).await;
    
    Ok(())
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
    unwrap!(spawner.spawn(cyw43_task(runner)));

    control.init(clm).await;
    control.set_power_management(cyw43::PowerManagementMode::PowerSave).await;

    // Initialize UART for EC800K
    // GP0 = TX (to EC800K RX)
    // GP1 = RX (from EC800K TX)
    let uart_tx = p.PIN_0;
    let uart_rx = p.PIN_1;
    
    static UART_TX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 256]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 256]);
    
    let mut uart_config = UartConfig::default();
    uart_config.baudrate = 115200;
    
    let uart = BufferedUart::new(
        p.UART0,
        Irqs,
        uart_rx,
        uart_tx,
        uart_tx_buf,
        uart_rx_buf,
        uart_config,
    );
    
    let (_uart_tx, uart_rx_split) = uart.split();
    
    static UART_RX_MUTEX: StaticCell<embassy_sync::mutex::Mutex<embassy_sync::blocking_mutex::raw::NoopRawMutex, BufferedUartRx>> = StaticCell::new();
    let _uart_rx_mutex = UART_RX_MUTEX.init(embassy_sync::mutex::Mutex::new(uart_rx_split));

    // Configure network stack for AP mode
    let config = Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 4, 1), 24),
        gateway: None,
        dns_servers: heapless::Vec::new(),
    });

    let seed = 0x0123_4567_89ab_cdef; // Random seed for network stack

    static STACK: StaticCell<Stack<'static>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let stack = &*STACK.init(Stack::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<3>::new()),
        seed,
    ));

    unwrap!(spawner.spawn(net_task(stack)));

    // Start WiFi AP
    info!("Starting WiFi AP...");
    info!("SSID: {}, Password: {}", WIFI_SSID, WIFI_PASSWORD);
    
    control.start_ap_wpa2(WIFI_SSID, WIFI_PASSWORD, 5).await;
    info!("AP started successfully!");

    // Blink LED to indicate AP is running
    loop {
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(100)).await;
        control.gpio_set(0, false).await;
        Timer::after(Duration::from_millis(900)).await;
    }

    // Note: HTTP server task is commented out for now as we need to implement EC800K AT commands
    // unwrap!(spawner.spawn(http_server_task(stack, uart_rx_mutex)));
}
