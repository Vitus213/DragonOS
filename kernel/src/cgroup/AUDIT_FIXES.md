## cgroup v2 审计修复落地说明

本轮收口提交：`6ac0c6eb fix(cgroup): 收口审计发现的 v2 语义缺陷`。

### 已修复

- memcg 内核堆/页表分配走 `allocate_unaccounted`，不把用户 `memory.max` 拒绝传播为内核不可失败分配 panic。
- `memory.max` 拒绝保留强 CSS 引用，fault OOM 继续走 cgroup-scoped victim 选择；`max_exceeded_now` 使用事务拒绝后的 `usage >= max` 语义。
- freezer 父请求继承、SELF-only `cgroup.freeze` 读取、空 cgroup `frozen`、冻结期唤醒保存、跨 cgroup 迁移解冻。
- cgroup v2 控制器列表不再公布 freezer；`memory.min/low` 默认读取 `0`；`cpu.max` finite quota 校验 `1ms <= quota <= period`。
- `make kernel ARCH=x86_64` 通过（最后一次构建：零 error，保留既有 warnings）。

### 未执行

Darwin arm64 无 KVM，TCG guest smoke 启动不可接受；此前连续观察 10 分钟无串口输出，因此本轮不重复启动。剩余边界见 `CONTROLLER_FRAMEWORK.md`。
