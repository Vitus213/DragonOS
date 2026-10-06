# cgroup v2 审计修复变更说明

- memcg: 内核堆/页表分配通过 `allocate_unaccounted` 绕过用户 memory.max 计费；memory.max 拒绝保留强 CSS 引用，scoped OOM 使用 `usage >= max`；riscv/x86 计费路径保持一致。
- freezer: 修复父冻结继承、SELF-only `cgroup.freeze` 读取、空 cgroup frozen 状态、冻结期唤醒保存与跨 cgroup 迁移解冻；v2 `cgroup.controllers` 不再公布 freezer。
- 控制器: memory.min/low 默认读 0，cpu.max quota 边界校验收紧，保留 io.weight/cpuset.mems 的已知实现边界。
- 验证: `make kernel ARCH=x86_64` 通过；Darwin arm64 TCG guest smoke 仍因启动时间不可接受未执行。
