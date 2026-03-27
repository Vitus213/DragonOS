# Cgroup Benchmark 设计文档

## 概述

为 DragonOS cgroup v2 实现设计一个性能基准测试工具，用于量化 cgroup 操作的开销，并可与 Linux 对比。

## 目标

1. 测量 cgroup 文件系统操作延迟
2. 测量进程迁移开销
3. 测量 pids 控制器在 fork 时的额外开销
4. 输出结构化数据，便于 DragonOS 与 Linux 对比

## 技术选型

- **语言**: C
- **运行环境**: 同时支持 DragonOS 和 Linux
- **输出格式**: JSON

## 测试项

### 1. 文件系统操作

| 测试名 | 描述 | 测量方法 |
|--------|------|----------|
| `mkdir` | 创建 cgroup 目录 | clock_gettime 测量 mkdir() 耗时 |
| `rmdir` | 删除 cgroup 目录 | clock_gettime 测量 rmdir() 耗时 |
| `read_procs` | 读取 cgroup.procs | clock_gettime 测量 read() 耗时 |
| `read_controllers` | 读取 cgroup.controllers | clock_gettime 测量 read() 耗时 |
| `read_subtree` | 读取 cgroup.subtree_control | clock_gettime 测量 read() 耗时 |
| `write_subtree` | 写入 cgroup.subtree_control | clock_gettime 测量 write() 耗时 |

### 2. 进程迁移

| 测试名 | 描述 | 测量方法 |
|--------|------|----------|
| `migrate_in` | 进程移入 cgroup | 测量写入 cgroup.procs 的耗时 |
| `migrate_out` | 进程移出 cgroup | 测量写回根 cgroup 的耗时 |

### 3. pids 控制器

| 测试名 | 描述 | 测量方法 |
|--------|------|----------|
| `fork_baseline` | 无限制时 fork | 测量 fork+wait 的基准耗时 |
| `fork_with_pids` | 有 pids.max 时 fork | 设置 pids.max 后测量 fork+wait 耗时 |
| `read_pids_current` | 读取 pids.current | clock_gettime 测量 read() 耗时 |

## 输出格式

```json
{
  "system": "DragonOS",
  "kernel_version": "x.y.z",
  "iterations": 1000,
  "results": [
    {
      "test": "mkdir",
      "min_us": 15,
      "avg_us": 23,
      "max_us": 45,
      "p50_us": 22,
      "p99_us": 38
    }
  ]
}
```

## 命令行接口

```
Usage: cgroup_bench [OPTIONS]

Options:
  -i, --iterations N   每个测试的迭代次数 (默认: 1000)
  -t, --tests LIST     只运行指定测试，逗号分隔
  -o, --output FILE    输出到文件 (默认: stdout)
  -h, --help           显示帮助
```

## 文件位置

```
user/apps/c_unitest/cgroup_bench.c
```

## 实现要点

1. **时间测量**: 使用 `clock_gettime(CLOCK_MONOTONIC, ...)` 获取纳秒级精度
2. **统计计算**: 计算 min/max/avg/p50/p99
3. **跨平台兼容**: 使用 POSIX 标准接口，确保在 DragonOS 和 Linux 都能编译运行
4. **清理机制**: 测试完成后清理创建的 cgroup，避免残留
5. **权限检查**: 运行前检查是否有 cgroup 操作权限

## 测试流程

```
1. 检查 /sys/fs/cgroup 是否存在且可访问
2. 创建临时测试 cgroup: /sys/fs/cgroup/cgroup_bench_tmp
3. 运行各项测试，记录时间
4. 清理临时 cgroup
5. 输出 JSON 结果
```
