---
name: dragonos-asterinas-code-research
description: 专用于调研 Asterinas 参考实现的技能。面向 DragonOS 新功能开发或缺陷修补，提取 Asterinas 在目标功能上的可观测语义（返回值、errno、边界条件、并发语义）与关键实现路径，形成可直接对齐的参考基线。当需要“以 Asterinas 行为为参考”时使用。
---

# Asterinas 参考调研

## 目标

建立目标功能在 Asterinas 下的行为基线，明确 DragonOS 必须对齐的外部语义与可自主设计的内部实现点。

## 前提

- Asterinas 源码可在当前目录访问（例如 `./asterinas` 或用户指定路径）。
- 若路径不同，先定位真实路径再继续。

## 工作步骤

1. 定位 Asterinas 对应实现入口（如 syscall 入口、`fs/`、`kernel/`、`process/`）。
2. 提取语义规则：参数校验、状态转换、返回值与 errno、并发与锁顺序。
3. 提取关键数据结构与生命周期管理点。
4. 用最小代码片段佐证，记录 `file:line`。

## 输出格式

```markdown
## Asterinas 语义基线

### 1) 关键语义
| 场景 | Asterinas 期望行为 | 返回值/errno | 证据 |
|---|---|---|---|

### 2) 并发与资源管理
| 主题 | Asterinas 做法 | 对 DragonOS 的约束 | 证据 |
|---|---|---|---|

### 3) 对齐建议
- 必须严格对齐的语义
- 可按 DragonOS 架构简化的内部实现
```

## 质量要求

- 仅引用 Asterinas 相关证据。
- 优先描述外部可观测行为，不陷入无关实现细节。
- 若存在多路径实现，明确各路径适用条件。
