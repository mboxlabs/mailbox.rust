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

echo "✅ Package created and verified!"

read -p "Publish to crates.io? (y/n) " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]
then
    echo "📤 Publishing to crates.io..."
    # 如果配置了国内镜像源，需要明确指定 --registry crates-io
    if cargo publish --registry crates-io --dry-run 2>&1 | grep -q "crates-io is replaced"; then
        echo "检测到自定义镜像源配置，使用 --registry crates-io"
        cargo publish --registry crates-io
    else
        cargo publish
    fi
    echo ""
    echo "🎉 Published! Check it out at:"
    echo "https://crates.io/crates/mailbox"
    echo "https://docs.rs/mailbox"
else
    echo "❌ Cancelled. You can publish later with: cargo publish --registry crates-io"
fi
