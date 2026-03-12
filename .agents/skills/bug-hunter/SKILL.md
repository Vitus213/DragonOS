---
name: bug-hunter
description: 分布式多智能体缺陷检测总控技能。基于输入随机化、角色化并行评审、语义桶化、加权共识与裁决复核输出高信噪比代码评审报告。用于大规模 PR、复杂逻辑变更、安全敏感改动或单智能体评审召回率不足的场景。
---

# Bug Hunter 总控

## 目标

构建一个可复用的多阶段代码评审流水线：

1. 随机化输入，缓解位置偏差。
2. 并行化子智能体评审，提升召回率。
3. 聚合与去重，压缩重复告警。
4. 通过加权投票和裁决复核，降低误报。
5. 记录分辨率反馈，动态优化后续轮次。

## 目录结构

- `subskills/bug-hunter-stage1-input-randomization/SKILL.md`
- `subskills/bug-hunter-stage2-parallel-review/SKILL.md`
- `subskills/bug-hunter-stage3-evidence-fusion/SKILL.md`
- `subskills/bug-hunter-stage4-consensus-judge/SKILL.md`
- `subskills/bug-hunter-stage5-resolution-learning/SKILL.md`
- `subskills/bug-hunter-stage6-sandbox-safety/SKILL.md`
- `scripts/shuffle_diff.py`
- `scripts/redact_sensitive.py`
- `scripts/semantic_bucket.py`
- `scripts/weighted_vote.py`
- `scripts/debate_picker.py`
- `scripts/render_report.py`
- `scripts/update_resolution_history.py`
- `scripts/run_pipeline.py`

## 执行顺序

1. **Stage 1 输入处理**：提取 diff，脱敏，按文件/块级生成 N 轮随机输入。
2. **Stage 2 并行评审**：按 persona 矩阵并发发起 7-8 个子智能体任务。
3. **Stage 3 证据融合**：将 JSON 发现项做语义去重与冲突标记。
4. **Stage 4 共识裁决**：按权重计算共识分，筛选过阈值问题并格式化输出。
5. **Stage 5 闭环学习**：记录建议被接受/拒绝情况，更新人格权重参考。
6. **Stage 6 安全隔离**：确保评审执行在只读工作树与脱敏上下文中完成。

## 标准输入输出

- 子智能体输出统一 JSON schema：

```json
[
  {
    "file": "kernel/src/foo.rs",
    "line": 42,
    "type": "security|concurrency|performance|logic",
    "severity": "critical|major|minor",
    "description": "问题描述",
    "fix_code": "建议修复代码",
    "confidence": 0.0
  }
]
```

- 总控最终输出包含：
  - 通过阈值的问题表格（按严重度降序）
  - 共识强度与证据数
  - 边界争议项（需人工复核）

## 快速执行

当已有子智能体原始发现时，可直接运行：

```bash
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --raw-findings artifacts/raw_findings.json \
  --out-dir artifacts
```

当需要从 diff 开始，可先生成随机化输入：

```bash
git diff main...HEAD > /tmp/current.diff
python3 .agents/skills/bug-hunter/scripts/run_pipeline.py \
  --diff-file /tmp/current.diff \
  --raw-findings artifacts/raw_findings.json \
  --out-dir artifacts
```

说明：Stage 2 并行子智能体评审在当前环境中由外部编排器负责，`run_pipeline.py` 负责 Stage 1/3/4 的可复用自动化与产物落盘。

## 阈值建议

- 默认投票阈值：`0.60`
- 语义合并阈值：`0.88`
- 辩论触发区间：`[0.50, 0.60)`

## 规则

- 不允许跳过 Stage 3 和 Stage 4。
- 无 `fix_code` 的发现项默认降权。
- 不报告纯格式问题或命名偏好。
- 结论必须可回溯到 `file:line`。
