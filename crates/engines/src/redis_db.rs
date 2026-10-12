use super::*;
use redis::aio::MultiplexedConnection;
const OWNER: &str = "__dbfork_owner__";
async fn connect(url: &str) -> Result<MultiplexedConnection> {
    redis::Client::open(url).map_err(|_| anyhow::anyhow!("invalid Redis URL"))?.get_multiplexed_async_connection().await.map_err(|_| anyhow::anyhow!("Redis connection failed"))
}
fn safe(_: redis::RedisError) -> anyhow::Error { anyhow::anyhow!("Redis command failed; check permissions and server support") }
pub(crate) async fn create(request: &CloneRequest) -> Result<Resource> {
    let source = parse_url(&request.source.connection_url)?;
    let mut target = parse_url(request.config.options.get("target_server_url").unwrap_or(&request.source.connection_url))?;
    let index: u32 = request.config.options.get("target_db").context("Redis cloning requires options.target_db naming a dedicated empty logical database, or the Docker instance provider")?.parse().context("Redis target_db must be an unsigned integer")?;
    target.set_path(&format!("/{index}"));
    let source_index = source.path().trim_start_matches('/').parse::<u32>().unwrap_or(0);
    ensure!(!(target.host_str() == source.host_str() && target.port_or_known_default() == source.port_or_known_default() && index == source_index), "Redis source and target must be separate databases");
    ensure!(target.query().is_none() && source.query().is_none(), "Redis URL query overrides are unsupported");
    let r = resource(request, target.to_string());
    let mut dest = connect(&r.connection_url).await?;
    let mut src = connect(&request.source.connection_url).await?;
    // Host aliases must not make the source appear to be a separate instance.
    let source_info: String = redis::cmd("INFO").arg("server").query_async(&mut src).await.map_err(safe)?;
    let target_info: String = redis::cmd("INFO").arg("server").query_async(&mut dest).await.map_err(safe)?;
    let run_id = |info: &str| info.lines().find_map(|line| line.trim().strip_prefix("run_id:").map(str::to_owned));
    let source_run = run_id(&source_info).context("Redis server identity unavailable; cannot prove isolation")?;
    let target_run = run_id(&target_info).context("Redis server identity unavailable; cannot prove isolation")?;
    ensure!(source_run != target_run || index != source_index, "Redis source and target resolve to the same database");
    let claimed: i64 = redis::Script::new("if redis.call('DBSIZE') ~= 0 then return 0 end redis.call('SET', KEYS[1], ARGV[1]); return 1")
        .key(OWNER).arg(&r.id).invoke_async(&mut dest).await.map_err(safe)?;
    ensure!(claimed == 1, "Redis target database must be empty and dedicated to this clone");
    let result: Result<()> = async {
        if request.strategy == Strategy::Full {
            let mut cursor = 0u64;
            loop {
                let (next, keys): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN").arg(cursor).arg("COUNT").arg(500).query_async(&mut src).await.map_err(safe)?;
                for key in keys {
                    if key == OWNER.as_bytes() { continue; }
                    let start = std::time::Instant::now();
                    let record: Option<(Vec<u8>, i64)> = redis::Script::new("local data=redis.call('DUMP', KEYS[1]); if not data then return nil end return {data, redis.call('PTTL', KEYS[1])}").key(&key).invoke_async(&mut src).await.map_err(safe)?;
                    if let Some((data, ttl)) = record {
                        let remaining = if ttl > 0 { ttl - start.elapsed().as_millis() as i64 } else { ttl };
                        if remaining == -2 || (ttl > 0 && remaining <= 0) { continue; }
                        let _: () = redis::cmd("RESTORE").arg(&key).arg(remaining.max(0)).arg(data).arg("REPLACE").query_async(&mut dest).await.map_err(safe)?;
                    }
                }
                cursor = next;
                if cursor == 0 { break; }
            }
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        if delete(&r).await.is_err() { bail!("{error}; cleanup failed for Redis resource {}", r.id); }
        return Err(error);
    }
    Ok(r)
}
pub(crate) async fn validate(r: &Resource) -> Result<()> {
    let mut con = connect(&r.connection_url).await?;
    let owner: Option<String> = redis::cmd("GET").arg(OWNER).query_async(&mut con).await.map_err(safe)?;
    ensure!(owner.as_deref() == Some(&r.id), "Redis ownership marker missing or mismatched");
    Ok(())
}
pub(crate) async fn delete(r: &Resource) -> Result<()> {
    let mut con = connect(&r.connection_url).await?;
    let removed: i64 = redis::Script::new("if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end redis.call('FLUSHDB'); return 1")
        .key(OWNER).arg(&r.id).invoke_async(&mut con).await.map_err(safe)?;
    ensure!(removed == 1, "Redis ownership marker missing or mismatched; refusing cleanup");
    Ok(())
}
