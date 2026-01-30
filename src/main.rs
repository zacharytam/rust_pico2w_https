#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::UART0;
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
    embassy_rp::binary_info::rp_program_name!(c"EC800K AT Test"),
    embassy_rp::binary_info::rp_program_description!(
        c"Minimal test for EC800K LTE module"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("EC800K AT Test Starting...");
    let p = embassy_rp::init(Default::default());

    static UART_TX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; 1024]> = StaticCell::new();
    let uart_tx_buf = UART_TX_BUF.init([0u8; 1024]);
    let uart_rx_buf = UART_RX_BUF.init([0u8; 1024]);

    // 配置UART为921600波特率
    let mut uart_config = UartConfig::default();
    uart_config.baudrate = 921600;

    info!("Initializing UART at 921600 baud...");
    info!("GP12 (TX) -> EC800K RX");
    info!("GP13 (RX) <- EC800K TX");

    let uart = BufferedUart::new(
        p.UART0,
        p.PIN_12,  // TX -> EC800K RX
        p.PIN_13,  // RX <- EC800K TX
        Irqs,
        uart_tx_buf,
        uart_rx_buf,
        uart_config,
    );

    let (mut tx, mut rx) = uart.split();
    
    // 等待系统稳定
    Timer::after(Duration::from_millis(1000)).await;
    info!("System ready. Starting AT command test...");
    
    // 发送简单的AT命令测试
    info!("Sending: AT");
    let cmd = b"AT\r\n";
    
    match tx.write_all(cmd).await {
        Ok(_) => {
            info!("AT command sent successfully");
            tx.flush().await.ok();
        }
        Err(e) => {
            error!("Failed to send AT command: {:?}", e);
            return;
        }
    }
    
    // 等待响应
    Timer::after(Duration::from_millis(500)).await;
    
    // 尝试读取响应
    let mut buf = [0u8; 256];
    let mut total_bytes = 0;
    
    // 尝试多次读取，因为响应可能分多次到达
    for attempt in 0..10 {
        match rx.read(&mut buf).await {
            Ok(n) if n > 0 => {
                total_bytes += n;
                if let Ok(response) = core::str::from_utf8(&buf[..n]) {
                    info!("Response (attempt {}): {}", attempt + 1, response);
                    
                    if response.contains("OK") || response.contains("ERROR") {
                        break;
                    }
                }
            }
            Ok(_) => {
                // 没有数据
            }
            Err(e) => {
                warn!("Read error: {:?}", e);
            }
        }
        
        Timer::after(Duration::from_millis(100)).await;
    }
    
    if total_bytes == 0 {
        error!("No response from EC800K!");
        info!("Troubleshooting:");
        info!("1. Check wiring: Pico GP12(TX) -> EC800K RX");
        info!("2. Check wiring: Pico GP13(RX) <- EC800K TX");
        info!("3. Check GND connection");
        info!("4. Check power (3.3V or 3.8V depending on module)");
        info!("5. Verify baudrate (921600)");
        info!("6. Try sending AT manually via serial monitor");
    } else {
        info!("Test completed. Received {} bytes total.", total_bytes);
    }
    
    // 现在进入主循环，可以手动输入AT命令
    info!("Entering interactive mode. Send AT commands via defmt_rtt console.");
    
    let mut command_buffer = [0u8; 128];
    let mut cmd_index = 0;
    
    loop {
        // 检查是否有数据可读
        let mut temp_buf = [0u8; 256];
        match rx.read(&mut temp_buf).await {
            Ok(n) if n > 0 => {
                if let Ok(response) = core::str::from_utf8(&temp_buf[..n]) {
                    info!("<< {}", response);
                }
            }
            _ => {}
        }
        
        // 简单的命令输入处理（通过defmt_rtt输入）
        // 注意：这需要你通过probe-rs或类似工具输入命令
        
        Timer::after(Duration::from_millis(100)).await;
    }
}
