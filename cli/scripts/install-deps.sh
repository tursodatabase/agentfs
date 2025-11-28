#!/bin/bash
set -e

# Install system dependencies for agentfs

if [[ "$OSTYPE" == "linux-gnu"* ]]; then
    echo "Installing dependencies for Linux..."

    # Detect Linux distribution
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        DISTRO=$ID
    else
        echo "Cannot detect Linux distribution"
        exit 1
    fi

    case "$DISTRO" in
        ubuntu|debian)
            echo "Detected Debian/Ubuntu"
            sudo apt-get update
            sudo apt-get install -y libunwind-dev liblzma-dev libfuse3-dev
            ;;
        fedora|rhel|centos)
            echo "Detected Fedora/RHEL/CentOS"
            sudo dnf install -y libunwind-devel xz-devel fuse3-devel
            ;;
        *)
            echo "Unsupported Linux distribution: $DISTRO"
            exit 1
            ;;
    esac

    echo "Linux dependencies installed successfully"

else
    echo "Unsupported OS: $OSTYPE"
    exit 1
fi

echo "All dependencies installed successfully"
