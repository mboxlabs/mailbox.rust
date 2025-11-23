# WASM 测试指南

> **⚠️ 重要说明**: 当前的 Rust 实现主要面向库使用，**尚未添加完整的 WASM 绑定**。本文档中的 JavaScript 示例展示了理想的 API 使用方式，但要实际运行需要：
> 1. 在 Rust 代码中添加 `#[wasm_bindgen]` 注解导出公共 API
> 2. 或者创建一个专门的 WASM wrapper 层
>
> 目前 `wasm-pack build` 可以成功编译，但生成的包主要用于验证 WASM 兼容性。

## 构建 WASM

### 1. 构建为 Node.js 模块

```bash
wasm-pack build --target nodejs
```

生成的文件在 `pkg/` 目录下，可以在 Node.js 环境中使用。

### 2. 构建为 Web 模块

```bash
wasm-pack build --target web
```

生成的文件可以直接在浏览器中使用。

### 3. 构建为 Bundler 模块

```bash
wasm-pack build --target bundler
```

生成的文件可以与 Webpack、Rollup 等打包工具一起使用。

## 在 Node.js 中测试

创建一个测试文件 `test-wasm.js`:

```javascript
const { Mailbox, MemoryProvider } = require('./pkg/mailbox.js');

async function test() {
    console.log('Testing WASM Mailbox...');

    // 注意：WASM 版本可能需要不同的 API
    // 以下是概念性示例，实际 API 取决于 wasm-bindgen 的导出

    try {
        // 创建 mailbox 实例
        const mailbox = new Mailbox();

        // 注册内存提供者
        const provider = new MemoryProvider();
        mailbox.register_provider(provider);

        console.log('Mailbox initialized successfully!');

        // 订阅消息
        const address = 'mem:test@example.com/inbox';
        await mailbox.subscribe(address, (message) => {
            console.log('Received message:', message);
        });

        // 发送消息
        await mailbox.post({
            from: 'mem:sender@example.com',
            to: address,
            body: { text: 'Hello from WASM!' }
        });

        console.log('Message sent successfully!');

    } catch (error) {
        console.error('Error:', error);
    }
}

test().catch(console.error);
```

**注意**: 由于当前的 Rust 实现主要面向库使用，WASM 绑定可能需要额外的 `#[wasm_bindgen]` 注解。如果上述代码不工作，你可能需要：

1. 在 Rust 代码中添加 WASM 绑定
2. 或者在 Node.js 中直接使用编译好的 native 版本

运行测试：

```bash
node test-wasm.js
```

## 在浏览器中测试

创建一个 HTML 文件 `test.html`:

```html
<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Mailbox WASM Test</title>
    <style>
        body {
            font-family: Arial, sans-serif;
            max-width: 800px;
            margin: 50px auto;
            padding: 20px;
        }
        #output {
            background: #f5f5f5;
            padding: 15px;
            border-radius: 5px;
            margin-top: 20px;
            white-space: pre-wrap;
        }
        button {
            padding: 10px 20px;
            margin: 5px;
            cursor: pointer;
        }
    </style>
</head>
<body>
    <h1>Mailbox WASM Test</h1>
    <div>
        <button id="initBtn">Initialize Mailbox</button>
        <button id="sendBtn" disabled>Send Message</button>
        <button id="fetchBtn" disabled>Fetch Message</button>
    </div>
    <div id="output">等待初始化...</div>

    <script type="module">
        import init, { Mailbox, MemoryProvider } from './pkg/mailbox.js';

        let mailbox = null;
        const output = document.getElementById('output');

        function log(message) {
            output.textContent += '\n' + message;
        }

        async function run() {
            try {
                // 初始化 WASM 模块
                await init();
                log('✅ WASM module loaded successfully!');

                document.getElementById('initBtn').addEventListener('click', async () => {
                    try {
                        // 创建 mailbox 实例
                        mailbox = new Mailbox();

                        // 注册内存提供者
                        const provider = new MemoryProvider();
                        mailbox.register_provider(provider);

                        log('✅ Mailbox initialized!');

                        // 订阅消息
                        const address = 'mem:test@example.com/inbox';
                        await mailbox.subscribe(address, (message) => {
                            log(`📨 Received: ${JSON.stringify(message)}`);
                        });

                        log(`✅ Subscribed to ${address}`);

                        // 启用其他按钮
                        document.getElementById('sendBtn').disabled = false;
                        document.getElementById('fetchBtn').disabled = false;

                    } catch (error) {
                        log(`❌ Error: ${error.message}`);
                    }
                });

                document.getElementById('sendBtn').addEventListener('click', async () => {
                    try {
                        await mailbox.post({
                            from: 'mem:sender@example.com',
                            to: 'mem:test@example.com/inbox',
                            body: {
                                text: 'Hello from browser!',
                                timestamp: new Date().toISOString()
                            }
                        });
                        log('✅ Message sent!');
                    } catch (error) {
                        log(`❌ Send error: ${error.message}`);
                    }
                });

                document.getElementById('fetchBtn').addEventListener('click', async () => {
                    try {
                        const msg = await mailbox.fetch('mem:test@example.com/inbox');
                        if (msg) {
                            log(`📬 Fetched: ${JSON.stringify(msg)}`);
                        } else {
                            log('📭 No messages in queue');
                        }
                    } catch (error) {
                        log(`❌ Fetch error: ${error.message}`);
                    }
                });

            } catch (error) {
                log(`❌ Fatal error: ${error.message}`);
            }
        }

        run();
    </script>
</body>
</html>
```

**重要提示**:
1. 当前的 Rust 代码主要是库实现，**没有导出 WASM 绑定**
2. 要在浏览器中使用，需要在 Rust 代码中添加 `#[wasm_bindgen]` 注解
3. 或者创建一个专门的 WASM wrapper 层

### 添加 WASM 绑定示例

如果你想让上述代码工作，需要在 `src/lib.rs` 中添加：

```rust
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub struct WasmMailbox {
    inner: Mailbox,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WasmMailbox {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: Mailbox::new(),
        }
    }

    // 添加其他方法的绑定...
}
```

使用本地服务器运行：

```bash
python3 -m http.server 8000
# 或
npx serve .
```

然后在浏览器中访问 `http://localhost:8000/test.html`

## 清理构建

```bash
rm -rf pkg target/wasm32-unknown-unknown Cargo.lock
```

## 注意事项

1. **依赖兼容性**: 确保所有依赖都支持 WASM 目标
2. **Feature flags**: 某些依赖在 WASM 下需要特定的 feature（如 `getrandom` 的 `js` feature）
3. **异步运行时**: WASM 使用 `wasm-bindgen-futures` 而不是 `tokio` 的完整运行时
4. **文件大小**: 使用 `--release` 模式和 `wasm-opt` 优化可以显著减小文件大小

## 故障排查

### 问题：getrandom 错误

确保在 `Cargo.toml` 中为 WASM 目标启用了 `js` feature：

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
getrandom = { version = "0.3", features = ["wasm_js"] }
# getrandom = { version = "0.2", features = ["js"] }
```

### 问题：tokio 相关错误

WASM 目标只支持 tokio 的部分功能：

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
tokio = { version = "1", features = ["sync", "macros"] }
```

### 问题：lazy_static 不工作

使用 `once_cell` 替代：

```toml
[dependencies]
once_cell = "1.21"
```

```rust
use once_cell::sync::Lazy;

static MY_STATIC: Lazy<MyType> = Lazy::new(|| {
    MyType::new()
});
```
