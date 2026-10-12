use super::*;
use std::{fs, io::Write};
async fn shell(url: &str, script: &str, source: Option<&str>) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    writeln!(file, "const db = connect(process.env.DBFORK_MONGO_URL);\n{script}")?;
    let mut c = Command::new("mongosh");
    c.args(["--nodb", "--quiet", "--file"]).arg(file.path()).env("DBFORK_MONGO_URL", url).stdin(Stdio::null());
    if let Some(source) = source { c.env("DBFORK_MONGO_SOURCE", source); }
    run(&mut c, "mongosh").await
}
fn config(url: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    // JSON is valid YAML; config files keep the URI out of process arguments.
    write!(file, "{}", serde_json::json!({"uri": url}))?;
    Ok(file)
}
pub(crate) async fn create(request: &CloneRequest) -> Result<Resource> {
    seed_files(request)?;
    let target = destination(request)?;
    let source = parse_url(&request.source.connection_url)?;
    let source_name = database_name(&source)?;
    ensure!(source_name != request.target_id, "destination database name must differ from source");
    ensure!(!source_name.contains(['*', '$']), "unsupported MongoDB database name");
    let r = resource(request, target.to_string());
    shell(&r.connection_url, &format!("if (db.getCollectionNames().length) throw new Error('destination already exists'); db._dbfork_owner.insertOne({{_id:'owner', id:{}}});", serde_json::to_string(&r.id)?), None).await?;
    let result: Result<()> = async {
        if request.strategy == Strategy::Full {
            let archive = tempfile::NamedTempFile::new()?;
            let source_config = config(source.as_str())?;
            let mut dump = Command::new("mongodump");
            dump.arg("--config").arg(source_config.path()).arg(format!("--archive={}", archive.path().display())).arg("--excludeCollection=_dbfork_owner").stdin(Stdio::null());
            run(&mut dump, "mongodump").await?;
            let target_config = config(target.as_str())?;
            let mut restore = Command::new("mongorestore");
            restore.arg("--config").arg(target_config.path()).arg(format!("--archive={}", archive.path().display())).arg(format!("--nsInclude={source_name}.*")).arg(format!("--nsFrom={source_name}.*")).arg(format!("--nsTo={}.*", r.id)).arg("--stopOnError").stdin(Stdio::null());
            run(&mut restore, "mongorestore").await?;
        } else {
            shell(&r.connection_url, r#"
const source = connect(process.env.DBFORK_MONGO_SOURCE);
const collections = source.getCollectionInfos().filter(c => !c.name.startsWith('system.') && c.name !== '_dbfork_owner');
for (const c of collections.filter(c => c.type !== 'view')) {
  db.createCollection(c.name, c.options || {});
  for (const index of source.getCollection(c.name).getIndexes()) {
    if (index.name === '_id_') continue;
    const { key, ns, v, ...options } = index;
    db.getCollection(c.name).createIndex(key, options);
  }
}
for (const c of collections.filter(c => c.type === 'view')) db.createCollection(c.name, c.options);
"#, Some(source.as_str())).await?;
        }
        if request.strategy == Strategy::Seeded {
            for file in seed_files(request)? { shell(&r.connection_url, &fs::read_to_string(file).context("cannot read MongoDB seed script")?, None).await?; }
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        if delete(&r).await.is_err() { bail!("{error}; cleanup failed for MongoDB database {}", r.id); }
        return Err(error);
    }
    Ok(r)
}
fn ownership_script(r: &Resource) -> Result<String> {
    ensure!(database_name(&parse_url(&r.connection_url)?)? == r.id, "MongoDB database does not match owned resource");
    Ok(format!("const owner = db._dbfork_owner.findOne({{_id:'owner'}}); if (!owner || owner.id !== {}) throw new Error('ownership mismatch');", serde_json::to_string(&r.id)?))
}
pub(crate) async fn validate(r: &Resource) -> Result<()> { shell(&r.connection_url, &ownership_script(r)?, None).await }
pub(crate) async fn delete(r: &Resource) -> Result<()> {
    shell(&r.connection_url, &format!("{}\ndb.dropDatabase();", ownership_script(r)?), None).await
}
