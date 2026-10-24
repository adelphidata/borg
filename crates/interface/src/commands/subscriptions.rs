//! `borg subscriptions {list|pause|resume|events|test}`: the terminal view of
//! the subscriptions the app and the `subscribe` tool create.

use anyhow::Result;
use clap::Subcommand;
use protocols::admin::{Subscription, SubscriptionEvent};

use super::{format_ts, listed_id, truncate_str, TS_FMT};
use crate::daemon_client::{connect_or_spawn, DaemonClient};

/// `borg subscriptions <subcommand>` action.
#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubscriptionsAction {
    /// List subscriptions with their status
    List,
    /// Stop a subscription from waking Borg
    Pause {
        /// Subscription id (or prefix)
        id: String,
    },
    /// Let a paused subscription wake Borg again
    Resume {
        /// Subscription id (or prefix)
        id: String,
    },
    /// Show the latest events a subscription received
    Events {
        /// Subscription id (or prefix)
        id: String,
        /// Number of events to show
        #[arg(long, short = 'n', default_value_t = 20)]
        count: u32,
    },
    /// Send the source's sample event through the real path
    Test {
        /// Subscription id (or prefix)
        id: String,
    },
}

/// Dispatch for `borg subscriptions ...`.
pub(crate) async fn dispatch(action: Option<SubscriptionsAction>) -> Result<()> {
    let mut client = connect_or_spawn().await?;
    let subs = client.list_subscriptions().await?.subscriptions;
    match action.unwrap_or(SubscriptionsAction::List) {
        SubscriptionsAction::List => super::print_lines(list_lines(&subs)),
        SubscriptionsAction::Pause { id } => set_active(&mut client, &subs, &id, false).await,
        SubscriptionsAction::Resume { id } => set_active(&mut client, &subs, &id, true).await,
        SubscriptionsAction::Events { id, count } => {
            let sub = resolve(&subs, &id)?;
            let events = client
                .list_subscription_events(&sub.id, count)
                .await?
                .events;
            super::print_lines(event_lines(&events))
        }
        SubscriptionsAction::Test { id } => {
            let sub = resolve(&subs, &id)?;
            let event = client.test_subscription(&sub.id).await?;
            println!(
                "Sent a test event to \"{}\" ({}). See it with `borg subscriptions events {}`.",
                sub.name, event.id, sub.id
            );
            Ok(())
        }
    }
}

async fn set_active(
    client: &mut DaemonClient,
    subs: &[Subscription],
    id: &str,
    active: bool,
) -> Result<()> {
    let sub = resolve(subs, id)?;
    let row = client.set_subscription_status(&sub.id, active).await?;
    println!("\"{}\" is {}.", row.name, status_words(&row.status));
    Ok(())
}

/// The subscription a typed id or prefix names. An unknown id is an error
/// here, not at the daemon: a typo must not look like "nothing happened".
fn resolve<'a>(subs: &'a [Subscription], prefix: &str) -> Result<&'a Subscription> {
    let ids = subs.iter().map(|s| s.id.clone()).collect();
    let id = super::resolve_unique_prefix("subscriptions", prefix, ids)?;
    id.and_then(|id| subs.iter().find(|s| s.id == id))
        .ok_or_else(|| {
            anyhow::anyhow!("No subscription matches '{prefix}'. `borg subscriptions` lists them.")
        })
}

fn status_words(status: &str) -> &str {
    match status {
        "active" => "on",
        "paused" => "paused",
        "storm" => "paused itself after a burst",
        other => other,
    }
}

fn list_lines(subs: &[Subscription]) -> Vec<String> {
    if subs.is_empty() {
        return vec![
            "No subscriptions. Add one on the Connections page, or ask Borg in chat.".into(),
        ];
    }
    let ids: Vec<&str> = subs.iter().map(|s| s.id.as_str()).collect();
    let w = ids
        .iter()
        .map(|id| listed_id(id, &ids).len())
        .max()
        .unwrap_or(8);
    let mut out = vec![format!(
        "{:w$}  {:24}  {:10}  {:28}  LAST EVENT",
        "ID", "NAME", "SOURCE", "STATUS"
    )];
    for s in subs {
        let last = if s.last_event_at_unix > 0 {
            format_ts(s.last_event_at_unix, TS_FMT)
        } else {
            "-".into()
        };
        let mut status = status_words(&s.status).to_string();
        if s.last_run_status == "failed" {
            status.push_str(", last run failed");
        }
        out.push(format!(
            "{:w$}  {:24}  {:10}  {:28}  {last}",
            listed_id(&s.id, &ids),
            truncate_str(&s.name, 24),
            s.provider,
            status,
        ));
    }
    out
}

fn event_lines(events: &[SubscriptionEvent]) -> Vec<String> {
    if events.is_empty() {
        return vec!["No events yet.".into()];
    }
    events
        .iter()
        .map(|e| {
            let mut line = format!(
                "{}  {:9}  {}",
                format_ts(e.received_at_unix, TS_FMT),
                e.status,
                truncate_str(&e.summary, 72)
            );
            if !e.error.is_empty() {
                line.push_str(&format!("  ({})", truncate_str(&e.error, 60)));
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &str, name: &str, status: &str) -> Subscription {
        Subscription {
            id: id.into(),
            name: name.into(),
            provider: "sentry".into(),
            status: status.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_prefix_names_one_subscription_and_a_typo_is_an_error() {
        let subs = [
            sub("sub_aaaa1111", "A", "active"),
            sub("sub_aaab2222", "B", "paused"),
        ];
        assert_eq!(resolve(&subs, "sub_aaab").unwrap().name, "B");
        assert!(
            resolve(&subs, "sub_aaa").is_err(),
            "an ambiguous prefix picks nothing"
        );
        let err = resolve(&subs, "sub_zzz").unwrap_err().to_string();
        assert!(err.contains("No subscription matches"), "{err}");
    }

    #[test]
    fn the_list_says_storms_and_failures_in_words() {
        let mut failed = sub("sub_b", "Deploys", "active");
        failed.last_run_status = "failed".into();
        failed.last_event_at_unix = 1_700_000_000;
        let lines = list_lines(&[sub("sub_a", "Sentry errors", "storm"), failed]);
        assert!(
            lines[1].contains("paused itself after a burst"),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].trim_end().ends_with('-'),
            "no event yet: {}",
            lines[1]
        );
        assert!(lines[2].contains("on, last run failed"), "{}", lines[2]);
        assert!(lines[2].contains("2023-11-1"), "{}", lines[2]);
        assert!(list_lines(&[])[0].contains("Connections page"));
    }

    #[test]
    fn events_show_their_failure_reason() {
        let e = SubscriptionEvent {
            status: "failed".into(),
            summary: "CPU high".into(),
            error: "timed out".into(),
            received_at_unix: 1_700_000_000,
            ..Default::default()
        };
        let lines = event_lines(&[e]);
        assert!(lines[0].contains("failed") && lines[0].contains("CPU high"));
        assert!(lines[0].ends_with("(timed out)"), "{}", lines[0]);
        assert_eq!(event_lines(&[]), vec!["No events yet.".to_string()]);
    }
}
