# DragonOS cgroup AUTO_TEST 验证流程

这份文档描述的是**已经在当前仓库里跑通并验证过**的 cgroup guest 自动测试链路。

目标不是“知道大概怎么做”，而是让你按步骤执行后，能够在当前仓库里**可复现**地得到同样的验证结果。

---

## 1. 这条链路到底在做什么

是的，这条测试链路的本质就是：

1. 先用 Nix 构建最新内核和 rootfs
2. 把要运行的 guest 测试提前打进镜像
3. 通过 boot 参数指定 `AUTO_TEST=cgroup`
4. 在 `rcS` 启动脚本里自动运行预先写好的测试
5. 通过串口输出判断 PASS / FAIL / cleanup warning

也就是说，测试**不是**靠你进入 guest 后再手敲命令，而是：

- **Nix 负责编译和打包**
- **QEMU 负责挂载并启动镜像**
- **`rcS` 负责自动执行预先指定的测试**
- **串口日志负责给出最终验证结果**

如果你看到的不是这条链路，那说明你没有跑在这份文档描述的流程上。

---

## 2. 适用范围

这份专项文档适用于：

- 验证 cgroup v2 memory controller 的 guest 行为
- 验证 `user/apps/c_unitest/` 下的 cgroup 测试是否真的进了镜像并执行
- 验证 `AUTO_TEST=cgroup` 是否真的进入了 boot cmdline
- 验证 `rcS` 的 cgroup 分支是否真的运行
- 验证 teardown / cleanup 是否已经干净，不再有 warning

不适用于：

- 纯代码阅读
- 不需要 guest 启动的单元测试
- 其他与 cgroup 无关的 AUTO_TEST 分支

---

## 3. 当前仓库中涉及的关键路径

这些路径已经在这次验证中确认过：

### 代码与配置路径

- cgroup guest 测试目录：`user/apps/c_unitest/`
- 启动期自动测试入口：`user/sysconfig/etc/init.d/rcS`
- QEMU boot 参数透传：`tools/qemu/default.nix`

### 构建产物路径

- 内核 ELF：`./bin/kernel/kernel.elf`
- rootfs 镜像：`./bin/disk-image-x86_64.img`

### 运行期状态路径

每轮 fresh 验证前可能需要检查或清理：

- `/dev/shm/dragonos-qemu-shm.ram`
- `./bin/vmstate/pid`
- `./bin/vmstate/port`
- `./bin/vmstate/vsock_cid`
- `/tmp/dragonos-vsock-cid-registry`

---

## 4. 前置条件

在开始之前，确认：

1. 你当前就在 **DragonOS 仓库根目录**
2. 如果后续会跑 `nix run .#rootfs-x86_64` 或 `nix run .#start-x86_64`，你已经准备好 `sudo`
3. 你要验证的测试文件已经存在于 `user/apps/c_unitest/`
4. 如果是新加文件，文件已经被 git 跟踪，否则可能不会正确进入构建输入

推荐先确认这些文件存在并内容正确：

- `user/apps/c_unitest/test_cgroup_mvp_basic.c`
- `user/apps/c_unitest/test_cgroup_memory_accounting.c`
- `user/sysconfig/etc/init.d/rcS`
- `tools/qemu/default.nix`

---

## 5. 当前仓库里这条自动测试链是怎么接起来的

### 5.1 rcS 里提前写好了运行哪些测试

当前仓库中的 `user/sysconfig/etc/init.d/rcS` 应该有：

- `AUTO_TEST=cgroup` 分支
- 分支内自动执行：
  - `/bin/test_cgroup_mvp_basic`
  - `/bin/test_cgroup_memory_accounting`

也就是说，**运行什么测试，是提前写死在 `rcS` 里的**。

不是启动后临时决定，也不是进 guest 后手动执行。

### 5.2 default.nix 负责把 AUTO_TEST 带进 boot cmdline

`tools/qemu/default.nix` 里应该把运行时环境变量 `AUTO_TEST` 透传到最终 cmdline，核心效果是：

```sh
AUTO_TEST=cgroup nix run .#start-x86_64
```

会让 guest 实际收到：

```text
AUTO_TEST=cgroup
```

如果这一步没有生效，系统虽然能启动，但不会进入 `rcS` 的 cgroup 自动测试分支。

---

## 6. 构建规则

### 6.1 什么时候必须重建 kernel

只要改了 `kernel/`，就要重新执行：

```sh
nix develop -c make kernel
```

### 6.2 什么时候必须重建 rootfs

只要改了下面任意内容，就必须重新执行：

```sh
nix run .#rootfs-x86_64
```

典型情况：

- `user/apps/c_unitest/` 下的测试代码
- `user/sysconfig/etc/init.d/rcS`
- 其他任何会进入 guest 镜像的 `user/` 内容

### 6.3 常见误判

下面这些情况**不能**省略 rootfs 重建：

- 只是改了测试文件，没改内核
- 只是改了 `rcS`
- 只是新增了一个 guest 测试二进制

这些都属于“镜像内容变化”，必须重建 rootfs。

---

## 7. 可复现执行步骤

下面这组步骤是可直接照做的操作手册。

### 第 1 步：确认代码已经就位

确认以下逻辑已经在代码里：

- `rcS` 存在 `AUTO_TEST=cgroup` 分支
- `default.nix` 透传运行时 `AUTO_TEST`
- cgroup 测试位于 `user/apps/c_unitest/`

### 第 2 步：按改动范围构建

如果内核和用户态都改了，执行：

```sh
nix develop -c make kernel
nix run .#rootfs-x86_64
```

如果只改了 guest 测试或 `rcS`，执行：

```sh
nix run .#rootfs-x86_64
```

### 第 3 步：清理 stale QEMU 状态

每轮真正的验证前，都建议检查并在需要时清理：

- 旧 `qemu-system-x86_64` 进程
- `/dev/shm/dragonos-qemu-shm.ram`
- `./bin/vmstate/pid`
- `./bin/vmstate/port`
- `./bin/vmstate/vsock_cid`
- `/tmp/dragonos-vsock-cid-registry`

如果不清理，常见假失败包括：

- 镜像锁冲突
- stale shm 残留
- vmstate 复用
- vsock guest CID 冲突
- 实际连接到旧实例

可直接参考下面这组清理命令（在仓库根目录执行）：

```sh
pids=$(ps -eo pid=,cmd= | awk '/qemu-system-x86_64/ && !/awk/ {print $1}')
if [ -n "$pids" ]; then
    sudo kill $pids || true
fi

sudo rm -f /dev/shm/dragonos-qemu-shm.ram
rm -f ./bin/vmstate/pid ./bin/vmstate/port ./bin/vmstate/vsock_cid
rm -f /tmp/dragonos-vsock-cid-registry
```

如果 `./bin/vmstate/` 文件是 root-owned，改用：

```sh
sudo rm -f ./bin/vmstate/pid ./bin/vmstate/port ./bin/vmstate/vsock_cid
```

### 第 4 步：使用 cgroup AUTO_TEST 启动

在仓库根目录执行：

```sh
AUTO_TEST=cgroup nix run .#start-x86_64
```

这条命令的含义是：

- 启动最新构建出来的 DragonOS
- boot cmdline 包含 `AUTO_TEST=cgroup`
- 启动时由 `rcS` 自动执行 cgroup 测试
- 最终把测试结果打到串口输出

### 第 5 步：只根据明确输出判定成功或失败

期望看到：

```text
[rcS][cgroup] PASS /bin/test_cgroup_mvp_basic
[rcS][cgroup] PASS /bin/test_cgroup_memory_accounting
[rcS][cgroup] ALL TESTS PASSED
```

以下都必须视为失败：

```text
[WARN] cleanup ...
[FAIL] ...
[rcS][cgroup] TESTS FAILED
```

---

## 8. 验证标准

### 8.1 什么叫通过

这条专项流程里，“通过”必须同时满足：

1. `AUTO_TEST=cgroup` 确实进入 boot cmdline
2. `rcS` 的 cgroup 分支确实运行
3. `/bin/test_cgroup_mvp_basic` 通过
4. `/bin/test_cgroup_memory_accounting` 通过
5. 没有 cleanup warning
6. 最后出现：

```text
[rcS][cgroup] ALL TESTS PASSED
```

### 8.2 什么不算通过

以下情况都**不算通过**：

- 主要功能 PASS，但 cleanup 还有 warning
- 只看到单个测试 PASS，没有看到总 PASS
- `rcS` 分支没执行，只是系统正常启动
- 测试二进制不存在，结果根本没跑到目标测试
- 使用了旧 QEMU 实例输出冒充新结果

特别说明：

> 对 cgroup cleanup / teardown 相关问题，warning 不能忽略。warning 就表示验证还不干净。

---

## 9. 推荐检查点

每次跑完后，至少确认这些点：

### 9.1 构建层

- 最新修改是否真的参与了构建
- rootfs 是否在修改 `user/` 后重新构建
- 新测试文件是否真的被打进镜像

### 9.2 启动层

- boot cmdline 是否包含 `AUTO_TEST=cgroup`
- `rcS` 是否进入了 cgroup 分支
- QEMU 是否是 fresh 实例

### 9.3 测试层

- `test_cgroup_mvp_basic` 是否 PASS
- `test_cgroup_memory_accounting` 是否 PASS
- cleanup 是否无 warning

---

## 10. 常见坑与排查

### 坑 1：新增测试文件没被打进镜像

现象：
- guest 中提示 `/bin/test_cgroup_memory_accounting` not found

常见原因：
- 新文件没被 git 跟踪
- rootfs 没重建

排查方向：
- 确认文件存在于 `user/apps/c_unitest/`
- 确认文件已被跟踪
- 重新构建 rootfs

### 坑 2：AUTO_TEST 没真的进 boot cmdline

现象：
- 系统启动了，但没有进入 cgroup 自动测试分支

常见原因：
- `tools/qemu/default.nix` 没有正确透传运行时 `AUTO_TEST`

排查方向：
- 检查 boot cmdline 输出
- 检查 `default.nix` 中是否使用运行时 `AUTO_TEST`

### 坑 3：连接到了旧 QEMU 实例

现象：
- 输出和最新改动不一致
- 行为像旧版本
- 启动时报锁或 guest CID 冲突

常见原因：
- 旧 QEMU 没清掉
- stale shm / vmstate / vsock registry 还在

排查方向：
- 清理旧进程和运行时状态后重新启动

### 坑 4：cleanup warning 被误判为成功

现象：
- 主测试逻辑 PASS
- 但串口里仍出现 `[WARN] cleanup ...`

正确结论：
- 这不是通过
- 这表示 teardown 仍然不干净

---

## 11. 样例日志

### 11.1 成功样例

下面是这次链路跑通时的关键成功输出：

```text
[rcS][cgroup] Running /bin/test_cgroup_mvp_basic
[PASS] cgroup_mvp_basic
[rcS][cgroup] PASS /bin/test_cgroup_mvp_basic
[rcS][cgroup] Running /bin/test_cgroup_memory_accounting
[PASS] cgroup_memory_accounting
[rcS][cgroup] PASS /bin/test_cgroup_memory_accounting
[rcS][cgroup] ALL TESTS PASSED
```

只要你跑的是同一条链路，最终至少应该看到同等级别的 PASS 结论。

### 11.2 典型失败样例：cleanup 没修干净

这是这次调试中明确出现过、并且必须判为失败的日志：

```text
[rcS][cgroup] Running /bin/test_cgroup_memory_accounting
[WARN] cleanup find cleanup destination for original restore: Resource busy
[WARN] cleanup remove staging cgroup: Resource busy
[FAIL] cleanup reported warnings: Resource busy
[rcS][cgroup] FAIL /bin/test_cgroup_memory_accounting (exit=1)
[rcS][cgroup] TESTS FAILED
```

这类情况说明：

- 主测试逻辑可能已经基本工作
- 但 teardown / cleanup 还不干净
- 不能把这种结果当作“已经验证通过”

### 11.3 典型失败样例：测试二进制没进镜像

如果新测试文件没被正确打进 rootfs，常见现象会像这样：

```text
[rcS][cgroup] Running /bin/test_cgroup_memory_accounting
/bin/test_cgroup_memory_accounting: not found
[rcS][cgroup] FAIL /bin/test_cgroup_memory_accounting (exit=...)
[rcS][cgroup] TESTS FAILED
```

优先检查：

- 测试文件是否存在于 `user/apps/c_unitest/`
- 文件是否已被 git 跟踪
- rootfs 是否重新构建

### 11.4 典型失败样例：stale QEMU / vsock 状态

如果启动前没有清理旧状态，可能不是 guest 代码错，而是宿主机运行时状态冲突，例如：

```text
qemu-system-x86_64: -device vhost-vsock-pci-non-transitional,guest-cid=3: vhost-vsock: unable to set guest cid: Address already in use
```

这种情况下应先清理：

- 旧 `qemu-system-x86_64`
- `/dev/shm/dragonos-qemu-shm.ram`
- `./bin/vmstate/*`
- `/tmp/dragonos-vsock-cid-registry`

再重新启动验证。

## 12. 最小可复现命令集

如果你需要一组最小命令，按这个顺序执行：

### 情况 A：内核和用户态都改了

```sh
nix develop -c make kernel
nix run .#rootfs-x86_64
AUTO_TEST=cgroup nix run .#start-x86_64
```

### 情况 B：只改了 guest 测试或 rcS

```sh
nix run .#rootfs-x86_64
AUTO_TEST=cgroup nix run .#start-x86_64
```

前提：
- 启动前已经清理 stale QEMU 状态
- 需要时已准备好 `sudo`

---

## 13. 结论

这份专项流程当前的可复现含义是：

- **先编译**：Nix 构建 kernel / rootfs
- **再挂载启动**：QEMU 使用最新镜像启动
- **提前写好运行什么**：`rcS` 的 `AUTO_TEST=cgroup` 分支定义测试入口
- **最后检测输出**：根据串口中的 PASS / FAIL / cleanup warning 判定结果

所以答案就是：

> 是的，现在测试就是会自己通过 Nix 编译、QEMU 启动并挂载最新镜像、由 `rcS` 提前写好的 cgroup 自动测试入口去运行指定测试，然后通过串口输出做结果检测。
