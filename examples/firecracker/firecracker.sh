#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIRECRACKER="${SCRIPT_DIR}/firecracker"
FC_VERSION="v1.14.0"
TAP_DEV="fc-tap0"
TAP_IP="172.16.0.1"
VM_IP="172.16.0.2"
NFS_PORT="11111"
AGENTFS="${SCRIPT_DIR}/../../cli/target/release/agentfs"
AGENT_ID="${1:-firecracker}"
DB_PATH="${SCRIPT_DIR}/.agentfs/${AGENT_ID}.db"

# Download Firecracker if not present
if [ ! -f "${FIRECRACKER}" ]; then
    echo "Downloading Firecracker ${FC_VERSION}..."
    curl -L -o firecracker.tgz "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-x86_64.tgz"
    tar xf firecracker.tgz
    mv "release-${FC_VERSION}-x86_64/firecracker-${FC_VERSION}-x86_64" "${FIRECRACKER}"
    rm -rf "release-${FC_VERSION}-x86_64" firecracker.tgz
    chmod +x "${FIRECRACKER}"
fi

# Check prerequisites
if [ ! -f "${SCRIPT_DIR}/linux-amazon/vmlinux" ]; then
    echo "Error: Kernel not found. Run ./build-kernel.sh first."
    exit 1
fi

if [ ! -d "${SCRIPT_DIR}/rootfs" ]; then
    echo "Error: rootfs not found. Run ./create-nfs-rootfs.sh first."
    exit 1
fi

if [ ! -f "${AGENTFS}" ]; then
    echo "Error: agentfs binary not found. Build it first:"
    echo "  cd ../../cli && cargo build --release"
    exit 1
fi

# Initialize agentfs database if needed
if [ ! -f "${DB_PATH}" ]; then
    cd "${SCRIPT_DIR}"
    ${AGENTFS} init --base ./rootfs "${AGENT_ID}" >/dev/null
fi

# Clean up function
cleanup() {
    kill $AGENTFS_PID 2>/dev/null || true
    sudo ip link del "${TAP_DEV}" 2>/dev/null || true
    echo ""
    echo "Changes saved. View with: agentfs diff ${AGENT_ID}"
}
trap cleanup EXIT

# Clean up from previous failed run
sudo ip link del "${TAP_DEV}" 2>/dev/null || true
pkill -f "agentfs nfs.*${NFS_PORT}" 2>/dev/null || true
sleep 0.5

# Set up TAP device
sudo ip tuntap add dev "${TAP_DEV}" mode tap
sudo ip addr add "${TAP_IP}/24" dev "${TAP_DEV}"
sudo ip link set "${TAP_DEV}" up

# Start agentfs NFS server (suppress output)
cd "${SCRIPT_DIR}"
${AGENTFS} nfs --bind "${TAP_IP}" --port "${NFS_PORT}" "${AGENT_ID}" >/dev/null 2>&1 &
AGENTFS_PID=$!
sleep 1

if ! kill -0 $AGENTFS_PID 2>/dev/null; then
    echo "Error: agentfs NFS server failed to start"
    exit 1
fi

# Create Firecracker config
KERNEL_PATH="${SCRIPT_DIR}/linux-amazon/vmlinux"
cat > "${SCRIPT_DIR}/vm_config.json" << EOF
{
  "boot-source": {
    "kernel_image_path": "${KERNEL_PATH}",
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off quiet loglevel=0 ip=${VM_IP}::${TAP_IP}:255.255.255.0::eth0:off root=/dev/nfs nfsroot=${TAP_IP}:/,nfsvers=3,tcp,nolock,port=${NFS_PORT},mountport=${NFS_PORT} rw init=/init"
  },
  "drives": [],
  "network-interfaces": [
    {
      "iface_id": "eth0",
      "guest_mac": "AA:FC:00:00:00:01",
      "host_dev_name": "${TAP_DEV}"
    }
  ],
  "machine-config": {
    "vcpu_count": 1,
    "mem_size_mib": 256
  }
}
EOF

# Run Firecracker
${FIRECRACKER} --no-api --config-file "${SCRIPT_DIR}/vm_config.json" --level Error
