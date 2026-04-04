# PRD: cgroup v2 内存控制器（memory controller）

## 1. Introduction / Overview

为 DragonOS 的 cgroup v2 补齐 memory controller，实现与 Linux 6.6 命名和核心语义尽量一致的内存控制能力。第一阶段聚焦**用户态匿名页**的记账、限制、回收与 OOM 联动，不覆盖页缓存、swap、slab 等更复杂的内存类型。

本功能要解决两个问题：

1. DragonOS 当前缺少 cgroup memory controller，无法对容器/进程组进行内存隔离与资源治理。
2. 当前除了功能缺口，还缺少一套可与 Linux 对比的 baseline 基准测试，无法量化 memory controller 的行为与性能开销。

本 PRD 目标是在完成现有测试的基础上，交付一个较完整的 memory controller 基线版本，包括 `memory.current`、`memory.max`、`memory.high`、`memory.low`、基础统计/事件接口，以及“超限时先回收、失败再 OOM kill”的行为；同时补齐 DragonOS 与 Linux 的对比基准测试。

## 2. Goals

- 提供与 Linux cgroup v2 对齐的 memory controller 基础接口命名。
- 仅针对**用户态匿名页**完成准确记账与归属。
- 支持 `memory.max`、`memory.high`、`memory.low`、`memory.current` 的基础语义。
- 当内存使用超过限制时，先尝试该 cgroup 内部回收；回收失败后触发该 cgroup 范围内的 OOM 处理。
- 提供最小但可用的 `memory.stat` / `memory.events` 类信息，满足现有测试与调试需要。
- 在实现完成后，通过现有相关测试。
- 提供 baseline 基准测试，并输出 DragonOS 与 Linux 的对比结果，便于后续性能迭代。

## 3. User Stories

### US-001: 暴露 memory controller 文件接口
**Description:** 作为内核开发者，我希望在 cgroup v2 目录下看到 Linux 风格的 memory controller 文件，这样用户态工具和测试可以按统一接口访问。

**Acceptance Criteria:**
- [ ] 在启用 memory controller 的 cgroup 目录下可见 `memory.current`。
- [ ] 在启用 memory controller 的 cgroup 目录下可见 `memory.max`。
- [ ] 在启用 memory controller 的 cgroup 目录下可见 `memory.high`。
- [ ] 在启用 memory controller 的 cgroup 目录下可见 `memory.low`。
- [ ] 在启用 memory controller 的 cgroup 目录下可见 `memory.stat`。
- [ ] 在启用 memory controller 的 cgroup 目录下可见 `memory.events` 或等价的 Linux 风格事件文件。
- [ ] 文件读写格式与 Linux 6.6 的 cgroup v2 约定一致或在 PR 中明确列出差异。
- [ ] `make kernel` 通过。

### US-002: 为匿名页建立 cgroup 记账
**Description:** 作为内核开发者，我希望用户态匿名页在分配和释放时被正确归属到对应 cgroup，这样限制和统计才有意义。

**Acceptance Criteria:**
- [ ] 用户进程分配匿名内存时，所属 cgroup 的内存使用量增加。
- [ ] 进程退出或释放匿名内存时，所属 cgroup 的内存使用量减少。
- [ ] 同一 cgroup 内多个进程的匿名页使用量可正确累计。
- [ ] 不将页缓存/文件页计入本阶段的 memory controller 使用量。
- [ ] 提供至少 1 个自动化测试覆盖“分配后增加、释放后减少”的行为。
- [ ] `make kernel` 通过。

### US-003: 实现 memory.current 读取语义
**Description:** 作为测试开发者，我希望读取 `memory.current` 能得到当前 cgroup 已记账匿名内存值，这样可以验证限制器行为。

**Acceptance Criteria:**
- [ ] 读取 `memory.current` 返回当前 cgroup 的已记账内存字节数。
- [ ] 在匿名内存增长后再次读取，值会相应增加。
- [ ] 在匿名内存释放后再次读取，值会相应减少。
- [ ] 返回值格式为纯数字文本，兼容 Linux 风格读取方式。
- [ ] 至少 1 个自动化测试验证该接口。
- [ ] `make kernel` 通过。

### US-004: 实现 memory.max 硬限制
**Description:** 作为容器运行时开发者，我希望配置 `memory.max` 后能阻止 cgroup 无限制占用内存，这样单个工作负载不会拖垮系统。

**Acceptance Criteria:**
- [ ] 可向 `memory.max` 写入数值限制。
- [ ] 可按 Linux 风格处理 `max` 关键字或在 PR 中明确不支持并给出原因。
- [ ] 当新分配会导致使用量超过 `memory.max` 时，系统先尝试该 cgroup 内部回收。
- [ ] 若回收后仍无法满足分配，则本次分配不能成功无界推进。
- [ ] 超限失败路径会更新事件统计或返回可观测结果，便于测试断言。
- [ ] 至少 1 个自动化测试验证超限后的限制行为。
- [ ] `make kernel` 通过。

### US-005: 实现 memory.high 超限回收触发
**Description:** 作为内核开发者，我希望 `memory.high` 在超过阈值时触发回收压力，而不是直接作为硬失败点，这样可以接近 Linux 的分级治理语义。

**Acceptance Criteria:**
- [ ] 可向 `memory.high` 写入数值阈值。
- [ ] 当 cgroup 使用量超过 `memory.high` 时，会触发该 cgroup 的回收路径。
- [ ] 超过 `memory.high` 不直接等价于 OOM kill。
- [ ] 相关事件或统计可体现 high 阈值被命中。
- [ ] 至少 1 个自动化测试验证“超过 high 会回收，但不立即 OOM”的行为。
- [ ] `make kernel` 通过。

### US-006: 实现 memory.low 基础保护语义
**Description:** 作为内核开发者，我希望 `memory.low` 能在回收选择时为 cgroup 提供基础保护，这样低优先级 cgroup 会先成为回收对象。

**Acceptance Criteria:**
- [ ] 可向 `memory.low` 写入数值阈值。
- [ ] 当系统或层级内需要回收时，低于 `memory.low` 的 cgroup 相比其他候选对象更少被回收。
- [ ] 若所有候选 cgroup 都低于其 low 保护值，系统仍可在必要时继续前进，不出现永久卡死。
- [ ] PR 中明确记录 DragonOS 当前阶段对 `memory.low` 的具体近似语义与 Linux 差异。
- [ ] 至少 1 个自动化测试验证 low 对回收选择的影响。
- [ ] `make kernel` 通过。

### US-007: 实现回收失败后的 cgroup OOM 处理
**Description:** 作为系统开发者，我希望在 cgroup 内回收失败时触发该 cgroup 范围内的 OOM 处理，这样超限负载会被局部治理，而不是影响全局。

**Acceptance Criteria:**
- [ ] 当分配会超过 `memory.max` 且回收失败时，触发该 cgroup 范围内的 OOM 处理逻辑。
- [ ] OOM 处理的影响范围限定在目标 cgroup 或 PR 中明确定义的最小可接受范围。
- [ ] OOM 事件可通过 `memory.events` 或等价接口观测。
- [ ] 至少 1 个自动化测试验证“先回收，失败再 OOM”的顺序。
- [ ] `make kernel` 通过。

### US-008: 输出基础统计与事件信息
**Description:** 作为调试和测试人员，我希望通过 `memory.stat` 和 `memory.events` 观察 controller 行为，这样可以定位语义和性能问题。

**Acceptance Criteria:**
- [ ] `memory.stat` 至少包含本阶段实现直接需要的匿名页/当前使用量/回收次数等键值信息。
- [ ] `memory.events` 至少包含本阶段实现直接需要的 low/high/max/oom/oom_kill 等计数信息，若某些键暂不支持需在 PR 中说明。
- [ ] 输出格式为 Linux 风格的逐行 `key value` 文本。
- [ ] 至少 1 个自动化测试验证统计/事件文件可读且字段稳定。
- [ ] `make kernel` 通过。

### US-009: 通过现有相关测试
**Description:** 作为项目维护者，我希望 memory controller 合入前能通过现有相关测试，这样新增能力不会破坏当前行为。

**Acceptance Criteria:**
- [ ] 现有 cgroup 相关测试全部通过。
- [ ] 现有与内存控制相关的单元测试/集成测试全部通过。
- [ ] 若存在 gVisor 或兼容性测试覆盖 memory controller，相关用例通过或在 PR 中列出剩余差异。
- [ ] 提交一份测试结果摘要，列出测试名称、结果、失败项（如有）与原因。

### US-010: 增加 baseline 与 Linux 对比基准测试
**Description:** 作为性能分析人员，我希望为 memory controller 增加 baseline 和 Linux 对比基准，这样可以判断 DragonOS 当前实现的功能代价和优化方向。

**Acceptance Criteria:**
- [ ] 提供可在 DragonOS 与 Linux 上运行的 memory controller 基准程序或扩展现有 cgroup benchmark。
- [ ] 至少覆盖以下场景：无 controller 的 baseline、启用 memory controller 但未命中限制、命中 `memory.high` 的回收路径、命中 `memory.max` 的失败/治理路径。
- [ ] 输出结构化结果，便于横向对比 DragonOS 与 Linux。
- [ ] 文档中明确 benchmark 的运行环境、输入参数、采样次数与输出字段定义。
- [ ] 至少产出一份 DragonOS 与 Linux 的基准对比结果样例。

## 4. Functional Requirements

- FR-1: 系统必须在 cgroup v2 中提供 Linux 风格命名的 memory controller 文件接口，至少包括 `memory.current`、`memory.max`、`memory.high`、`memory.low`、`memory.stat`、`memory.events`。
- FR-2: 系统必须对**用户态匿名页**进行 cgroup 级别的记账，并在分配、释放、退出等路径上更新统计。
- FR-3: `memory.current` 必须返回当前 cgroup 已记账匿名页占用的字节数。
- FR-4: `memory.max` 必须作为硬限制生效；当分配将超过上限时，先进入回收路径，失败后进入 OOM 治理路径。
- FR-5: `memory.high` 必须作为高水位阈值生效；超过阈值时触发回收压力，而不是直接等价于硬失败。
- FR-6: `memory.low` 必须影响回收优先级，使低于保护值的 cgroup 在回收选择中获得基础保护。
- FR-7: 当回收失败且无法满足新的匿名页分配时，系统必须触发 cgroup 范围内的 OOM 处理，而不是静默突破限制。
- FR-8: `memory.stat` 必须提供与当前实现直接相关的统计字段，格式兼容 Linux 风格的 `key value` 文本。
- FR-9: `memory.events` 必须提供与当前实现直接相关的事件计数，至少覆盖 `high`、`max`、`oom`、`oom_kill` 等语义；若存在差异必须文档化。
- FR-10: 现有相关测试必须在该功能合入前通过。
- FR-11: 系统必须提供 memory controller 的 baseline 基准测试，并支持与 Linux 对比。
- FR-12: benchmark 输出必须是结构化数据，能够比较 baseline 与 controller 开销，以及 DragonOS 与 Linux 差异。

## 5. Non-Goals (Out of Scope)

- 本阶段**不**统计或限制页缓存 / 文件页缓存。
- 本阶段**不**实现 `memory.swap.*`、swap 记账或 swap 限制。
- 本阶段**不**覆盖 slab、socket memory、hugetlb、NUMA 统计、PSI 等高级内存控制能力。
- 本阶段**不**追求完整复刻 Linux 所有 memory.stat 字段，只要求覆盖当前实现直接需要的最小字段集。
- 本阶段**不**实现完整的全局内存回收策略重构，只要求补齐与 cgroup memory controller 直接相关的最小闭环。
- 本阶段**不**包含用户态管理工具、图形界面或额外运维面板。

## 6. Design Considerations

- 用户态接口命名优先与 Linux cgroup v2 保持一致，便于已有测试、脚本和容器运行时复用。
- 对 Linux 语义的任何已知偏差都需要在 PR 和设计文档中明确写出，避免“同名不同义”。
- benchmark 结果应能直接回答两个问题：
  1. DragonOS 开启 memory controller 后比 baseline 多了多少开销？
  2. 相同 workload 下 DragonOS 与 Linux 的差距在哪里？

## 7. Technical Considerations

- 参考 Linux 6.6 的 cgroup v2 memory controller 语义进行实现和校验。
- 需要明确匿名页 charge / uncharge 的时机，例如页错误分配、扩展堆、匿名映射首次实际分配、释放与进程退出路径。
- cgroup 迁移时需定义已存在匿名页的归属规则，并在实现文档中写清是否“历史页不迁移”或采用其他策略。
- `memory.low` 的 Linux 完整语义较复杂；若第一阶段只能实现近似保护，需要在文档中明确近似规则和限制。
- OOM 处理需要避免扩大影响范围，优先保持在触发超限的 cgroup 内部闭环处理。
- 统计与事件文件必须稳定，便于自动化测试断言。
- benchmark 建议复用或扩展已有 `docs/superpowers/specs/2026-03-28-cgroup-benchmark-design.md` 的思路，新增 memory controller 相关测试项，而不是另起一套完全独立框架。

## 8. Success Metrics

- 现有 cgroup / memory 相关测试通过率达到 100%。
- 能稳定复现实验：超过 `memory.high` 时触发回收，超过 `memory.max` 且回收失败时触发 OOM 治理。
- `memory.current`、`memory.stat`、`memory.events` 可被自动化测试稳定读取并断言。
- 在同一 benchmark 输入下，DragonOS 与 Linux 都能产出结构化结果。
- benchmark 报告中能同时看到 baseline 开销、controller 开销、以及与 Linux 的差距。

## 9. Open Questions

- 是否要求第一阶段严格实现 cgroup v2 的层级聚合与父子层级联动限制，还是先保证单 cgroup / 基础层级行为正确？
- `memory.events.local` 是否需要与 `memory.events` 同阶段提供？
- `memory.min` 是否明确不做，还是后续作为 `memory.low` 完成后的下一步？
- cgroup 迁移后，已有匿名页是否保留原归属，是否需要与 Linux 完全一致？
- benchmark 最终放在现有 cgroup benchmark 程序中扩展，还是拆成独立 memory benchmark 程序？
