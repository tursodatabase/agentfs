#!/bin/bash
set -e

KERNEL_DIR="linux-amazon"

# Clone Amazon Linux kernel if not present
if [ ! -d "${KERNEL_DIR}" ]; then
    echo "Cloning Amazon Linux kernel..."
    git clone --depth 1 --branch microvm-kernel-6.1.102-1.182.amzn2023 \
        https://github.com/amazonlinux/linux.git "${KERNEL_DIR}"
fi

cd "${KERNEL_DIR}"

# Download Firecracker's kernel config
echo "Downloading Firecracker kernel config..."
curl -L -o .config "https://raw.githubusercontent.com/firecracker-microvm/firecracker/refs/heads/firecracker-v1.10/resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config"

# Disable certificate/keyring stuff that requires OpenSSL
./scripts/config --disable SYSTEM_TRUSTED_KEYRING
./scripts/config --disable SECONDARY_TRUSTED_KEYRING
./scripts/config --disable SYSTEM_REVOCATION_KEYS
./scripts/config --disable MODULE_SIG
./scripts/config --disable INTEGRITY
./scripts/config --disable IMA
./scripts/config --disable EVM
./scripts/config --set-str SYSTEM_TRUSTED_KEYS ""
./scripts/config --set-str SYSTEM_REVOCATION_KEYS ""

# Enable virtio-mmio cmdline devices (needed for Firecracker to register devices)
./scripts/config --enable VIRTIO_MMIO_CMDLINE_DEVICES
./scripts/config --disable BLK_DEV_INTEGRITY

# Enable NFSv3 client support
./scripts/config --enable NFS_V3
./scripts/config --enable NFS_V3_ACL
./scripts/config --enable LOCKD_V4

# Update config
make olddefconfig

echo "Building kernel..."
# Use C11 to avoid C23 keyword conflicts with older kernel versions
make -j$(nproc) vmlinux CC="gcc -std=gnu11"

echo "Done! Kernel built at: ${KERNEL_DIR}/vmlinux"
cd ..
ln -sf "${KERNEL_DIR}/vmlinux" vmlinux
echo "Symlinked to: vmlinux"
