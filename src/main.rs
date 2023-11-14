use std::collections::HashMap;

use actix_web::{error, web, App, Error, HttpResponse, HttpServer};
use anyhow::Context;
use clap::Parser;
use dashmap::DashMap;
use env_logger::Env;
use redis::Commands;
use reqwest::Url;
use serde_json::{json, Value};
use std::env;

use crate::cli::Cli;
use crate::rpc_cache_handler::RpcCacheHandler;
use lazy_static::lazy_static;

mod cli;
mod rpc_cache_handler;

lazy_static! {
    static ref REDIS: redis::Client = {
        let redis_host = env::var("REDIS_HOST").unwrap_or_else(|_| "localhost".to_string());
        let redis_url = format!("redis://{}", redis_host);
        redis::Client::open(redis_url).expect("Failed to create Redis client")
    };
}

struct ChainState {
    rpc_url: Url,
    cache_entries: HashMap<String, CacheEntry>,
}

impl ChainState {
    fn new(rpc_url: Url) -> Self {
        Self {
            rpc_url,
            cache_entries: Default::default(),
        }
    }
}

pub type ChainStorePersistedCache = HashMap<String, DashMap<String, String>>;

struct CacheEntry {
    handler: Box<dyn RpcCacheHandler>,
}

impl CacheEntry {
    fn new(handler: Box<dyn RpcCacheHandler>) -> Self {
        Self { handler }
    }
}

#[derive(Default)]
struct AppState {
    chains: HashMap<String, ChainState>,
}

enum CacheStatus {
    NotAvailable,
    Cached(String, Value),
    Missed(String),
}

async fn request_rpc(rpc_url: Url, body: &Value) -> anyhow::Result<Value> {
    let client = reqwest::Client::new();

    let result = client
        .post(rpc_url)
        .json(body)
        .send()
        .await?
        .json::<Value>()
        .await?;

    Ok(result)
}

fn read_cache(handler: &dyn RpcCacheHandler, params: &Value) -> anyhow::Result<CacheStatus> {
    let cache_key = handler
        .extract_cache_key(params)
        .context("fail to extract cache key")?;

    let cache_key = match cache_key {
        Some(cache_key) => cache_key,
        None => return Ok(CacheStatus::NotAvailable),
    };

    let value: Option<String> = REDIS.get_connection().unwrap().get(&cache_key).unwrap();

    Ok(if let Some(value) = value {
        let cache_value =
            serde_json::from_str::<Value>(&value).context("fail to deserialize cache value")?;
        CacheStatus::Cached(cache_key, cache_value)
    } else {
        CacheStatus::Missed(cache_key)
    })
}

#[actix_web::post("/{chain}")]
async fn rpc_call(
    path: web::Path<(String,)>,
    data: web::Data<AppState>,
    body: web::Json<Value>,
) -> Result<HttpResponse, Error> {
    let (chain,) = path.into_inner();
    let chain_state = data
        .chains
        .get(&chain.to_uppercase())
        .ok_or_else(|| error::ErrorNotFound("endpoint not supported"))?;

    let requests = if let Some(requests) = body.as_array() {
        requests.to_vec()
    } else {
        vec![body.0]
    };

    let mut request_result = HashMap::new();
    let mut uncached_requests = HashMap::new();
    let mut ordered_id = vec![];

    for request in &requests {
        let id = request["id"]
            .as_u64()
            .ok_or_else(|| error::ErrorBadRequest("id not found"))?;
        let method = request["method"]
            .as_str()
            .ok_or_else(|| error::ErrorBadRequest("method not found"))?;
        let params = &request["params"];

        ordered_id.push(id);

        let cache_entry = match chain_state.cache_entries.get(method) {
            Some(cache_entry) => cache_entry,
            None => {
                uncached_requests.insert(id, (method.to_string(), params.clone(), None));
                continue;
            }
        };

        let result = read_cache(cache_entry.handler.as_ref(), params);

        match result {
            Err(err) => {
                log::error!("fail to read cache because: {}", err);
                uncached_requests.insert(id, (method.to_string(), params.clone(), None));
            }
            Ok(CacheStatus::NotAvailable) => {
                log::info!("cache not available for method {}", method);
                uncached_requests.insert(id, (method.to_string(), params.clone(), None));
            }
            Ok(CacheStatus::Cached(cache_key, value)) => {
                log::info!("cache hit for method {} with key {}", method, cache_key);
                request_result.insert(id, value);
            }
            Ok(CacheStatus::Missed(cache_key)) => {
                log::info!("cache missed for method {} with key {}", method, cache_key);
                uncached_requests.insert(id, (method.to_string(), params.clone(), Some(cache_key)));
            }
        }
    }

    if !uncached_requests.is_empty() {
        let request_body = Value::Array(
            uncached_requests
                .iter()
                .map(|(id, (method, params, _))| {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id.clone(),
                        "method": method.to_string(),
                        "params": params.clone(),
                    })
                })
                .collect::<Vec<Value>>(),
        );

        let rpc_result = request_rpc(chain_state.rpc_url.clone(), &request_body)
            .await
            .map_err(|err| {
                log::error!("fail to make rpc request because: {}", err);
                error::ErrorInternalServerError(format!(
                    "fail to make rpc request because: {}",
                    err
                ))
            })?;

        let rpc_result = rpc_result.as_array().ok_or_else(|| {
            log::error!("invalid rpc response: {}", rpc_result.to_string());
            error::ErrorInternalServerError("invalid rpc response")
        })?;

        for response in rpc_result {
            let id = response["id"]
                .as_u64()
                .ok_or_else(|| error::ErrorBadRequest("id not found"))?;
            let (method, params, cache_key) = uncached_requests.get(&id).unwrap();

            let error = &response["error"];
            if !error.is_null() {
                log::error!(
                    "rpc error: {}, request: {}({}), response: {}",
                    error.to_string(),
                    method,
                    params.to_string(),
                    response.to_string()
                );
                return Err(error::ErrorInternalServerError("remote rpc error"));
            }

            let result = &response["result"];
            request_result.insert(id, result.clone());

            let cache_key = match cache_key {
                Some(cache_key) => cache_key.clone(),
                None => continue,
            };

            let cache_entry = chain_state.cache_entries.get(method).unwrap();

            let (can_cache, extracted_value) = cache_entry
                .handler
                .extract_cache_value(result)
                .expect("fail to extract cache value");

            if can_cache {
                let value = extracted_value.as_str();
                let _ = REDIS
                    .get_connection()
                    .unwrap()
                    .set::<_, _, String>(cache_key.clone(), value);
            }
        }
    }

    let response = ordered_id
        .iter()
        .map(|id| {
            let result = request_result
                .get(id)
                .unwrap_or_else(|| panic!("result for id {} not found", id));

            json!({ "jsonrpc": "2.0", "id": id, "result": result })
        })
        .collect::<Vec<Value>>();

    Ok(HttpResponse::Ok().json(if response.len() == 1 {
        response[0].clone()
    } else {
        Value::Array(response)
    }))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let arg = Cli::parse();

    env_logger::init_from_env(Env::default().default_filter_or("info"));

    let mut app_state = AppState::default();
    let handler_factories = rpc_cache_handler::all_factories();

    log::info!("Provisioning cache tables");

    for (name, rpc_url) in arg.endpoints.iter() {
        log::info!("Adding endpoint {} linked to {}", name, rpc_url);

        let mut chain_state = ChainState::new(rpc_url.clone());

        for factory in &handler_factories {
            let handler = factory();
            chain_state
                .cache_entries
                .insert(handler.method_name().to_string(), CacheEntry::new(handler));
        }

        app_state.chains.insert(name.to_string(), chain_state);
    }

    let app_state = web::Data::new(app_state);

    log::info!("Server listening on {}:{}", arg.bind, arg.port);

    {
        let app_state = app_state.clone();

        HttpServer::new(move || App::new().service(rpc_call).app_data(app_state.clone()))
            .bind((arg.bind, arg.port))?
            .run()
            .await?;
    }

    log::info!("Server stopped");

    Ok(())
}
