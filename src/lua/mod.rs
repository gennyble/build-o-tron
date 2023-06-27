use crate::RunningJob;

use rlua::prelude::*;

use std::sync::{Arc, Mutex};
use std::path::PathBuf;

pub const DEFAULT_RUST_GOODFILE: &'static [u8] = include_bytes!("../../config/goodfiles/rust.lua");

pub struct BuildEnv {
    lua: Lua,
    job: Arc<Mutex<RunningJob>>,
}

mod lua_exports {
    use crate::RunningJob;

    use std::sync::{Arc, Mutex};
    use std::path::PathBuf;

    use rlua::prelude::*;

    pub fn build_command_impl(command: LuaValue, params: LuaValue, job_ctx: Arc<Mutex<RunningJob>>) -> Result<(), rlua::Error> {
        let args = match command {
            LuaValue::Table(table) => {
                let len = table.len().expect("command table has a length");
                let mut command_args = Vec::new();
                for i in 0..len {
                    let value = table.get(i + 1).expect("command arg is gettble");
                    match value {
                        LuaValue::String(s) => {
                            command_args.push(s.to_str().unwrap().to_owned());
                        },
                        other => {
                            return Err(LuaError::RuntimeError(format!("argument {} was not a string, was {:?}", i, other)));
                        }
                    };
                }

                command_args
            },
            other => {
                return Err(LuaError::RuntimeError(format!("argument 1 was not a table: {:?}", other)));
            }
        };

        #[derive(Debug)]
        struct RunParams {
            step: Option<String>,
            name: Option<String>,
            cwd: Option<String>,
        }

        let params = match params {
            LuaValue::Table(table) => {
                let step = match table.get("step").expect("can get from table") {
                    LuaValue::String(v) => {
                        Some(v.to_str()?.to_owned())
                    },
                    LuaValue::Nil => {
                        None
                    },
                    other => {
                        return Err(LuaError::RuntimeError(format!("params[\"step\"] must be a string")));
                    }
                };
                let name = match table.get("name").expect("can get from table") {
                    LuaValue::String(v) => {
                        Some(v.to_str()?.to_owned())
                    },
                    LuaValue::Nil => {
                        None
                    },
                    other => {
                        return Err(LuaError::RuntimeError(format!("params[\"name\"] must be a string")));
                    }
                };
                let cwd = match table.get("cwd").expect("can get from table") {
                    LuaValue::String(v) => {
                        Some(v.to_str()?.to_owned())
                    },
                    LuaValue::Nil => {
                        None
                    },
                    other => {
                        return Err(LuaError::RuntimeError(format!("params[\"cwd\"] must be a string")));
                    }
                };

                RunParams {
                    step,
                    name,
                    cwd,
                }
            },
            LuaValue::Nil => {
                RunParams {
                    step: None,
                    name: None,
                    cwd: None,
                }
            }
            other => {
                return Err(LuaError::RuntimeError(format!("argument 2 was not a table: {:?}", other)));
            }
        };
        eprintln!("args: {:?}", args);
        eprintln!("  params: {:?}", params);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            job_ctx.lock().unwrap().run_command(&args, params.cwd.as_ref().map(|x| x.as_str())).await
                .map_err(|e| LuaError::RuntimeError(format!("run_command error: {:?}", e)))
        })
    }

    pub fn metric(name: String, value: String, job_ctx: Arc<Mutex<RunningJob>>) -> Result<(), rlua::Error> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            job_ctx.lock().unwrap().send_metric(&name, value).await
                .map_err(|e| LuaError::RuntimeError(format!("send_metric error: {:?}", e)))
        })
    }

    pub fn artifact(path: String, name: Option<String>, job_ctx: Arc<Mutex<RunningJob>>) -> Result<(), rlua::Error> {
        let path: PathBuf = path.into();

        let default_name: String = match (path.file_name(), path.parent()) {
            (Some(name), _) => name
                .to_str()
                .ok_or(LuaError::RuntimeError("artifact name is not a unicode string".to_string()))?
                .to_string(),
            (_, Some(parent)) => format!("{}", parent.display()),
            (None, None) => {
                // one day using directories for artifacts might work but / is still not going
                // to be accepted
                return Err(LuaError::RuntimeError(format!("cannot infer a default path name for {}", path.display())));
            }
        };

        let name: String = match name {
            Some(name) => name,
            None => default_name,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut artifact = job_ctx.lock().unwrap().create_artifact(&name, &format!("{} (from {})", name, path.display())).await
                .map_err(|e| LuaError::RuntimeError(format!("create_artifact error: {:?}", e)))
                .unwrap();
            let mut file = tokio::fs::File::open(&format!("tmpdir/{}", path.display())).await.unwrap();
            eprintln!("uploading...");
            crate::io::forward_data(&mut file, &mut artifact).await
                .map_err(|e| LuaError::RuntimeError(format!("failed uploading data for {}: {:?}", name, e)))?;
            std::mem::drop(artifact);
            Ok(())
        })
    }

    pub fn has_cmd(name: &str) -> Result<bool, rlua::Error> {
        Ok(std::process::Command::new("which")
            .arg(name)
            .status()
            .map_err(|e| LuaError::RuntimeError(format!("could not fork which? {:?}", e)))?
            .success())
    }

    pub fn file_size(path: &str) -> Result<u64, rlua::Error> {
        Ok(std::fs::metadata(&format!("tmpdir/{}", path))
            .map_err(|e| LuaError::RuntimeError(format!("could not stat {:?}", path)))?
            .len())
    }

    pub mod step {
        use crate::RunningJob;
        use std::sync::{Arc, Mutex};

        pub fn start(job_ref: Arc<Mutex<RunningJob>>, name: String) -> Result<(), rlua::Error> {
            let mut job = job_ref.lock().unwrap();
            job.current_step.clear();
            job.current_step.push(name);
            Ok(())
        }

        pub fn push(job_ref: Arc<Mutex<RunningJob>>, name: String) -> Result<(), rlua::Error> {
            let mut job = job_ref.lock().unwrap();
            job.current_step.push(name);
            Ok(())
        }

        pub fn advance(job_ref: Arc<Mutex<RunningJob>>, name: String) -> Result<(), rlua::Error> {
            let mut job = job_ref.lock().unwrap();
            job.current_step.pop();
            job.current_step.push(name);
            Ok(())
        }
    }
}

struct DeclEnv<'lua, 'env> {
    lua_ctx: &'env rlua::Context<'lua>,
    job_ref: &'env Arc<Mutex<RunningJob>>,
}
impl<'lua, 'env> DeclEnv<'lua, 'env> {
    fn create_function<A, R, F>(&self, name: &str, f: F) ->  Result<rlua::Function<'lua>, String>
        where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + Send + Fn(rlua::Context<'lua>, Arc<Mutex<RunningJob>>, A) -> Result<R, rlua::Error> {

        let job_ref = Arc::clone(self.job_ref);
        self.lua_ctx.create_function(move |ctx, args| {
            let job_ref = Arc::clone(&job_ref);
            f(ctx, job_ref, args)
        })
            .map_err(|e| format!("problem defining {} function: {:?}", name, e))
    }
}

impl BuildEnv {
    pub fn new(job: &Arc<Mutex<RunningJob>>) -> Self {
        let env = BuildEnv {
            lua: Lua::new(),
            job: Arc::clone(job),
        };
        env.lua.context(|lua_ctx| {
            env.define_env(lua_ctx)
        }).expect("can define context");
        env
    }

    fn define_env(&self, lua_ctx: rlua::Context) -> Result<(), String> {
        let decl_env = DeclEnv {
            lua_ctx: &lua_ctx,
            job_ref: &self.job,
        };

        let hello = decl_env.create_function("hello", |_, _, ()| {
            eprintln!("hello from lua!!!");
            Ok(())
        })?;

        let build = decl_env.create_function("build", move |_, job_ref, (command, params): (LuaValue, LuaValue)| {
            lua_exports::build_command_impl(command, params, job_ref)
        })?;

        let metric = decl_env.create_function("metric", move |_, job_ref, (name, value): (String, String)| {
            lua_exports::metric(name, value, job_ref)
        })?;

        let now_ms = decl_env.create_function("now_ms", move |_, job_ref, ()| Ok(crate::io::now_ms()))?;

        let artifact = decl_env.create_function("artifact", move |_, job_ref, (path, name): (String, Option<String>)| {
            lua_exports::artifact(path, name, job_ref)
        })?;

        let error = decl_env.create_function("error", move |_, job_ref, msg: String| {
            Err::<(), LuaError>(LuaError::RuntimeError(format!("explicit error: {}", msg)))
        })?;

        let path_has_cmd = decl_env.create_function("path_has_cmd", move |_, job_ref, name: String| {
            lua_exports::has_cmd(&name)
        })?;

        let size_of_file = decl_env.create_function("size_of_file", move |_, job_ref, name: String| {
            lua_exports::file_size(&name)
        })?;

        let build_environment = lua_ctx.create_table_from(
            vec![
                ("has", path_has_cmd),
                ("size", size_of_file),
            ]
        ).unwrap();

        let build_functions = lua_ctx.create_table_from(
            vec![
                ("hello", hello),
                ("run", build),
                ("metric", metric),
                ("error", error),
                ("artifact", artifact),
                ("now_ms", now_ms),
            ]
        ).unwrap();
        build_functions.set("environment", build_environment).unwrap();
        let globals = lua_ctx.globals();
        globals.set("Build", build_functions).unwrap();


        let step_start = decl_env.create_function("step_start", move |_, job_ref, name: String| {
            lua_exports::step::start(job_ref, name)
        })?;

        let step_push = decl_env.create_function("step_push", move |_, job_ref, name: String| {
            lua_exports::step::push(job_ref, name)
        })?;

        let step_advance = decl_env.create_function("step_advance", move |_, job_ref, name: String| {
            lua_exports::step::advance(job_ref, name)
        })?;

        let step_functions = lua_ctx.create_table_from(
            vec![
                ("start", step_start),
                ("push", step_push),
                ("advance", step_advance),
            ]
        ).unwrap();
        globals.set("Step", step_functions).unwrap();
        Ok(())
    }

    pub async fn run_build(self, script: &[u8]) -> Result<(), LuaError> {
        let script = script.to_vec();
        let res: Result<(), LuaError> = tokio::task::spawn_blocking(|| {
            std::thread::spawn(move || {
                self.lua.context(|lua_ctx| {
                    lua_ctx.load(&script)
                        .set_name("goodfile")?
                        .exec()
                })
            }).join().unwrap()
        }).await.unwrap();
        eprintln!("lua res: {:?}", res);
        res
    }
}
