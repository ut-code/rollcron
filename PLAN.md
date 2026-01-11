# Refactoring Plan: Runner / Job Actor 分離

**Status: COMPLETED**

## 現状の問題

scheduler.rs に責務が混在している:

```
Scheduler Actor
├── Tick (1秒ごと cron評価)
├── try_sync_job() ← Tick をブロック
├── Concurrency 制御
├── Executor spawn
├── job_handles 管理
└── JobCompleted 処理
```

問題点:
1. **Sync が Tick をブロック** - git sync がスケジューラのイベントループを止める
2. **ConfigUpdate で job_handles 孤児化** - 削除されたジョブのタスクが残る
3. **Wait モードが無制限キュー** - バックプレッシャーなし
4. **Job Actor 不在** - Executor は単なる async task

## 提案: Runner / Job Actor 分離

```
┌─────────────────────────────────────────────────────────────────┐
│                      ROOT RUNNER                                │
│  責務: ライフサイクル管理                                        │
├─────────────────────────────────────────────────────────────────┤
│  • CLI解析                                                      │
│  • Git pull cycle (async task)                                  │
│  • Config parse → Job Actor 生成/更新/削除                      │
│  • Graceful shutdown (SIGTERM → 全 Actor に通知)                │
│  • グローバル env 解決                                          │
└──────────────────────────┬──────────────────────────────────────┘
                           │ spawn / update / kill
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│                    JOB ACTOR (per job)                          │
│  責務: 単一ジョブの完全な自律制御                                │
├─────────────────────────────────────────────────────────────────┤
│  • 自身の Tick (毎秒 cron 評価)                                 │
│  • Jitter 適用                                                  │
│  • Concurrency 制御 (状態機械)                                  │
│  • Git sync (自分のディレクトリのみ)                            │
│  • Command 実行 + timeout                                       │
│  • Retry (backoff + jitter)                                     │
│  • Log 出力・ローテーション                                      │
│  • Webhook 送信                                                 │
│  • Job 固有 env 解決                                            │
└─────────────────────────────────────────────────────────────────┘
```

## 通信プロトコル

```
                  ┌──────────────────────────────────────────────────────┐
                  │                   Runner Actor                       │
                  │                                                      │
                  │  ┌─────────────┐      ┌─────────────────────────┐   │
   SIGTERM ──────▶│  │ Signal Loop │      │ Git Poll Loop           │   │
                  │  └──────┬──────┘      │  fetch → parse config   │   │
                  │         │             └───────────┬─────────────┘   │
                  │         │                         │                  │
                  │         │    ┌────────────────────┘                  │
                  │         │    │                                       │
                  │         ▼    ▼                                       │
                  │  ┌─────────────────────────────────────────────┐    │
                  │  │              Lifecycle Manager               │    │
                  │  │  HashMap<JobId, Addr<JobActor>>             │    │
                  │  └─────────────────────────────────────────────┘    │
                  └───────────────────────┬──────────────────────────────┘
                                          │
            ┌─────────────────────────────┼─────────────────────────────┐
            │                             │                             │
            ▼                             ▼                             ▼
   ┌─────────────────┐           ┌─────────────────┐           ┌─────────────────┐
   │  Job Actor [A]  │           │  Job Actor [B]  │           │  Job Actor [C]  │
   └─────────────────┘           └─────────────────┘           └─────────────────┘
```

### メッセージ一覧

```
Runner → Job Actor:
  ┌─────────────────┬────────────────────────────────────────────────┐
  │ Update(Job)     │ config変更時。Actor は次の tick から新設定で動作 │
  │ Shutdown        │ job削除時。即座に停止                           │
  │ GracefulStop    │ SIGTERM時。現在の実行完了後に停止               │
  └─────────────────┴────────────────────────────────────────────────┘

Job Actor → Runner:
  (明示的メッセージなし - xtra の Addr で停止検知)
```

### Supervision

```
Runner の監視責務:
  - 各 Actor の Addr<JobActor> を保持
  - 予期せぬ停止を検知 → 同じ設定で再spawn
  - GracefulStop 後は addr.stopped() で完了待機
```

### ログの責務

```
Runner がログ:
  - 設定エラー (cron パース失敗、必須フィールド欠落など)
  - Job Actor の spawn/停止/再spawn
  - Git pull の結果

Job Actor がログ:
  - コマンド実行結果 (log_file へ)
  - sync 失敗、retry、timeout (stderr へ)
```

### ライフサイクルイベント

```
[起動時]
  Runner: config parse → 有効な job ごとに spawn(Job) → HashMap に登録

[git pull 後]
  Runner: 新旧 config を diff
    - 新規 job   → spawn(Job)
    - 既存 job   → send Update(Job)
    - 削除 job   → send Shutdown, HashMap から削除
    - 無効 job   → スキップ + error log

[SIGTERM]
  Runner: 全 Job Actor に GracefulStop → 完了待ち → exit
```

## 設計原則

- **Job Actor は停止しない** - コマンド失敗、sync失敗、timeout は状態遷移であり Actor の停止理由にならない
- **停止するのは Runner からの指示のみ** - Shutdown / GracefulStop メッセージ
- **ジョブは独立** - 1つのジョブの設定エラーが他のジョブに影響しない
  - 起動時: 無効なジョブはスキップ + エラーログ、有効なジョブは起動
  - 更新時: 無効なジョブはスキップ + エラーログ、有効なジョブは更新

## Job Actor 状態遷移

```
            ┌────────┐
   ─────────│  Idle  │◀────────────────────────────────────┐
            └───┬────┘                                     │
                │ [cron due]                               │
                ▼                                          │
            ┌────────┐                                     │
            │Syncing │── fail ──▶ [retry next tick] ──────┤
            └───┬────┘                                     │
                │ [ok]                                     │
                ▼                                          │
            ┌────────┐                                     │
            │Running │── success ─────────────────────────▶│
            └───┬────┘                                     │
                │ [fail + retry left]                      │
                ▼                                          │
            ┌────────┐                                     │
            │Backoff │── delay ──▶ Running ───────────────▶│
            └────────┘
```

## ファイル分割案

```
src/
├── main.rs               # CLI, bootstrap, signal handling
├── actor/
│   ├── runner/           # Runner Actor
│   │   ├── mod.rs        # Actor 定義、メッセージ
│   │   ├── git_poll.rs   # git fetch/reset ループ
│   │   └── lifecycle.rs  # Job Actor の生成/削除
│   └── job/              # Job Actor
│       ├── mod.rs        # Actor 定義、状態機械
│       ├── tick.rs       # cron評価、jitter
│       ├── executor.rs   # command実行、retry、timeout
│       └── logger.rs     # log出力、rotation
├── config.rs             # (変更なし)
├── git.rs                # (変更なし)
└── env.rs                # (変更なし)
```

各 actor ディレクトリを見ればその actor の責務が完結する。

## 実装順序

1. actor/job/mod.rs - Job Actor の骨格
2. actor/job/* - Scheduler から job 単位ロジックを抽出
3. actor/runner/mod.rs - Runner Actor の骨格
4. actor/runner/lifecycle.rs - Job Actor の生成/更新/削除
5. actor/runner/git_poll.rs - git fetch ループを移動
6. main.rs - Runner Actor を起動するだけに簡素化
