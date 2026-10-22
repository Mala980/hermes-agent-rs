//! `hermes cron` handler.

use super::common::hermes_state_root;
use hermes_cli::cli::Cli;
use hermes_core::AgentError;
use hermes_cron::cron_scheduler_for_data_dir;
use hermes_cron::CronError;

fn cron_cli_error(e: CronError) -> AgentError {
    AgentError::Config(e.to_string())
}

pub(crate) async fn run_cron(
    cli: Cli,
    action: Option<String>,
    id: Option<String>,
    schedule: Option<String>,
    prompt: Option<String>,
    timezone: Option<String>,
    overlap: Option<String>,
) -> Result<(), AgentError> {
    let data_dir = hermes_state_root(&cli).join("cron");
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| AgentError::Io(format!("cron dir {}: {}", data_dir.display(), e)))?;
    let sched = cron_scheduler_for_data_dir(data_dir.clone());
    // Load persisted jobs so list/delete/pause/resume/run/history see jobs
    // created by previous invocations.
    sched.load_persisted_jobs().await.map_err(cron_cli_error)?;

    match action.as_deref().unwrap_or("list") {
        "list" => {
            let jobs = sched.list_jobs().await;
            if jobs.is_empty() {
                println!("(no cron jobs in {})", data_dir.display());
                return Ok(());
            }
            println!("Cron jobs ({}):", data_dir.display());
            for j in jobs {
                let snippet: String = j.prompt.chars().take(48).collect();
                println!(
                    "  {}  [{}]  {:?}  next_run={:?}  tz={}  overlap={}  {}",
                    j.id,
                    j.schedule,
                    j.status,
                    j.next_run,
                    j.timezone.as_deref().unwrap_or("system-local"),
                    j.overlap,
                    snippet
                );
            }
        }
        "create" => {
            let schedule = schedule.unwrap_or_else(|| "0 * * * *".to_string());
            let prompt = prompt
                .ok_or_else(|| AgentError::Config("cron create: use --prompt \"...\"".into()))?;
            let mut job = hermes_cron::CronJob::new(schedule, prompt);
            if let Some(tz) = timezone {
                hermes_cron::validate_timezone(&tz)
                    .map_err(|e| AgentError::Config(format!("cron create: {e}")))?;
                job.timezone = Some(tz);
            }
            if let Some(ov) = overlap {
                job.overlap = ov
                    .parse::<hermes_cron::OverlapPolicy>()
                    .map_err(|e: String| AgentError::Config(format!("cron create: {e}")))?;
            }
            // Recompute next_run: a new timezone changes the schedule's wall-clock.
            job.next_run = job.compute_next_run(chrono::Utc::now());
            let jid = sched.create_job(job).await.map_err(cron_cli_error)?;
            let created = sched.get_job(&jid).await.expect("job just created");
            println!(
                "Created cron job id={} schedule={:?} timezone={} overlap={} next_run={:?} (persisted under {})",
                jid,
                created.schedule,
                created.timezone.as_deref().unwrap_or("system-local"),
                created.overlap,
                created.next_run,
                data_dir.display()
            );
        }
        "delete" | "pause" | "resume" | "run" | "history" => {
            let act = action.as_deref().unwrap_or("cron");
            let jid = id
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AgentError::Config(format!("{}: use --id <job-id>", act)))?;
            match act {
                "delete" => {
                    sched.remove_job(&jid).await.map_err(cron_cli_error)?;
                    println!("Deleted job {}", jid);
                }
                "pause" => {
                    sched.pause_job(&jid).await.map_err(cron_cli_error)?;
                    println!("Paused job {}", jid);
                }
                "resume" => {
                    sched.resume_job(&jid).await.map_err(cron_cli_error)?;
                    println!("Resumed job {}", jid);
                }
                "run" => {
                    let result = sched.run_job(&jid).await.map_err(cron_cli_error)?;
                    let json = serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| format!("{result:#?}"));
                    println!("{}", json);
                }
                "history" => {
                    let job = sched
                        .get_job(&jid)
                        .await
                        .ok_or_else(|| AgentError::Config(format!("unknown job id: {}", jid)))?;
                    let json = serde_json::to_string_pretty(&job)
                        .map_err(|e| AgentError::Config(e.to_string()))?;
                    println!("{}", json);
                }
                _ => {
                    return Err(AgentError::Config(format!(
                        "internal: unexpected cron action '{}'",
                        act
                    )));
                }
            }
        }
        other => {
            return Err(AgentError::Config(format!(
                "Unknown cron action: {} (use list|create|delete|pause|resume|run|history)",
                other
            )));
        }
    }
    Ok(())
}
