//! `umsp usage` — aggregate the JSONL ledger.

use std::path::Path;

use serde_json::{Value, json};

use crate::cli::{PruneArgs, UsageCmd, UsageFilterArgs};
use crate::error::Result;
use crate::usage::{Aggregate, LogStore, RequestRecord, aggregate};
use crate::util;

pub fn run(path: &Path, cmd: UsageCmd, json: bool) -> Result<i32> {
    let cfg = crate::commands::load_config(path)?;
    let store = LogStore::new(&cfg.server.state_path());
    match cmd {
        UsageCmd::Summary(args) => summary(&store, args, json),
        UsageCmd::ByClient(args) => grouped(&store, args, GroupBy::Client, json),
        UsageCmd::ByModel(args) => grouped(&store, args, GroupBy::Model, json),
        UsageCmd::ByProtocol(args) => grouped(&store, args, GroupBy::Protocol, json),
        UsageCmd::Prune(args) => prune(&store, args, json),
    }
}

fn filter_records(records: Vec<RequestRecord>, args: &UsageFilterArgs) -> Vec<RequestRecord> {
    records
        .into_iter()
        .filter(|r| args.client.as_ref().is_none_or(|c| &r.client == c))
        .filter(|r| args.model.as_ref().is_none_or(|m| &r.requested_model == m))
        .filter(|r| args.provider.as_ref().is_none_or(|p| &r.provider == p))
        .filter(|r| args.protocol.as_ref().is_none_or(|p| &r.protocol == p))
        .collect()
}

fn filtered(
    store: &LogStore,
    args: &UsageFilterArgs,
) -> Result<(Vec<RequestRecord>, String, String)> {
    let since = util::parse_since(&args.since)?;
    let records = filter_records(store.read_since(since), args);
    let since_s = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let until_s = util::now_rfc3339();
    Ok((records, since_s, until_s))
}

fn summary(store: &LogStore, args: UsageFilterArgs, json: bool) -> Result<i32> {
    let (records, since, until) = filtered(store, &args)?;
    let agg = aggregate(&records);
    let value = json!({
        // `ok` is the command-status boolean, same as every other --json command;
        // the successful-request count is `ok_requests` so the two never collide.
        "ok": true,
        "since": since,
        "until": until,
        "filters": {
            "client": args.client,
            "model": args.model,
            "provider": args.provider,
            "protocol": args.protocol,
        },
        "requests": agg.requests,
        "ok_requests": agg.ok,
        "errors": agg.errors,
        "prompt_tokens": agg.prompt_tokens,
        "completion_tokens": agg.completion_tokens,
        "cached_tokens": agg.cached_tokens,
        "reasoning_tokens": agg.reasoning_tokens,
        "total_tokens": agg.total_tokens,
        "bytes_in": agg.bytes_in,
        "bytes_out": agg.bytes_out,
        "avg_duration_ms": agg.avg_duration_ms(),
        "by_status": agg.by_status,
        "by_protocol": agg.by_protocol,
        "by_client": agg.by_client,
        "by_model": agg.by_model,
        "by_provider": agg.by_provider,
    });
    crate::commands::emit(json, value, || {
        format!(
            "{} requests ({} ok, {} errors), tokens in/out {}/{} (cached {}, reasoning {}), avg {} ms",
            agg.requests,
            agg.ok,
            agg.errors,
            agg.prompt_tokens,
            agg.completion_tokens,
            agg.cached_tokens,
            agg.reasoning_tokens,
            agg.avg_duration_ms().round()
        )
    })?;
    Ok(0)
}

enum GroupBy {
    Client,
    Model,
    Protocol,
}

fn grouped(store: &LogStore, args: UsageFilterArgs, by: GroupBy, json: bool) -> Result<i32> {
    let (records, since, until) = filtered(store, &args)?;
    let mut keys: Vec<String> = records
        .iter()
        .map(|r| match by {
            GroupBy::Client => r.client.clone(),
            GroupBy::Model => r.requested_model.clone(),
            GroupBy::Protocol => r.protocol.clone(),
        })
        .collect();
    keys.sort();
    keys.dedup();

    let mut rows: Vec<(String, Aggregate)> = Vec::new();
    for key in keys {
        let subset: Vec<RequestRecord> = records
            .iter()
            .filter(|r| match by {
                GroupBy::Client => r.client == key,
                GroupBy::Model => r.requested_model == key,
                GroupBy::Protocol => r.protocol == key,
            })
            .cloned()
            .collect();
        rows.push((key, aggregate(&subset)));
    }

    let dim = match by {
        GroupBy::Client => "client",
        GroupBy::Model => "model",
        GroupBy::Protocol => "protocol",
    };
    let table: Vec<Value> = rows
        .iter()
        .map(|(key, agg)| {
            json!({
                dim: key,
                "requests": agg.requests,
                "errors": agg.errors,
                "prompt_tokens": agg.prompt_tokens,
                "completion_tokens": agg.completion_tokens,
                "cached_tokens": agg.cached_tokens,
                "reasoning_tokens": agg.reasoning_tokens,
                "total_tokens": agg.total_tokens,
                "avg_duration_ms": agg.avg_duration_ms(),
            })
        })
        .collect();

    let value = json!({
        "ok": true,
        "since": since,
        "until": until,
        "group_by": dim,
        "rows": table,
    });
    crate::commands::emit(json, value, || {
        rows.iter()
            .map(|(key, agg)| {
                format!(
                    "{:<32} {:>6} req  {:>10} in  {:>10} out  {:>10} total  avg {:.0} ms",
                    key,
                    agg.requests,
                    agg.prompt_tokens,
                    agg.completion_tokens,
                    agg.total_tokens,
                    agg.avg_duration_ms()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    Ok(0)
}

fn prune_cutoff(args: &PruneArgs) -> Result<chrono::DateTime<chrono::Utc>> {
    // `--days N` is the same cutoff as `--before Nd`. Only whole files with a
    // date stem strictly older than the cutoff date are removed, so today's
    // ledger is never touched.
    match args.days {
        Some(days) => Ok(chrono::Utc::now() - chrono::TimeDelta::days(i64::from(days))),
        None => util::parse_since(&args.before),
    }
}

fn prune(store: &LogStore, args: PruneArgs, json: bool) -> Result<i32> {
    let cutoff = prune_cutoff(&args)?;
    let removed = store
        .prune_before(cutoff)
        .map_err(|e| crate::error::UmspError::other("PRUNE", format!("prune failed: {e}")))?;
    let empty: Vec<String> = Vec::new();
    let paths: Vec<String> = if removed.is_empty() {
        empty
    } else {
        removed.iter().map(|p| p.display().to_string()).collect()
    };
    let value = json!({
        "ok": true,
        "before": cutoff.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "keep_days": args.days,
        "removed": paths,
        "removed_count": paths.len(),
    });
    crate::commands::emit(json, value, || {
        format!("removed {} ledger file(s)", paths.len())
    })?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{LogStore, RequestRecord, aggregate};

    fn record(
        client: &str,
        provider: &str,
        protocol: &str,
        model: &str,
        status: u16,
        tokens: (u64, u64),
    ) -> RequestRecord {
        RequestRecord {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            request_id: "req".into(),
            client: client.into(),
            protocol: protocol.into(),
            requested_model: model.into(),
            provider: provider.into(),
            upstream_model: "upstream".into(),
            stream: false,
            status,
            upstream_status: Some(status),
            upstream_body: None,
            prompt_tokens: tokens.0,
            completion_tokens: tokens.1,
            cached_tokens: 0,
            reasoning_tokens: 0,
            ttft_ms: None,
            duration_ms: 10,
            bytes_in: 1,
            bytes_out: 1,
            error_code: None,
            upstream_request_id: None,
            client_disconnected: false,
            fallbacks: 0,
            quirks: vec![],
        }
    }

    fn args() -> UsageFilterArgs {
        UsageFilterArgs {
            since: "24h".into(),
            client: None,
            model: None,
            provider: None,
            protocol: None,
        }
    }

    fn fixture() -> Vec<RequestRecord> {
        vec![
            record("planner", "od", "chat", "od/m", 200, (10, 5)),
            record("planner", "deepseek", "chat", "deepseek/m", 502, (1, 0)),
            record("wallpaper", "od", "responses", "od/m", 200, (7, 3)),
        ]
    }

    #[test]
    fn filters_narrow_by_client_model_provider_and_protocol() {
        let mut a = args();
        a.provider = Some("od".into());
        let kept = filter_records(fixture(), &a);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|r| r.provider == "od"));

        let mut a = args();
        a.client = Some("wallpaper".into());
        a.protocol = Some("responses".into());
        assert_eq!(filter_records(fixture(), &a).len(), 1);

        let mut a = args();
        a.model = Some("deepseek/m".into());
        assert_eq!(filter_records(fixture(), &a).len(), 1);

        // All filters together are an AND, not an OR.
        let mut a = args();
        a.provider = Some("od".into());
        a.client = Some("planner".into());
        a.model = Some("deepseek/m".into());
        assert!(filter_records(fixture(), &a).is_empty());
    }

    #[test]
    fn aggregate_buckets_and_tokens_match_the_record_count() {
        let records = fixture();
        let mut by_client = args();
        by_client.client = Some("planner".into());
        let mut by_provider = args();
        by_provider.provider = Some("od".into());
        let mut by_chat = args();
        by_chat.protocol = Some("chat".into());
        let mut by_responses = args();
        by_responses.protocol = Some("responses".into());
        let mut by_model = args();
        by_model.model = Some("deepseek/m".into());
        let cases = vec![
            args(),
            by_client,
            by_provider,
            by_chat,
            by_responses,
            by_model,
        ];
        for a in cases {
            let kept = filter_records(records.clone(), &a);
            let agg = aggregate(&kept);
            let sum = |map: &std::collections::BTreeMap<String, u64>| -> u64 {
                map.values().copied().sum()
            };
            assert_eq!(sum(&agg.by_status), agg.requests);
            assert_eq!(sum(&agg.by_protocol), agg.requests);
            assert_eq!(sum(&agg.by_model), agg.requests);
            assert_eq!(sum(&agg.by_client), agg.requests);
            assert_eq!(sum(&agg.by_provider), agg.requests);
            assert_eq!(agg.requests, kept.len() as u64);
            assert_eq!(
                agg.total_tokens,
                kept.iter().map(|r| r.total_tokens()).sum::<u64>()
            );
        }
    }

    #[test]
    fn prune_days_never_removes_the_current_file() {
        let dir = std::env::temp_dir().join(format!(
            "umsp-prune-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = LogStore::new(&dir);
        std::fs::create_dir_all(store.dir()).unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let ancient = (chrono::Local::now() - chrono::TimeDelta::days(40))
            .format("%Y-%m-%d")
            .to_string();
        std::fs::write(store.dir().join(format!("{today}.jsonl")), "{}\n").unwrap();
        std::fs::write(store.dir().join(format!("{ancient}.jsonl")), "{}\n").unwrap();

        let cutoff = prune_cutoff(&PruneArgs {
            before: "30d".into(),
            days: Some(30),
        })
        .unwrap();
        let removed = store.prune_before(cutoff).unwrap();
        assert_eq!(removed.len(), 1, "only the 40-day-old file goes");
        assert!(
            store.dir().join(format!("{today}.jsonl")).exists(),
            "today's ledger must survive prune"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_cutoff_accepts_before_and_days() {
        let by_days = prune_cutoff(&PruneArgs {
            before: "30d".into(),
            days: Some(7),
        })
        .unwrap();
        let by_before = prune_cutoff(&PruneArgs {
            before: "7d".into(),
            days: None,
        })
        .unwrap();
        let delta = (by_days - by_before).num_seconds().abs();
        assert!(delta < 5, "7 days either way, got {delta}s apart");
    }
}
