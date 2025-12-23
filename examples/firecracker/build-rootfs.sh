#!/bin/bash
set -e

BUSYBOX_VERSION="1.36.1"
BUSYBOX_DIR="busybox-${BUSYBOX_VERSION}"
BUSYBOX_MIRROR="https://github.com/mirror/busybox/archive/refs/tags/${BUSYBOX_VERSION//./_}.tar.gz"
ROOTFS_DIR="rootfs"

# Download busybox if not present
if [ ! -d "${BUSYBOX_DIR}" ]; then
    echo "Downloading BusyBox ${BUSYBOX_VERSION}..."
    curl -L --connect-timeout 10 -o busybox.tar.gz "${BUSYBOX_MIRROR}"
    tar xf busybox.tar.gz
    mv "busybox-${BUSYBOX_VERSION//./_}" "${BUSYBOX_DIR}"
    rm busybox.tar.gz
fi

# Build busybox statically if not already built
if [ ! -f "${BUSYBOX_DIR}/_install/bin/busybox" ]; then
    cd "${BUSYBOX_DIR}"
    echo "Configuring BusyBox..."
    make defconfig
    sed -i 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config
    sed -i 's/CONFIG_TC=y/# CONFIG_TC is not set/' .config
    echo "Building BusyBox..."
    make -j$(nproc)
    make install
    cd ..
fi

echo "Creating rootfs..."

# Create directory structure
rm -rf "${ROOTFS_DIR}"
mkdir -p "${ROOTFS_DIR}"/{bin,sbin,etc,proc,sys,dev,tmp,root,var/log,mnt}

# Copy busybox
cp -a "${BUSYBOX_DIR}/_install/"* "${ROOTFS_DIR}/"

# Create init script
cat > "${ROOTFS_DIR}/init" << 'INITEOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
hostname agentfs

cat << 'EOF'

       _                    _   _____ ____
      / \   __ _  ___ _ __ | |_|  ___/ ___|
     / _ \ / _` |/ _ \ '_ \| __| |_  \___ \
    / ___ \ (_| |  __/ | | | |_|  _|  ___) |
   /_/   \_\__, |\___|_| |_|\__|_|   |____/
           |___/

  Welcome to AgentFS + Firecracker!

  NFS root from host. Changes stored in SQLite.

EOF

exec /bin/sh
INITEOF
chmod +x "${ROOTFS_DIR}/init"

# Create /etc/passwd and /etc/group
echo "root:x:0:0:root:/root:/bin/sh" > "${ROOTFS_DIR}/etc/passwd"
echo "root:x:0:" > "${ROOTFS_DIR}/etc/group"

echo "Done! Rootfs created at: ${ROOTFS_DIR}/"
