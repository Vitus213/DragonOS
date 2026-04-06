# DragonOS 调试用 Nix / QEMU 工作流

这份清单只保留和“真实复现 + 真实验证”直接相关的命令与规则。

## 在仓库根目录执行

下面所有命令都默认在 **DragonOS 仓库根目录** 执行。

## 标准构建 / 启动链路

```sh
nix develop -c make kernel
nix run .#rootfs-x86_64
nix run .#start-x86_64
```

含义：

- `nix develop -c make kernel`：构建内核，产物是 `./bin/kernel/kernel.elf`
- `nix run .#rootfs-x86_64`：构建 rootfs 镜像，产物是 `./bin/disk-image-x86_64.img`
- `nix run .#start-x86_64`：启动 DragonOS

## 重建规则

### 什么时候至少要重建内核

只要改了 `kernel/`，就至少执行：

```sh
nix develop -c make kernel
```

### 什么时候必须重建 rootfs

遇到这些变更时，必须执行：

```sh
nix run .#rootfs-x86_64
```

典型情况：

- 修改了 `user/` 下会进入镜像的程序
- 修改了 `user/apps/c_unitest/` 下的 guest 测试
- 修改了 `user/sysconfig/etc/init.d/` 下的启动脚本
- 修改了任何需要进入 guest rootfs 的内容

## fresh QEMU 规则

每轮真正的验证前，都要确认你不是在复用旧实例。

需要时清理：

- 旧 `qemu-system-x86_64` 进程
- `/dev/shm/dragonos-qemu-shm.ram`
- `./bin/vmstate/pid`
- `./bin/vmstate/port`
- `./bin/vmstate/vsock_cid`
- `/tmp/dragonos-vsock-cid-registry`

这样可以避免：

- 磁盘镜像被旧 QEMU 占用
- stale shm 残留
- vmstate 误复用
- vsock guest CID already in use

## 推荐的 boot-time 自动化模式

当 guest 控制台交互不稳定时，优先用 `rcS` 自动化，而不是硬写交互脚本。

推荐做法：

1. 在 `user/sysconfig/etc/init.d/rcS` 增加 `AUTO_TEST=<name>` 分支
2. 在 `tools/qemu/default.nix` 确保运行时 `AUTO_TEST` 会传进 boot cmdline
3. 通过下面命令启动：

```sh
AUTO_TEST=<name> nix run .#start-x86_64
```

## 已验证的 cgroup 测试流程

这次确认可用的流程：

1. 修改 `user/apps/c_unitest/` 下的 cgroup 测试
2. 修改 `user/sysconfig/etc/init.d/rcS` 中的 `AUTO_TEST=cgroup` 分支
3. 确认 `tools/qemu/default.nix` 透传运行时 `AUTO_TEST`
4. 如有内核改动，重建内核
5. 因为 guest 测试 / init 改了，重建 rootfs
6. 清理 stale QEMU 状态
7. 启动：

```sh
AUTO_TEST=cgroup nix run .#start-x86_64
```

期望成功输出：

```text
[rcS][cgroup] PASS /bin/test_cgroup_mvp_basic
[rcS][cgroup] PASS /bin/test_cgroup_memory_accounting
[rcS][cgroup] ALL TESTS PASSED
```

下面这些都应视为失败：

```text
[WARN] cleanup ...
[FAIL] ...
[rcS][cgroup] TESTS FAILED
```

## 启动前检查清单

在每轮启动前确认：

1. 这轮改动已经保存
2. 最新构建已经完成，没有沿用旧产物
3. 如果改了 `user/` 或 init，已经重建 rootfs
4. 即将启动的是新的 QEMU 实例
5. 如果依赖 `AUTO_TEST`，boot cmdline 会包含正确值

## 常见坑

- 只重编内核，忘了 rootfs
- 新增 guest 测试文件未被跟踪，镜像里没有这个文件
- 看到 cleanup warning 还把结果当成功
- 连接到了旧 QEMU 输出
- 启动失败其实是 stale vsock / shm / vmstate，而不是代码问题
