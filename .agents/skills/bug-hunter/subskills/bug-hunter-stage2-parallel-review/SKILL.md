---
name: bug-hunter-stage2-parallel-review
description: bug-hunter 阶段 2 技能。负责将随机化后的 diff 按 persona 矩阵分发给 7-8 个子智能体并行评审，并收集统一 JSON 结果。
---

# Stage 2 并行评审

## 角色矩阵

- Security Sentinel
- Concurrency Engineer
- Performance Analyst
- Diverse Reviewer A
- Diverse Reviewer B
- Diverse Reviewer C
- Diverse Reviewer D
- Diverse Reviewer E（可选）

## 步骤

1. 读取 Stage 1 的 `shuffled_passes.json`。
2. 为每个子智能体绑定不同轮次输入与角色提示词。
3. 在单条 assistant 消息中并行启动全部 Task。
4. 要求输出统一 JSON：`{file, line, type, severity, description, fix_code, confidence}`。
5. 收集并合并为 `artifacts/raw_findings.json`。

## 约束

- 每个发现必须提供 `file:line`。
- 置信度范围限定在 `[0, 1]`。
- 纯风格建议直接过滤。
