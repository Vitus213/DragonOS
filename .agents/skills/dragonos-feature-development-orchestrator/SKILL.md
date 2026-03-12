---
name: dragonos-feature-development-orchestrator
description: DragonOS 新功能开发的总控技能。负责编排六阶段闭环流程：1) dragonos-dragonos-code-research，2) dragonos-asterinas-code-research，3) dragonos-feature-solution-design，4) dragonos-main-branch-code-review，5) bug-hunter，6) bug-fix。阶段 5 与阶段 6 形成循环，直到 bug-hunter 报告无阻塞缺陷后结束。当用户提出“实现xx功能”“新增xx内核能力”“修补xx测例”“修复xx测试失败”并希望端到端推进调研、实现、评审时使用。
---

# DragonOS 功能开发总控

## 目标

统一编排从调研到实现再到审查的完整流程，保证每一步输入充分、输出可复用。

## 子技能编排顺序

1. `dragonos-dragonos-code-research`
2. `dragonos-asterinas-code-research`
3. `dragonos-feature-solution-design`
4. `dragonos-main-branch-code-review`
5. `bug-hunter`
6. `bug-fix`（总控内执行修补，不依赖独立技能文件）

## 子技能路径

优先使用总控目录内的子技能副本，路径如下。

相对路径（相对于本技能目录）：

1. `subskills/dragonos-dragonos-code-research/SKILL.md`
2. `subskills/dragonos-asterinas-code-research/SKILL.md`
3. `subskills/dragonos-feature-solution-design/SKILL.md`
4. `subskills/dragonos-main-branch-code-review/SKILL.md`
5. `bug-hunter`（按环境中已安装的同名 skill 调用）

绝对路径（仓库当前约定）：

1. `/home/vitus/DragonOS/.agents/skills/dragonos-feature-development-orchestrator/subskills/dragonos-dragonos-code-research/SKILL.md`
2. `/home/vitus/DragonOS/.agents/skills/dragonos-feature-development-orchestrator/subskills/dragonos-asterinas-code-research/SKILL.md`
3. `/home/vitus/DragonOS/.agents/skills/dragonos-feature-development-orchestrator/subskills/dragonos-feature-solution-design/SKILL.md`
4. `/home/vitus/DragonOS/.agents/skills/dragonos-feature-development-orchestrator/subskills/dragonos-main-branch-code-review/SKILL.md`
5. `bug-hunter`（例如 `/home/vitus/DragonOS/.agents/skills/bug-hunter/SKILL.md`，以实际安装路径为准）

## 阶段闸门

- Gate A（进入阶段 2 前）：DragonOS 调研已包含调用链、关键结构、已实现/未实现边界。
- Gate B（进入阶段 3 前）：Asterinas 参考语义基线已覆盖返回值/errno/边界与并发语义。
- Gate C（进入阶段 4 前）：实现已完成，且有测试结果与变更说明。
- Gate D（进入阶段 5 前）：代码评审阻塞项已处理，当前分支可进入自动缺陷扫描。
- Gate E（进入阶段 6 前）：已拿到 bug-hunter 结构化报告（含严重级别与 file:line 证据）。

若任一 Gate 未满足：先补齐缺失信息，不跳步。

## 调用规则

- 当用户表达“实现xx功能”“修补xx测例”“修复xx测试失败”时，必须按 1 -> 2 -> 3 -> 4 -> 5 -> 6 顺序执行。
- 默认不允许跳过阶段；仅当用户明确指定跳过某阶段时才可跳过。
- 每阶段结束都要输出该阶段结果摘要，再进入下一阶段。
- 阶段 5 与阶段 6 必须循环执行：`5(找 bug) -> 6(修 bug) -> 5(复扫)`，直到阶段 5 报告“无阻塞缺陷（Critical/Major=0）”。
- 每一轮循环必须记录“已修复项 / 未修复项 / 剩余风险”，并附 file:line 证据。

## 阶段 6（bug-fix）执行要求

1. 仅修复阶段 5 报告中的缺陷，不引入无关重构。
2. 每个缺陷修复后需补充最小验证（至少构建或对应单测）。
3. 若某缺陷暂不修复，必须给出阻塞原因与后续计划。
4. 修复完成后立即回到阶段 5 复扫。

## 构建与环境规则

- 若仓库根目录存在 `flake.nix`，构建/测试命令必须优先在 `nix develop` 环境内执行。
- 推荐执行形式：`nix develop -c <command>`。
- 仅当 `nix develop` 不可用或失败时，才允许回退到宿主环境，并在报告中注明原因。
- 在本仓库中，优先使用：`nix develop -c make kernel`。

## 总控输出格式

```markdown
## 总控进度
- 阶段 1：完成/未完成（缺失项）
- 阶段 2：完成/未完成（缺失项）
- 阶段 3：完成/未完成（缺失项）
- 阶段 4：完成/未完成（缺失项）
- 阶段 5：完成/未完成（本轮 bug-hunter 报告摘要）
- 阶段 6：完成/未完成（本轮修复摘要）

## 闭环轮次
- 第 N 轮：阶段 5 -> 阶段 6 -> 阶段 5
- 退出条件：Critical/Major 缺陷为 0
- 当前剩余：...

## 当前结论
- ...

## 完整交付报告
### 1) 新增文件
| 文件路径 | 用途 | 所属阶段 |
|---|---|---|

### 2) 实现功能
| 功能点 | 实现说明 | 关键文件 | 验证结果 |
|---|---|---|---|

### 3) 修复 Bug
| Bug ID | 问题摘要 | 修复方式 | 影响文件 | 验证结果 |
|---|---|---|---|---|

## 下一步
- 进入下一阶段所需输入
```

## 规则

- 不得跳过 Asterinas 参考语义对齐。
- 不得以 workaround 替代根因修复。
- 必须保留 `file:line` 级证据链。
- bug-hunter 报告中的 Critical 必须全部修复后方可结束流程。
- 流程结束时必须输出“完整交付报告”，且至少包含：新增文件、实现功能、修复 Bug 三部分。
- 构建与测试结果必须注明是否在 `nix develop` 环境中执行。
