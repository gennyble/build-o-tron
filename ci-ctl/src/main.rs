use clap::{Parser, Subcommand};

use ci_lib_core::dbctx::DbCtx;
use ci_lib_native::{GithubApi, notifier::NotifierConfig};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// path to a database to manage (defaults to "./state.db")
    db_path: Option<String>,

    /// path to where configs should be found (defaults to "./config")
    config_path: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// add _something_ to the db
    Add {
        #[command(subcommand)]
        what: AddItem,
    },

    /// make sure that the state looks reasonable.
    ///
    /// currently, ensure that all notifiers have a config, that config references an existing
    /// file, and that the referenced file is valid.
    Validate,

    /// do something with jobs
    Job {
        #[command(subcommand)]
        what: JobAction,
    },
}

#[derive(Subcommand)]
enum JobAction {
    List,
    Rerun {
        which: u32
    },
    RerunCommit {
        commit: String
    },
    Create {
        repo: String,
        commit: String,
        pusher_email: String,
    }
}

#[derive(Subcommand)]
enum AddItem {
    Repo {
        name: String,
        remote: Option<String>,
        remote_kind: Option<String>,
        config: Option<String>,
    },
    Remote {
        repo_name: String,
        remote: String,
        remote_kind: String,
        config: String,
    },
}

fn main() {
    let args = Args::parse();

    let db_path = args.db_path.unwrap_or_else(|| "./state.db".to_owned());
    let config_path = args.config_path.unwrap_or_else(|| "./config".to_owned());

    match args.command {
        Command::Job { what } => {
            match what {
                JobAction::List => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let conn = db.conn.lock().unwrap();
                    let mut query = conn.prepare(ci_lib_core::sql::SELECT_ALL_RUNS_WITH_JOB_INFO).unwrap();
                    let mut jobs = query.query([]).unwrap();
                    while let Some(row) = jobs.next().unwrap() {
                        let (job_id, run_id, state, created_time, commit_id, run_preferences): (u64, u64, u64, u64, u64, Option<String>) = row.try_into().unwrap();

                        eprint!("[+] {:04} ({:04}) | {: >8?} | {} | {}", run_id, job_id, state, created_time, commit_id);
                        if let Some(run_preferences) = run_preferences {
                            eprintln!(" | run preference: {}", run_preferences);
                        } else {
                            eprintln!("");
                        }
                    }
                    eprintln!("jobs");
                },
                JobAction::Rerun { which } => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let task_id = db.new_run(which as u64, None).expect("db can be queried").id;
                    eprintln!("[+] rerunning job {} as task {}", which, task_id);
                }
                JobAction::RerunCommit { commit } => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let job_id = db.job_for_commit(&commit).unwrap();
                    if let Some(job_id) = job_id {
                        let task_id = db.new_run(job_id, None).expect("db can be queried").id;
                        eprintln!("[+] rerunning job {} (commit {}) as task {}", job_id, commit, task_id);
                    } else {
                        eprintln!("[-] no job for commit {}", commit);
                    }
                }
                JobAction::Create { repo, commit, pusher_email } => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let parts = repo.split(":").collect::<Vec<&str>>();
                    let (remote_kind, repo_path) = (parts[0], parts[1]);
                    let remote = match db.remote_by_path_and_api(&remote_kind, &repo_path).expect("can query") {
                        Some(remote) => remote,
                        None => {
                            eprintln!("[-] no remote registered as {}:{}", remote_kind, repo_path);
                            return;
                        }
                    };

                    let repo_default_run_pref: Option<String> = db.conn.lock().unwrap()
                        .query_row("select default_run_preference from repos where id=?1;", [remote.repo_id], |row| {
                            Ok((row.get(0)).unwrap())
                        })
                        .expect("can query");

                    let (job_id, _commit_id) = db.new_job(remote.id, &commit, Some(&pusher_email), repo_default_run_pref).expect("can create");
                    let _ = db.new_run(job_id, None).unwrap();
                }
            }
        },
        Command::Add { what } => {
            match what {
                AddItem::Repo { name, remote, remote_kind, config } => {
                    let remote_config = match (remote, remote_kind, config) {
                        (Some(remote), Some(remote_kind), Some(config_path)) => {
                            // do something
                            if remote_kind != "github" {
                                eprintln!("unknown remote kind: {}", remote);
                                return;
                            }
                            Some((remote, remote_kind, config_path))
                        },
                        (None, None, None) => {
                            None
                        },
                        _ => {
                            eprintln!("when specifying a remote, `remote`, `remote_kind`, and `config_path` must either all be provided together or not at all");
                            return;
                        }
                    };

                    let db = DbCtx::new(&config_path, &db_path);
                    let repo_id = match db.new_repo(&name) {
                        Ok(repo_id) => repo_id,
                        Err(e) => {
                            if e.contains("UNIQUE constraint failed") {
                                eprintln!("[!] repo '{}' already exists", name);
                                return;
                            } else {
                                eprintln!("[!] failed to create repo entry: {}", e);
                                return;
                            }
                        }
                    };
                    println!("[+] new repo created: '{}' id {}", &name, repo_id);
                    if let Some((remote, remote_kind, config_path)) = remote_config {
                        let full_config_file_path = format!("{}/{}", &db.config_path.display(), config_path);
                        let _config = match remote_kind.as_ref() {
                            "github" => {
                                assert!(NotifierConfig::github_from_file(&full_config_file_path).is_ok());
                            }
                            "github-email" => {
                                assert!(NotifierConfig::email_from_file(&full_config_file_path).is_ok());
                            }
                            other => {
                                panic!("[-] notifiers for '{}' remotes are not supported", other);
                            }
                        };
                        db.new_remote(repo_id, remote.as_str(), remote_kind.as_str(), config_path.as_str()).unwrap();
                        println!("[+] new remote created: repo '{}', {} remote at {}", &name, remote_kind, remote);
                        match remote_kind.as_str() {
                            "github" => {
                                // attempt to create a webhook now...
                                let (ci_server, token, webhook_token) = match NotifierConfig::github_from_file(&full_config_file_path).expect("notifier config is valid") {
                                    NotifierConfig::GitHub { ci_server, token, webhook_token } => (ci_server, token, webhook_token),
                                    _ => {
                                        panic!("unexpected notifier config format, should have been github..")
                                    }
                                };
                                let gh = GithubApi { ci_server: &ci_server, token: &token, webhook_token: &webhook_token };
                                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                                    match gh.has_ci_webhook(remote.as_str()).await {
                                        Ok(present) => {
                                            if !present {
                                                println!("[.] trying to create push webhook on github.com/{}", remote.as_str());
                                                let res = gh.create_ci_webhook(remote.as_str()).await;
                                                if let Err(e) = res {
                                                    println!("[!] failed to create webhook on github.com/{}: {}", remote.as_str(), e);
                                                } else {
                                                    println!("[+] created webhook on github.com/{}. CI is good to go?", remote.as_str());
                                                }
                                            } else {
                                                println!("[+] ci.butactuallyin.space webhook appears to already be present on github.com/{}", remote.as_str());
                                            }
                                        }
                                        Err(e) => {
                                            println!("[!] unable to check for presence of ci.butactuallin.space webhook on github.com/{}: {}", remote.as_str(), e);
                                            println!("[!] you must make sure your github repo has a webhook set for `https://ci.butactuallyin.space/{}` to receive at least the `push` event.", remote.as_str());
                                            println!("      the secret sent with calls to this webhook should be the same preshared secret as the CI server is configured to know.");
                                        }
                                    }
                                });
                            }
                            _ => { }
                        }
                    }
                },
                AddItem::Remote { repo_name, remote, remote_kind, config } => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let repo_id = match db.repo_id_by_name(&repo_name) {
                        Ok(Some(id)) => id,
                        Ok(None) => {
                            eprintln!("[-] repo '{}' does not exist", repo_name);
                            return;
                        },
                        Err(e) => {
                            eprintln!("[!] couldn't look up repo '{}': {:?}", repo_name, e);
                            return;
                        }
                    };
                    let config_file = format!("{}/{}", config_path, config);
                    match remote_kind.as_ref() {
                        "github" => {
                            NotifierConfig::github_from_file(&config_file).unwrap();
                        }
                        "github-email" => {
                            NotifierConfig::email_from_file(&config_file).unwrap();
                        }
                        other => {
                            panic!("notifiers for '{}' remotes are not supported", other);
                        }
                    };
                    db.new_remote(repo_id, remote.as_str(), remote_kind.as_str(), config.as_str()).unwrap();
                    println!("[+] new remote created: repo '{}', {} remote at {}", &repo_name, remote_kind, remote);
                },
            }
        },
        Command::Validate => {
            println!("ok");
        }
    }
}
