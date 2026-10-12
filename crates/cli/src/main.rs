use clap::{Parser, Subcommand, ValueEnum};
use dbfork_model::{Engine,Strategy};
use serde_json::{json,Value};
use std::{io::{self,IsTerminal,Write},path::PathBuf};

#[derive(Parser)]
#[command(version,about="Create isolated database environments for branches and worktrees")]
struct Cli {
    #[arg(long,global=true,help="Emit JSON, including errors; never print connection credentials")] json:bool,
    #[arg(long,short='y',global=true,help="Confirm destructive operations without prompting")] yes:bool,
    #[arg(long,short='C',global=true,default_value=".")] directory:PathBuf,
    #[command(subcommand)] command:Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create a TOML configuration and private project state.
    Init {
        #[arg(long,value_enum,default_value="sqlite")] engine:EngineArg,
        #[arg(long,default_value="local")] provider:String,
        #[arg(long,default_value="DATABASE_URL")] connection_env:String,
        #[arg(long)] source_env:Option<String>,
    },
    /// Clone a source or existing environment, then update this worktree's env files.
    Create {
        branch:String,
        #[arg(long)] from:Option<String>,
        #[arg(long,conflicts_with="strategy")] schema_only:bool,
        #[arg(long,value_enum)] strategy:Option<StrategyArg>,
        #[arg(long,help="Create without updating this worktree's environment")] no_switch:bool,
    },
    /// Update this worktree's env files. Does not check out a Git branch.
    Switch {branch:String},
    List,
    Status {#[arg(long,help="Check active database connectivity")] check:bool},
    /// Delete owned resources. Active environments must be switched away first.
    Delete {branch:String},
    /// Create/switch environments to match Git branches. Never deletes databases.
    Sync {#[arg(long,help="Synchronize every Git worktree")] all:bool},
    /// Show the supported engines and strategies for a provider.
    Capabilities {#[arg(default_value="local")] provider:String},
    /// Manage automatic sync after Git branch checkout and worktree creation.
    Hooks {#[command(subcommand)] action:HookCommand},
}
#[derive(Subcommand)] enum HookCommand {Install,Uninstall}
#[derive(Clone,Copy,ValueEnum)] enum EngineArg {Postgres,Mysql,Sqlite,Mongodb,Redis}
impl From<EngineArg> for Engine {fn from(v:EngineArg)->Self {match v{EngineArg::Postgres=>Self::Postgres,EngineArg::Mysql=>Self::Mysql,EngineArg::Sqlite=>Self::Sqlite,EngineArg::Mongodb=>Self::Mongodb,EngineArg::Redis=>Self::Redis}}}
#[derive(Clone,Copy,ValueEnum)] enum StrategyArg {Full,SchemaOnly,Seeded,Template,Native,File,Instance,Snapshot}
impl From<StrategyArg> for Strategy {fn from(v:StrategyArg)->Self {match v{StrategyArg::Full=>Self::Full,StrategyArg::SchemaOnly=>Self::SchemaOnly,StrategyArg::Seeded=>Self::Seeded,StrategyArg::Template=>Self::Template,StrategyArg::Native=>Self::Native,StrategyArg::File=>Self::File,StrategyArg::Instance=>Self::Instance,StrategyArg::Snapshot=>Self::Snapshot}}}

#[tokio::main]
async fn main() {
    let cli=match Cli::try_parse() {
        Ok(cli)=>cli,
        Err(error)=>{
            if std::env::args().any(|a|a=="--json") && error.exit_code()!=0 {emit_error(true,2,"invalid_arguments",&error.to_string());}
            error.exit()
        }
    };
    if let Command::Delete{branch}=&cli.command {if !cli.yes {
        if cli.json || !io::stdin().is_terminal() {emit_error(cli.json,3,"confirmation_required","delete requires --yes in non-interactive mode");}
        eprint!("Delete database environment {branch:?}? [y/N] ");let _=io::stderr().flush();let mut reply=String::new();let _=io::stdin().read_line(&mut reply);
        if !matches!(reply.trim(),"y"|"Y"|"yes") {emit_error(cli.json,3,"confirmation_required","deletion cancelled");}
    }}
    match run(&cli).await {
        Ok(value)=>{if cli.json {println!("{}",json!({"ok":true,"result":value}));}else{println!("{}",serde_json::to_string_pretty(&value).unwrap());}},
        Err(e)=>emit_error(cli.json,1,"operation_failed",&redact(&e.to_string())),
    }
}
fn emit_error(json_output:bool,code:i32,kind:&str,message:&str)->! {
    if json_output {println!("{}",json!({"ok":false,"error":{"code":kind,"message":message}}));}else{eprintln!("error: {message}");}std::process::exit(code)
}
fn redact(message:&str)->String {message.split_whitespace().map(|word|if word.contains("://"){"[connection redacted]"}else{word}).collect::<Vec<_>>().join(" ")}
async fn run(cli:&Cli)->anyhow::Result<Value> {
    if let Command::Hooks{action}=&cli.command {
        match action {HookCommand::Install=>{let app=dbfork_core::App::open(&cli.directory)?;dbfork_git::install_hooks(&app.root,&std::env::current_exe()?)?;},HookCommand::Uninstall=>dbfork_git::uninstall_hooks(&cli.directory)?}
        return Ok(json!({"hooks":if matches!(action,HookCommand::Install){"installed"}else{"uninstalled"}}));
    }
    if let Command::Capabilities{provider}=&cli.command {return Ok(serde_json::to_value(dbfork_providers::capabilities(provider)?)?)}
    if let Command::Init{engine,provider,connection_env,source_env}=&cli.command {return dbfork_core::init(&cli.directory,(*engine).into(),provider.clone(),connection_env.clone(),source_env.clone())}
    let mut app=dbfork_core::App::open(&cli.directory)?;
    match &cli.command {
        Command::Create{branch,from,schema_only,strategy,no_switch}=>app.create(branch,from.as_deref(),if *schema_only{Some(Strategy::SchemaOnly)}else{strategy.map(Into::into)},!*no_switch).await,
        Command::Switch{branch}=>app.switch(branch).await,
        Command::List=>Ok(app.list()),
        Command::Status{check}=>app.status(*check).await,
        Command::Delete{branch}=>app.delete(branch).await,
        Command::Sync{all}=>app.sync(*all).await,
        _=>unreachable!(),
    }
}
