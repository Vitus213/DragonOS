---
name: dragonos-nix-repo-debug
description: 在用户希望 AI 亲自复现 DragonOS 问题、自己加证据、自己重建并启动 QEMU、并基于真实 guest 输出定位根因时使用。适用于内核、驱动、VFS、调度、IPC、init、用户态测试或 QEMU 启动链路改动后，必须通过 Nix + QEMU 新鲜验证来确认行为的场景。
---

# DragonOS Nix Repo Debug

## 目标

把“代码看起来像是这样”推进到“我已经在 DragonOS 里实际跑过，并拿到了可重复、可解释的证据链”。

这不是静态阅读 skill，而是一个完整闭环：

1. 收敛最小复现路径
2. 有目的地加证据
3. 只重建必要产物
4. 启动一个**新的** DragonOS 实例
5. 在 guest 内或 boot-time 自动化路径里复现
6. 用输出更新假设，直到得到根因

## 什么时候用

优先用于这些情况：

- 用户明确要你“自己跑起来看看”
- 需要你自己启动 DragonOS / QEMU 复现问题
- 修改了 `kernel/`、`user/`、`user/sysconfig/etc/init.d/`、`tools/qemu/` 后，必须做 guest 验证
- 需要验证用户态测试、启动脚本、boot 参数、镜像内容、内核行为是否真的生效
- 需要把“编译成功但不知道运行时为什么错”推进到“拿到真实运行证据”

不要用于这些情况：

- 只是读代码、做设计、写方案
- 明显是高扰动日志会改变现象的 Heisenbug，此时优先参考 `dragonos-atomic-snapshot-debug`

## 权限闸门

如果你预判后续步骤会碰到这些事情，就要在一开始确认权限或密码，不要做到一半再卡住：

- `nix run .#rootfs-x86_64` 可能需要 `sudo` 做 loop 设备、分区、挂载
- `nix run .#start-x86_64` 可能需要 `sudo` 拉起 QEMU/KVM
- 清理旧 QEMU 残留时，可能需要删除 root-owned 的 `/dev/shm` 或 `bin/vmstate/` 文件

如果整条链路不需要提权，就直接继续，不要无意义打断用户。

## 工作流

### 1. 先收敛最小问题陈述

开始前先明确：

- 现象：实际发生了什么
- 预期：按 Linux 6.6 或 DragonOS 设计语义应该怎样
- 触发方式：哪个程序、syscall、测试、启动阶段或脚本路径能复现
- 影响范围：稳定复现、概率复现，还是只在特定时序下出现

优先检查这些位置：

- `kernel/`
- `user/apps/c_unitest/`
- `user/sysconfig/etc/init.d/`
- `tools/qemu/`
- 用户提供的测试程序或复现脚本

如果能先收敛成一个最小命令、一个最小 guest 测试或一个 boot-time 自动入口，就先做这件事。

### 2. 设计证据点，不要先刷屏

插桩必须服务于一个明确问题，比如：

- 我们到底有没有进到目标路径？
- 哪个 guard 提前返回了？
- 哪个状态在迁移、清理、charge / uncharge 前后不对？
- 哪个对象在 cleanup 时还没离开子树？

优先加这些证据：

- 函数入口 / 出口
- 关键参数
- 状态转换边界
- 迁移、charge、uncharge、reclaim、teardown 等语义边界
- 容易偏离 Linux 语义的返回条件

避免：

- 无差别高频 `debug!` / `println!`
- 无法关联上下文的随机输出
- 用 workaround 掩盖问题

### 3. 按正确范围重建

所有命令都默认在 **DragonOS 仓库根目录** 执行。

基础命令：

```sh
nix develop -c make kernel
nix run .#rootfs-x86_64
nix run .#start-x86_64
```

含义：

- `nix develop -c make kernel`：构建内核，产物是 `./bin/kernel/kernel.elf`
- `nix run .#rootfs-x86_64`：构建 rootfs 镜像，产物是 `./bin/disk-image-x86_64.img`
- `nix run .#start-x86_64`：启动 DragonOS

重建规则：

- 改了 `kernel/`：至少重建内核
- 改了 `user/`、`user/sysconfig/`、guest 测试、init 脚本：必须重建 rootfs
- 改了内核和用户态：两者都重建
- 不要假设旧产物仍然有效

### 4. 每次验证前优先保证是 fresh QEMU

在一轮真正有意义的验证前，先确认你不是在复用旧实例。

优先检查并在需要时清理：

- 旧的 `qemu-system-x86_64` 进程
- `/dev/shm/dragonos-qemu-shm.ram`
- `./bin/vmstate/pid`
- `./bin/vmstate/port`
- `./bin/vmstate/vsock_cid`
- `/tmp/dragonos-vsock-cid-registry`

这是为了避免这些假失败：

- 磁盘镜像锁冲突
- 共享内存残留
- 老的 vmstate 被复用
- vsock guest CID already in use
- 实际连到的是旧 QEMU，不是刚启动的新实例

### 5. guest 交互不稳定时，优先改成 boot-time 自动化

如果控制台需要手动激活、交互输入不稳定、或串口难以驱动，不要硬凹交互自动化。

优先改成 boot-time 自动执行：

- 在 `user/sysconfig/etc/init.d/rcS` 里加 `AUTO_TEST=<name>` 分支
- 在 `tools/qemu/default.nix` 里确保运行时的 `AUTO_TEST` 能透传到 boot cmdline
- 用 `AUTO_TEST=<name> nix run .#start-x86_64` 启动

这个路径通常比模拟交互控制台更稳。

### 6. cgroup memory controller 的专项流程

如果当前问题是 cgroup memory controller 或它的 guest 自动测试链路，不要把专项规则散落在主 skill 里，直接参考：

- [references/cgroup-autotest-workflow.md](references/cgroup-autotest-workflow.md)

这份参考文档包含：

- 这条链路到底是不是“先 Nix 编译、再 QEMU 挂载启动、再由 `rcS` 预先写好的测试入口自动执行”
- `AUTO_TEST=cgroup` 的正确使用方式
- 应该检查哪些 PASS / FAIL / cleanup warning 输出
- 这次在仓库里验证过的真实路径和常见坑

### 7. 每轮运行后都更新假设

每轮调试至少回答：

1. 这轮运行证明了什么？
2. 哪条假设已经被证伪？
3. 下一步最小改动应该放在哪里？

有价值的中间结论包括：

- 问题在 accounting 本体还是 teardown
- `AUTO_TEST` 是否真的进了 boot cmdline
- init 分支是否真的执行了
- 测试二进制是否真的进了镜像
- 失败是 guest 真实行为，还是 stale runtime state 导致的假象

## 快速参考

| 场景 | 必做动作 |
| --- | --- |
| 改了 `kernel/` | `nix develop -c make kernel` |
| 改了 `user/` / `user/sysconfig/` | `nix run .#rootfs-x86_64` |
| 需要真实 guest 证据 | `nix run .#start-x86_64` |
| 控制台交互不稳 | 改成 `rcS` + `AUTO_TEST` |
| 启动时报锁、shm、vsock 冲突 | 先清理 stale QEMU state |
| 新增 guest 测试不在镜像里 | 确认文件已被跟踪，并重建 rootfs |

## 常见坑

- 改了 `user/` 还只重编内核
- 新增测试文件没被跟踪，结果镜像里根本没有这个二进制
- guest 输出里已经有 `[WARN] cleanup`，却还把结果当成功
- 以为自己在测最新实例，实际上连的是旧 QEMU
- 没有先定义假设就开始乱打日志
- 做到一半才发现需要 `sudo`

## 输出格式

默认按这个结构汇报：

### Bug Summary
- 症状
- 预期语义
- 当前复现路径

### Reproduction
- 使用了哪些构建命令
- 使用了哪个启动命令
- guest 内执行了什么测试，或用了什么 boot-time 自动化入口
- 复现是否稳定

### Evidence
- 关键日志 / 插桩结论
- 被证伪的假设
- 当前最强证据链

### Root Cause
- 根因判断
- 对应代码路径
- 为什么这是根因而不是表象

### Next Change
- 建议修复点，或下一轮最小改动
