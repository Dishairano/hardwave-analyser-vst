#!/usr/bin/env bash
#
# Hardwave Bridge VST Installer
# Builds and installs the VST3/CLAP plugin to the appropriate system location
#

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_NAME="Hardwave Bridge"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

print_header() {
    echo -e "${CYAN}"
    echo "╔═══════════════════════════════════════════════════════════╗"
    echo "║           Hardwave Bridge VST/CLAP Installer              ║"
    echo "║         Stream audio from your DAW to Hardwave Suite      ║"
    echo "╚═══════════════════════════════════════════════════════════╝"
    echo -e "${NC}"
}

print_step() {
    echo -e "${YELLOW}▶${NC} $1"
}

print_success() {
    echo -e "${GREEN}✓${NC} $1"
}

print_error() {
    echo -e "${RED}✗${NC} $1"
}

# Detect OS
detect_os() {
    case "$(uname -s)" in
        Linux*)     OS="linux";;
        Darwin*)    OS="macos";;
        MINGW*|MSYS*|CYGWIN*)    OS="windows";;
        *)          OS="unknown";;
    esac
    echo "$OS"
}

# Get VST3 install path
get_vst3_path() {
    case "$1" in
        linux)
            echo "$HOME/.vst3"
            ;;
        macos)
            echo "$HOME/Library/Audio/Plug-Ins/VST3"
            ;;
        windows)
            echo "/c/Program Files/Common Files/VST3"
            ;;
    esac
}

# Get CLAP install path
get_clap_path() {
    case "$1" in
        linux)
            echo "$HOME/.clap"
            ;;
        macos)
            echo "$HOME/Library/Audio/Plug-Ins/CLAP"
            ;;
        windows)
            echo "/c/Program Files/Common Files/CLAP"
            ;;
    esac
}

# Check for Rust toolchain
check_rust() {
    if ! command -v cargo &> /dev/null; then
        print_error "Rust/Cargo not found. Please install from https://rustup.rs"
        exit 1
    fi
    print_success "Rust toolchain found: $(rustc --version)"
}

# Build the plugin
build_plugin() {
    print_step "Building plugin (release mode)..."
    cd "$SCRIPT_DIR"

    # Check if xtask exists, if not use cargo build
    if [ -f "xtask/src/main.rs" ] || cargo xtask --help &> /dev/null 2>&1; then
        cargo xtask bundle hardwave-bridge --release
    else
        # Fallback: just build and manually create bundle structure
        cargo build --release

        # Create VST3 bundle manually
        local target_dir="$SCRIPT_DIR/target/bundled"
        mkdir -p "$target_dir"

        case "$(detect_os)" in
            linux)
                mkdir -p "$target_dir/Hardwave Bridge.vst3/Contents/x86_64-linux"
                cp "$SCRIPT_DIR/target/release/libhardwave_bridge.so" \
                   "$target_dir/Hardwave Bridge.vst3/Contents/x86_64-linux/Hardwave Bridge.so" 2>/dev/null || true
                ;;
            macos)
                mkdir -p "$target_dir/Hardwave Bridge.vst3/Contents/MacOS"
                cp "$SCRIPT_DIR/target/release/libhardwave_bridge.dylib" \
                   "$target_dir/Hardwave Bridge.vst3/Contents/MacOS/Hardwave Bridge" 2>/dev/null || true
                ;;
        esac
    fi

    print_success "Build complete"
}

# Install the plugin
install_plugin() {
    local os="$1"
    local vst3_path="$(get_vst3_path "$os")"
    local clap_path="$(get_clap_path "$os")"

    print_step "Installing plugins..."

    # Create directories
    mkdir -p "$vst3_path"
    mkdir -p "$clap_path"

    # Find and copy VST3
    local vst3_bundle=$(find "$SCRIPT_DIR/target" -name "*.vst3" -type d 2>/dev/null | head -1)
    if [ -n "$vst3_bundle" ] && [ -d "$vst3_bundle" ]; then
        print_step "Installing VST3 to $vst3_path"
        rm -rf "$vst3_path/Hardwave Bridge.vst3"
        cp -r "$vst3_bundle" "$vst3_path/"
        print_success "VST3 installed: $vst3_path/Hardwave Bridge.vst3"
    else
        print_error "VST3 bundle not found"
    fi

    # Find and copy CLAP
    local clap_bundle=$(find "$SCRIPT_DIR/target" -name "*.clap" -type f 2>/dev/null | head -1)
    if [ -n "$clap_bundle" ] && [ -f "$clap_bundle" ]; then
        print_step "Installing CLAP to $clap_path"
        cp "$clap_bundle" "$clap_path/"
        print_success "CLAP installed: $clap_path/$(basename "$clap_bundle")"
    fi
}

# Main installation flow
main() {
    print_header

    local os=$(detect_os)
    echo -e "Detected OS: ${CYAN}$os${NC}\n"

    if [ "$os" = "unknown" ]; then
        print_error "Unsupported operating system"
        exit 1
    fi

    check_rust
    echo ""

    build_plugin
    echo ""

    install_plugin "$os"
    echo ""

    echo -e "${GREEN}╔═══════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║              Installation Complete!                       ║${NC}"
    echo -e "${GREEN}╚═══════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo "Next steps:"
    echo "  1. Open your DAW (Ableton, FL Studio, Logic, Reaper, etc.)"
    echo "  2. Rescan for plugins if needed"
    echo "  3. Add 'Hardwave Bridge' to your master channel"
    echo "  4. Open Hardwave Suite and switch to VST mode"
    echo ""
    echo "The plugin will automatically connect to Hardwave Suite on port 9847."
}

# Handle command line args
case "${1:-}" in
    --help|-h)
        echo "Usage: $0 [options]"
        echo ""
        echo "Options:"
        echo "  --help, -h     Show this help message"
        echo "  --build-only   Only build, don't install"
        echo "  --uninstall    Remove installed plugins"
        exit 0
        ;;
    --build-only)
        print_header
        check_rust
        build_plugin
        print_success "Build complete. Bundles are in target/bundled/"
        exit 0
        ;;
    --uninstall)
        print_header
        os=$(detect_os)
        vst3_path="$(get_vst3_path "$os")"
        clap_path="$(get_clap_path "$os")"

        print_step "Uninstalling plugins..."
        rm -rf "$vst3_path/Hardwave Bridge.vst3"
        rm -f "$clap_path/Hardwave Bridge.clap"
        rm -f "$clap_path/hardwave_bridge.clap"
        print_success "Plugins uninstalled"
        exit 0
        ;;
    *)
        main
        ;;
esac
