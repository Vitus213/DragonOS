{
  lib,
  pkgs,
  diskPath,
  kernel,
  testOpt,
  debug ? false,
  vmstateDir ? null,
}:

let
  qemuFirmware = pkgs.callPackage ./qemu-firmware.nix { };

  baseConfig = {
    nographic = true;
    memory = "512M";
    cores = "2";
    shmId = "dragonos-qemu-shm.ram";
  };

  riscv-uboot = pkgs.pkgsCross.riscv64-embedded.buildUBoot {
    defconfig = "qemu-riscv64_smode_defconfig";
    extraMeta.platforms = [ "riscv64-linux" ];
    filesToInstall = [ "u-boot.bin" ];
  };

  # 3. 参数生成器 (Nix List -> Nix List)
  # 注意：网络配置中的端口现在使用 $HOST_PORT 变量，在运行时动态替换
  mkQemuArgs =
    { arch, isNographic }:
    let
      baseArgs = [
        "-m"
        baseConfig.memory
        "-smp"
        "${baseConfig.cores},cores=${baseConfig.cores},threads=1,sockets=1"
        "-object"
        "memory-backend-file,size=${baseConfig.memory},id=${baseConfig.shmId},mem-path=/dev/shm/${baseConfig.shmId},share=on"
        "-usb"
        "-device"
        "qemu-xhci,id=xhci,p2=8,p3=4"
        "-D"
        "qemu.log"

        # Boot Order
        "-boot"
        "order=d"
        "-rtc"
        "clock=host,base=localtime"
        # Trace events
        "-d"
        "cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq*"
        "-trace"
        "fw_cfg*"
      ]
      ++ lib.optionals debug [
        # GDB Stub
        "-s"
        "-S"
      ];
      nographicArgs = lib.optionals isNographic (
        [
          "--nographic"
          "-serial"
          "chardev:mux"
          "-monitor"
          "chardev:mux"
          "-chardev"
          "stdio,id=mux,mux=on,signal=off,logfile=serial_opt.txt"
        ]
        ++ (
          if arch == "riscv64" then
            [
              "-device"
              "virtio-serial-device"
              "-device"
              "virtconsole,chardev=mux"
            ]
          else
            [
              "-device"
              "virtio-serial"
              "-device"
              "virtconsole,chardev=mux"
            ]
        )
      );
      kernelCmdlinePart = if isNographic then "console=/dev/hvc0" else "";
    in
    {
      flags = baseArgs ++ nographicArgs;
      cmdlineExtra = kernelCmdlinePart;
    };

  # 4. 运行脚本生成器
  mkRunScript =
    {
      name,
      arch,
      isNographic,
      qemuBin,
      testMode ? false,
    }:
    let
      qemuConfig = mkQemuArgs { inherit arch isNographic; };
      qemuFlagsStr = lib.escapeShellArgs qemuConfig.flags;

      initProgram = if arch == "riscv64" then "/bin/riscv_rust_init" else "/bin/busybox init";

      # cmdline 中的 AUTO_TEST 参数
      autoTestValue = if testMode then "syscall" else testOpt.autotest;

      # Define static parts of arguments using Nix lists
      commonArchArgs =
        if arch == "x86_64" then
          [
            "-machine"
            "q35,memory-backend=${baseConfig.shmId}"
            "-cpu"
            "IvyBridge,apic,x2apic,+fpu,check,+vmx,"
          ]
        else
          [
            "-cpu"
            "sifive-u54"
          ];

      kernelPath = if arch == "x86_64" then kernel else "${riscv-uboot}/u-boot.bin";

      diskArgs =
        if arch == "x86_64" then
          [
            "-device"
            "virtio-blk-pci,drive=disk"
            "-device"
            "pci-bridge,chassis_nr=1,id=pci.1"
            "-device"
            "pcie-root-port"
            "-drive"
            "id=disk,file=${diskPath},if=none"
          ]
        else
          [
            "-device"
            "virtio-blk-device,drive=disk"
            "-drive"
            "id=disk,file=${diskPath},if=none"
          ];

      # Generate bash code for dynamic parts
      archSpecificBash =
        if arch == "x86_64" then
          ''
            if [ "$ACCEL" == "kvm" ]; then
                ARCH_FLAGS+=( "-machine" "accel=kvm" "-enable-kvm" )
            else
                ARCH_FLAGS+=( "-machine" "accel=tcg" )
            fi
          ''
        else
          ''
            ARCH_FLAGS+=( "-machine" "virt,accel=$ACCEL,memory-backend=${baseConfig.shmId}" )
          '';

      # VM 状态目录配置
      vmstateDirStr =
        if vmstateDir != null then
          vmstateDir
        else if testMode then
          "./bin/vmstate"
        else
          "";
      hasVmstateDir = vmstateDir != null || testMode;

    in
    pkgs.writeScriptBin name ''
      #!${pkgs.runtimeShell}

      if [ ! -d "bin" ]; then echo "Error: Please run from project root (bin/ missing)."; exit 1; fi

      # 端口查找函数：从指定端口开始查找可用端口
      find_available_port() {
        local start_port=$1
        local port=$start_port
        while [ $port -lt 65535 ]; do
          if ! ${pkgs.iproute2}/bin/ss -tuln 2>/dev/null | grep -q ":$port "; then
            echo $port
            return 0
          fi
          port=$((port + 1))
        done
        echo $start_port
      }

      # 动态分配端口
      HOST_PORT=$(find_available_port 12580)

      ACCEL="tcg"
      if [ -e /dev/kvm ] && [ -w /dev/kvm ]; then ACCEL="kvm"; fi

      VMSTATE_DIR="${vmstateDirStr}"
      ${
        if hasVmstateDir then
          ''
            mkdir -p "$VMSTATE_DIR"
            echo "$HOST_PORT" > "$VMSTATE_DIR/port"
          ''
        else
          ""
      }

      EXTRA_CMDLINE="${qemuConfig.cmdlineExtra}"
      FINAL_CMDLINE="init=${initProgram} AUTO_TEST=${autoTestValue} SYSCALL_TEST_DIR=${testOpt.syscall.testDir} $EXTRA_CMDLINE"

      ARCH_FLAGS=( ${lib.escapeShellArgs commonArchArgs} )
      ${archSpecificBash}

      BOOT_ARGS=( "-kernel" "${kernelPath}" "-append" "$FINAL_CMDLINE" )
      DISK_ARGS=( ${lib.escapeShellArgs diskArgs} )

      # 动态网络配置（使用动态分配的端口）
      NET_ARGS=( "-netdev" "user,id=hostnet0,hostfwd=tcp::$HOST_PORT-:12580" "-device" "virtio-net-pci,vectors=5,netdev=hostnet0,id=net0" )

      echo -e "================== DragonOS QEMU Command Preview =================="
      echo -e "Binary: sudo ${qemuBin}"
      echo -e "Base Flags: ${qemuFlagsStr}"
      echo -e "Arch Flags: ''${ARCH_FLAGS[*]}"
      echo -e "Boot Args: ''${BOOT_ARGS[*]}"
      echo -e "Disk Args: ''${DISK_ARGS[*]}"
      echo -e "Net Args: ''${NET_ARGS[*]}"
      echo -e "Host Port: $HOST_PORT"
      echo -e "=================================================================="
      echo ""

      # --- 执行 ---
      ${
        if testMode then
          ''
            TAIL_PID=""

            cleanup() {
              if [ -n "$TAIL_PID" ]; then
                kill $TAIL_PID 2>/dev/null || true
              fi
              if [ -f "$VMSTATE_DIR/pid" ]; then
                QEMU_PID=$(cat "$VMSTATE_DIR/pid" 2>/dev/null)
                if [ -n "$QEMU_PID" ]; then
                  sudo kill -TERM $QEMU_PID 2>/dev/null || true
                  sleep 2
                  sudo kill -9 $QEMU_PID 2>/dev/null || true
                fi
                rm -f "$VMSTATE_DIR/pid"
              fi
              sudo rm -f /dev/shm/${baseConfig.shmId} 2>/dev/null || true
            }
            trap cleanup EXIT

            rm -f serial_opt.txt

            sudo bash -c 'pidfile="$1"; shift; echo $$ > "$pidfile"; exec "$@"' bash "$VMSTATE_DIR/pid" \
              ${qemuBin} ${qemuFlagsStr} "''${NET_ARGS[@]}" -L ${qemuFirmware} "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" > /dev/null 2>&1 &

            sleep 2
            tail -f serial_opt.txt 2>/dev/null &
            TAIL_PID=$!
            sleep 3

            export ROOT_PATH="$(pwd)"
            export VMSTATE_DIR="$VMSTATE_DIR"
            bash user/apps/tests/syscall/gvisor/monitor_test_results.sh
            TEST_RESULT=$?

            kill $TAIL_PID 2>/dev/null || true
            exit $TEST_RESULT
          ''
        else
          ''
            cleanup() {
              sudo rm -f /dev/shm/${baseConfig.shmId}
              ${if hasVmstateDir then ''rm -f "$VMSTATE_DIR/pid"'' else ""}
            }
            trap cleanup EXIT

            ${qemuBin} --version

            ${
              if hasVmstateDir then
                ''
                  sudo bash -c 'pidfile="$1"; shift; echo $$ > "$pidfile"; exec "$@"' bash "$VMSTATE_DIR/pid" ${qemuBin} ${qemuFlagsStr} "''${NET_ARGS[@]}" -L ${qemuFirmware} "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "$@"
                ''
              else
                ''
                  sudo ${qemuBin} ${qemuFlagsStr} "''${NET_ARGS[@]}" -L ${qemuFirmware} "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "$@"
                ''
            }
          ''
      }
    '';

  script = lib.genAttrs [ "x86_64" "riscv64" ] (
    arch:
    mkRunScript {
      name = "dragonos-run";
      inherit arch;
      isNographic = if arch == "riscv64" then true else baseConfig.nographic;
      qemuBin = "${pkgs.qemu_kvm}/bin/qemu-system-${arch}";
    }
  );

  testScript = lib.genAttrs [ "x86_64" "riscv64" ] (
    arch:
    mkRunScript {
      name = "dragonos-test";
      inherit arch;
      isNographic = true;
      qemuBin = "${pkgs.qemu_kvm}/bin/qemu-system-${arch}";
      testMode = true;
    }
  );
in
{
  inherit script testScript;
}
