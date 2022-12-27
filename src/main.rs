#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_imports)]

use tokio::spawn;
use std::path::PathBuf;
use axum_server::tls_rustls::RustlsConfig;
use axum::routing::*;
use axum::Router;
use axum::response::{IntoResponse, Response, Html};
use std::net::SocketAddr;
use axum::extract::{Path, State};
use http_body::combinators::UnsyncBoxBody;
use axum::{Error, Json};
use axum::extract::rejection::JsonRejection;
use axum::body::Bytes;
use axum::http::{StatusCode, Uri};
use http::header::HeaderMap;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

mod sql;
mod notifier;
mod dbctx;

use sql::JobState;

use dbctx::DbCtx;

use rusqlite::OptionalExtension;

const PSKS: &'static [&'static [u8]] = &[];

#[derive(Copy, Clone, Debug)]
enum GithubHookError {
    BodyNotObject,
    MissingElement { path: &'static str },
    BadType { path: &'static str, expected: &'static str },
}

#[derive(Debug)]
enum GithubEvent {
    Push { tip: String, repo_name: String, head_commit: serde_json::Map<String, serde_json::Value>, pusher: serde_json::Map<String, serde_json::Value> },
    Other {}
}

fn parse_push_event(body: serde_json::Value) -> Result<GithubEvent, GithubHookError> {
    let body = body.as_object()
        .ok_or(GithubHookError::BodyNotObject)?;

    let tip = body.get("after")
        .ok_or(GithubHookError::MissingElement { path: "after" })?
        .as_str()
        .ok_or(GithubHookError::BadType { path: "after", expected: "str" })?
        .to_owned();

    let repo_name = body.get("repository")
        .ok_or(GithubHookError::MissingElement { path: "repository" })?
        .as_object()
        .ok_or(GithubHookError::BadType { path: "repository", expected: "obj" })?
        .get("full_name")
        .ok_or(GithubHookError::MissingElement { path: "repository/full_name" })?
        .as_str()
        .ok_or(GithubHookError::BadType { path: "repository/full_name", expected: "str" })?
        .to_owned();

    let head_commit = body.get("head_commit")
        .ok_or(GithubHookError::MissingElement { path: "head_commit" })?
        .as_object()
        .ok_or(GithubHookError::BadType { path: "head_commit", expected: "obj" })?
        .to_owned();

    let pusher = body.get("pusher")
        .ok_or(GithubHookError::MissingElement { path: "pusher" })?
        .as_object()
        .ok_or(GithubHookError::BadType { path: "pusher", expected: "obj" })?
        .to_owned();

    Ok(GithubEvent::Push { tip, repo_name, head_commit, pusher })
}

async fn process_push_event(ctx: Arc<DbCtx>, owner: String, repo: String, event: GithubEvent) -> impl IntoResponse {
    let (sha, repo, head_commit, pusher) = if let GithubEvent::Push { tip, repo_name, head_commit, pusher } = event {
        (tip, repo_name, head_commit, pusher)
    } else {
        panic!("process push event on non-push event");
    };

    println!("handling push event to {}/{}: sha {} in repo {}, {:?}\n  pusher: {:?}", owner, repo, sha, repo, head_commit, pusher);

    // push event is in terms of a ref, but we don't know if it's a new commit (yet).
    // in terms of CI jobs, we care mainly about new commits.
    // so...
    // * look up the commit,
    // * if it known, bail out (new ref for existing commit we've already handled some way)
    // * create a new commit ref
    // * create a new job (state=pending) for the commit ref
    let commit_id: Option<u64> = ctx.conn.lock().unwrap()
        .query_row(sql::COMMIT_TO_ID, [sha.clone()], |row| row.get(0))
        .optional()
        .expect("can run query");

    if commit_id.is_some() {
        eprintln!("commit already exists");
        return (StatusCode::OK, String::new());
    }

    let remote_url = format!("https://www.github.com/{}.git", repo);
    eprintln!("looking for remote url: {}", remote_url);
    let (remote_id, repo_id): (u64, u64) = match ctx.conn.lock().unwrap()
        .query_row("select id, repo_id from remotes where remote_git_url=?1;", [&remote_url], |row| Ok((row.get(0).unwrap(), row.get(1).unwrap())))
        .optional()
        .unwrap() {
        Some(elems) => elems,
        None => {
            eprintln!("no remote registered for url {} (repo {})", remote_url, repo);
            return (StatusCode::NOT_FOUND, String::new());
        }
    };

    let pusher_email = pusher
        .get("email")
        .expect("has email")
        .as_str()
        .expect("is str");

    let job_id = ctx.new_job(remote_id, &sha, Some(pusher_email)).unwrap();

    let notifiers = ctx.notifiers_by_repo(repo_id).expect("can get notifiers");

    for notifier in notifiers {
        notifier.tell_pending_job(&ctx, repo_id, &sha, job_id).await.expect("can notify");
    }

    (StatusCode::OK, String::new())
}

async fn handle_github_event(ctx: Arc<DbCtx>, owner: String, repo: String, event_kind: String, body: serde_json::Value) -> Response<UnsyncBoxBody<Bytes, Error>> {
    println!("got github event: {}, {}, {}", owner, repo, event_kind);
    match event_kind.as_str() {
        "push" => {
            let push_event = parse_push_event(body)
                .map_err(|e| {
                    eprintln!("TODO: handle push event error: {:?}", e);
                    panic!()
                })
                .expect("parse works");
            let res = process_push_event(ctx, owner, repo, push_event).await;
            "ok".into_response()
        },
        "status" => {
            eprintln!("[.] status update");
            "ok".into_response()
        }
        other => {
            eprintln!("unhandled event kind: {}, repo {}/{}. content: {:?}", other, owner, repo, body);
            "".into_response()
        }
    }
}

async fn handle_commit_status(Path(path): Path<(String, String, String)>, State(ctx): State<Arc<DbCtx>>) -> impl IntoResponse {
    eprintln!("path: {}/{}, sha {}", path.0, path.1, path.2);
    let remote_path = format!("{}/{}", path.0, path.1);
    let sha = path.2;

    let commit_id: Option<u64> = ctx.conn.lock().unwrap()
        .query_row("select id from commits where sha=?1;", [&sha], |row| row.get(0))
        .optional()
        .expect("can query");

    let commit_id: u64 = match commit_id {
        Some(commit_id) => {
            commit_id
        },
        None => {
            return (StatusCode::NOT_FOUND, Html("<html><body>no such commit</body></html>".to_string()));
        }
    };

    let (remote_id, repo_id): (u64, u64) = ctx.conn.lock().unwrap()
        .query_row("select id, repo_id from remotes where remote_path=?1;", [&remote_path], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
        .expect("can query");

    let (job_id, state): (u64, u8) = ctx.conn.lock().unwrap()
        .query_row("select id, state from jobs where commit_id=?1;", [commit_id], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
        .expect("can query");

    let state: sql::JobState = unsafe { std::mem::transmute(state) };

    let repo_name: String = ctx.conn.lock().unwrap()
        .query_row("select repo_name from repos where id=?1;", [repo_id], |row| row.get(0))
        .expect("can query");

    let deployed = false;

    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("now is before epoch");

    let resp = format!("\
        <html>\n\
          <head>\n\
            <title>ci.butactuallyin.space - {}</title>\n\
          </head>\n\
          <body>\n\
            <pre>\n\
            repo: {}\n\
            commit: <a href='https://www.github.com/{}/commit/{}'>{}</a>\n  \
            status: {}\n  \
            deployed: {}\n\
            </pre>\n\
          </body>\n\
        </html>\n",
        repo_name,
        repo_name,
        &remote_path, &sha, &sha,
        match state {
            JobState::Pending | JobState::Started => {
                "<span style='color:#660;'>pending</span>"
            },
            JobState::Complete => {
                "<span style='color:green;'>pass</span>"
            },
            JobState::Error => {
                "<span style='color:red;'>pass</span>"
            }
            JobState::Invalid => {
                "<span style='color:red;'>(server error)</span>"
            }
        },
        deployed,
    );

    (StatusCode::OK, Html(resp))
}

async fn handle_repo_event(Path(path): Path<(String, String)>, headers: HeaderMap, State(ctx): State<Arc<DbCtx>>, body: Bytes) -> impl IntoResponse {
    let json: Result<serde_json::Value, _> = serde_json::from_slice(&body);
    eprintln!("repo event: {:?} {:?} {:?}", path.0, path.1, headers);

    let payload = match json {
        Ok(payload) => { payload },
        Err(e) => {
            eprintln!("bad request: path={}/{}\nheaders: {:?}\nbody err: {:?}", path.0, path.1, headers, e); 
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let sent_hmac = match headers.get("x-hub-signature-256") {
        Some(sent_hmac) => { sent_hmac.to_str().expect("valid ascii string").to_owned() },
        None => {
            eprintln!("bad request: path={}/{}\nheaders: {:?}\nno x-hub-signature-256", path.0, path.1, headers); 
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let mut hmac_ok = false;

    for psk in PSKS.iter() {
        let mut mac = Hmac::<Sha256>::new_from_slice(psk)
            .expect("hmac can be constructed");
        mac.update(&body);
        let result = mac.finalize().into_bytes().to_vec();

        // hack: skip sha256=
        let decoded = hex::decode(&sent_hmac[7..]).expect("provided hmac is valid hex");
        if decoded == result {
            hmac_ok = true;
            break;
        }
    }

    if !hmac_ok {
        eprintln!("bad hmac by all psks");
        return (StatusCode::BAD_REQUEST, "").into_response();
    }

    let kind = match headers.get("x-github-event") {
        Some(kind) => { kind.to_str().expect("valid ascii string").to_owned() },
        None => {
            eprintln!("bad request: path={}/{}\nheaders: {:?}\nno x-github-event", path.0, path.1, headers); 
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    handle_github_event(ctx, path.0, path.1, kind, payload).await
}


async fn make_app_server(cfg_path: &'static str, db_path: &'static str) -> Router {
    /*

    // GET /hello/warp => 200 OK with body "Hello, warp!"
    let hello = warp::path!("hello" / String)
        .map(|name| format!("Hello, {}!\n", name));

    let github_event = warp::post()
        .and(warp::path!(String / String))
        .and_then(|owner, repo| {
            warp::header::<String>("x-github-event")
                .and(warp::body::content_length_limit(1024 * 1024))
                .and(warp::body::json())
                .and_then(|event, json| handle_github_event(owner, repo, event, json))
                .recover(|e| {
                    async fn handle_rejection(err: Rejection) -> Result<impl Reply, Rejection> {
                       Ok(warp::reply::with_status("65308", StatusCode::BAD_REQUEST))
                    }
                    handle_rejection(e)
                })
        });

    let repo_status = warp::get()
        .and(warp::path!(String / String / String))
        .map(|owner, repo, sha| format!("CI status for {}/{} commit {}\n", owner, repo, sha));

    let other =
            warp::post()
                .and(warp::path::full())
                .and(warp::addr::remote())
                .and(warp::body::content_length_limit(1024 * 1024))
                .and(warp::body::bytes())
                .map(move |path, addr: Option<std::net::SocketAddr>, body| {
                    println!("{}: lets see what i got {:?}, {:?}", addr.unwrap(), path, body);
                    "hello :)\n"
                })
            .or(
                warp::get()
                    .and(warp::path::full())
                    .and(warp::addr::remote())
                    .map(move |path, addr: Option<std::net::SocketAddr>| {
                        println!("{}: GET to {:?}", addr.unwrap(), path);
                        "hello!\n"
                    })
            )
        .recover(|e| {
            async fn handle_rejection(err: Rejection) -> Result<impl Reply, std::convert::Infallible> {
               Ok(warp::reply::with_status("50834", StatusCode::BAD_REQUEST))
            }
            handle_rejection(e)
        });
    */

    async fn fallback_get(uri: Uri) -> impl IntoResponse {
        (StatusCode::OK, "get resp")
    }

    async fn fallback_post(Path(path): Path<String>) -> impl IntoResponse {
        "post resp"
    }

    Router::new()
        .route("/:owner/:repo/:sha", get(handle_commit_status))
        .route("/:owner/:repo", post(handle_repo_event))
        .fallback(fallback_get)
        .with_state(Arc::new(DbCtx::new(cfg_path, db_path)))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = RustlsConfig::from_pem_file(
        PathBuf::from("/etc/letsencrypt/live/ci.butactuallyin.space/fullchain.pem"),
        PathBuf::from("/etc/letsencrypt/live/ci.butactuallyin.space/privkey.pem"),
    ).await.unwrap();
    spawn(axum_server::bind_rustls("127.0.0.1:8080".parse().unwrap(), config.clone())
        .serve(make_app_server("/root/ixi_ci_server/config", "/root/ixi_ci_server/state.db").await.into_make_service()));
    axum_server::bind_rustls("0.0.0.0:443".parse().unwrap(), config)
        .serve(make_app_server("/root/ixi_ci_server/config", "/root/ixi_ci_server/state.db").await.into_make_service())
        .await
        .unwrap();
}
