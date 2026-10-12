use anyhow::{bail, Context, Result};
use dbfork_config::{Config, Environment, Lock, Project, State};
use dbfork_model::{CloneRequest, DatabaseConfig, Engine, Resource, Strategy};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::{collections::BTreeMap, fs, path::{Path,PathBuf}, time::{SystemTime,UNIX_EPOCH}};

pub struct App {
    pub root:PathBuf, pub config:Config, pub state:State,
    pub state_dir:PathBuf, config_root:PathBuf, _lock:Lock,
}
fn layout(dir:&Path)->Result<(PathBuf,PathBuf,PathBuf)> {
    let git=dbfork_git::discover(dir)?;
    let state_dir=git.common_dir.as_ref().map(|p|p.join("dbfork-proto")).unwrap_or_else(||git.root.join(".dbfork"));
    let mut config=git.root.join(".dbfork.toml");
    if !config.exists() && git.common_dir.is_some() {
        for tree in dbfork_git::worktrees(&git.root)? { let candidate=tree.path.join(".dbfork.toml"); if candidate.exists() {config=candidate;break} }
    }
    Ok((git.root,state_dir,config))
}

pub fn init(dir:&Path,engine:Engine,provider:String,connection_env:String,source_env:Option<String>)->Result<Value> {
    let (root,state_dir,path)=layout(dir)?;
    let _lock=Lock::acquire(&state_dir)?;
    if path.exists() { bail!("project already has .dbfork.toml") }
    let caps=dbfork_providers::capabilities(&provider)?;
    if !caps.engines.contains(&engine) {bail!("provider does not support this engine")}
    let config=Config {
        project:Project { name:root.file_name().unwrap_or_default().to_string_lossy().into(), default_branch:"main".into(), env_files:vec![".env".into()], protected_branches:vec!["main".into(),"master".into(),"production".into(),"prod".into()] },
        databases:vec![DatabaseConfig { name:"primary".into(),engine,provider,connection_env:connection_env.clone(),source_env:source_env.clone(),strategy:Strategy::Full,seed:vec![],options:BTreeMap::new() }],
    };
    config.validate()?;
    dbfork_env::ensure_ignored(&root,Path::new(".env"))?;
    dbfork_env::ensure_ignored(&root,Path::new(".dbfork"))?;
    let mut state=State {version:1,project_id:uuid::Uuid::new_v4().simple().to_string(),sources:BTreeMap::new(),environments:BTreeMap::new(),bindings:BTreeMap::new()};
    let env=dbfork_env::read(&root.join(".env"))?;
    let key=source_env.as_ref().unwrap_or(&connection_env);
    let source=std::env::var(key).ok().or_else(||env.get(key).cloned());
    if let Some(mut source)=source {
        if engine==Engine::Sqlite {source=absolute_sqlite(&source,&root)?;}
        state.sources.insert("primary".into(),Resource {database:"primary".into(),engine,provider:config.databases[0].provider.clone(),id:"source".into(),connection_url:source,strategy:Strategy::Full,metadata:BTreeMap::new()});
    } else if engine==Engine::Sqlite {
        let source_path=state_dir.join("source.sqlite");
        dbfork_config::private_write(&source_path,b"")?;
        let value=format!("sqlite://{}",source_path.display());
        dbfork_env::update(&root.join(".env"),&BTreeMap::from([(connection_env.clone(),value.clone())]))?;
        state.sources.insert("primary".into(),Resource {database:"primary".into(),engine,provider:"local".into(),id:"source".into(),connection_url:value,strategy:Strategy::Full,metadata:BTreeMap::new()});
    }
    state.save(&state_dir.join("state.json"))?;
    config.save(&root.join(".dbfork.toml"))?;
    Ok(json!({"initialized":true,"config":root.join(".dbfork.toml"),"next":"Set source connection variables, then run dbfork-proto create <branch>."}))
}

impl App {
    pub fn open(dir:&Path)->Result<Self> {
        let (root,state_dir,config_path)=layout(dir)?;
        if !config_path.exists() {bail!("no .dbfork.toml found; run dbfork-proto init")}
        let config=Config::load(&config_path)?;
        let lock=Lock::acquire(&state_dir)?;
        let state=State::load(&state_dir.join("state.json"))?;
        let config_root=config_path.parent().context("configuration has no parent")?.to_owned();
        Ok(Self {root,config,state,state_dir,config_root,_lock:lock})
    }
    fn save(&self)->Result<()> {self.state.save(&self.state_dir.join("state.json"))}
    fn protected(&self,branch:&str)->bool {branch==self.config.project.default_branch || self.config.project.protected_branches.iter().any(|p|p==branch)}
    fn load_sources(&mut self)->Result<()> {
        let mut env=BTreeMap::new();
        for file in &self.config.project.env_files {env.extend(dbfork_env::read(&self.config_root.join(file))?)}
        for db in &self.config.databases {
            if let Some(source)=self.state.sources.get(&db.name) {
                if source.engine!=db.engine || source.provider!=db.provider {bail!("database engine/provider changed; use a new project state")}
                continue
            }
            let key=db.source_env.as_ref().unwrap_or(&db.connection_env);
            let mut source=std::env::var(key).ok().or_else(||env.get(key).cloned()).with_context(||format!("source variable {key} is missing"))?;
            if db.engine==Engine::Sqlite { source=absolute_sqlite(&source,&self.config_root)?; }
            if self.state.environments.values().flat_map(|e|&e.resources).any(|r|r.connection_url==source) {bail!("source variable points at an owned branch database")}
            self.state.sources.insert(db.name.clone(),Resource {database:db.name.clone(),engine:db.engine,provider:db.provider.clone(),id:"source".into(),connection_url:source,strategy:db.strategy,metadata:db.options.clone()});
        }
        self.save()
    }
    pub async fn create(&mut self,branch:&str,parent:Option<&str>,strategy:Option<Strategy>,activate:bool)->Result<Value> {
        validate_branch(branch)?;
        if self.protected(branch) {bail!("refusing to create or replace a protected source branch")}
        if let Some(env)=self.state.environments.get(branch) {
            if env.status!="ready" {bail!("environment has an incomplete operation; delete it with --yes before retrying")}
            if parent.is_some_and(|p|p!=env.parent) || strategy.is_some_and(|s|env.resources.iter().any(|r|r.strategy!=s)) {bail!("environment already exists with a different parent or strategy")}
            if activate {self.switch(branch).await?;}
            return Ok(json!({"branch":branch,"created":false,"status":"ready"}))
        }
        self.load_sources()?;
        let parent=parent.unwrap_or(&self.config.project.default_branch).to_owned();
        if parent==branch {bail!("a branch cannot clone itself")}
        let sources=if parent==self.config.project.default_branch {self.state.sources.clone()} else {
            let env=self.state.environments.get(&parent).context("parent environment does not exist")?;
            if env.status!="ready" {bail!("parent environment is not ready")}
            env.resources.iter().map(|r|(r.database.clone(),r.clone())).collect()
        };
        let mut requests=Vec::new();
        for db in &self.config.databases {
            let strategy=strategy.unwrap_or(db.strategy);
            let caps=dbfork_providers::capabilities(&db.provider)?;
            if !caps.engines.contains(&db.engine) || !caps.strategies.contains(&strategy) {bail!("provider {} does not support {:?} with {:?}",db.provider,db.engine,strategy)}
            if !provider_strategy(strategy) && !dbfork_engines::supported(db.engine,strategy) {bail!("engine {:?} does not support {:?}",db.engine,strategy)}
            let source=sources.get(&db.name).context("parent is missing a configured database")?.clone();
            let digest=Sha256::digest(format!("{}\0{}\0{}",self.state.project_id,branch,db.name));
            let target_id=format!("dbf_{:x}",digest)[..36].to_owned();
            requests.push(CloneRequest {config:db.clone(),source,target_id,data_dir:self.state_dir.join("databases"),project_dir:self.root.clone(),strategy});
        }
        self.state.environments.insert(branch.into(),Environment {branch:branch.into(),parent:parent.clone(),status:"creating".into(),resources:vec![],created_at:SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()});
        self.save()?;
        for request in requests {
            let result=if provider_strategy(request.strategy) { dbfork_providers::create(&request).await } else {dbfork_engines::create(&request).await};
            match result {
                Ok(mut resource)=>{
                    resource.metadata.insert("owner_project".into(),self.state.project_id.clone());
                    self.state.environments.get_mut(branch).unwrap().resources.push(resource);
                    if let Err(e)=self.save() {let _=self.rollback(branch).await;return Err(e.context("could not persist created resource"))}
                },
                Err(e)=>{
                    let cleanup=self.rollback(branch).await;
                    if cleanup.is_err() {bail!("clone failed; rollback is incomplete. State retains owned resources; retry delete --yes")}
                    return Err(e.context("database clone failed"))
                }
            }
        }
        let resources=self.state.environments[branch].resources.clone();
        for resource in &resources {
            if let Err(e)=validate(resource).await {let _=self.rollback(branch).await;return Err(e.context("cloned database validation failed"))}
        }
        self.state.environments.get_mut(branch).unwrap().status="ready".into();self.save()?;
        if activate {
            if let Err(error)=self.switch(branch).await {
                self.rollback(branch).await.context("environment update failed and resource rollback needs retry via delete --yes")?;
                return Err(error);
            }
        }
        Ok(json!({"branch":branch,"parent":parent,"created":true,"status":"ready","databases":resources.iter().map(public_resource).collect::<Vec<_>>() }))
    }
    async fn rollback(&mut self,branch:&str)->Result<()> {
        let resources=self.state.environments[branch].resources.clone();
        self.state.environments.get_mut(branch).unwrap().status="cleanup_required".into();self.save()?;
        let mut failed=false;
        for resource in resources.iter().rev() {
            self.check_owned(resource)?;
            if destroy(resource).await.is_ok() {
                self.state.environments.get_mut(branch).unwrap().resources.retain(|r|r.id!=resource.id); self.save()?;
            } else {failed=true;}
        }
        if failed {bail!("cleanup failed; retained resources remain in state")}
        self.state.environments.remove(branch);self.save()
    }
    fn check_owned(&self,r:&Resource)->Result<()> {
        if !r.id.starts_with("dbf_") || r.metadata.get("owner_project")!=Some(&self.state.project_id) || self.state.sources.values().any(|s|s.connection_url==r.connection_url || s.id==r.id) {bail!("refusing operation on an unowned or source resource")} Ok(())
    }
    pub async fn switch(&mut self,branch:&str)->Result<Value> {
        self.load_sources()?;
        let resources=if branch==self.config.project.default_branch {self.state.sources.values().cloned().collect::<Vec<_>>()} else {
            let env=self.state.environments.get(branch).context("environment does not exist")?;
            if env.status!="ready" {bail!("environment is not ready")}
            env.resources.clone()
        };
        let mut values=BTreeMap::new();
        for db in &self.config.databases {
            let resource=resources.iter().find(|r|r.database==db.name).context("environment is missing a configured database")?;
            values.insert(db.connection_env.clone(),resource.connection_url.clone());
        }
        let mut backups=Vec::new();
        for file in &self.config.project.env_files {
            let path=self.root.join(file);dbfork_config::reject_symlinks(&path)?;
            dbfork_env::ensure_ignored(&self.root,file)?;
            backups.push((path.clone(),if path.exists(){Some(fs::read(&path)?)}else{None}));
        }
        for (path,_) in &backups {
            if let Err(e)=dbfork_env::update(path,&values) {
                restore_files(&backups)?;return Err(e.context("environment file update failed; original files restored"))
            }
        }
        let old=self.state.bindings.insert(self.root.clone(),branch.into());
        if let Err(e)=self.save() {
            restore_files(&backups)?;
            if let Some(old)=old {self.state.bindings.insert(self.root.clone(),old);} else {self.state.bindings.remove(&self.root);}
            return Err(e)
        }
        Ok(json!({"branch":branch,"worktree":self.root,"env_files":self.config.project.env_files}))
    }
    pub async fn delete(&mut self,branch:&str)->Result<Value> {
        if self.protected(branch) {bail!("refusing to delete a protected source branch")}
        if !self.state.environments.contains_key(branch) {return Ok(json!({"branch":branch,"deleted":false}))}
        if self.state.bindings.iter().any(|(path,b)|b==branch && path.exists()) {bail!("environment is active in a worktree; switch that worktree to the default branch first")}
        self.rollback(branch).await?;
        self.state.bindings.retain(|_,b|b!=branch);self.save()?;
        Ok(json!({"branch":branch,"deleted":true}))
    }
    pub fn list(&self)->Value {json!({"environments":self.state.environments.values().map(|e|json!({"branch":e.branch,"parent":e.parent,"status":e.status,"created_at":e.created_at,"databases":e.resources.iter().map(public_resource).collect::<Vec<_>>(),"worktrees":self.state.bindings.iter().filter(|(_,b)|**b==e.branch).map(|(p,_)|p).collect::<Vec<_>>()})).collect::<Vec<_>>()})}
    pub async fn status(&self,check:bool)->Result<Value> {
        let git=dbfork_git::discover(&self.root)?;
        let branch=self.state.bindings.get(&self.root);
        let mut health=vec![];
        if check {if let Some(e)=branch.and_then(|b|self.state.environments.get(b)) {for r in &e.resources {health.push(json!({"database":r.database,"healthy":validate(r).await.is_ok()}));}}}
        Ok(json!({"project":self.config.project.name,"worktree":self.root,"git_branch":git.branch,"environment":branch,"environments":self.state.environments.len(),"health":health}))
    }
    pub async fn sync(&mut self,all:bool)->Result<Value> {
        let roots=if all {dbfork_git::worktrees(&self.root)?.into_iter().map(|w|w.path).collect::<Vec<_>>()}else{vec![self.root.clone()]};
        let original=self.root.clone();let mut results=vec![];
        for root in roots {
            let git=dbfork_git::discover(&root)?;
            let Some(branch)=git.branch else {results.push(json!({"worktree":root,"skipped":"detached HEAD or manual project"}));continue};
            self.root=root;
            let result=if branch==self.config.project.default_branch {self.switch(&branch).await} else {self.create(&branch,None,None,true).await};
            self.root=original.clone();results.push(result?);
        }
        self.state.bindings.retain(|p,_|p.exists());self.save()?;
        Ok(json!({"synced":results}))
    }
}
fn restore_files(backups:&[(PathBuf,Option<Vec<u8>>)]) -> Result<()> {
    for (path,bytes) in backups {if let Some(bytes)=bytes {dbfork_config::private_write(path,bytes)?} else if path.exists(){fs::remove_file(path)?}} Ok(())
}
fn provider_strategy(s:Strategy)->bool {matches!(s,Strategy::Native|Strategy::Instance|Strategy::Snapshot)}
async fn destroy(r:&Resource)->Result<()> {if provider_strategy(r.strategy){dbfork_providers::delete(r).await}else{dbfork_engines::delete(r).await}}
async fn validate(r:&Resource)->Result<()> {if provider_strategy(r.strategy){dbfork_providers::validate(r).await}else{dbfork_engines::validate(r).await}}
fn public_resource(r:&Resource)->Value {json!({"database":r.database,"engine":r.engine,"provider":r.provider,"id":r.id,"strategy":r.strategy})}
fn validate_branch(branch:&str)->Result<()> {if branch.is_empty() || branch.len()>255 || branch.chars().any(|c|c.is_control()) {bail!("branch must be 1-255 bytes without control characters")}Ok(())}
fn absolute_sqlite(value:&str,root:&Path)->Result<String> {
    let path=value.strip_prefix("sqlite://").or_else(||value.strip_prefix("sqlite:")).unwrap_or(value);
    if path.contains('?') || path==":memory:" {bail!("SQLite sources must be persistent file paths without URL query options")}
    let p=Path::new(path);let p=if p.is_absolute(){p.to_owned()}else{root.join(p)};
    let p=p.canonicalize().context("SQLite source file does not exist")?;Ok(format!("sqlite://{}",p.display()))
}
