//! `--all`: run one action on every saved display concurrently, then report
//! the results grouped per display, in a stable order.

use std::future::Future;

use anyhow::{anyhow, Result};

use crate::config::Target;

pub(crate) struct Outcome {
    pub target: Target,
    /// Lines to print for this display, or why it failed
    pub result: Result<Vec<String>>,
}

/// Start `action` for every target at once; results keep the targets' order.
pub(crate) async fn run_all<F, Fut>(targets: Vec<Target>, action: F) -> Vec<Outcome>
where
    F: Fn(Target) -> Fut,
    Fut: Future<Output = Result<Vec<String>>> + Send + 'static,
{
    let tasks: Vec<_> = targets
        .into_iter()
        .map(|t| {
            let task = tokio::spawn(action(t.clone()));
            (t, task)
        })
        .collect();
    let mut outcomes = Vec::with_capacity(tasks.len());
    for (target, task) in tasks {
        let result = task
            .await
            .unwrap_or_else(|e| Err(anyhow!("task failed: {e}")));
        outcomes.push(Outcome { target, result });
    }
    outcomes
}

fn heading(t: &Target) -> String {
    match &t.name {
        Some(name) => format!("── \"{name}\" ({})", t.host),
        None => format!("── {}", t.host),
    }
}

/// Text to print, and how many displays failed.
pub(crate) fn report(outcomes: &[Outcome]) -> (String, usize) {
    let mut out = String::new();
    let mut failed = 0;
    for o in outcomes {
        out.push_str(&heading(&o.target));
        out.push('\n');
        match &o.result {
            Ok(lines) => {
                for line in lines {
                    out.push_str(&format!("   {line}\n"));
                }
            }
            Err(e) => {
                failed += 1;
                out.push_str(&format!("   ❌ {e:#}\n"));
            }
        }
    }
    let total = outcomes.len();
    out.push_str(&format!(
        "{total} display{}: {} ok{}",
        if total == 1 { "" } else { "s" },
        total - failed,
        if failed > 0 {
            format!(", {failed} failed")
        } else {
            String::new()
        }
    ));
    (out, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(name: &str, host: &str) -> Target {
        Target {
            name: Some(name.into()),
            host: host.into(),
            pin: "1".into(),
            mac: None,
        }
    }

    #[tokio::test]
    async fn runs_concurrently_and_keeps_order() {
        let started = std::time::Instant::now();
        let targets = vec![t("a", "slow"), t("b", "fast"), t("c", "bad")];
        let outcomes = run_all(targets, |t| async move {
            match t.host.as_str() {
                "slow" => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(vec!["slow done".into()])
                }
                "fast" => Ok(vec!["fast done".into()]),
                _ => Err(anyhow!("unreachable display")),
            }
        })
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "ran in parallel"
        );
        let names: Vec<_> = outcomes
            .iter()
            .map(|o| o.target.name.clone().unwrap())
            .collect();
        assert_eq!(
            names,
            ["a", "b", "c"],
            "order kept despite finishing out of order"
        );
        assert!(outcomes[2].result.is_err());
    }

    #[test]
    fn report_groups_lines_and_counts_failures() {
        let outcomes = vec![
            Outcome {
                target: t("lobby", "10.0.0.1"),
                result: Ok(vec!["Battery:  86%".into(), "Name:     Lobby".into()]),
            },
            Outcome {
                target: t("office", "10.0.0.2"),
                result: Err(anyhow!("Connection timed out")),
            },
        ];
        let (text, failed) = report(&outcomes);
        assert_eq!(failed, 1);
        assert_eq!(
            text,
            "── \"lobby\" (10.0.0.1)\n   Battery:  86%\n   Name:     Lobby\n\
             ── \"office\" (10.0.0.2)\n   ❌ Connection timed out\n\
             2 displays: 1 ok, 1 failed"
        );
    }

    #[test]
    fn report_all_ok() {
        let outcomes = vec![Outcome {
            target: t("lobby", "10.0.0.1"),
            result: Ok(vec![]),
        }];
        let (text, failed) = report(&outcomes);
        assert_eq!(failed, 0);
        assert!(text.ends_with("1 display: 1 ok"), "{text}");
    }
}
