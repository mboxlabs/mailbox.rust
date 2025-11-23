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

read -p "Publish to crates.io? (y/n) " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]
then
    echo "📤 Publishing to crates.io..."
    cargo publish
    echo ""
    echo "🎉 Published! Check it out at:"
    echo "https://crates.io/crates/mailbox"
    echo "https://docs.rs/mailbox"
else
    echo "❌ Cancelled. You can publish later with: cargo publish"
fi
