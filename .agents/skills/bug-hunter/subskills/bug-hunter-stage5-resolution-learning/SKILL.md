---
name: bug-hunter-stage5-resolution-learning
description: bug-hunter 阶段 5 技能。负责跟踪评审建议被开发者接受的比例（Resolution Rate），并沉淀历史反馈用于后续动态调权。
---

# Stage 5 闭环学习

## 步骤

1. 收集本轮通过阈值的问题 ID。
2. 对比后续提交中是否出现对应修复痕迹（人工或脚本标注）。
3. 运行 `scripts/update_resolution_history.py` 更新历史文件。
4. 输出分辨率指标与下轮权重建议。

## 指标

- Resolution Rate = 已采纳建议 / 总建议
- False Positive Rate = 被拒绝建议 / 总建议

## 产物

- `artifacts/review_history.json`
- `artifacts/weight_suggestion.json`
