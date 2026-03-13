## Bug Hunter Report

- Threshold: `0.6`
- Accepted findings: `4`
- Rejected findings: `0`
- Disputed findings: `0`

| 缺陷编号 | 位置 | 类型 | 严重级别 | 描述 | 建议修复 | 共识强度 |
|---|---|---|---|---|---|---|
| BUG-001 | .agents/skills/bug-hunter/scripts/debate_picker.py:25 | logic | major | debate candidate score uses plain confidence average, not Stage4 weighted score with fix_code penalty, so borderline classification can drift from final verdict threshold. | Use the same scoring function as weighted_vote.py (agent weight + fix_code penalty) to compute borderline/dispute score. | 8.8/10 |
| BUG-002 | .agents/skills/bug-hunter/scripts/debate_picker.py:25 | logic | major | Stage3 debate selection is based on unweighted confidence mean, which is inconsistent with consensus score used by acceptance gate and may surface non-borderline buckets as disputed. | Import a shared score helper and evaluate debate interval against the same score scale used for acceptance decisions. | 8.4/10 |
| BUG-003 | .agents/skills/bug-hunter/scripts/render_report.py:56 | logic | minor | accepted findings sorting misses evidence_count tie-breaker required by report spec, causing nondeterministic order for equal severity and score. | Extend sorting key with -int(item.get('evidence_count', 0)) after severity and score. | 7.6/10 |
| BUG-004 | .agents/skills/bug-hunter/scripts/render_report.py:56 | logic | minor | report ordering does not apply the documented third key evidence_count when severity and score tie. | Sort accepted by (severity_rank, -score, -evidence_count). | 7.4/10 |

## Developer TODO

- [ ] `BUG-001` `major` `.agents/skills/bug-hunter/scripts/debate_picker.py:25` owner=`Diverse Reviewer C`: debate candidate score uses plain confidence average, not Stage4 weighted score with fix_code penalty, so borderline classification can drift from final verdict threshold. | 修复建议: Use the same scoring function as weighted_vote.py (agent weight + fix_code penalty) to compute borderline/dispute score.
- [ ] `BUG-002` `major` `.agents/skills/bug-hunter/scripts/debate_picker.py:25` owner=`Performance Analyst`: Stage3 debate selection is based on unweighted confidence mean, which is inconsistent with consensus score used by acceptance gate and may surface non-borderline buckets as disputed. | 修复建议: Import a shared score helper and evaluate debate interval against the same score scale used for acceptance decisions.
- [ ] `BUG-003` `minor` `.agents/skills/bug-hunter/scripts/render_report.py:56` owner=`Diverse Reviewer A`: accepted findings sorting misses evidence_count tie-breaker required by report spec, causing nondeterministic order for equal severity and score. | 修复建议: Extend sorting key with -int(item.get('evidence_count', 0)) after severity and score.
- [ ] `BUG-004` `minor` `.agents/skills/bug-hunter/scripts/render_report.py:56` owner=`Diverse Reviewer D`: report ordering does not apply the documented third key evidence_count when severity and score tie. | 修复建议: Sort accepted by (severity_rank, -score, -evidence_count).

## Disputed Findings

| 缺陷编号 | 位置 | 争议原因 | 分数 |
|---|---|---|---|

## Rejected Findings

| 缺陷编号 | 位置 | 类型 | 严重级别 | 分数 |
|---|---|---|---|---|
