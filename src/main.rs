use bankbot::{Job, LocalQueue, Queue, job::Repository};
use async_std::sync::{Arc, RwLock, Mutex};
use std::convert::TryInto;
use std::path::PathBuf;
use structopt::StructOpt;
use tide::prelude::*;
use tide_github::Event;
use octocrab::Octocrab;
use octocrab::params::apps::CreateInstallationAccessToken;

#[derive(Debug, StructOpt)]
#[structopt(name = "bankbot", about = "The benchmarking bot\n\nSee the [Github docs][1] for instructions on setting up a Github app (and thus acquiring the required Github credentials).

[1]: https://docs.github.com/en/developers/apps/building-github-apps/creating-a-github-app")]
struct Config {
    /// Github Webhook secret
    #[structopt(short, long, env, hide_env_values = true)]
    webhook_secret: String,
    /// Github App ID
    #[structopt(long, env)]
    app_id: u64,
    /// Github App key
    #[structopt(long, env, hide_env_values = true)]
    app_key: String,
    /// Port to listen on
    #[structopt(short, long, env, default_value = "3000")]
    port: u16,
    /// Address to listen on
    #[structopt(short, long, env, default_value = "127.0.0.1")]
    address: String,
    /// Log level
    #[structopt(short, long, env, default_value = "info")]
    log_level: log::LevelFilter,
    /// Bot command prefix
    #[structopt(short, long, env, default_value = "/benchbot")]
    command_prefix: String,
    /// Repositories root working directory
    #[structopt(short, long, env, default_value = "./repos")]
    repos_root: PathBuf,
}

type State = Arc<Mutex<LocalQueue<String, Job>>>;

async fn remove_from_queue(req: tide::Request<State>) -> tide::Result {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Options {
        long_poll: bool,
    }

    // We lock the Mutex in a separate scope so it can be unlocked (dropped)
    // before we try to .await another future (MutexGuard is not Send).
    let recv = {
        let queue = req.state();

        let mut queue = queue.lock().await;

        match queue.remove() {
            Some(job) => return Ok(tide::Body::from_json(&job)?.into()),
            None => {
                let Options { long_poll } = req.query()?;
                if long_poll {
                    let (send, recv) = async_std::channel::bounded(1);
                    queue.register_watcher(send);
                    Some(recv)
                } else {
                    None
                }
            }
        }
    };

    match recv {
        Some(recv) => {
            let mut res = tide::Response::new(200);
            let job = recv.recv().await?;
            res.set_body(tide::Body::from_json(&job)?);
            Ok(res)
        }
        None => Ok(tide::Response::builder(404).build()),
    }
}

#[async_std::main]
async fn main() -> tide::Result<()> {
    let config = Config::from_args();
    pretty_env_logger::formatted_timed_builder()
        .filter(None, config.log_level)
        .init();

    let command_prefix = config.command_prefix.clone();

    let queue = Arc::new(Mutex::new(LocalQueue::new()));

    let mut app = tide::with_state(queue.clone());
    let github = tide_github::new(&config.webhook_secret)
        .on(Event::IssueComment, move |payload| {
            let payload: tide_github::payload::IssueCommentPayload = match payload.try_into() {
                Ok(payload) => payload,
                Err(e) => {
                    log::warn!("Failed to parse payload: {}", e);
                    return;
                }
            };

            if let Some(body) = payload.comment.body {
                if body.starts_with(&command_prefix) {
                    let command = body
                        .split_once('\n')
                        .map(|(cmd, _)| cmd.into())
                        .unwrap_or(body);

                    let id = format!(
                        "{}_{}_{}",
                        payload.repository.name,
                        command,
                        chrono::Utc::now().timestamp_nanos()
                    );

                    let repo: Repository = match payload.repository.try_into() {
                        Ok(repo) => repo,
                        Err(err) => {
                            log::warn!("Failed to parse repository payload: {}", err);
                            return;
                        }
                    };

                    let job = Job {
                        command,
                        user: payload.comment.user,
                        repository: repo,
                        issue: payload.issue,
                    };

                    let q = queue.clone();
                    async_std::task::spawn (async move { q.lock().await.add(id, job); });
                }
            }
        })
        .build();
    app.at("/").nest(github);
    app.at("/queue/remove").post(remove_from_queue);

    let self_url = format!("http://{}:{}", config.address, config.port);
    let repos_root = config.repos_root.clone();
    let octocrab = {
        let token = {
            let app_id = octocrab::models::AppId::from(config.app_id);
            let app_key = jsonwebtoken::EncodingKey::from_rsa_pem(config.app_key.as_bytes())?;
            octocrab::auth::create_jwt(app_id, &app_key)?
        };
        Octocrab::builder().personal_token(token).build()?
    };

    let tokio_rt = tokio::runtime::Runtime::new()?;

    let rt_handle = tokio_rt.handle();
    async_std::task::spawn(async move {
        async fn run<P: AsRef<std::path::Path> + AsRef<std::ffi::OsStr>>(
            repos_root: P,
            job: Job,
            github_client: Arc<RwLock<octocrab::Octocrab>>,
        ) -> anyhow::Result<()> {
            job.checkout(&repos_root)?.prepare_script(github_client)?.run()?;
            Ok(())
        }

        async fn get_job<D: std::fmt::Display>(url: D) -> anyhow::Result<Job> {
            let mut res = surf::post(format!("{}/queue/remove?long_poll=true", url))
                .await
                .map_err(|e| e.into_inner())?;
            res.body_json::<Job>().await.map_err(|e| e.into_inner())
        }

        let github_client = Arc::new(RwLock::new(octocrab));
        let rt_handle = tokio_rt.handle();
        loop {
            match get_job(&self_url).await {
                Ok(job) => {
                    log::info!(
                        "Processing command {} by user {} from repo {}",
                        job.command,
                        job.user.login,
                        job.repository.url
                    );

                    // TODO: Fix block_on
                    let octo_client = match rt_handle.block_on(async {
                        let github_client = github_client.read().await;
                        let installations = github_client.apps().installations().send().await.unwrap().take_items();
                        let mut access_token_req = CreateInstallationAccessToken::default();
                        access_token_req.repository_ids = vec!(job.repository.id);
                        println!("installations: {:?}", installations);
                        let access: octocrab::models::InstallationToken = github_client.post(installations[0].access_tokens_url.as_ref().unwrap(), Some(&access_token_req)).await?;
                        octocrab::OctocrabBuilder::new().personal_token(access.token).build()
                    }) {
                        Ok(octo_client) => octo_client,
                        _ => { log::warn!("Failed to require octocrab Github client"); return },
                    };

                    let octo_client = Arc::new(RwLock::new(octo_client));

                    let repo_owner = job.repository.owner.login.clone();
                    let repo_name = job.repository.name.clone();
                    let issue_nr = job.issue.number.try_into();

                    if let Err(job_err) = run(&repos_root, job, octo_client.clone()).await {
                        log::warn!("Error running job: {job_err}");

                        if let Ok(issue_nr) = issue_nr {
                            let bla = match rt_handle.block_on(async {
                                octo_client.read().await
                                    .issues(&repo_owner, &repo_name)
                                    .create_comment(issue_nr, format!("Error running job: {job_err}")).await
                            }) {
                                Ok(_) => {},
                                Err(err) => log::warn!("Failed to comment on issue: {err}"),
                            };
                            ()
                        };
                    };
                },
                Err(e) => log::warn!("Failed to retrieve job from queue: {}", e),
            }
        }
    });

    app.listen((config.address, config.port)).await?;
    Ok(())
}
