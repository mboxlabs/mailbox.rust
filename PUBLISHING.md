# 发布到 crates.io 指南

## 1. 注册 crates.io 账号

### 1.1 使用 GitHub 登录
- 访问：https://crates.io/
- 点击右上角 "Log in with GitHub"
- 授权 crates.io 访问你的 GitHub 账号

### 1.2 生成 API Token
1. 登录后，访问：https://crates.io/settings/tokens
2. 点击 "New Token"
3. 设置 Token 名称（如 "mailbox-publish"）
4. 复制生成的 token（只显示一次！）

### 1.3 配置 Token

```bash
cargo login
```

粘贴你的 API token，它会保存到 `~/.cargo/credentials`

或者手动创建/编辑 `~/.cargo/credentials`:
```toml
[registry]
token = "your-api-token-here"
```

**重要：** 设置文件权限
```bash
chmod 600 ~/.cargo/credentials
```

## 2. 准备发布

### 2.1 检查 Cargo.toml

确保包含所有必要的元数据（已配置）：

```toml
[package]
name = "mailbox"
version = "0.1.0"
edition = "2021"
authors = ["MboxLabs Team"]
license = "MIT"
description = "A lightweight, pluggable mailbox/queue kernel inspired by the Erlang Actor Model"
repository = "https://github.com/mboxlabs/mailbox.rust"
homepage = "https://github.com/mboxlabs/mailbox.rust"
documentation = "https://github.com/mboxlabs/mailbox.rust"
keywords = ["mailbox", "actor-model", "message-queue", "async", "erlang"]
categories = ["asynchronous", "network-programming"]
```

### 2.2 添加 README 和 LICENSE

确保项目根目录有：
- `README.md` ✅ (已存在)
- `LICENSE` 或 `LICENSE-MIT` (需要添加)

### 2.3 创建 .gitignore

确保不提交不必要的文件：
```gitignore
/target/
**/*.rs.bk
Cargo.lock  # 对于库项目，不提交 Cargo.lock
```

## 3. 验证包

### 3.1 运行测试

```bash
cargo test
```

### 3.2 检查包内容

```bash
cargo package --list
```

这会列出将要发布的所有文件。

### 3.3 本地构建测试

```bash
cargo package
```

这会在 `target/package/` 创建一个 `.crate` 文件，并自动验证它可以正常构建。

如果只想打包而不验证，使用：
```bash
cargo package --no-verify
```

## 4. 发布到 crates.io

### 4.1 首次发布

```bash
cargo publish
```

### 4.2 发布特定版本

更新 `Cargo.toml` 中的版本号，然后：

```bash
cargo publish
```

### 4.3 干运行（不实际发布）

```bash
cargo publish --dry-run
```

### 4.4 使用国内镜像源的注意事项

如果你的 `.cargo/config.toml` 配置了镜像源，发布时需要明确指定使用 crates.io：

```bash
# 干运行
cargo publish --registry crates-io --dry-run

# 正式发布
cargo publish --registry crates-io
```

或者临时禁用镜像源配置：
```bash
# 重命名配置文件
mv ~/.cargo/config.toml ~/.cargo/config.toml.bak

# 发布
cargo publish

# 恢复配置
mv ~/.cargo/config.toml.bak ~/.cargo/config.toml
```

## 5. 验证发布

访问：https://crates.io/crates/mailbox

安装测试：
```bash
cargo add mailbox
```

或在 `Cargo.toml` 中：
```toml
[dependencies]
mailbox = "0.1.0"
```

## 6. 版本管理

### 6.1 更新版本号

编辑 `Cargo.toml`:
```toml
version = "0.2.0"  # 更新版本号
```

### 6.2 语义化版本规则

- **MAJOR.MINOR.PATCH** (例如: 1.2.3)
- **MAJOR**: 不兼容的 API 变更
- **MINOR**: 向后兼容的功能新增
- **PATCH**: 向后兼容的问题修正

### 6.3 预发布版本

```toml
version = "0.2.0-alpha.1"
version = "0.2.0-beta.2"
version = "0.2.0-rc.1"
```

## 7. 自动化发布

### 7.1 使用 GitHub Actions

创建 `.github/workflows/publish.yml`:

```yaml
name: Publish to crates.io

on:
  release:
    types: [published]

jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
    - uses: actions/checkout@v3

    - name: Install Rust
      uses: actions-rs/toolchain@v1
      with:
        toolchain: stable
        override: true

    - name: Publish to crates.io
      env:
        CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}
      run: cargo publish
```

### 7.2 配置 GitHub Secrets

1. 在 GitHub 仓库设置中
2. Settings → Secrets and variables → Actions
3. 添加 secret: `CARGO_REGISTRY_TOKEN`（值为你的 crates.io API token）

## 8. 使用 cargo-release（推荐）

### 8.1 安装 cargo-release

```bash
cargo install cargo-release
```

### 8.2 配置 release.toml

在项目根目录创建 `release.toml`:

```toml
[workspace]
# 自动更新版本号
pre-release-commit-message = "chore: Release {{crate_name}} version {{version}}"
# 创建 git tag
tag-message = "chore: Release {{crate_name}} version {{version}}"
tag-name = "v{{version}}"
# 推送到 git
push = true
```

### 8.3 发布流程

```bash
# 检查将要做什么（干运行）
cargo release --dry-run

# 发布 patch 版本 (0.1.0 -> 0.1.1)
cargo release patch

# 发布 minor 版本 (0.1.0 -> 0.2.0)
cargo release minor

# 发布 major 版本 (0.1.0 -> 1.0.0)
cargo release major

# 发布预发布版本
cargo release alpha
cargo release beta
cargo release rc
```

## 9. 文档

### 9.1 生成文档

```bash
cargo doc --no-deps --open
```

### 9.2 发布到 docs.rs

文档会自动发布到 https://docs.rs/mailbox

确保代码中有良好的文档注释：

```rust
/// 这是一个公共函数的文档
///
/// # Examples
///
/// ```
/// use mailbox::Mailbox;
/// let mailbox = Mailbox::new();
/// ```
pub fn example() {}
```

## 10. 常见问题

### Q: 包名已存在
A: crates.io 包名是全局唯一的，需要选择其他名称

### Q: 发布失败 - 权限错误
A: 检查 API token 是否正确配置

### Q: 版本号已存在
A: crates.io 不允许覆盖已发布的版本，需要增加版本号

### Q: 如何撤回已发布的版本？
A: 使用 `cargo yank --vers 0.1.0`，但不会删除，只是标记为不推荐

### Q: 如何取消撤回？
A: 使用 `cargo yank --vers 0.1.0 --undo`

## 11. 最佳实践

1. **发布前运行完整测试**
   ```bash
   cargo test --all-features
   cargo clippy -- -D warnings
   cargo fmt -- --check
   ```

2. **使用 CI/CD 自动化测试**

3. **版本号遵循语义化版本**

4. **保持 README 和文档更新**

5. **添加 CHANGELOG.md 记录变更**

6. **使用 GitHub Releases 管理版本**

7. **为公共 API 编写文档注释**

8. **使用 `#[doc(hidden)]` 隐藏内部 API**

## 12. 快速发布脚本

创建 `scripts/publish.sh`:

```bash
#!/bin/bash
set -e

echo "🧪 Running tests..."
cargo test --all-features

echo "🔍 Running clippy..."
cargo clippy -- -D warnings

echo "📝 Checking formatting..."
cargo fmt -- --check

echo "📦 Packaging..."
cargo package

echo "✅ Verifying package..."
cargo package --verify

echo "📤 Publishing to crates.io..."
cargo publish

echo "✨ Done!"
```

使用：
```bash
chmod +x scripts/publish.sh
./scripts/publish.sh
```

## 13. 发布检查清单

- [ ] 更新版本号 (Cargo.toml)
- [ ] 更新 CHANGELOG.md
- [ ] 运行所有测试 (`cargo test --all-features`)
- [ ] 运行 clippy (`cargo clippy`)
- [ ] 检查格式 (`cargo fmt -- --check`)
- [ ] 更新文档注释
- [ ] 验证打包 (`cargo package`)
- [ ] 发布到 crates.io (`cargo publish`)
- [ ] 创建 GitHub Release
- [ ] 验证安装 (`cargo add mailbox`)
- [ ] 检查 docs.rs 文档

## 14. WASM 支持注意事项

由于此项目支持 WASM，确保：

1. **测试 WASM 构建**
   ```bash
   cargo build --target wasm32-unknown-unknown
   ```

2. **使用 wasm-pack 测试**
   ```bash
   wasm-pack build --target nodejs
   wasm-pack test --node
   ```

3. **在文档中说明 WASM 支持**

## 15. 相关资源

- **crates.io**: https://crates.io/
- **Cargo Book**: https://doc.rust-lang.org/cargo/
- **API Guidelines**: https://rust-lang.github.io/api-guidelines/
- **docs.rs**: https://docs.rs/

---

**首次发布建议流程：**

1. 确保 Cargo.toml 元数据完整
2. 运行 `cargo package` (会自动验证)
3. 运行 `cargo publish --dry-run`
4. 运行 `cargo publish`
5. 验证 https://crates.io/crates/mailbox
6. 检查 https://docs.rs/mailbox
7. 庆祝！🎉
