async fn perform_http_get(tx: &mut BufferedUartTx, rx: &mut BufferedUartRx) {
    info!("Starting HTTP GET process for httpbin.org/get");
    
    // 更新状态
    {
        let mut result = AT_RESULT.lock().await;
        result.clear();
        let _ = result.push_str("🚀 Starting HTTP GET process...\n");
        let _ = result.push_str("This will take about 30-60 seconds.\n\n");
        let _ = result.push_str("Step 1/9: Checking SIM status...\n");
    }
    
    // 步骤1: AT+CPIN?
    if !send_at_command(tx, rx, "AT+CPIN?\r\n", "Checking SIM status", 1, 9).await {
        return;
    }
    
    // 步骤2: AT+CREG?
    if !send_at_command(tx, rx, "AT+CREG?\r\n", "Checking network registration", 2, 9).await {
        return;
    }
    
    // 步骤3: AT+CGATT=1
    if !send_at_command(tx, rx, "AT+CGATT=1\r\n", "Attaching to network", 3, 9).await {
        return;
    }
    
    // 步骤4: AT+QICSGP=1,1,"CMNET"
    if !send_at_command(tx, rx, "AT+QICSGP=1,1,\"CMNET\"\r\n", "Setting APN", 4, 9).await {
        return;
    }
    
    // ==== 步骤5: 激活PDP上下文（关键修正）====
    {
        let mut result = AT_RESULT.lock().await;
        let _ = result.push_str("\nStep 5/9: Activating PDP context...\n");
    }
    
    // 先尝试激活
    let activate_cmd = b"AT+QIACT=1\r\n";
    match tx.write_all(activate_cmd).await {
        Ok(_) => {
            tx.flush().await.ok();
            
            // 等待响应
            Timer::after(Duration::from_millis(500)).await;
            
            let mut response = heapless::String::<512>::new();
            let mut activation_done = false;
            let mut got_error = false;
            
            for _ in 0..10 {
                let mut buf = [0u8; 256];
                match rx.read(&mut buf).await {
                    Ok(n) if n > 0 => {
                        if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                            info!("QIACT=1 response: {}", s);
                            
                            {
                                let mut result = AT_RESULT.lock().await;
                                let _ = result.push_str("Response: ");
                                let _ = result.push_str(s);
                            }
                            
                            let _ = response.push_str(s);
                            
                            if s.contains("OK") {
                                // 激活成功！
                                activation_done = true;
                                break;
                            } else if s.contains("ERROR") {
                                // 可能已经激活了，我们稍后通过查询确认
                                got_error = true;
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                Timer::after(Duration::from_millis(500)).await;
            }
            
            if got_error {
                // 激活命令返回ERROR，可能是因为已经激活了
                // 让我们查询状态来确认
                {
                    let mut result = AT_RESULT.lock().await;
                    let _ = result.push_str("\n⚠️ Activation command returned ERROR.\n");
                    let _ = result.push_str("Checking if PDP is already active...\n");
                }
                
                // 查询当前状态
                if !send_at_command(tx, rx, "AT+QIACT?\r\n", "Checking PDP status", 5, 9).await {
                    // 查询失败，彻底失败
                    return;
                }
                
                // 如果查询成功（有IP地址），我们可以继续
                activation_done = true;
            }
            
            if !activation_done {
                let mut result = AT_RESULT.lock().await;
                let _ = result.push_str("\n❌ Failed to activate PDP context\n");
                return;
            }
        }
        Err(e) => {
            error!("Failed to send activation command: {:?}", e);
            let mut result = AT_RESULT.lock().await;
            let _ = result.push_str("\n❌ Failed to send activation command\n");
            return;
        }
    }
    
    // 步骤6: AT+QIOPEN=1,0,"TCP","httpbin.org",80,0,0
    {
        let mut result = AT_RESULT.lock().await;
        let _ = result.push_str("\nStep 6/9: Opening TCP connection to httpbin.org:80...\n");
    }
    
    let open_cmd = b"AT+QIOPEN=1,0,\"TCP\",\"httpbin.org\",80,0,0\r\n";
    match tx.write_all(open_cmd).await {
        Ok(_) => {
            tx.flush().await.ok();
            info!("TCP open command sent");
            
            // 等待响应：先收到OK，然后等+QIOPEN: 0,0
            let mut opened = false;
            let mut got_ok = false;
            
            for _ in 0..60 { // 给网络操作更长的时间
                let mut buf = [0u8; 256];
                match rx.read(&mut buf).await {
                    Ok(n) if n > 0 => {
                        if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                            info!("Open response: {}", s);
                            
                            {
                                let mut result = AT_RESULT.lock().await;
                                let _ = result.push_str("Response: ");
                                let _ = result.push_str(s);
                            }
                            
                            // 先检查是否有OK响应
                            if s.contains("OK") && !got_ok {
                                got_ok = true;
                                info!("Got OK for QIOPEN, waiting for +QIOPEN: 0,0");
                            }
                            
                            // 然后等待+QIOPEN: 0,0
                            if s.contains("+QIOPEN: 0,0") {
                                opened = true;
                                break;
                            } else if s.contains("ERROR") || s.contains("+QIOPEN: 0,4") {
                                let mut result = AT_RESULT.lock().await;
                                let _ = result.push_str("\n❌ Failed to open TCP connection\n");
                                return;
                            }
                        }
                    }
                    _ => {}
                }
                Timer::after(Duration::from_millis(500)).await;
            }
            
            if !opened {
                let mut result = AT_RESULT.lock().await;
                let _ = result.push_str("\n❌ Timeout waiting for +QIOPEN: 0,0\n");
                return;
            }
        }
        Err(e) => {
            error!("Failed to send TCP open command: {:?}", e);
            let mut result = AT_RESULT.lock().await;
            let _ = result.push_str("\n❌ Failed to send TCP open command\n");
            return;
        }
    }
    
    // 步骤7: AT+QISEND=0
    {
        let mut result = AT_RESULT.lock().await;
        let _ = result.push_str("\nStep 7/9: Preparing to send HTTP request...\n");
    }
    
    let send_cmd = b"AT+QISEND=0\r\n";
    match tx.write_all(send_cmd).await {
        Ok(_) => {
            tx.flush().await.ok();
            info!("Send command sent, waiting for '>' prompt");
            
            // 等待'>'提示符
            let mut got_prompt = false;
            for _ in 0..30 {
                let mut buf = [0u8; 256];
                match rx.read(&mut buf).await {
                    Ok(n) if n > 0 => {
                        if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                            info!("Send response: {}", s);
                            
                            {
                                let mut result = AT_RESULT.lock().await;
                                let _ = result.push_str("Response: ");
                                let _ = result.push_str(s);
                            }
                            
                            if s.contains(">") {
                                got_prompt = true;
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                Timer::after(Duration::from_millis(500)).await;
            }
            
            if !got_prompt {
                let mut result = AT_RESULT.lock().await;
                let _ = result.push_str("\n❌ Timeout waiting for '>' prompt\n");
                return;
            }
            
            // 步骤8: 发送HTTP请求
            {
                let mut result = AT_RESULT.lock().await;
                let _ = result.push_str("\nStep 8/9: Sending HTTP GET request...\n");
            }
            
            // 构建HTTP请求
            let http_request = "GET /get HTTP/1.1\r\nHost: httpbin.org\r\nUser-Agent: EC800K\r\nAccept: */*\r\nConnection: close\r\n\r\n";
            let request_bytes = http_request.as_bytes();
            
            match tx.write_all(request_bytes).await {
                Ok(_) => {
                    // 发送Ctrl+Z (0x1A) 结束请求
                    let ctrl_z = [0x1A];
                    if let Err(e) = tx.write_all(&ctrl_z).await {
                        error!("Failed to send Ctrl+Z: {:?}", e);
                        let mut result = AT_RESULT.lock().await;
                        let _ = result.push_str("\n❌ Failed to send Ctrl+Z\n");
                        return;
                    }
                    
                    tx.flush().await.ok();
                    info!("HTTP request sent");
                    
                    {
                        let mut result = AT_RESULT.lock().await;
                        let _ = result.push_str("HTTP request sent, waiting for response...\n");
                    }
                    
                    // 等待SEND OK
                    Timer::after(Duration::from_secs(3)).await;
                    
                    // 步骤9: 等待数据通知并读取
                    {
                        let mut result = AT_RESULT.lock().await;
                        let _ = result.push_str("\nStep 9/9: Waiting for data...\n");
                    }
                    
                    // 先等待+QIURC: "recv"通知
                    let mut data_notified = false;
                    for _ in 0..60 {
                        let mut buf = [0u8; 256];
                        match rx.read(&mut buf).await {
                            Ok(n) if n > 0 => {
                                if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                                    info!("Post-send notification: {}", s);
                                    
                                    {
                                        let mut result = AT_RESULT.lock().await;
                                        let _ = result.push_str("Notification: ");
                                        let _ = result.push_str(s);
                                    }
                                    
                                    if s.contains("+QIURC: \"recv\"") {
                                        data_notified = true;
                                        break;
                                    }
                                }
                            }
                            _ => {}
                        }
                        Timer::after(Duration::from_secs(1)).await;
                    }
                    
                    if !data_notified {
                        let mut result = AT_RESULT.lock().await;
                        let _ = result.push_str("\n⚠️ No data notification received\n");
                        // 即使没有通知，也尝试读取
                    }
                    
                    // 主动读取数据
                    {
                        let mut result = AT_RESULT.lock().await;
                        let _ = result.push_str("\nReading data with AT+QIRD=0...\n");
                    }
                    
                    if let Err(e) = tx.write_all(b"AT+QIRD=0\r\n").await {
                        error!("Failed to send AT+QIRD: {:?}", e);
                    } else {
                        tx.flush().await.ok();
                        Timer::after(Duration::from_secs(3)).await;
                        
                        // 读取HTTP响应数据
                        let mut full_response = heapless::String::<2048>::new();
                        let mut received_data = false;
                        
                        for _ in 0..10 {
                            let mut buf = [0u8; 512];
                            match rx.read(&mut buf).await {
                                Ok(n) if n > 0 => {
                                    received_data = true;
                                    if let Ok(s) = core::str::from_utf8(&buf[..n]) {
                                        info!("HTTP data: {}", s);
                                        let _ = full_response.push_str(s);
                                        
                                        // 检查是否收到了完整响应
                                        if s.contains("\r\n\r\n") && s.contains('{') {
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }
                            Timer::after(Duration::from_secs(2)).await;
                        }
                        
                        // 更新最终结果
                        {
                            let mut result = AT_RESULT.lock().await;
                            result.clear();
                            
                            if received_data {
                                let _ = result.push_str("✅ HTTP GET Process Complete!\n\n");
                                let _ = result.push_str("=== Full HTTP Response ===\n");
                                let _ = result.push_str(&full_response);
                            } else {
                                let _ = result.push_str("⚠️ HTTP GET Process finished\n");
                                let _ = result.push_str("No data received or timeout.\n");
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to send HTTP request: {:?}", e);
                    let mut result = AT_RESULT.lock().await;
                    let _ = result.push_str("\n❌ Failed to send HTTP request\n");
                    return;
                }
            }
        }
        Err(e) => {
            error!("Failed to send AT+QISEND command: {:?}", e);
            let mut result = AT_RESULT.lock().await;
            let _ = result.push_str("\n❌ Failed to send AT+QISEND command\n");
            return;
        }
    }
}
