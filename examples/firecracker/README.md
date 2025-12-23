# Firecracker with AgentFS Example

A minimal Firecracker VM setup with NFSv3 support for AgentFS.

## Prerequisites

- Linux host with KVM support
- Firecracker binary installed
- Build tools: `gcc`, `make`, `curl`, `sudo`

## Build

1. Build the Linux kernel with NFSv3 support:

```bash
./build-kernel.sh
```

2. Build the rootfs with BusyBox:

```bash
./build-rootfs.sh
```

## Run

Start the Firecracker VM:

```bash
./run.sh
```

Press `Ctrl+C` to stop the VM.

## Files

- `build-kernel.sh` - Downloads and builds Linux kernel 6.1.x with NFSv3 support
- `build-rootfs.sh` - Creates minimal ext4 rootfs with BusyBox
- `run.sh` - Launches Firecracker VM with the built kernel and rootfs
