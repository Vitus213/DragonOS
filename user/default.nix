{
  lib,
  pkgs,
  nixpkgs,
  system,
  target,
  fenix,
  buildDir,
  testOpt,
  rootfsType ? "vfat",
  diskPath,
  partitionType ? "mbr",
}:

let
  image = import ./rootfs-tar.nix {
    inherit
      lib
      pkgs
      nixpkgs
      system
      target
      fenix
      testOpt
      ;
  };

  # parted 使用 msdos 而不是 mbr
  partedLabel = if partitionType == "mbr" then "msdos" else partitionType;

  # 根据文件系统类型生成 mkfs 命令
  mkfsCommand =
    if rootfsType == "vfat" then
      ''sudo mkfs.vfat "''${LOOP_DEV}p1"''
    else if rootfsType == "ext4" then
      ''sudo mkfs.ext4 -F "''${LOOP_DEV}p1"''
    else
      ''sudo mkfs -t ${rootfsType} "''${LOOP_DEV}p1"'';

  # vfat 特殊处理脚本片段
  vfatProcessing = ''
    echo "  Processing rootfs for vfat (excluding /nix/store, dereferencing symlinks)..."

    EXTRACT_DIR=$(mktemp -d)
    FILTERED_TAR="${buildDir}/rootfs-filtered.tar"

    # 解压原始 tar，排除 /nix/store
    echo "    Extracting and not filtering..."
    chmod +w -R "$TEMP_DIR" "$EXTRACT_DIR"
    fakeroot tar --owner=0 --group=0 --numeric-owner --exclude='proc' --exclude='dev' \
        --exclude='sys' -xf "$OUTPUT_TAR" -C "$EXTRACT_DIR"

    # 重新打包，解引用符号链接和硬链接
    echo "    Re-packing with dereferenced links..."
    fakeroot tar --owner=0 --group=0 --numeric-owner --dereference --hard-dereference -cf "$FILTERED_TAR" -C "$EXTRACT_DIR" .

    FILTERED_SIZE=$(du -h "$FILTERED_TAR" | cut -f1)
    echo "  ✓ Re-packed rootfs.tar created ($FILTERED_SIZE)"

    FINAL_TAR="$FILTERED_TAR"
  '';

  # 非 vfat 不需要特殊处理
  nonVfatProcessing = ''
    FINAL_TAR="$OUTPUT_TAR"
  '';

  # 根据 rootfsType 选择处理逻辑
  rootfsProcessing = if rootfsType == "vfat" then vfatProcessing else nonVfatProcessing;

  # guestfish 写盘逻辑
  guestfishWrite = ''
    echo "  Using guestfish (unprivileged mode)..."
    export LIBGUESTFS_CACHEDIR=/tmp
    export LIBGUESTFS_BACKEND=direct

    # 使用 guestfish 创建分区并注入 tar
    echo "  Initializing disk and copying rootfs..."
    guestfish -a "$TEMP_IMG" <<EOF
      run
      part-init /dev/sda ${partitionType}
      part-add /dev/sda primary 2048 -2048
      mkfs ${rootfsType} /dev/sda1
      mount /dev/sda1 /
      tar-in $FINAL_TAR /
      chmod 0755 /
      umount /
      sync
      shutdown
    EOF
  '';

  # loop 设备写盘逻辑
  loopWrite = ''
    echo "  Using loop device (privileged mode, faster)..."

    # 使用 parted 创建分区表和分区
    echo "    Creating partition table..."
    parted -s "$TEMP_IMG" mklabel ${partedLabel}
    parted -s "$TEMP_IMG" mkpart primary ${rootfsType} 1MiB 100%

    # 设置 loop 设备
    echo "    Setting up loop device..."
    LOOP_DEV=$(sudo losetup --find --show --partscan "$TEMP_IMG")
    echo "    Loop device: $LOOP_DEV"

    # 确保清理 loop 设备
    # shellcheck disable=SC2317,SC2329
    cleanup_loop() {
      echo "    Cleaning up loop device..."
      sudo umount "''${LOOP_DEV}p1" 2>/dev/null || true
      sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
    }
    trap 'cleanup_loop; chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT

    # 等待分区设备出现
    echo "    Waiting for partition device..."
    for _ in $(seq 1 10); do
      if [ -b "''${LOOP_DEV}p1" ]; then
        break
      fi
      sleep 0.1
    done

    if [ ! -b "''${LOOP_DEV}p1" ]; then
      echo "Error: Partition device ''${LOOP_DEV}p1 not found"
      exit 1
    fi

    # 格式化分区
    echo "    Formatting partition as ${rootfsType}..."
    ${mkfsCommand}

    # 挂载分区
    MOUNT_DIR=$(mktemp -d)
    echo "    Mounting partition to $MOUNT_DIR..."
    sudo mount "''${LOOP_DEV}p1" "$MOUNT_DIR"

    # 更新 trap 以包含 MOUNT_DIR
    # shellcheck disable=SC2317,SC2329
    cleanup_loop() {
      echo "    Cleaning up..."
      sudo umount "$MOUNT_DIR" 2>/dev/null || true
      sudo losetup -d "$LOOP_DEV" 2>/dev/null || true
      rm -rf "$MOUNT_DIR" 2>/dev/null || true
    }
    trap 'cleanup_loop; chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT

    # 解压 tar 到分区
    echo "    Extracting rootfs to partition..."
    sudo tar -xf "$FINAL_TAR" -C "$MOUNT_DIR"
    sudo chmod 0755 "$MOUNT_DIR"

    # 同步并卸载
    echo "    Syncing and unmounting..."
    sync
    sudo umount "$MOUNT_DIR"
    sudo losetup -d "$LOOP_DEV"
    rm -rf "$MOUNT_DIR"

    # 重置 trap
    trap 'chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT
  '';
  # Darwin 没有 Linux loop/parted/guestfish；使用原生 e2fsprogs + Python 写入 MBR。
  darwinWrite = ''
    echo "  Using native Darwin ext4 image writer..."
    EXTRACT_DIR=$(mktemp -d)
    PART_IMG="${diskPath}.partition.tmp"
    trap 'chmod -R u+rwX "$TEMP_DIR" "$EXTRACT_DIR" 2>/dev/null || true; rm -rf "$TEMP_DIR" "$EXTRACT_DIR" "$PART_IMG" "$TEMP_IMG"' EXIT
    tar --delay-directory-restore --no-same-owner --no-same-permissions -xf "$FINAL_TAR" -C "$EXTRACT_DIR"

    TAR_SIZE_BYTES=$(wc -c < "$FINAL_TAR")
    PART_START_SECTORS=2048
    PART_SIZE_SECTORS=$(( (TAR_SIZE_BYTES + 1024 * 1024 * 1024 + 511) / 512 ))
    DISK_SIZE_SECTORS=$(( PART_START_SECTORS + PART_SIZE_SECTORS ))
    truncate -s "$((PART_SIZE_SECTORS * 512))" "$PART_IMG"
    mke2fs -q -t ext4 -F -d "$EXTRACT_DIR" "$PART_IMG"
    truncate -s "$((DISK_SIZE_SECTORS * 512))" "$TEMP_IMG"

    python3 - "$TEMP_IMG" "$PART_START_SECTORS" "$PART_SIZE_SECTORS" <<'PY'
import struct
import sys

image, start, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
mbr = bytearray(512)
entry = 446
mbr[entry] = 0x80
mbr[entry + 1:entry + 4] = b"\xfe\xff\xff"
mbr[entry + 4] = 0x83
mbr[entry + 5:entry + 8] = b"\xfe\xff\xff"
mbr[entry + 8:entry + 12] = struct.pack("<I", start)
mbr[entry + 12:entry + 16] = struct.pack("<I", size)
mbr[510:512] = b"\x55\xaa"
with open(image, "r+b") as f:
    f.write(mbr)
PY
    dd if="$PART_IMG" of="$TEMP_IMG" bs=512 seek="$PART_START_SECTORS" conv=notrunc
    rm -f "$PART_IMG"
  '';

  runtimeInputs =
    [
      pkgs.coreutils
      pkgs.gnutar
      pkgs.findutils
      pkgs.e2fsprogs
    ]
    ++ lib.optionals (system != "aarch64-darwin") [
      pkgs.libguestfs-with-appliance
      pkgs.parted
      pkgs.dosfstools
      pkgs.util-linux
    ]
    ++ lib.optionals (system == "aarch64-darwin") [
      pkgs.python3
    ];

  buildScript = pkgs.writeShellApplication {
    name = "dragonos-rootfs";
    inherit runtimeInputs;
    text = ''
      set -euo pipefail

      # Ensure build directory exists
      mkdir -p "${buildDir}"

      OUTPUT_TAR="${buildDir}/rootfs.tar"

      echo "==> Generating rootfs"

      # 创建临时目录
      TEMP_DIR=$(mktemp -d)
      EXTRACT_DIR=""  # 初始化为空，vfat 处理时会赋值
      trap 'chmod +w -R "$TEMP_DIR" 2>/dev/null && rm -rf "$TEMP_DIR"; [ -n "$EXTRACT_DIR" ] && chmod +w -R "$EXTRACT_DIR" 2>/dev/null && rm -rf "$EXTRACT_DIR"' EXIT

      # 提取 layer.tar (rootfs)
      echo "  Extracting rootfs layer..."
      cd "$TEMP_DIR"
      tar -xzf ${image}

      # 找到 layer.tar 并复制到 bin/
      LAYER_TAR=$(find . -name "layer.tar" | head -1)
      if [ -z "$LAYER_TAR" ]; then
        echo "Error: layer.tar not found in docker image"
        exit 1
      fi

      cp "$LAYER_TAR" "$OLDPWD/$OUTPUT_TAR"
      cd "$OLDPWD"
      chmod +w "$OUTPUT_TAR"

      TAR_SIZE=$(du -h "$OUTPUT_TAR" | cut -f1)
      echo "  ✓ rootfs.tar created ($TAR_SIZE)"

      # 根据文件系统类型处理 rootfs（Nix 编译时决定）
      ${rootfsProcessing}

      echo "==> Building disk image at ${diskPath}"

      # 创建磁盘镜像并初始化文件系统
      echo "  Creating disk image..."
      TEMP_IMG="${diskPath}.tmp"

      ${lib.optionalString (system == "aarch64-darwin") darwinWrite}
      ${lib.optionalString (system != "aarch64-darwin") ''
        TAR_SIZE_KB=$(du -k "$FINAL_TAR" | cut -f1)
        DISK_SIZE_KB=$(( TAR_SIZE_KB + 1024 * 1024 ))
        truncate -s "''${DISK_SIZE_KB}K" "$TEMP_IMG"
        if [ "''${DRAGONOS_UNPRIVILEGED_BUILD:-0}" = "1" ]; then
          ${guestfishWrite}
        else
          ${loopWrite}
        fi
      ''}

      mv -f "$TEMP_IMG" "${diskPath}"

      IMG_SIZE=$(du -h "${diskPath}" | cut -f1)
      echo "  ✓ disk image created ($IMG_SIZE)"

      echo "==> Build complete!"
      echo "    Rootfs tar: $OUTPUT_TAR"
      echo "    Disk image: ${diskPath}"
    '';
  };

in
buildScript
