use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lru::LruCache;
use serde_json::{Map, Value, json};
use spargebra::algebra::{Expression, Function, PropertyPathExpression as Ppe};
use spargebra::term::{
    GroundQuad, GroundTerm, Literal, NamedNode, NamedOrBlankNode, Quad, Term as SparqlTerm,
    Variable,
};
use spargebra::{Query as SparqlQuery, SparqlParser};
use tokio_stream::wrappers::ReceiverStream;

use crate::hypertrie::{
    Dictionary, GraphPattern, HybridEngine, NULL_ID, PatternTerm, RowBlock, TermType,
    TriplePattern, TripleStore, max_result_rows,
};
use crate::wal::Wal;

const DEFAULT_CACHE_SIZE: usize = 256;

/// Shared application state: the loaded triple store plus the query engine.
pub struct AppState {
    pub store: RwLock<TripleStore>,
    pub engine: HybridEngine,
    /// Query string -> fully serialized JSON response body.
    pub cache: Mutex<LruCache<String, String>>,
    /// Optional write-ahead log. If set, updates are logged durably
    /// (append + fsync) so they survive a restart.
    pub wal: Option<Mutex<crate::wal::Wal>>,
}

impl AppState {
    pub fn new(store: TripleStore) -> Self {
        Self::with_wal(store, None)
    }

    pub fn with_wal(store: TripleStore, wal: Option<crate::wal::Wal>) -> Self {
        Self {
            store: RwLock::new(store),
            engine: HybridEngine::new(),
            cache: Mutex::new(LruCache::new(
                std::num::NonZeroUsize::new(DEFAULT_CACHE_SIZE).unwrap(),
            )),
            wal: wal.map(Mutex::new),
        }
    }

    fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    /// Whether `query_str` has to run under the write lock. Deciding needs the
    /// dictionary, so the read lock is taken and released before upgrading.
    fn needs_write(&self, query_str: &str) -> bool {
        let store = self.store.read().unwrap_or_else(|e| e.into_inner());
        query_needs_write(query_str, &store)
    }

    /// Runs `f` under the write lock and logs whatever it interned to the WAL
    /// (see [`log_interned_since`]).
    fn write_locked<T>(
        &self,
        f: impl FnOnce(&mut TripleStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
        // Same order as `update_handler` takes the two locks.
        let mut wal = self
            .wal
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        let first_new_id = store.dict.len();
        let result = f(&mut store);
        if let Some(w) = wal.as_deref_mut() {
            log_interned_and_sync(&store, first_new_id, w)?;
        }
        result
    }
}

/// Either lock guard on [`AppState::store`], for call sites (like
/// `stream_handler`) that pick read vs. write access at runtime depending on
/// whether the query needs to intern a `BIND`-computed value.
enum StoreGuard<'a> {
    Read(std::sync::RwLockReadGuard<'a, TripleStore>),
    Write(std::sync::RwLockWriteGuard<'a, TripleStore>),
}

impl StoreGuard<'_> {
    fn as_ref(&self) -> &TripleStore {
        match self {
            StoreGuard::Read(g) => g,
            StoreGuard::Write(g) => g,
        }
    }
}

/// Start the SPARQL HTTP endpoint on the given port (without durability).
pub async fn serve(store: TripleStore, port: u16) {
    serve_durable(store, port, None).await
}

/// Maximum accepted request-body size in bytes (guards against OOM from huge
/// POST bodies). Overridable via `TRILLIAN_MAX_BODY_BYTES`; default 64 MiB.
fn max_body_bytes() -> usize {
    use std::sync::OnceLock;
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("TRILLIAN_MAX_BODY_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64 * 1024 * 1024)
    })
}

/// Like [`serve`], but with an optional write-ahead log for durable updates.
pub async fn serve_durable(store: TripleStore, port: u16, wal: Option<crate::wal::Wal>) {
    let state = Arc::new(AppState::with_wal(store, wal));

    let app = Router::new()
        .route("/sparql", get(sparql_handler).post(sparql_handler))
        .route("/stream", get(stream_handler).post(stream_handler))
        .route("/count", get(count_handler).post(count_handler))
        .route("/update", post(update_handler))
        // Cap request bodies so a giant POST can't exhaust memory.
        .layer(DefaultBodyLimit::max(max_body_bytes()))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("could not bind to {addr}: {e}"));
    println!(
        "SPARQL endpoint listening on http://{}/sparql, /stream, /count, /update",
        addr
    );
    axum::serve(listener, app)
        .await
        .expect("HTTP server terminated unexpectedly");
}

#[derive(serde::Deserialize, Debug, Default)]
pub struct SparqlQueryParams {
    query: Option<String>,
    /// Enable RDFS inference via backward chaining. Supported values: `rdfs`.
    #[serde(alias = "infer")]
    infer: Option<String>,
}

#[derive(serde::Deserialize, Debug, Default)]
pub struct UpdateParams {
    update: Option<String>,
}

pub async fn sparql_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SparqlQueryParams>,
    body: String,
) -> Response {
    let query_str = normalize_query(params.query, body);
    if query_str.is_empty() {
        return sparql_error(
            "Missing query parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    // Cache lookup (finished JSON body).
    {
        if let Ok(cache) = state.cache.lock()
            && let Some(body) = cache.peek(&query_str)
        {
            return json_response(body.clone());
        }
    }

    let infer = params.infer.as_deref();
    // A query that can put an unknown term into the result needs write access;
    // everything else keeps the fully concurrent read lock. The `infer != rdfs`
    // guard is what makes those unsupported under inference.
    let result = if infer != Some("rdfs") && state.needs_write(&query_str) {
        state.write_locked(|store| execute_sparql_bind(store, &state.engine, &query_str))
    } else {
        let store = state.store.read().unwrap_or_else(|e| e.into_inner());
        if infer == Some("rdfs") {
            execute_sparql_infer(&store, &state.engine, &query_str)
        } else {
            execute_sparql(&store, &state.engine, &query_str)
        }
    };
    match result {
        Ok(body) => {
            if let Ok(mut cache) = state.cache.lock() {
                cache.put(query_str, body.clone());
            }
            json_response(body)
        }
        Err(e) => sparql_error(&e, StatusCode::BAD_REQUEST),
    }
}

pub async fn stream_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SparqlQueryParams>,
    body: String,
) -> Response {
    let query_str = normalize_query(params.query, body);
    if query_str.is_empty() {
        return sparql_error(
            "Missing query parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    // The stream materializes the results internally, but sends them chunked as
    // NDJSON before the full JSON body is built.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(128);
    let state = Arc::clone(&state);
    let infer = params.infer.clone();

    tokio::task::spawn_blocking(move || {
        // A query that can put an unknown term into the result needs write access.
        let needs_write = infer.as_deref() != Some("rdfs") && state.needs_write(&query_str);
        let mut guard = if needs_write {
            StoreGuard::Write(state.store.write().unwrap_or_else(|e| e.into_inner()))
        } else {
            StoreGuard::Read(state.store.read().unwrap_or_else(|e| e.into_inner()))
        };
        // Same lock order as `update_handler`. The guard has to outlive the
        // serialization below, so `AppState::write_locked` does not fit here.
        let mut wal = needs_write
            .then(|| {
                state
                    .wal
                    .as_ref()
                    .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
            })
            .flatten();
        let first_new_id = guard.as_ref().dict.len();
        let mut result = match &mut guard {
            StoreGuard::Write(store) => evaluate_select_bind(store, &state.engine, &query_str),
            StoreGuard::Read(store) if infer.as_deref() == Some("rdfs") => {
                evaluate_select_infer(store, &state.engine, &query_str)
            }
            StoreGuard::Read(store) => evaluate_select(store, &state.engine, &query_str),
        };
        if let Some(w) = wal.as_deref_mut()
            && let Err(e) = log_interned_and_sync(guard.as_ref(), first_new_id, w)
        {
            result = Err(e);
        }
        drop(wal);
        let dict = &guard.as_ref().dict;
        match result {
            Ok(select) => {
                let var_order = select.var_order;
                let vars = select.vars;
                // Column index per SELECT variable; `None` if it is not in the
                // result (omit it rather than silently reading column 0).
                let var_indices: Vec<Option<usize>> = vars
                    .iter()
                    .map(|var| var_order.iter().position(|v| v == var))
                    .collect();

                // Header line with the variable names as an NDJSON object.
                let header = json!({ "head": { "vars": vars } }).to_string();
                let _ = tx.blocking_send(Ok(Bytes::from(header + "\n")));

                for row in select.rows.rows() {
                    let obj: Map<String, Value> = vars
                        .iter()
                        .enumerate()
                        .filter_map(|(i, var)| {
                            let id = row[var_indices[i]?];
                            Some((var.clone(), term_to_json(id, dict)))
                        })
                        .collect();
                    let line = serde_json::to_string(&obj).unwrap_or_default() + "\n";
                    if tx.blocking_send(Ok(Bytes::from(line))).is_err() {
                        break;
                    }
                }
            }
            Err(e) => {
                let err = json!({ "error": e }).to_string() + "\n";
                let _ = tx.blocking_send(Ok(Bytes::from(err)));
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    let body = Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .body(body)
        .unwrap()
}

pub async fn count_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SparqlQueryParams>,
    body: String,
) -> Response {
    let query_str = normalize_query(params.query, body);
    if query_str.is_empty() {
        return sparql_error(
            "Missing query parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    let infer = params.infer.as_deref();
    let result = if infer != Some("rdfs") && state.needs_write(&query_str) {
        state.write_locked(|store| execute_count_bind(store, &state.engine, &query_str))
    } else {
        let store = state.store.read().unwrap_or_else(|e| e.into_inner());
        if infer == Some("rdfs") {
            execute_count_infer(&store, &state.engine, &query_str)
        } else {
            execute_count(&store, &state.engine, &query_str)
        }
    };
    match result {
        Ok(result) => (StatusCode::OK, axum::Json(result)).into_response(),
        Err(e) => sparql_error(&e, StatusCode::BAD_REQUEST),
    }
}

pub async fn update_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UpdateParams>,
    body: String,
) -> Response {
    let update_str = params
        .update
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| body.trim().to_string());

    if update_str.is_empty() {
        return sparql_error(
            "Missing update parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    let mut store = state.store.write().unwrap_or_else(|e| e.into_inner());
    // Lock the WAL for the duration of the update (log durably).
    let mut wal_guard = state
        .wal
        .as_ref()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
    match execute_update(&mut store, &update_str, wal_guard.as_deref_mut()) {
        Ok(()) => {
            // Force the write-ahead log to disk BEFORE we report success.
            if let Some(w) = wal_guard.as_deref_mut()
                && let Err(e) = w.sync()
            {
                return sparql_error(
                    &format!("Update applied but WAL sync failed: {}", e),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
            drop(wal_guard);
            drop(store);
            state.clear_cache();
            (StatusCode::OK, axum::Json(json!({ "status": "ok" }))).into_response()
        }
        Err(e) => sparql_error(&e, StatusCode::BAD_REQUEST),
    }
}

fn normalize_query(query_param: Option<String>, body: String) -> String {
    query_param
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| body.trim().to_string())
}

/// Whether executing `query_str` needs write access, i.e. it can put a term
/// into the result the dictionary does not hold yet: `BIND` and a `GROUP BY`
/// aggregate always compute one, a `VALUES` table only when it names an unknown
/// term. A parse failure is reported by the read-locked path, so it is `false`
/// here rather than duplicating error handling.
///
/// Callers hold the read lock across this and release it before upgrading.
/// Terms are never removed, so the answer stays valid; one added in between
/// only costs a needless write lock.
fn query_needs_write(query_str: &str, store: &TripleStore) -> bool {
    match SparqlParser::new().parse_query(query_str) {
        Ok(SparqlQuery::Select { pattern, .. } | SparqlQuery::Ask { pattern, .. }) => {
            contains_extend(&pattern)
                || contains_group(&pattern)
                || contains_unknown_values_term(&pattern, &store.dict)
        }
        _ => false,
    }
}

fn sparql_error(msg: &str, status: StatusCode) -> Response {
    let body = json!({ "error": msg });
    (status, axum::Json(body)).into_response()
}

/// Builds the HTTP response from an already-serialized JSON body.
fn json_response(body: String) -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/sparql-results+json",
        )],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// SPARQL Query Execution
// ---------------------------------------------------------------------------

struct SelectResult {
    vars: Vec<String>,
    rows: RowBlock,
    var_order: Vec<String>,
}

/// Profiling: runs a SELECT query `runs` times and reports the median times of
/// the parse / eval (plan+join) / serialize (SPARQL-JSON) phases. This separates
/// where the time of large queries goes.
pub fn profile_query(store: &TripleStore, engine: &HybridEngine, query_str: &str, runs: usize) {
    use std::time::Instant;
    let (mut parse, mut eval, mut ser) = (Vec::new(), Vec::new(), Vec::new());
    let mut rows = 0usize;
    for _ in 0..runs {
        let t = Instant::now();
        let query = SparqlParser::new().parse_query(query_str).expect("parse");
        parse.push(t.elapsed().as_secs_f64() * 1000.0);
        let SparqlQuery::Select { pattern, .. } = query else {
            eprintln!("profile_query: SELECT only");
            return;
        };
        let m = peel_modifiers(&pattern);
        let t = Instant::now();
        let result = evaluate_select_with_modifiers(store, engine, &m).unwrap();
        eval.push(t.elapsed().as_secs_f64() * 1000.0);
        rows = result.rows.n_rows();
        let t = Instant::now();
        let _ = write_sparql_json(&result, store);
        ser.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let med = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let (p, e, s) = (med(parse), med(eval), med(ser));
    println!("=== Profile ({} runs, {} rows) ===", runs, rows);
    println!("  parse:     {:.3} ms", p);
    println!("  eval:      {:.3} ms  (plan + join + materialization)", e);
    println!("  serialize: {:.3} ms  (SPARQL-JSON)", s);
    println!("  total:     {:.3} ms", p + e + s);
}

fn evaluate_select(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<SelectResult, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    let SparqlQuery::Select { pattern, .. } = query else {
        return Err("Only SELECT queries are supported here".to_string());
    };

    let m = peel_modifiers(&pattern);
    evaluate_select_with_modifiers(store, engine, &m)
}

/// Mutable twin of [`evaluate_select`] for queries that have to intern a term.
fn evaluate_select_bind(
    store: &mut TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<SelectResult, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    let SparqlQuery::Select { pattern, .. } = query else {
        return Err("Only SELECT queries are supported here".to_string());
    };

    let m = peel_modifiers(&pattern);
    evaluate_select_with_modifiers_mut(store, engine, &m)
}

/// Executes a SPARQL `SELECT`/`ASK` query against the store and returns the
/// SPARQL-results JSON body (the same payload the HTTP `/sparql` endpoint
/// serves). Lets embedders and tests run queries without standing up the server.
///
/// Read-locked, so a `VALUES` term the store does not know has no ID and the
/// query fails here rather than losing the row (see [`eval_values`]).
/// [`execute_sparql_bind`] interns it instead, and is what the HTTP endpoints
/// route such a query to (see [`query_needs_write`]).
pub fn execute_sparql(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<String, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match query {
        SparqlQuery::Select { pattern, .. } => {
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(write_sparql_json(&result, store))
        }
        SparqlQuery::Ask { pattern, .. } => {
            // ASK over the full WHERE path (incl. OPTIONAL/FILTER/UNION) -> ≥1 solution?
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(format!(
                "{{\"head\":{{}},\"boolean\":{}}}",
                result.rows.n_rows() > 0
            ))
        }
        _ => Err("Only SELECT and ASK queries are supported".to_string()),
    }
}

/// Mutable twin of [`execute_sparql`] for queries that put a term into the
/// result which the dictionary does not hold yet: `BIND` and `GROUP BY`
/// aggregates compute one, a `VALUES` table may name one. Interning needs write
/// access to the store (see [`eval_where_mut`]).
///
/// A server calling this outside the HTTP handlers must log what it interned to
/// its write-ahead log — see [`log_interned_since`].
pub fn execute_sparql_bind(
    store: &mut TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<String, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match query {
        SparqlQuery::Select { pattern, .. } => {
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers_mut(store, engine, &m)?;
            Ok(write_sparql_json(&result, store))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers_mut(store, engine, &m)?;
            Ok(format!(
                "{{\"head\":{{}},\"boolean\":{}}}",
                result.rows.n_rows() > 0
            ))
        }
        _ => Err("Only SELECT and ASK queries are supported".to_string()),
    }
}

/// Execute a SPARQL query with RDFS inference enabled (backward chaining).
///
/// The query algebra is rewritten **after** parsing so that every `Bgp` node
/// is expanded via `Union` with branches that capture RDFS-derivable triples.
/// The stored index is never modified — inference is purely at query time.
///
/// See [`crate::inference`] for the supported rule set.
///
/// When `infer` is passed as a query parameter (e.g. `?infer=rdfs`) the HTTP
/// handlers call this function automatically.
///
/// **Limitation**, tracked in ROADMAP under "SPARQL features": inference only
/// routes through the read-locked [`eval_where`], so anything needing a term
/// the dictionary does not hold fails here — `BIND`, `GROUP BY` and an
/// aggregate sub-`SELECT` as "unsupported WHERE pattern", an unknown `VALUES`
/// term with the error from [`eval_values`]. One root cause: the callers skip
/// the write path whenever `infer=rdfs` is set. Fixing that fixes all four, so
/// widen the tests along with it.
pub fn execute_sparql_infer(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<String, String> {
    use spargebra::algebra::GraphPattern as GP;
    let mut query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match &mut query {
        SparqlQuery::Select { pattern, .. } => {
            let old = std::mem::replace(pattern, GP::Bgp { patterns: vec![] });
            *pattern = crate::inference::rewrite(old);
            let m = peel_modifiers(pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(write_sparql_json(&result, store))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let old = std::mem::replace(pattern, GP::Bgp { patterns: vec![] });
            *pattern = crate::inference::rewrite(old);
            let m = peel_modifiers(pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(format!(
                "{{\"head\":{{}},\"boolean\":{}}}",
                result.rows.n_rows() > 0
            ))
        }
        _ => Err("Only SELECT and ASK queries are supported".to_string()),
    }
}

fn evaluate_select_infer(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<SelectResult, String> {
    use spargebra::algebra::GraphPattern as GP;
    let mut query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    let SparqlQuery::Select { pattern, .. } = &mut query else {
        return Err("Only SELECT queries are supported here".to_string());
    };

    let old = std::mem::replace(pattern, GP::Bgp { patterns: vec![] });
    *pattern = crate::inference::rewrite(old);
    let m = peel_modifiers(pattern);
    evaluate_select_with_modifiers(store, engine, &m)
}

fn execute_count_infer(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<Value, String> {
    use spargebra::algebra::GraphPattern as GP;
    let mut query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match &mut query {
        SparqlQuery::Select { pattern, .. } => {
            let old = std::mem::replace(pattern, GP::Bgp { patterns: vec![] });
            *pattern = crate::inference::rewrite(old);
            let m = peel_modifiers(pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(json!({ "count": result.rows.n_rows() }))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let old = std::mem::replace(pattern, GP::Bgp { patterns: vec![] });
            *pattern = crate::inference::rewrite(old);
            let m = peel_modifiers(pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(json!({ "boolean": result.rows.n_rows() > 0 }))
        }
        _ => Err("Only SELECT and ASK queries are supported for /count".to_string()),
    }
}

/// SELECT modifiers (peeled off the algebra), plus the inner WHERE pattern.
struct Modifiers<'a> {
    where_pat: &'a spargebra::algebra::GraphPattern,
    projection: Option<Vec<String>>,
    distinct: bool,
    order_by: Vec<(&'a Expression, bool)>, // (expression, descending?)
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Peels Project/Distinct/Reduced/Slice/OrderBy off the algebra and returns the
/// inner WHERE pattern (Bgp/Filter/LeftJoin/Join/Union) + the modifiers.
///
/// One query level nests its modifiers in a fixed order, outermost first:
/// `Slice > Distinct/Reduced > Project > OrderBy`. Peeling therefore stops as
/// soon as a node does not sit strictly deeper in that order than the last one
/// peeled — such a node opens a **sub-`SELECT`**, whose modifiers belong to it
/// and not to this level. [`eval_where`] evaluates it as its own query.
///
/// This assumes spargebra emits a `Project` for every `SELECT`, `SELECT *`
/// included, so both sides of a boundary always reach `Project`'s rank.
fn peel_modifiers(pattern: &spargebra::algebra::GraphPattern) -> Modifiers<'_> {
    use spargebra::algebra::{GraphPattern as GP, OrderExpression};
    let mut projection = None;
    let mut distinct = false;
    let mut order_by = Vec::new();
    let mut limit = None;
    let mut offset = None;
    let mut cur = pattern;
    let mut last_rank = -1i8;
    loop {
        let rank = match cur {
            GP::Slice { .. } => 0,
            GP::Distinct { .. } | GP::Reduced { .. } => 1,
            GP::Project { .. } => 2,
            GP::OrderBy { .. } => 3,
            _ => break,
        };
        if rank <= last_rank {
            break; // start of a nested sub-SELECT
        }
        last_rank = rank;
        match cur {
            GP::Project { variables, inner } => {
                projection = Some(variables.iter().map(|v| v.as_str().to_string()).collect());
                cur = inner;
            }
            GP::Distinct { inner } | GP::Reduced { inner } => {
                distinct = true;
                cur = inner;
            }
            GP::Slice {
                inner,
                start,
                length,
            } => {
                offset = Some(*start);
                limit = *length;
                cur = inner;
            }
            GP::OrderBy { inner, expression } => {
                for oe in expression {
                    match oe {
                        OrderExpression::Asc(e) => order_by.push((e, false)),
                        OrderExpression::Desc(e) => order_by.push((e, true)),
                    }
                }
                cur = inner;
            }
            _ => break,
        }
    }
    Modifiers {
        where_pat: cur,
        projection,
        distinct,
        order_by,
        limit,
        offset,
    }
}

/// Recursively evaluates a WHERE pattern: BGP, FILTER, OPTIONAL (LeftJoin),
/// Join, UNION. Returns (rows, variable order).
fn eval_where(
    gp: &spargebra::algebra::GraphPattern,
    store: &TripleStore,
    engine: &HybridEngine,
    limit: Option<usize>,
) -> Result<(RowBlock, Vec<String>), String> {
    use spargebra::algebra::GraphPattern as GP;
    // `limit` may ONLY be pushed into a direct BGP. Across Join/LeftJoin/
    // Union/Filter/Path, early termination of the subtrees is not
    // result-preserving -> children get None, the limit applies post hoc.
    match gp {
        GP::Bgp { patterns } => eval_bgp(patterns, store, engine, limit),
        GP::Filter { expr, inner } => {
            check_expr(expr)?;
            let (rows, vo) = eval_where(inner, store, engine, None)?;
            let mut kept = RowBlock::new(rows.n_vars());
            for row in rows.rows() {
                if row_passes(&[expr], row, &vo, store) {
                    kept.push_row(row);
                }
            }
            Ok((kept, vo))
        }
        GP::LeftJoin {
            left,
            right,
            expression,
        } => {
            let (lr, lvo) = eval_where(left, store, engine, None)?;
            let (rr, rvo) = eval_where(right, store, engine, None)?;
            hash_join(lr, &lvo, rr, &rvo, true, expression.as_ref(), store)
        }
        GP::Join { left, right } => {
            let (lr, lvo) = eval_where(left, store, engine, None)?;
            let (rr, rvo) = eval_where(right, store, engine, None)?;
            hash_join(lr, &lvo, rr, &rvo, false, None, store)
        }
        GP::Union { left, right } => {
            let (lr, lvo) = eval_where(left, store, engine, None)?;
            let (rr, rvo) = eval_where(right, store, engine, None)?;
            union_rows(lr, &lvo, rr, &rvo)
        }
        GP::Path {
            subject,
            path,
            object,
        } => eval_path(store, subject, path, object),
        GP::Minus { left, right } => {
            let (lr, lvo) = eval_where(left, store, engine, None)?;
            let (rr, rvo) = eval_where(right, store, engine, None)?;
            Ok(minus_rows(lr, &lvo, rr, &rvo))
        }
        GP::Values {
            variables,
            bindings,
        } => eval_values(variables, bindings, |t| {
            store.dict.lookup_term(&t.value, &t.typ)
        }),
        // Sub-SELECT: evaluated as its own query, so what it does not project
        // stays out of scope for the enclosing pattern.
        GP::Project { .. }
        | GP::Distinct { .. }
        | GP::Reduced { .. }
        | GP::Slice { .. }
        | GP::OrderBy { .. } => {
            let sub = peel_modifiers(gp);
            // `limit` is non-None only for a whole-body sub-SELECT, whose
            // rows map 1:1 onto the enclosing result -> the cap carries in.
            let (rows, vo) = eval_where(sub.where_pat, store, engine, pushdown_rows(&sub, limit))?;
            let r = apply_modifiers(&sub, rows, vo, store)?;
            Ok((r.rows, r.var_order))
        }
        _ => Err(
            "Unsupported WHERE pattern (only BGP/FILTER/OPTIONAL/UNION/Join/Path/MINUS/VALUES/sub-SELECT; use the mutable API for BIND/GROUP BY)".to_string(),
        ),
    }
}

/// Whether any node in `gp` satisfies `pred`. Every variant with children is
/// spelled out, so a new one in the algebra is a compile error here rather than
/// a silently unvisited subtree.
fn contains_node(
    gp: &spargebra::algebra::GraphPattern,
    pred: &impl Fn(&spargebra::algebra::GraphPattern) -> bool,
) -> bool {
    use spargebra::algebra::GraphPattern as GP;
    if pred(gp) {
        return true;
    }
    match gp {
        GP::Bgp { .. } | GP::Path { .. } | GP::Values { .. } => false,
        GP::Join { left, right }
        | GP::LeftJoin { left, right, .. }
        | GP::Union { left, right }
        | GP::Minus { left, right } => contains_node(left, pred) || contains_node(right, pred),
        GP::Filter { inner, .. }
        | GP::Extend { inner, .. }
        | GP::Graph { inner, .. }
        | GP::OrderBy { inner, .. }
        | GP::Project { inner, .. }
        | GP::Distinct { inner }
        | GP::Reduced { inner }
        | GP::Slice { inner, .. }
        | GP::Group { inner, .. }
        | GP::Service { inner, .. } => contains_node(inner, pred),
    }
}

/// Whether `gp` contains a `BIND` (`Extend`) node anywhere in the tree. BIND
/// materializes a value not yet in the dictionary, so queries containing it
/// need write access to the store (see [`eval_where_mut`]) instead of the
/// usual read lock.
fn contains_extend(gp: &spargebra::algebra::GraphPattern) -> bool {
    contains_node(gp, &|g| {
        matches!(g, spargebra::algebra::GraphPattern::Extend { .. })
    })
}

/// Whether `gp` contains a `GROUP BY` (`Group`) node anywhere in the tree.
/// Aggregate results are computed values that must be interned into the
/// dictionary, so such queries also need write access to the store.
fn contains_group(gp: &spargebra::algebra::GraphPattern) -> bool {
    contains_node(gp, &|g| {
        matches!(g, spargebra::algebra::GraphPattern::Group { .. })
    })
}

/// Whether `gp` contains a `VALUES` table naming a term `dict` does not hold —
/// the only case where the table forces the query off the read lock (see
/// [`eval_values`]). An `UNDEF` cell names no term, so it does not count.
fn contains_unknown_values_term(gp: &spargebra::algebra::GraphPattern, dict: &Dictionary) -> bool {
    contains_node(gp, &|g| match g {
        spargebra::algebra::GraphPattern::Values { bindings, .. } => {
            bindings.iter().flatten().any(|cell| {
                cell.as_ref().is_some_and(|term| {
                    ground_term_to_parsed(term)
                        .is_ok_and(|t| dict.lookup_term(&t.value, &t.typ).is_none())
                })
            })
        }
        _ => false,
    })
}

/// Mutable twin of [`eval_where`] for queries that need to intern computed
/// values (`BIND` and `GROUP BY` aggregates). Identical to `eval_where` except
/// for the added `Extend` and `Group` arms — kept in sync with it by hand since
/// Rust has no way to share one body across an `&`/`&mut` store parameter here
/// without a larger refactor.
fn eval_where_mut(
    gp: &spargebra::algebra::GraphPattern,
    store: &mut TripleStore,
    engine: &HybridEngine,
    limit: Option<usize>,
) -> Result<(RowBlock, Vec<String>), String> {
    use spargebra::algebra::GraphPattern as GP;
    match gp {
        GP::Bgp { patterns } => eval_bgp(patterns, store, engine, limit),
        GP::Filter { expr, inner } => {
            check_expr(expr)?;
            let (rows, vo) = eval_where_mut(inner, store, engine, None)?;
            let mut kept = RowBlock::new(rows.n_vars());
            for row in rows.rows() {
                if row_passes(&[expr], row, &vo, store) {
                    kept.push_row(row);
                }
            }
            Ok((kept, vo))
        }
        GP::LeftJoin {
            left,
            right,
            expression,
        } => {
            let (lr, lvo) = eval_where_mut(left, store, engine, None)?;
            let (rr, rvo) = eval_where_mut(right, store, engine, None)?;
            hash_join(lr, &lvo, rr, &rvo, true, expression.as_ref(), store)
        }
        GP::Join { left, right } => {
            let (lr, lvo) = eval_where_mut(left, store, engine, None)?;
            let (rr, rvo) = eval_where_mut(right, store, engine, None)?;
            hash_join(lr, &lvo, rr, &rvo, false, None, store)
        }
        GP::Union { left, right } => {
            let (lr, lvo) = eval_where_mut(left, store, engine, None)?;
            let (rr, rvo) = eval_where_mut(right, store, engine, None)?;
            union_rows(lr, &lvo, rr, &rvo)
        }
        GP::Path {
            subject,
            path,
            object,
        } => eval_path(store, subject, path, object),
        GP::Extend {
            inner,
            variable,
            expression,
        } => {
            check_expr(expression)?;
            let (rows, mut vo) = eval_where_mut(inner, store, engine, None)?;
            let mut extended = RowBlock::new(rows.n_vars() + 1);
            for row in rows.rows() {
                // BIND leaves the variable unbound on a type error rather than
                // dropping the row (unlike FILTER).
                let id = match expression {
                    // Fast path: alias such as `COUNT(*) AS ?cnt`. Preserving
                    // the original dictionary ID keeps the datatype (e.g.
                    // xsd:integer) instead of re-interning as xsd:double.
                    Expression::Variable(v) => vo
                        .iter()
                        .position(|x| x == v.as_str())
                        .map(|c| row[c])
                        .filter(|&id| id != NULL_ID)
                        .unwrap_or(NULL_ID),
                    _ => match eval(expression, row, &vo, store) {
                        Ok(fv) => intern_fv(store, &fv),
                        Err(()) => NULL_ID,
                    },
                };
                extended.push_row_concat(row, &[id]);
            }
            vo.push(variable.as_str().to_string());
            Ok((extended, vo))
        }
        GP::Group {
            inner,
            variables,
            aggregates,
        } => eval_group(inner, variables, aggregates, store, engine),
        GP::Minus { left, right } => {
            let (lr, lvo) = eval_where_mut(left, store, engine, None)?;
            let (rr, rvo) = eval_where_mut(right, store, engine, None)?;
            Ok(minus_rows(lr, &lvo, rr, &rvo))
        }
        // Under the write lock an inline value the store has never seen can be
        // interned, so a standalone VALUES hands the term back verbatim.
        GP::Values {
            variables,
            bindings,
        } => eval_values(variables, bindings, |t| {
            Some(store.dict.insert_with_type(&t.value, t.typ.clone()))
        }),
        // Sub-SELECT: evaluated as its own query, so what it does not project
        // stays out of scope for the enclosing pattern.
        GP::Project { .. }
        | GP::Distinct { .. }
        | GP::Reduced { .. }
        | GP::Slice { .. }
        | GP::OrderBy { .. } => {
            let sub = peel_modifiers(gp);
            // `limit` is non-None only for a whole-body sub-SELECT, whose
            // rows map 1:1 onto the enclosing result -> the cap carries in.
            let (rows, vo) = eval_where_mut(sub.where_pat, store, engine, pushdown_rows(&sub, limit))?;
            let r = apply_modifiers(&sub, rows, vo, store)?;
            Ok((r.rows, r.var_order))
        }
        _ => Err(
            "Unsupported WHERE pattern (only BGP/FILTER/OPTIONAL/UNION/Join/Path/BIND/GROUP BY/MINUS/VALUES/sub-SELECT)"
                .to_string(),
        ),
    }
}

/// Evaluates a `GROUP BY ... (COUNT(...) AS ?var)` pattern. The inner pattern
/// is evaluated, rows are partitioned by the GROUP BY variables, and one row
/// per group is emitted containing the group key plus the aggregate values.
/// Aggregate results are interned into the dictionary, so this needs write
/// access to the store.
fn eval_group(
    inner: &spargebra::algebra::GraphPattern,
    group_by_vars: &[Variable],
    aggregates: &[(Variable, spargebra::algebra::AggregateExpression)],
    store: &mut TripleStore,
    engine: &HybridEngine,
) -> Result<(RowBlock, Vec<String>), String> {
    use rustc_hash::FxHashMap;
    use rustc_hash::FxHashSet;
    use spargebra::algebra::AggregateExpression as AE;
    use spargebra::algebra::AggregateFunction as AF;

    // 1. Evaluate the inner pattern.
    let (rows, vo) = eval_where_mut(inner, store, engine, None)?;

    // 2. Find the column indices for the GROUP BY variables.
    let group_cols: Vec<usize> = group_by_vars
        .iter()
        .map(|v| {
            vo.iter()
                .position(|x| x == v.as_str())
                .ok_or_else(|| format!("GROUP BY variable ?{} is not in scope", v.as_str()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // 3. Partition rows into groups keyed by the GROUP BY column values.
    let mut groups: FxHashMap<Vec<u32>, Vec<usize>> = FxHashMap::default();
    for (i, row) in rows.rows().enumerate() {
        let key: Vec<u32> = group_cols.iter().map(|&c| row[c]).collect();
        groups.entry(key).or_default().push(i);
    }

    // SPARQL 1.1: without GROUP BY variables there is always one (possibly
    // empty) group, so an aggregate-only query over no solutions still yields
    // one row (e.g. COUNT(*) = 0) instead of no rows at all.
    if groups.is_empty() && group_by_vars.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }

    // 4. Output variable order: group keys first, then aggregate results.
    let mut out_vo = Vec::with_capacity(group_by_vars.len() + aggregates.len());
    for v in group_by_vars {
        out_vo.push(v.as_str().to_string());
    }
    for (var, _) in aggregates {
        out_vo.push(var.as_str().to_string());
    }

    // 5. Build one output row per group.
    let mut result = RowBlock::new(out_vo.len());
    for (key, row_indices) in groups {
        let mut out_row: Vec<u32> = Vec::with_capacity(out_vo.len());
        out_row.extend_from_slice(&key);
        for (_, agg_expr) in aggregates {
            let id = match agg_expr {
                AE::CountSolutions { distinct } => {
                    // COUNT(*) counts the solutions in the group;
                    // COUNT(DISTINCT *) counts only distinct full solutions
                    // (duplicates arise e.g. from overlapping UNION branches).
                    let n = if *distinct {
                        let mut seen: FxHashSet<&[u32]> = FxHashSet::default();
                        row_indices
                            .iter()
                            .filter(|&&i| seen.insert(rows.row(i)))
                            .count() as i64
                    } else {
                        row_indices.len() as i64
                    };
                    intern_count(store, n)
                }
                AE::FunctionCall {
                    name,
                    expr,
                    distinct,
                } => {
                    // SUM/AVG compute a fresh number instead of handing back a
                    // term from the group, so they take any expression argument
                    // rather than the bare column the others need.
                    if matches!(name, AF::Sum | AF::Avg) {
                        out_row.push(agg_sum_avg(
                            expr,
                            &rows,
                            &vo,
                            &row_indices,
                            *distinct,
                            matches!(name, AF::Avg),
                            store,
                        )?);
                        continue;
                    }
                    // Before agg_col, so it reports the right reason.
                    if let AF::Custom(n) = name {
                        return Err(format!("Aggregate <{}> is not supported", n.as_str()));
                    }
                    let col = agg_col(expr, &vo)?;
                    let bound = || row_indices.iter().filter(|&&i| rows.row(i)[col] != NULL_ID);
                    match name {
                        // COUNT(?x) counts the non-null values of ?x in the group; COUNT(DISTINCT ?x) counts only distinct non-null values.
                        AF::Count => {
                            let n = if *distinct {
                                // Dictionary ids are one-to-one with terms, so
                                // deduping ids is exactly term-distinctness.
                                bound()
                                    .map(|&i| rows.row(i)[col])
                                    .collect::<FxHashSet<u32>>()
                                    .len()
                            } else {
                                bound().count()
                            };
                            intern_count(store, n as i64)
                        }
                        // Joins the terms' lexical forms. Always a plain string, and
                        // "" over an empty group rather than unbound.
                        AF::GroupConcat { separator } => {
                            let sep = separator.as_deref().unwrap_or(" ");
                            let mut ids: Vec<u32> = bound().map(|&i| rows.row(i)[col]).collect();
                            if *distinct {
                                let mut seen = FxHashSet::default();
                                ids.retain(|&id| seen.insert(id));
                            }
                            let joined = ids
                                .iter()
                                .filter_map(|&id| store.dict.resolve(id))
                                .collect::<Vec<_>>()
                                .join(sep);
                            intern_fv(store, &Fv::Str(joined))
                        }
                        // SPARQL lets SAMPLE return any element, so take the first.
                        AF::Sample => bound().next().map_or(NULL_ID, |&i| rows.row(i)[col]),
                        AF::Min | AF::Max => {
                            // By the ORDER BY ordering, not by id: ids follow interning
                            // order, not value order.
                            let want_max = matches!(name, AF::Max);
                            bound()
                                .map(|&i| {
                                    (rows.row(i)[col], order_key(expr, rows.row(i), &vo, store))
                                })
                                .reduce(|best, cur| {
                                    let ord = cmp_key(&cur.1, &best.1);
                                    if ord.is_gt() == want_max && ord.is_ne() {
                                        cur
                                    } else {
                                        best
                                    }
                                })
                                .map_or(NULL_ID, |(id, _)| id)
                        }
                        // Unreachable, but listed so a new spargebra aggregate
                        // is a compile error rather than a silent fallthrough.
                        AF::Sum | AF::Avg | AF::Custom(_) => {
                            return Err("Unsupported aggregate".to_string());
                        }
                    }
                }
            };
            out_row.push(id);
        }
        result.push_row(&out_row);
    }

    Ok((result, out_vo))
}

/// Interns an integer count as an `xsd:integer` literal. COUNT results are
/// defined as integer typed literals by SPARQL 1.1.
fn intern_count(store: &mut TripleStore, n: i64) -> u32 {
    let lex = n.to_string();
    let typ = TermType::literal_datatype(format!("{XSD}integer"));
    store
        .dict
        .lookup_term(&lex, &typ)
        .unwrap_or_else(|| store.dict.insert_with_type(&lex, typ))
}

/// Interns a `BIND`-computed value into the dictionary, reusing the existing
/// term if the same value is already present — so equality/joins against
/// stored data keep working for the bound variable. Requires write access to
/// the store.
fn intern_fv(store: &mut TripleStore, fv: &Fv) -> u32 {
    let (lex, typ) = match fv {
        Fv::Iri(s) => (s.clone(), TermType::iri()),
        Fv::Blank(s) => (s.clone(), TermType::BlankNode),
        Fv::Str(s) => (s.clone(), TermType::literal_plain()),
        Fv::Num(n) => (
            format_num(*n),
            TermType::literal_datatype(format!("{XSD}double")),
        ),
        Fv::Bool(b) => (
            b.to_string(),
            TermType::literal_datatype(format!("{XSD}boolean")),
        ),
        Fv::Lang(s, l) => (s.clone(), TermType::literal_lang(l.clone())),
        Fv::Typed(s, dt) => (s.clone(), TermType::literal_datatype(dt.clone())),
    };
    store
        .dict
        .lookup_term(&lex, &typ)
        .unwrap_or_else(|| store.dict.insert_with_type(&lex, typ))
}

/// Column read by a bare-variable aggregate argument, e.g. the `?v` in MIN(?v).
///
/// Every aggregate except SUM/AVG hands back (or splices together) a term taken
/// straight out of the group, so restricting those to a variable lets them reuse
/// that row's dictionary id and keep the source datatype, rather than interning
/// a fresh term — which would coerce numerics to `xsd:double`. `MIN(?v + 1)` is
/// therefore rejected. SUM/AVG intern a computed value anyway and accept any
/// expression; they call this only to scope-check a bare variable.
fn agg_col(expr: &Expression, vo: &[String]) -> Result<usize, String> {
    match expr {
        Expression::Variable(v) => vo
            .iter()
            .position(|x| x == v.as_str())
            .ok_or_else(|| format!("aggregate variable ?{} is not in scope", v.as_str())),
        _ => Err(
            "COUNT, MIN, MAX, SAMPLE and GROUP_CONCAT take a bare variable argument".to_string(),
        ),
    }
}

/// Folds `SUM`/`AVG` over one group.
///
/// Both intern a computed number instead of returning a term from the group, so
/// the argument may be any expression and the result is always `xsd:double`:
/// `Fv::Num` is an `f64`, so the input datatypes are gone before the fold sees
/// a value. A group with no contributors is `0`, as SPARQL 1.1 defines for both
/// — unlike MIN/MAX/SAMPLE, which are unbound over an empty group.
fn agg_sum_avg(
    expr: &Expression,
    rows: &RowBlock,
    vo: &[String],
    row_indices: &[usize],
    distinct: bool,
    avg: bool,
    store: &mut TripleStore,
) -> Result<u32, String> {
    use rustc_hash::FxHashSet;

    // A bare variable is still scope-checked, so a typo'd `SUM(?scr)` reports
    // the same error as `MIN(?scr)` instead of silently summing to zero.
    if matches!(expr, Expression::Variable(_)) {
        agg_col(expr, vo)?;
    }

    let mut vals: Vec<f64> = Vec::with_capacity(row_indices.len());
    for &i in row_indices {
        // An unbound argument - or one whose evaluation errors - is simply not
        // a contributor; a bound non-numeric one is a type error that makes the
        // whole aggregate unbound.
        if let Ok(fv) = eval(expr, rows.row(i), vo, store) {
            match as_num(&fv) {
                Some(n) => vals.push(n),
                None => return Ok(NULL_ID),
            }
        }
    }
    if distinct {
        // Value-distinct, not term-distinct as the other aggregates are: a
        // computed argument has no term behind it to compare.
        let mut seen = FxHashSet::default();
        vals.retain(|n| seen.insert(n.to_bits()));
    }
    if vals.is_empty() {
        return Ok(intern_count(store, 0));
    }
    let total: f64 = vals.iter().sum();
    let n = if avg {
        total / vals.len() as f64
    } else {
        total
    };
    Ok(intern_fv(store, &Fv::Num(n)))
}

// ---------------------------------------------------------------------------
// Property Paths (SPARQL 1.1): /, ^, |, *, +, ?, !{…}
// ---------------------------------------------------------------------------
//
// Evaluated as directed set propagation: starting from a known node set,
// `step_forward`/`step_backward` return the nodes reachable over the path.
// `*`/`+` are transitive closures (BFS to fixpoint). With exactly one bound
// endpoint this is efficient (closure only from the bound node); with two
// variables it enumerates over all start nodes (correct, but potentially
// expensive – for `*` with both variables the degenerate identity set over all
// nodes).

use rustc_hash::FxHashSet;

enum PathEnd {
    Bound(u32),
    Var(String),
}

fn resolve_path_end(tp: &spargebra::term::TermPattern, dict: &Dictionary) -> Option<PathEnd> {
    match tp {
        spargebra::term::TermPattern::Variable(v) => Some(PathEnd::Var(v.as_str().to_string())),
        spargebra::term::TermPattern::NamedNode(nn) => {
            dict.lookup_iri(nn.as_str()).map(PathEnd::Bound)
        }
        spargebra::term::TermPattern::Literal(lit) => dict
            .lookup_term(lit.value(), &literal_term_type(lit))
            .map(PathEnd::Bound),
        // Blank node = non-distinguished variable. spargebra decomposes
        // sequence paths with a closure (`p1/(p2)*`) into a join over a
        // blank-node node (`<s> p1 _:b . _:b (p2)* ?x`); without this handling
        // eval_path would wrongly return 0 here (WDBench paths bug).
        // Stable internal name as in translate_term_pattern.
        spargebra::term::TermPattern::BlankNode(bn) => {
            Some(PathEnd::Var(format!("__bn_{}", bn.as_str())))
        }
    }
}

/// Nodes reachable over `path` from `from` (forward).
fn step_forward(store: &TripleStore, path: &Ppe, from: &FxHashSet<u32>) -> FxHashSet<u32> {
    let mut out = FxHashSet::default();
    match path {
        Ppe::NamedNode(nn) => {
            if let Some(pid) = store.dict.lookup_iri(nn.as_str()) {
                for &s in from {
                    out.extend(store.objects_of(s, pid).iter().copied());
                }
            }
        }
        Ppe::Reverse(e) => return step_backward(store, e, from),
        Ppe::Sequence(a, b) => {
            let mid = step_forward(store, a, from);
            return step_forward(store, b, &mid);
        }
        Ppe::Alternative(a, b) => {
            out = step_forward(store, a, from);
            out.extend(step_forward(store, b, from));
        }
        Ppe::ZeroOrMore(e) => return closure(store, e, from, true, true),
        Ppe::OneOrMore(e) => return closure(store, e, from, false, true),
        Ppe::ZeroOrOne(e) => {
            out = from.clone();
            out.extend(step_forward(store, e, from));
        }
        Ppe::NegatedPropertySet(nns) => {
            let exclude: FxHashSet<u32> = nns
                .iter()
                .filter_map(|n| store.dict.lookup_iri(n.as_str()))
                .collect();

            for &s in from {
                for (pid, o) in store.po_pairs_of(s) {
                    if !exclude.contains(&pid) {
                        out.insert(o);
                    }
                }
            }
        }
    }
    out
}

/// Nodes leading to `from` over `path` (backward).
fn step_backward(store: &TripleStore, path: &Ppe, from: &FxHashSet<u32>) -> FxHashSet<u32> {
    let mut out = FxHashSet::default();
    match path {
        Ppe::NamedNode(nn) => {
            if let Some(pid) = store.dict.lookup_iri(nn.as_str()) {
                for &o in from {
                    out.extend(store.subjects_of(pid, o).iter().copied());
                }
            }
        }
        Ppe::Reverse(e) => return step_forward(store, e, from),
        Ppe::Sequence(a, b) => {
            // backward: first b^-1, then a^-1
            let mid = step_backward(store, b, from);
            return step_backward(store, a, &mid);
        }
        Ppe::Alternative(a, b) => {
            out = step_backward(store, a, from);
            out.extend(step_backward(store, b, from));
        }
        Ppe::ZeroOrMore(e) => return closure(store, e, from, true, false),
        Ppe::OneOrMore(e) => return closure(store, e, from, false, false),
        Ppe::ZeroOrOne(e) => {
            out = from.clone();
            out.extend(step_backward(store, e, from));
        }
        Ppe::NegatedPropertySet(nns) => {
            let exclude: FxHashSet<u32> = nns
                .iter()
                .filter_map(|n| store.dict.lookup_iri(n.as_str()))
                .collect();
            for &o in from {
                for (pid, s) in store.sp_pairs_of(o) {
                    if !exclude.contains(&pid) {
                        out.insert(s);
                    }
                }
            }
        }
    }
    out
}

/// Transitive closure (BFS to fixpoint). `reflexive` includes the start set
/// (`*` vs `+`). `forward` selects the direction.
fn closure(
    store: &TripleStore,
    e: &Ppe,
    from: &FxHashSet<u32>,
    reflexive: bool,
    forward: bool,
) -> FxHashSet<u32> {
    let mut result: FxHashSet<u32> = if reflexive {
        from.clone()
    } else {
        FxHashSet::default()
    };
    let mut frontier: FxHashSet<u32> = from.clone();
    loop {
        let next = if forward {
            step_forward(store, e, &frontier)
        } else {
            step_backward(store, e, &frontier)
        };
        let fresh: FxHashSet<u32> = next.into_iter().filter(|n| !result.contains(n)).collect();
        if fresh.is_empty() {
            break;
        }
        for &n in &fresh {
            result.insert(n);
        }
        frontier = fresh;
    }
    result
}

fn eval_path(
    store: &TripleStore,
    subject: &spargebra::term::TermPattern,
    path: &Ppe,
    object: &spargebra::term::TermPattern,
) -> Result<(RowBlock, Vec<String>), String> {
    let s_end = resolve_path_end(subject, &store.dict);
    let o_end = resolve_path_end(object, &store.dict);

    // Variable name (incl. blank node as __bn_) for result columns/join.
    let var_name = |tp: &spargebra::term::TermPattern| -> Option<String> {
        match tp {
            spargebra::term::TermPattern::Variable(v) => Some(v.as_str().to_string()),
            spargebra::term::TermPattern::BlankNode(bn) => Some(format!("__bn_{}", bn.as_str())),
            _ => None,
        }
    };
    let s_var = var_name(subject);
    let o_var = var_name(object);

    // Unknown constant on one side -> empty solution (with variable columns).
    if (s_var.is_none() && s_end.is_none()) || (o_var.is_none() && o_end.is_none()) {
        let mut vo = Vec::new();
        if let Some(v) = &s_var {
            vo.push(v.clone());
        }
        if let Some(v) = &o_var
            && Some(v) != s_var.as_ref()
        {
            vo.push(v.clone());
        }
        return Ok((RowBlock::new(vo.len()), vo));
    }

    match (s_end, o_end) {
        // Subject bound, object variable: forward closure.
        (Some(PathEnd::Bound(s)), Some(PathEnd::Var(ov))) => {
            path_from_bound(store, path, s, ov, true)
        }
        // Object bound, subject variable: backward closure.
        (Some(PathEnd::Var(sv)), Some(PathEnd::Bound(o))) => {
            path_from_bound(store, path, o, sv, false)
        }
        // Both bound: existence check (0 variables, 1 empty row on a hit).
        (Some(PathEnd::Bound(s)), Some(PathEnd::Bound(o))) => Ok(path_existence(store, path, s, o)),
        // Both variables: enumerate over all start nodes.
        (Some(PathEnd::Var(sv)), Some(PathEnd::Var(ov))) => path_both_vars(store, path, sv, ov),
        // unreachable: None cases handled above
        _ => Ok((RowBlock::new(0), Vec::new())),
    }
}

/// One bound endpoint, one variable endpoint: the reachable set as a single
/// column. `forward` selects the direction (bound subject → object, or bound
/// object → subject).
fn path_from_bound(
    store: &TripleStore,
    path: &Ppe,
    bound: u32,
    var: String,
    forward: bool,
) -> Result<(RowBlock, Vec<String>), String> {
    let mut from = FxHashSet::default();
    from.insert(bound);
    let reached = if forward {
        step_forward(store, path, &from)
    } else {
        step_backward(store, path, &from)
    };
    if reached.len() > max_result_rows() {
        return Err(op_too_large());
    }
    let mut rows = RowBlock::new(1);
    for n in reached {
        rows.push_row(&[n]);
    }
    Ok((rows, vec![var]))
}

/// Both endpoints bound: a membership test yielding zero variables and at most
/// one (empty) row.
fn path_existence(store: &TripleStore, path: &Ppe, s: u32, o: u32) -> (RowBlock, Vec<String>) {
    let mut from = FxHashSet::default();
    from.insert(s);
    let mut rows = RowBlock::new(0);
    if step_forward(store, path, &from).contains(&o) {
        rows.push_row(&[]);
    }
    (rows, Vec::new())
}

/// Both endpoints variable: enumerate reachable `(s, o)` pairs over all start
/// nodes. When the two variables are the same, only fixed points `(x, x)` match.
fn path_both_vars(
    store: &TripleStore,
    path: &Ppe,
    sv: String,
    ov: String,
) -> Result<(RowBlock, Vec<String>), String> {
    let cap = max_result_rows();
    let same = sv == ov;
    // Start candidates: distinct subjects; for reflexive paths also objects
    // (the identity (x, x) holds for every node).
    let mut starts = store.distinct_subjects();
    if path_is_reflexive(path) {
        starts.extend(store.distinct_objects());
        starts.sort_unstable();
        starts.dedup();
    }
    let rows_vars = if same { vec![sv] } else { vec![sv, ov] };
    let mut rows = RowBlock::new(rows_vars.len());
    for s in starts {
        let mut from = FxHashSet::default();
        from.insert(s);
        for o in step_forward(store, path, &from) {
            if same {
                if o == s {
                    rows.push_row(&[s]);
                }
            } else {
                rows.push_row(&[s, o]);
            }
        }
        if rows.n_rows() > cap {
            return Err(op_too_large());
        }
    }
    Ok((rows, rows_vars))
}

/// Whether a path contains the empty (reflexive) sequence (`*` or `?` at the root).
fn path_is_reflexive(path: &Ppe) -> bool {
    matches!(path, Ppe::ZeroOrMore(_) | Ppe::ZeroOrOne(_))
}

fn eval_bgp(
    patterns: &[spargebra::term::TriplePattern],
    store: &TripleStore,
    engine: &HybridEngine,
    limit: Option<usize>,
) -> Result<(RowBlock, Vec<String>), String> {
    match translate_bgp(patterns, &store.dict)? {
        Some(internal) => {
            let vo: Vec<String> = internal.variable_order().into_iter().cloned().collect();
            Ok((engine.execute_limited(store, &internal, limit)?, vo))
        }
        None => {
            // Unknown constant -> empty solution with the pattern variables.
            let vo = variables_in_bgp(patterns);
            Ok((RowBlock::new(vo.len()), vo))
        }
    }
}

/// Hash join of two result blocks on the shared variables.
/// `left_outer = true` keeps left rows without a match (NULL-padded).
/// Error message when the cap is exceeded in an eval_where operator.
fn op_too_large() -> String {
    format!(
        "result exceeds {} rows (join/union/path materialization); \
         raise TRILLIAN_MAX_ROWS to allow",
        max_result_rows()
    )
}

fn hash_join(
    left: RowBlock,
    lvo: &[String],
    right: RowBlock,
    rvo: &[String],
    left_outer: bool,
    on: Option<&Expression>,
    store: &TripleStore,
) -> Result<(RowBlock, Vec<String>), String> {
    // `eval` reports unsupported constructs as an error, which a left join reads
    // as "no match" — that would silently unbind instead of failing. Reject up front.
    if let Some(what) = on.and_then(unsupported_in_expr) {
        return Err(format!(
            "Unsupported expression in a FILTER inside OPTIONAL: {what}"
        ));
    }
    let cap = max_result_rows();
    let mut shared: Vec<(usize, usize)> = Vec::new(); // (left_pos, right_pos)
    let mut new_positions: Vec<usize> = Vec::new();
    let mut new_vars: Vec<String> = Vec::new();
    for (rp, v) in rvo.iter().enumerate() {
        if let Some(lp) = lvo.iter().position(|x| x == v) {
            shared.push((lp, rp));
        } else {
            new_positions.push(rp);
            new_vars.push(v.clone());
        }
    }
    let mut new_var_order = lvo.to_vec();
    new_var_order.extend(new_vars.iter().cloned());
    let new_arity = new_vars.len();

    // Index the right side by join key: (number of hits, new columns flat).
    // The hit count is tracked separately because for `new_arity == 0`
    // (semi-join: all right variables are join keys) it would otherwise be
    // impossible to distinguish "key missing" from "key hits, but 0 new
    // columns" – which previously swallowed all such joins (e.g. c2rpq paths
    // with a bound object -> join over the blank-node node).
    // A right row unbound on a join key is compatible with any value there, so
    // it cannot go in the hash. Usually there are none — and a left row unbound
    // on one falls back to a scan of the right for the same reason. With a
    // single join variable that scan is output-bound (every right row matches);
    // only a multi-variable key that is unbound in part both scans and filters.
    let mut index: rustc_hash::FxHashMap<Vec<u32>, (usize, Vec<u32>)> =
        rustc_hash::FxHashMap::default();
    let mut partial: Vec<&[u32]> = Vec::new();
    for rrow in right.rows() {
        let key: Vec<u32> = shared.iter().map(|&(_, rp)| rrow[rp]).collect();
        if key.contains(&NULL_ID) {
            partial.push(rrow);
            continue;
        }
        let bucket = index.entry(key).or_insert((0, Vec::new()));
        bucket.0 += 1;
        for &p in &new_positions {
            bucket.1.push(rrow[p]);
        }
    }

    let mut out = RowBlock::new(new_var_order.len());

    // Unfiltered fast path: every hit is a match, so no merged row is built —
    // only an unbound join key, on either side, falls back to a scan.
    let Some(on) = on else {
        let mut merged: Vec<u32> = Vec::new();
        for row in left.rows() {
            let key: Vec<u32> = shared.iter().map(|&(lp, _)| row[lp]).collect();
            let mut matched = false;
            if key.contains(&NULL_ID) {
                // An unbound key matches any value, so the hash rules nothing out.
                for rrow in right.rows() {
                    if compatible(row, rrow, &shared) {
                        merge_compatible(row, rrow, &shared, &new_positions, &mut merged);
                        out.push_row(&merged);
                        matched = true;
                    }
                }
            } else {
                if let Some(&(count, ref flat)) = index.get(&key) {
                    matched = true;
                    if new_arity == 0 {
                        // Match with no new columns -> one row per right hit.
                        for _ in 0..count {
                            out.push_row_padded(row, NULL_ID);
                        }
                    } else {
                        for chunk in flat.chunks(new_arity) {
                            out.push_row_concat(row, chunk);
                        }
                    }
                }
                for rrow in &partial {
                    if compatible(row, rrow, &shared) {
                        merge_compatible(row, rrow, &shared, &new_positions, &mut merged);
                        out.push_row(&merged);
                        matched = true;
                    }
                }
            }
            if !matched && left_outer {
                out.push_row_padded(row, NULL_ID);
            }
            if out.n_rows() > cap {
                return Err(op_too_large());
            }
        }
        return Ok((out, new_var_order));
    };

    // Filtered (`OPTIONAL { ... FILTER(...) }`): `on` is evaluated on the merged
    // row, and a match that fails it does not count — the left row then falls
    // through to the unbound-padded branch rather than being dropped.
    let mut merged: Vec<u32> = Vec::with_capacity(new_var_order.len());
    let keeps = |merged: &[u32]| ebv(on, merged, &new_var_order, store) == Ok(true);
    for row in left.rows() {
        let key: Vec<u32> = shared.iter().map(|&(lp, _)| row[lp]).collect();
        let mut matched = false;
        if key.contains(&NULL_ID) {
            // An unbound key matches any value, so the hash rules nothing out.
            for rrow in right.rows() {
                if compatible(row, rrow, &shared) {
                    merge_compatible(row, rrow, &shared, &new_positions, &mut merged);
                    if keeps(&merged) {
                        out.push_row(&merged);
                        matched = true;
                    }
                }
            }
        } else {
            if let Some(&(count, ref flat)) = index.get(&key) {
                if new_arity == 0 {
                    // Every hit merges to the same row -> one `on` check for all.
                    if keeps(row) {
                        for _ in 0..count {
                            out.push_row_padded(row, NULL_ID);
                        }
                        matched = true;
                    }
                } else {
                    for chunk in flat.chunks(new_arity) {
                        merged.clear();
                        merged.extend_from_slice(row);
                        merged.extend_from_slice(chunk);
                        if keeps(&merged) {
                            out.push_row_concat(row, chunk);
                            matched = true;
                        }
                    }
                }
            }
            for rrow in &partial {
                if compatible(row, rrow, &shared) {
                    merge_compatible(row, rrow, &shared, &new_positions, &mut merged);
                    if keeps(&merged) {
                        out.push_row(&merged);
                        matched = true;
                    }
                }
            }
        }
        if !matched && left_outer {
            out.push_row_padded(row, NULL_ID);
        }
        if out.n_rows() > cap {
            return Err(op_too_large());
        }
    }
    Ok((out, new_var_order))
}

/// UNION of two result blocks: combined variable set, missing ones -> NULL.
fn union_rows(
    left: RowBlock,
    lvo: &[String],
    right: RowBlock,
    rvo: &[String],
) -> Result<(RowBlock, Vec<String>), String> {
    let cap = max_result_rows();
    let mut vo = lvo.to_vec();
    for v in rvo {
        if !vo.contains(v) {
            vo.push(v.clone());
        }
    }
    // Column mapping source -> target.
    let map_cols = |src_vo: &[String]| -> Vec<Option<usize>> {
        vo.iter()
            .map(|v| src_vo.iter().position(|x| x == v))
            .collect()
    };
    let lmap = map_cols(lvo);
    let rmap = map_cols(rvo);
    let mut out = RowBlock::new(vo.len());
    let emit = |src: &RowBlock, map: &[Option<usize>], out: &mut RowBlock| -> Result<(), String> {
        let mut buf = vec![NULL_ID; map.len()];
        for row in src.rows() {
            for (i, m) in map.iter().enumerate() {
                buf[i] = m.map(|c| row[c]).unwrap_or(NULL_ID);
            }
            out.push_row(&buf);
            if out.n_rows() > cap {
                return Err(op_too_large());
            }
        }
        Ok(())
    };
    emit(&left, &lmap, &mut out)?;
    emit(&right, &rmap, &mut out)?;
    Ok((out, vo))
}

/// SPARQL `MINUS`: drops every left row that a right row is *compatible* with.
///
/// Compatible means the two agree on all variables both of them bind **and**
/// bind at least one variable in common — a right row with a disjoint domain
/// cancels nothing. That second condition is what separates `MINUS` from
/// `FILTER NOT EXISTS`: a `MINUS` whose pattern shares no variable with the
/// left side removes nothing at all. Only the left variables survive; the right
/// side is a test, not a source of bindings.
///
/// An unbound column is not in the row's domain, so it is compatible with *any*
/// value — which a hash over the shared columns cannot express. Rows with an
/// unbound column fall back to a scan and only they pay for it.
fn minus_rows(
    left: RowBlock,
    lvo: &[String],
    right: RowBlock,
    rvo: &[String],
) -> (RowBlock, Vec<String>) {
    let shared: Vec<(usize, usize)> = rvo
        .iter()
        .enumerate()
        .filter_map(|(rp, v)| lvo.iter().position(|x| x == v).map(|lp| (lp, rp)))
        .collect();
    if shared.is_empty() || right.is_empty() {
        return (left, lvo.to_vec());
    }

    // A partially bound right row can cancel rows with differing values, so it
    // cannot go in the hash. Usually there are none.
    let mut index: rustc_hash::FxHashSet<Vec<u32>> = rustc_hash::FxHashSet::default();
    let mut partial: Vec<&[u32]> = Vec::new();
    for rrow in right.rows() {
        let key: Vec<u32> = shared.iter().map(|&(_, rp)| rrow[rp]).collect();
        if key.contains(&NULL_ID) {
            partial.push(rrow);
        } else {
            index.insert(key);
        }
    }

    let mut out = RowBlock::new(lvo.len());
    let mut key: Vec<u32> = Vec::with_capacity(shared.len());
    for row in left.rows() {
        key.clear();
        key.extend(shared.iter().map(|&(lp, _)| row[lp]));
        let cancelled = if key.contains(&NULL_ID) {
            // An unbound left column matches any value, so every right row is
            // a candidate.
            right.rows().any(|rrow| cancels(row, rrow, &shared))
        } else {
            index.contains(&key) || partial.iter().any(|rrow| cancels(row, rrow, &shared))
        };
        if !cancelled {
            out.push_row(row);
        }
    }
    (out, lvo.to_vec())
}

/// Whether `right` cancels `left` under [`minus_rows`]: compatible, and binding
/// at least one variable in common. Shared variables are the only ones the
/// domains can share, so that is the disjointness test.
fn cancels(left: &[u32], right: &[u32], shared: &[(usize, usize)]) -> bool {
    compatible(left, right, shared)
        && shared
            .iter()
            .any(|&(lp, rp)| left[lp] != NULL_ID && right[rp] != NULL_ID)
}

/// SPARQL compatibility: two solutions agree wherever both bind a shared
/// variable. An unbound column is a wildcard, not a value of its own.
fn compatible(left: &[u32], right: &[u32], shared: &[(usize, usize)]) -> bool {
    shared
        .iter()
        .all(|&(lp, rp)| left[lp] == NULL_ID || right[rp] == NULL_ID || left[lp] == right[rp])
}

/// The joined row for a compatible pair: the left row, with every shared column
/// it leaves unbound taken from the right, then the right's new columns.
fn merge_compatible(
    left: &[u32],
    right: &[u32],
    shared: &[(usize, usize)],
    new_positions: &[usize],
    out: &mut Vec<u32>,
) {
    out.clear();
    out.extend_from_slice(left);
    for &(lp, rp) in shared {
        if out[lp] == NULL_ID {
            out[lp] = right[rp];
        }
    }
    out.extend(new_positions.iter().map(|&p| right[p]));
}

/// Evaluates a `VALUES` inline table into a row block.
///
/// Rows carry dictionary IDs, so a term the store has never seen has none to
/// carry. Dropping the row would be exact only under a plain join — not under
/// `OPTIONAL`, in a `UNION` branch, or standing alone — so `resolve` returning
/// `None` is an error, not a silent skip. [`query_needs_write`] routes such a
/// query to the write path, whose resolver interns instead; that leaves
/// `?infer=rdfs`, read-only by construction, as the one caller that sees it.
///
/// An `UNDEF` cell binds nothing, and [`hash_join`] reads the unbound column as
/// the wildcard SPARQL compatibility makes it.
fn eval_values(
    variables: &[Variable],
    bindings: &[Vec<Option<GroundTerm>>],
    mut resolve: impl FnMut(&ParsedTermRdf) -> Option<u32>,
) -> Result<(RowBlock, Vec<String>), String> {
    let vo: Vec<String> = variables.iter().map(|v| v.as_str().to_string()).collect();
    let mut rows = RowBlock::new(vo.len());
    let mut buf = vec![NULL_ID; vo.len()];
    for binding in bindings {
        buf.fill(NULL_ID);
        for (slot, cell) in buf.iter_mut().zip(binding) {
            // UNDEF: leave the slot at NULL_ID, which the join treats as a wildcard.
            let Some(term) = cell else { continue };
            let parsed = ground_term_to_parsed(term)?;
            *slot = resolve(&parsed).ok_or_else(|| {
                format!(
                    "VALUES names \"{}\", which the store does not contain; \
                     only the write path can mint an ID for it, and `?infer=rdfs` \
                     never takes it",
                    parsed.value
                )
            })?;
        }
        rows.push_row(&buf);
    }
    Ok((rows, vo))
}

/// Sort key for ORDER BY (SPARQL-like total ordering).
#[derive(PartialEq)]
enum OrderKey {
    Unbound,
    Blank(String),
    Num(f64),
    Iri(String),
    Str(String),
}

fn order_key(expr: &Expression, row: &[u32], vo: &[String], store: &TripleStore) -> OrderKey {
    match eval(expr, row, vo, store) {
        Ok(Fv::Num(n)) => OrderKey::Num(n),
        Ok(Fv::Bool(b)) => OrderKey::Num(if b { 1.0 } else { 0.0 }),
        Ok(Fv::Iri(s)) => OrderKey::Iri(s),
        Ok(Fv::Blank(s)) => OrderKey::Blank(s),
        Ok(Fv::Str(s)) | Ok(Fv::Lang(s, _)) | Ok(Fv::Typed(s, _)) => OrderKey::Str(s),
        Err(()) => OrderKey::Unbound,
    }
}

fn cmp_key(a: &OrderKey, b: &OrderKey) -> std::cmp::Ordering {
    use OrderKey::*;
    fn rank(k: &OrderKey) -> u8 {
        match k {
            Unbound => 0,
            Blank(_) => 1,
            Num(_) => 2,
            Iri(_) => 3,
            Str(_) => 4,
        }
    }
    match (a, b) {
        (Num(x), Num(y)) => x.total_cmp(y),
        (Blank(x), Blank(y)) | (Iri(x), Iri(y)) | (Str(x), Str(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

fn sort_rows(
    rows: &mut RowBlock,
    vo: &[String],
    order_by: &[(&Expression, bool)],
    store: &TripleStore,
) {
    let n = rows.n_rows();
    let keys: Vec<Vec<OrderKey>> = (0..n)
        .map(|i| {
            order_by
                .iter()
                .map(|(e, _)| order_key(e, rows.row(i), vo, store))
                .collect()
        })
        .collect();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| {
        for (c, (_, desc)) in order_by.iter().enumerate() {
            let ord = cmp_key(&keys[a][c], &keys[b][c]);
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    let mut sorted = RowBlock::new(rows.n_vars());
    for &i in &idx {
        sorted.push_row(rows.row(i));
    }
    *rows = sorted;
}

/// How many rows the inner pattern must produce for `m`'s modifiers, or `None`
/// if the full result is needed. `cap` is an outer row limit already known to
/// be result-preserving for this result (see the sub-`SELECT` arm of
/// [`eval_where`]); `None` means unlimited.
fn pushdown_rows(m: &Modifiers, cap: Option<usize>) -> Option<usize> {
    // ORDER BY needs the full sort, DISTINCT may need more raw rows for N
    // distinct ones, GROUP BY the full groups -> nothing may be cut early.
    if !m.order_by.is_empty() || m.distinct || contains_group(m.where_pat) {
        return None;
    }
    // Both limits are counted from before OFFSET, so offset+limit rows suffice;
    // apply_offset_limit trims exactly.
    let offset = m.offset.unwrap_or(0);
    let own = m.limit.map(|l| l.saturating_add(offset));
    let outer = cap.map(|c| c.saturating_add(offset));
    match (own, outer) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Applies ORDER BY, projection, DISTINCT and OFFSET/LIMIT to an evaluated
/// WHERE result. Shared by the top-level query path and by sub-`SELECT`s,
/// which are the same modifier stack one level down.
fn apply_modifiers(
    m: &Modifiers,
    rows: RowBlock,
    var_order: Vec<String>,
    store: &TripleStore,
) -> Result<SelectResult, String> {
    let mut rows = rows;

    // ORDER BY (on the full bindings, before projection).
    if !m.order_by.is_empty() {
        sort_rows(&mut rows, &var_order, &m.order_by, store);
    }

    // SELECT * : all variables except internal blank-node placeholders (__bn_),
    // which only come from expanded sequence paths and are not output.
    let vars = m.projection.clone().unwrap_or_else(|| {
        var_order
            .iter()
            .filter(|v| !v.starts_with("__bn_"))
            .cloned()
            .collect()
    });
    let mut var_indices = Vec::with_capacity(vars.len());
    for var in &vars {
        match var_order.iter().position(|v| v == var) {
            Some(pos) => var_indices.push(pos),
            None => {
                return Err(format!(
                    "SELECT variable ?{} does not appear in pattern",
                    var
                ));
            }
        }
    }

    let mut rows = rows.project(&var_indices);
    let var_order = vars.clone();

    if m.distinct {
        if m.order_by.is_empty() {
            rows.sort_distinct();
        } else {
            rows.dedup_preserving_order(); // keep the ORDER BY ordering
        }
    }
    rows.apply_offset_limit(m.offset.unwrap_or(0), m.limit);

    Ok(SelectResult {
        vars,
        rows,
        var_order,
    })
}

fn evaluate_select_with_modifiers(
    store: &TripleStore,
    engine: &HybridEngine,
    m: &Modifiers,
) -> Result<SelectResult, String> {
    let (rows, var_order) = eval_where(m.where_pat, store, engine, pushdown_rows(m, None))?;
    apply_modifiers(m, rows, var_order, store)
}

/// Mutable twin of [`evaluate_select_with_modifiers`] for queries containing
/// `BIND` or `GROUP BY`, routed to [`eval_where_mut`] instead of [`eval_where`].
fn evaluate_select_with_modifiers_mut(
    store: &mut TripleStore,
    engine: &HybridEngine,
    m: &Modifiers,
) -> Result<SelectResult, String> {
    let (rows, var_order) = eval_where_mut(m.where_pat, store, engine, pushdown_rows(m, None))?;
    apply_modifiers(m, rows, var_order, store)
}

fn variables_in_bgp(bgp: &[spargebra::term::TriplePattern]) -> Vec<String> {
    let mut vars = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    for tp in bgp {
        for v in term_pattern_variables(&tp.subject) {
            if seen.insert(v.clone()) {
                vars.push(v);
            }
        }
        for v in named_node_pattern_variables(&tp.predicate) {
            if seen.insert(v.clone()) {
                vars.push(v);
            }
        }
        for v in term_pattern_variables(&tp.object) {
            if seen.insert(v.clone()) {
                vars.push(v);
            }
        }
    }
    vars
}

fn named_node_pattern_variables(np: &spargebra::term::NamedNodePattern) -> Vec<String> {
    match np {
        spargebra::term::NamedNodePattern::Variable(v) => vec![v.as_str().to_string()],
        _ => Vec::new(),
    }
}

fn term_pattern_variables(tp: &spargebra::term::TermPattern) -> Vec<String> {
    match tp {
        spargebra::term::TermPattern::Variable(v) => vec![v.as_str().to_string()],
        _ => Vec::new(),
    }
}

/// Appends a JSON string literal (with escaping) to the buffer.
fn append_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Appends the SPARQL-JSON term object for an ID (uri/literal+datatype/lang).
fn append_term(out: &mut String, id: u32, dict: &Dictionary) {
    match (dict.resolve(id), dict.resolve_type(id)) {
        (Some(v), Some(TermType::Iri)) => {
            out.push_str("{\"type\":\"uri\",\"value\":");
            append_json_str(out, &v);
            out.push('}');
        }
        (Some(v), Some(TermType::BlankNode)) => {
            out.push_str("{\"type\":\"bnode\",\"value\":");
            append_json_str(out, &v);
            out.push('}');
        }
        (Some(v), Some(TermType::Literal { datatype, lang })) => {
            out.push_str("{\"type\":\"literal\",\"value\":");
            append_json_str(out, &v);
            if let Some(dt) = &datatype {
                out.push_str(",\"datatype\":");
                append_json_str(out, dt);
            }
            if let Some(l) = &lang {
                out.push_str(",\"xml:lang\":");
                append_json_str(out, l);
            }
            out.push('}');
        }
        (Some(v), _) => {
            out.push_str("{\"type\":\"literal\",\"value\":");
            append_json_str(out, &v);
            out.push('}');
        }
        (None, _) => {
            out.push_str("{\"type\":\"literal\",\"value\":\"__id_");
            out.push_str(&id.to_string());
            out.push_str("\"}");
        }
    }
}

/// Serializes the result **directly as a JSON string** – without allocating a
/// `serde_json::Map`/`Value` per row (that was ~95% of the time of large queries).
fn write_sparql_json(result: &SelectResult, store: &TripleStore) -> String {
    let mut var_indices = Vec::with_capacity(result.vars.len());
    for var in &result.vars {
        let pos = result
            .var_order
            .iter()
            .position(|v| v == var)
            .expect("validated variable");
        var_indices.push(pos);
    }

    let mut out = String::with_capacity(256 + result.rows.n_rows() * 48);
    out.push_str("{\"head\":{\"vars\":[");
    for (i, v) in result.vars.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        append_json_str(&mut out, v);
    }
    out.push_str("]},\"results\":{\"bindings\":[");
    let mut first_row = true;
    for row in result.rows.rows() {
        if !first_row {
            out.push(',');
        }
        first_row = false;
        out.push('{');
        let mut first_cell = true;
        for (i, var) in result.vars.iter().enumerate() {
            let id = row[var_indices[i]];
            if id == NULL_ID {
                continue; // unbound (OPTIONAL) variable: omit
            }
            if !first_cell {
                out.push(',');
            }
            first_cell = false;
            append_json_str(&mut out, var);
            out.push(':');
            append_term(&mut out, id, &store.dict);
        }
        out.push('}');
    }
    out.push_str("]}}");
    out
}

fn execute_count(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<Value, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match query {
        SparqlQuery::Select { pattern, .. } => {
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(json!({ "count": result.rows.n_rows() }))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(json!({ "boolean": result.rows.n_rows() > 0 }))
        }
        _ => Err("Only SELECT and ASK queries are supported for /count".to_string()),
    }
}

/// Mutable twin of [`execute_count`] for queries containing `BIND`.
fn execute_count_bind(
    store: &mut TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<Value, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match query {
        SparqlQuery::Select { pattern, .. } => {
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers_mut(store, engine, &m)?;
            Ok(json!({ "count": result.rows.n_rows() }))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers_mut(store, engine, &m)?;
            Ok(json!({ "boolean": result.rows.n_rows() > 0 }))
        }
        _ => Err("Only SELECT and ASK queries are supported for /count".to_string()),
    }
}

// ---------------------------------------------------------------------------
// SPARQL Update Execution
// ---------------------------------------------------------------------------

fn execute_update(
    store: &mut TripleStore,
    update_str: &str,
    mut wal: Option<&mut Wal>,
) -> Result<(), String> {
    let update = SparqlParser::new()
        .parse_update(update_str)
        .map_err(|e| e.to_string())?;

    // Collect all inserts/deletes and apply them at the end in a single index
    // rebuild; log them to the WAL in parallel.
    let mut inserts: Vec<(u32, u32, u32)> = Vec::new();
    let mut deletes: Vec<(u32, u32, u32)> = Vec::new();

    for op in update.operations {
        match op {
            spargebra::GraphUpdateOperation::InsertData { data } => {
                for quad in data {
                    let (s, p, o) = quad_to_triple_terms(&quad)?;
                    let sid = insert_term_logged(store, &s, wal.as_deref_mut())?;
                    let pid = insert_term_logged(store, &p, wal.as_deref_mut())?;
                    let oid = insert_term_logged(store, &o, wal.as_deref_mut())?;
                    if let Some(w) = wal.as_deref_mut() {
                        w.log_op(true, sid, pid, oid).map_err(|e| e.to_string())?;
                    }
                    inserts.push((sid, pid, oid));
                }
            }
            spargebra::GraphUpdateOperation::DeleteData { data } => {
                for quad in data {
                    let (s, p, o) = ground_quad_to_triple_terms(&quad)?;
                    if let (Some(sid), Some(pid), Some(oid)) = (
                        store.dict.lookup_term(&s.value, &s.typ),
                        store.dict.lookup_term(&p.value, &p.typ),
                        store.dict.lookup_term(&o.value, &o.typ),
                    ) {
                        if let Some(w) = wal.as_deref_mut() {
                            w.log_op(false, sid, pid, oid).map_err(|e| e.to_string())?;
                        }
                        deletes.push((sid, pid, oid));
                    }
                }
            }
            _ => return Err("Only INSERT DATA and DELETE DATA are supported".to_string()),
        }
    }

    store.apply_updates(&inserts, &deletes);
    Ok(())
}

/// Logs every term interned since `first_new_id` to the WAL, in ID order, so a
/// replay assigns them the same IDs.
///
/// The log records operations by ID but reconstructs IDs from the order of its
/// term records (see [`crate::wal`]), so an unlogged intern shifts every later
/// one and the replay rebuilds the wrong triples. Queries intern too: `BIND`
/// and `GROUP BY` a computed value, `VALUES` a term its table names. No `sync`
/// — these records share the log's buffer with the operations that reference
/// them, so the next update's sync flushes both, in order.
fn log_interned_since(
    store: &TripleStore,
    first_new_id: usize,
    wal: &mut Wal,
) -> Result<usize, String> {
    for id in first_new_id..store.dict.len() {
        let id = id as u32;
        let (Some(value), Some(typ)) = (store.dict.resolve(id), store.dict.resolve_type(id)) else {
            return Err(format!(
                "dictionary term {id} disappeared before it was logged"
            ));
        };
        wal.log_term(&value, &typ).map_err(|e| e.to_string())?;
    }
    Ok(store.dict.len() - first_new_id)
}

/// Logs what a query interned and forces it to disk. Unsynced term records sit
/// in the writer's buffer, so a crash tears the log mid-record.
fn log_interned_and_sync(
    store: &TripleStore,
    first_new_id: usize,
    wal: &mut Wal,
) -> Result<(), String> {
    if log_interned_since(store, first_new_id, wal)? > 0 {
        wal.sync().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Inserts a term into the dictionary and logs it to the WAL if it was new
/// (so the replay assigns the same IDs).
fn insert_term_logged(
    store: &mut TripleStore,
    t: &ParsedTermRdf,
    wal: Option<&mut Wal>,
) -> Result<u32, String> {
    let before = store.dict.len();
    let id = store.dict.insert_with_type(&t.value, t.typ.clone());
    if store.dict.len() > before
        && let Some(w) = wal
    {
        w.log_term(&t.value, &t.typ).map_err(|e| e.to_string())?;
    }
    Ok(id)
}

fn quad_to_triple_terms(
    quad: &Quad,
) -> Result<(ParsedTermRdf, ParsedTermRdf, ParsedTermRdf), String> {
    let s = named_or_blank_node_to_parsed(&quad.subject)?;
    let p = named_node_to_parsed(&quad.predicate);
    let o = sparql_term_to_parsed(&quad.object)?;
    Ok((s, p, o))
}

fn ground_quad_to_triple_terms(
    quad: &GroundQuad,
) -> Result<(ParsedTermRdf, ParsedTermRdf, ParsedTermRdf), String> {
    let s = named_node_to_parsed(&quad.subject);
    let p = named_node_to_parsed(&quad.predicate);
    let o = ground_term_to_parsed(&quad.object)?;
    Ok((s, p, o))
}

struct ParsedTermRdf {
    value: String,
    typ: TermType,
}

fn named_or_blank_node_to_parsed(node: &NamedOrBlankNode) -> Result<ParsedTermRdf, String> {
    match node {
        NamedOrBlankNode::NamedNode(nn) => Ok(named_node_to_parsed(nn)),
        NamedOrBlankNode::BlankNode(_) => {
            Err("Blank nodes in updates are not supported".to_string())
        }
    }
}

fn named_node_to_parsed(nn: &NamedNode) -> ParsedTermRdf {
    ParsedTermRdf {
        value: nn.as_str().to_string(),
        typ: TermType::Iri,
    }
}

fn sparql_term_to_parsed(term: &SparqlTerm) -> Result<ParsedTermRdf, String> {
    match term {
        SparqlTerm::NamedNode(nn) => Ok(named_node_to_parsed(nn)),
        SparqlTerm::Literal(lit) => Ok(literal_to_parsed(lit)),
        SparqlTerm::BlankNode(_) => Err("Blank nodes in updates are not supported".to_string()),
    }
}

fn ground_term_to_parsed(term: &GroundTerm) -> Result<ParsedTermRdf, String> {
    match term {
        GroundTerm::NamedNode(nn) => Ok(named_node_to_parsed(nn)),
        GroundTerm::Literal(lit) => Ok(literal_to_parsed(lit)),
    }
}

/// Term type of a parsed SPARQL literal (for dictionary lookup/insert).
fn literal_term_type(lit: &Literal) -> TermType {
    if let Some(lang) = lit.language() {
        TermType::literal_lang(lang)
    } else {
        TermType::literal_datatype(lit.datatype().as_str())
    }
}

fn literal_to_parsed(lit: &Literal) -> ParsedTermRdf {
    ParsedTermRdf {
        value: lit.value().to_string(),
        typ: literal_term_type(lit),
    }
}

// ---------------------------------------------------------------------------
// BGP Translation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// FILTER: SPARQL-Ausdrucks-Evaluator
// ---------------------------------------------------------------------------

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// Runtime value of a FILTER expression.
#[derive(Debug, Clone)]
enum Fv {
    Iri(String),
    Blank(String), // blank-node label (isBlank != isIri)
    Str(String),   // plain literal / xsd:string
    Num(f64),      // numeric datatype
    Bool(bool),
    Lang(String, String),  // (lexical, language tag)
    Typed(String, String), // (lexical, datatype IRI) – not numeric/string
}

fn is_numeric_dt(dt: &str) -> bool {
    matches!(
        dt.strip_prefix(XSD),
        Some(
            "integer"
                | "decimal"
                | "double"
                | "float"
                | "int"
                | "long"
                | "short"
                | "byte"
                | "nonNegativeInteger"
                | "positiveInteger"
                | "nonPositiveInteger"
                | "negativeInteger"
                | "unsignedInt"
                | "unsignedLong"
                | "unsignedShort"
                | "unsignedByte"
        )
    )
}

fn classify(lex: &str, datatype: Option<&str>, lang: Option<&str>) -> Fv {
    if let Some(l) = lang {
        return Fv::Lang(lex.to_string(), l.to_string());
    }
    match datatype {
        None => Fv::Str(lex.to_string()),
        // strip_prefix, not format!: the guards ran on every row. With this, the compiler can optimize the string comparison to a single pointer comparison.
        Some(dt) if dt.strip_prefix(XSD) == Some("string") => Fv::Str(lex.to_string()),
        Some(dt) if dt.strip_prefix(XSD) == Some("boolean") => {
            Fv::Bool(lex == "true" || lex == "1")
        }
        Some(dt) if is_numeric_dt(dt) => match lex.parse::<f64>() {
            Ok(n) => Fv::Num(n),
            Err(_) => Fv::Typed(lex.to_string(), dt.to_string()),
        },
        Some(dt) => Fv::Typed(lex.to_string(), dt.to_string()),
    }
}

fn term_to_fv(id: u32, store: &TripleStore) -> Option<Fv> {
    let v = store.dict.resolve(id)?;
    match store.dict.resolve_type(id)? {
        TermType::Iri => Some(Fv::Iri(v.to_string())),
        TermType::BlankNode => Some(Fv::Blank(v.to_string())),
        TermType::Literal { datatype, lang } => {
            Some(classify(&v, datatype.as_deref(), lang.as_deref()))
        }
    }
}

fn literal_to_fv(lit: &Literal) -> Fv {
    classify(lit.value(), Some(lit.datatype().as_str()), lit.language())
}

/// Numeric value, if the expression yields one.
fn as_num(fv: &Fv) -> Option<f64> {
    match fv {
        Fv::Num(n) => Some(*n),
        Fv::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Lexical comparison string (for =/< on strings).
fn as_str(fv: &Fv) -> Option<&str> {
    match fv {
        Fv::Str(s) | Fv::Lang(s, _) | Fv::Typed(s, _) => Some(s),
        Fv::Iri(s) => Some(s),
        _ => None,
    }
}

fn fv_equal(a: &Fv, b: &Fv) -> Option<bool> {
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return Some(x == y);
    }
    match (a, b) {
        (Fv::Iri(x), Fv::Iri(y)) => Some(x == y),
        (Fv::Blank(x), Fv::Blank(y)) => Some(x == y),
        (Fv::Bool(x), Fv::Bool(y)) => Some(x == y),
        (Fv::Str(x), Fv::Str(y)) => Some(x == y),
        (Fv::Lang(x, lx), Fv::Lang(y, ly)) => Some(x == y && lx == ly),
        (Fv::Typed(x, dx), Fv::Typed(y, dy)) => Some(x == y && dx == dy),
        _ => None, // incomparable -> error
    }
}

fn fv_cmp(a: &Fv, b: &Fv) -> Option<std::cmp::Ordering> {
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return x.partial_cmp(&y);
    }
    match (a, b) {
        (Fv::Str(x), Fv::Str(y)) => Some(x.cmp(y)),
        (Fv::Lang(x, _), Fv::Lang(y, _)) => Some(x.cmp(y)),
        // SPARQL allows ordering comparisons on IRIs and (same-typed) literals.
        (Fv::Iri(x), Fv::Iri(y)) => Some(x.cmp(y)),
        (Fv::Blank(x), Fv::Blank(y)) => Some(x.cmp(y)),
        (Fv::Bool(x), Fv::Bool(y)) => Some(x.cmp(y)),
        // Same datatype -> lexical comparison; different -> incomparable.
        (Fv::Typed(x, dx), Fv::Typed(y, dy)) if dx == dy => Some(x.cmp(y)),
        _ => None,
    }
}

/// Exact RDF term identity (for `sameTerm`): kind + lexical form + datatype +
/// language, with **no** value promotion. Unlike `=`/`fv_equal`, this treats
/// `"1"^^xsd:integer` and `"1"^^xsd:double` as different terms.
#[derive(PartialEq)]
enum TermKey {
    Iri(String),
    Blank(String),
    Lit(String, Option<String>, Option<String>), // lexical, datatype, language
}

/// Normalizes a literal to a `TermKey`. Per RDF 1.1 a plain literal and an
/// explicit `xsd:string` literal are the same term, so `xsd:string` collapses
/// to "no datatype".
fn lit_key(lexical: &str, datatype: Option<&str>, lang: Option<&str>) -> TermKey {
    let dt = datatype
        .filter(|d| d.strip_prefix(XSD) != Some("string"))
        .map(str::to_string);
    TermKey::Lit(lexical.to_string(), dt, lang.map(str::to_string))
}

/// Resolves an expression to its RDF term identity, or `None` if it is unbound
/// or a computed (non-term) expression — in which case `sameTerm` errors.
fn term_key(
    expr: &Expression,
    row: &[u32],
    vars: &[String],
    store: &TripleStore,
) -> Option<TermKey> {
    match expr {
        Expression::NamedNode(nn) => Some(TermKey::Iri(nn.as_str().to_string())),
        Expression::Literal(lit) => Some(lit_key(
            lit.value(),
            Some(lit.datatype().as_str()),
            lit.language(),
        )),
        Expression::Variable(v) => {
            let col = vars.iter().position(|x| x == v.as_str())?;
            let id = row[col];
            if id == NULL_ID {
                return None;
            }
            let value = store.dict.resolve(id)?;
            match store.dict.resolve_type(id)? {
                TermType::Iri => Some(TermKey::Iri(value.into_owned())),
                TermType::BlankNode => Some(TermKey::Blank(value.into_owned())),
                TermType::Literal { datatype, lang } => {
                    Some(lit_key(&value, datatype.as_deref(), lang.as_deref()))
                }
            }
        }
        _ => None,
    }
}

fn eval(expr: &Expression, row: &[u32], vars: &[String], store: &TripleStore) -> Result<Fv, ()> {
    match expr {
        Expression::NamedNode(nn) => Ok(Fv::Iri(nn.as_str().to_string())),
        Expression::Literal(lit) => Ok(literal_to_fv(lit)),
        Expression::Variable(v) => {
            let col = vars.iter().position(|x| x == v.as_str()).ok_or(())?;
            let id = row[col];
            if id == NULL_ID {
                return Err(());
            }
            term_to_fv(id, store).ok_or(())
        }
        Expression::Or(a, b) => {
            // SPARQL 3-valued OR: true if either operand is true (even if the
            // other errors). Short-circuits once a `true` is found.
            let ea = ebv(a, row, vars, store);
            if ea == Ok(true) {
                return Ok(Fv::Bool(true));
            }
            let eb = ebv(b, row, vars, store);
            if eb == Ok(true) {
                return Ok(Fv::Bool(true));
            }
            if ea.is_err() || eb.is_err() {
                Err(()) // false-or-error / error-or-error -> error
            } else {
                Ok(Fv::Bool(false))
            }
        }
        Expression::And(a, b) => {
            // SPARQL 3-valued AND: false if either operand is false (even if the
            // other errors). Short-circuits once a `false` is found.
            let ea = ebv(a, row, vars, store);
            if ea == Ok(false) {
                return Ok(Fv::Bool(false));
            }
            let eb = ebv(b, row, vars, store);
            if eb == Ok(false) {
                return Ok(Fv::Bool(false));
            }
            if ea.is_err() || eb.is_err() {
                Err(()) // true-and-error / error-and-error -> error
            } else {
                Ok(Fv::Bool(true))
            }
        }
        Expression::Not(a) => Ok(Fv::Bool(!ebv(a, row, vars, store)?)),
        Expression::Equal(a, b) => {
            let (x, y) = (eval(a, row, vars, store)?, eval(b, row, vars, store)?);
            fv_equal(&x, &y).map(Fv::Bool).ok_or(())
        }
        Expression::SameTerm(a, b) => {
            // Exact term identity — NOT value equality (no numeric promotion).
            match (term_key(a, row, vars, store), term_key(b, row, vars, store)) {
                (Some(x), Some(y)) => Ok(Fv::Bool(x == y)),
                _ => Err(()), // unbound or non-term operand
            }
        }
        Expression::Greater(a, b) => {
            cmp_op(a, b, row, vars, store, |o| o == std::cmp::Ordering::Greater)
        }
        Expression::GreaterOrEqual(a, b) => {
            cmp_op(a, b, row, vars, store, |o| o != std::cmp::Ordering::Less)
        }
        Expression::Less(a, b) => cmp_op(a, b, row, vars, store, |o| o == std::cmp::Ordering::Less),
        Expression::LessOrEqual(a, b) => {
            cmp_op(a, b, row, vars, store, |o| o != std::cmp::Ordering::Greater)
        }
        Expression::Add(a, b) => num_op(a, b, row, vars, store, |x, y| x + y),
        Expression::Subtract(a, b) => num_op(a, b, row, vars, store, |x, y| x - y),
        Expression::Multiply(a, b) => num_op(a, b, row, vars, store, |x, y| x * y),
        Expression::Divide(a, b) => {
            let x = as_num(&eval(a, row, vars, store)?).ok_or(())?;
            let y = as_num(&eval(b, row, vars, store)?).ok_or(())?;
            // SPARQL 1.1: division by zero is an error -> the row is dropped.
            if y == 0.0 {
                return Err(());
            }
            Ok(Fv::Num(x / y))
        }
        Expression::UnaryPlus(a) => Ok(Fv::Num(as_num(&eval(a, row, vars, store)?).ok_or(())?)),
        Expression::UnaryMinus(a) => Ok(Fv::Num(-as_num(&eval(a, row, vars, store)?).ok_or(())?)),
        Expression::Bound(v) => {
            let col = vars.iter().position(|x| x == v.as_str());
            Ok(Fv::Bool(col.is_some_and(|c| row[c] != NULL_ID)))
        }
        Expression::In(e, list) => {
            let x = eval(e, row, vars, store)?;
            for item in list {
                if let Ok(y) = eval(item, row, vars, store)
                    && fv_equal(&x, &y) == Some(true)
                {
                    return Ok(Fv::Bool(true));
                }
            }
            Ok(Fv::Bool(false))
        }
        Expression::If(c, a, b) => {
            if ebv(c, row, vars, store)? {
                eval(a, row, vars, store)
            } else {
                eval(b, row, vars, store)
            }
        }
        Expression::FunctionCall(func, args) => eval_func(func, args, row, vars, store),
        _ => Err(()), // unsupported -> error (row is dropped)
    }
}

fn cmp_op(
    a: &Expression,
    b: &Expression,
    row: &[u32],
    vars: &[String],
    store: &TripleStore,
    pred: impl Fn(std::cmp::Ordering) -> bool,
) -> Result<Fv, ()> {
    let (x, y) = (eval(a, row, vars, store)?, eval(b, row, vars, store)?);
    fv_cmp(&x, &y).map(|o| Fv::Bool(pred(o))).ok_or(())
}

fn num_op(
    a: &Expression,
    b: &Expression,
    row: &[u32],
    vars: &[String],
    store: &TripleStore,
    op: impl Fn(f64, f64) -> f64,
) -> Result<Fv, ()> {
    let x = as_num(&eval(a, row, vars, store)?).ok_or(())?;
    let y = as_num(&eval(b, row, vars, store)?).ok_or(())?;
    Ok(Fv::Num(op(x, y)))
}

fn eval_func(
    func: &Function,
    args: &[Expression],
    row: &[u32],
    vars: &[String],
    store: &TripleStore,
) -> Result<Fv, ()> {
    let arg = |i: usize| eval(&args[i], row, vars, store);
    match func {
        Function::Str => Ok(Fv::Str(match arg(0)? {
            Fv::Iri(s) | Fv::Blank(s) | Fv::Str(s) | Fv::Lang(s, _) | Fv::Typed(s, _) => s,
            Fv::Num(n) => format_num(n),
            Fv::Bool(b) => b.to_string(),
        })),
        Function::Lang => Ok(Fv::Str(match arg(0)? {
            Fv::Lang(_, l) => l,
            _ => String::new(),
        })),
        Function::Datatype => Ok(Fv::Iri(match arg(0)? {
            Fv::Num(_) => format!("{XSD}double"),
            Fv::Bool(_) => format!("{XSD}boolean"),
            Fv::Str(_) => format!("{XSD}string"),
            Fv::Lang(..) => "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString".to_string(),
            Fv::Typed(_, dt) => dt,
            Fv::Iri(_) | Fv::Blank(_) => return Err(()), // datatype() only for literals
        })),
        Function::StrLen => Ok(Fv::Num(as_str(&arg(0)?).ok_or(())?.chars().count() as f64)),
        Function::UCase => Ok(Fv::Str(as_str(&arg(0)?).ok_or(())?.to_uppercase())),
        Function::LCase => Ok(Fv::Str(as_str(&arg(0)?).ok_or(())?.to_lowercase())),
        Function::Contains => str2(&arg(0)?, &arg(1)?, |a, b| a.contains(b)),
        Function::StrStarts => str2(&arg(0)?, &arg(1)?, |a, b| a.starts_with(b)),
        Function::StrEnds => str2(&arg(0)?, &arg(1)?, |a, b| a.ends_with(b)),
        Function::IsIri => Ok(Fv::Bool(matches!(arg(0)?, Fv::Iri(_)))),
        Function::IsBlank => Ok(Fv::Bool(matches!(arg(0)?, Fv::Blank(_)))),
        Function::IsLiteral => Ok(Fv::Bool(!matches!(arg(0)?, Fv::Iri(_) | Fv::Blank(_)))),
        Function::IsNumeric => Ok(Fv::Bool(matches!(arg(0)?, Fv::Num(_)))),
        Function::Regex => {
            let text = as_str(&arg(0)?).ok_or(())?.to_string();
            let pattern = as_str(&arg(1)?).ok_or(())?.to_string();
            let flags = match args.get(2) {
                Some(_) => as_str(&arg(2)?).ok_or(())?.to_string(),
                None => String::new(),
            };
            let re = cached_regex(&pattern, &flags)?;
            Ok(Fv::Bool(re.is_match(&text)))
        }
        _ => Err(()),
    }
}

fn str2(a: &Fv, b: &Fv, f: impl Fn(&str, &str) -> bool) -> Result<Fv, ()> {
    Ok(Fv::Bool(f(as_str(a).ok_or(())?, as_str(b).ok_or(())?)))
}

/// Compiles (and process-wide caches) a `REGEX(text, pattern, flags)` regex.
/// Patterns are typically literal in the query and re-evaluated per row, so a
/// cache avoids recompiling the same pattern for every row of a large result.
/// SPARQL/XPath flags: `i` case-insensitive, `s` dot matches newline, `m`
/// multiline (`^`/`$` match at line boundaries); `x` (extended) is not
/// supported and is rejected like any other unknown flag.
fn cached_regex(pattern: &str, flags: &str) -> Result<regex::Regex, ()> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<rustc_hash::FxHashMap<(String, String), regex::Regex>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(rustc_hash::FxHashMap::default()));
    let key = (pattern.to_string(), flags.to_string());
    if let Some(re) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(re.clone());
    }
    if !flags.chars().all(|c| matches!(c, 'i' | 's' | 'm')) {
        return Err(());
    }
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(flags.contains('i'))
        .dot_matches_new_line(flags.contains('s'))
        .multi_line(flags.contains('m'))
        .build()
        .map_err(|_| ())?;
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, re.clone());
    Ok(re)
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Effective boolean value of an expression.
fn ebv(expr: &Expression, row: &[u32], vars: &[String], store: &TripleStore) -> Result<bool, ()> {
    match eval(expr, row, vars, store)? {
        Fv::Bool(b) => Ok(b),
        Fv::Num(n) => Ok(n != 0.0 && !n.is_nan()),
        Fv::Str(s) => Ok(!s.is_empty()),
        _ => Err(()),
    }
}

/// Names the first construct in `expr` that `eval` cannot evaluate, if any.
/// Conservative: a short-circuited operand still counts as used.
fn unsupported_in_expr(expr: &Expression) -> Option<String> {
    let any = |es: &[&Expression]| es.iter().copied().find_map(unsupported_in_expr);
    match expr {
        Expression::Exists(_) => Some("EXISTS".to_string()),
        Expression::Coalesce(_) => Some("COALESCE".to_string()),
        Expression::FunctionCall(f, args) => {
            if matches!(
                f,
                Function::Str
                    | Function::Lang
                    | Function::Datatype
                    | Function::StrLen
                    | Function::UCase
                    | Function::LCase
                    | Function::Contains
                    | Function::StrStarts
                    | Function::StrEnds
                    | Function::IsIri
                    | Function::IsBlank
                    | Function::IsLiteral
                    | Function::IsNumeric
                    | Function::Regex
            ) {
                any(&args.iter().collect::<Vec<_>>())
            } else {
                Some(f.to_string())
            }
        }
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => any(&[a, b]),
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => any(&[a]),
        Expression::If(c, a, b) => any(&[c, a, b]),
        Expression::In(e, list) => any(&[e]).or_else(|| any(&list.iter().collect::<Vec<_>>())),
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => None,
    }
}

/// Keeps a row if **all** FILTER expressions evaluate to EBV true.
fn row_passes(filters: &[&Expression], row: &[u32], vars: &[String], store: &TripleStore) -> bool {
    filters.iter().all(|f| ebv(f, row, vars, store) == Ok(true))
}

/// Rejects a `FILTER`/`BIND` expression [`eval`] cannot evaluate, before any row
/// sees it. `eval` reports an unimplemented construct exactly like a type error,
/// and `FILTER` reads an error as "row does not pass", so `FILTER NOT EXISTS`
/// would answer every query with zero rows and `BIND` leave the variable
/// unbound. [`hash_join`] does the same for the expression inside an `OPTIONAL`.
fn check_expr(expr: &Expression) -> Result<(), String> {
    match unsupported_in_expr(expr) {
        None => Ok(()),
        Some(what) => Err(format!("Unsupported expression in a FILTER/BIND: {what}")),
    }
}

fn translate_bgp(
    bgp: &[spargebra::term::TriplePattern],
    dict: &Dictionary,
) -> Result<Option<GraphPattern>, String> {
    let mut patterns = Vec::with_capacity(bgp.len());
    for tp in bgp {
        let subject = match translate_term_pattern(&tp.subject, dict)? {
            TranslationResult::Term(t) => t,
            TranslationResult::UnknownConstant => return Ok(None),
        };
        let predicate = match translate_named_node_pattern(&tp.predicate, dict)? {
            TranslationResult::Term(t) => t,
            TranslationResult::UnknownConstant => return Ok(None),
        };
        let object = match translate_term_pattern(&tp.object, dict)? {
            TranslationResult::Term(t) => t,
            TranslationResult::UnknownConstant => return Ok(None),
        };
        patterns.push(TriplePattern {
            subject,
            predicate,
            object,
        });
    }
    Ok(Some(GraphPattern { patterns }))
}

#[derive(Debug, Clone)]
enum TranslationResult {
    Term(PatternTerm),
    UnknownConstant,
}

fn translate_named_node_pattern(
    np: &spargebra::term::NamedNodePattern,
    dict: &Dictionary,
) -> Result<TranslationResult, String> {
    match np {
        spargebra::term::NamedNodePattern::NamedNode(nn) => {
            let iri = nn.as_str();
            match dict.lookup_iri(iri) {
                Some(id) => Ok(TranslationResult::Term(PatternTerm::Bound(id))),
                None => Ok(TranslationResult::UnknownConstant),
            }
        }
        spargebra::term::NamedNodePattern::Variable(v) => Ok(TranslationResult::Term(
            PatternTerm::Variable(v.as_str().to_string()),
        )),
    }
}

fn translate_term_pattern(
    tp: &spargebra::term::TermPattern,
    dict: &Dictionary,
) -> Result<TranslationResult, String> {
    match tp {
        spargebra::term::TermPattern::NamedNode(nn) => match dict.lookup_iri(nn.as_str()) {
            Some(id) => Ok(TranslationResult::Term(PatternTerm::Bound(id))),
            None => Ok(TranslationResult::UnknownConstant),
        },
        spargebra::term::TermPattern::Variable(v) => Ok(TranslationResult::Term(
            PatternTerm::Variable(v.as_str().to_string()),
        )),
        spargebra::term::TermPattern::Literal(lit) => {
            // Lexical value **and** datatype/language tag must match:
            // "25"^^xsd:integer must not match "25"^^xsd:string or the IRI 25.
            match dict.lookup_term(lit.value(), &literal_term_type(lit)) {
                Some(id) => Ok(TranslationResult::Term(PatternTerm::Bound(id))),
                None => Ok(TranslationResult::UnknownConstant),
            }
        }
        // Blank nodes in query patterns act like non-distinguished variables
        // (and are exactly what spargebra inserts as the intermediate node for
        // expanded sequence paths `p1/p2`). Map with a stable internal name so
        // the same `_:b` joins across multiple triples.
        spargebra::term::TermPattern::BlankNode(bn) => Ok(TranslationResult::Term(
            PatternTerm::Variable(format!("__bn_{}", bn.as_str())),
        )),
    }
}

// ---------------------------------------------------------------------------
// SPARQL-JSON Output
// ---------------------------------------------------------------------------

fn term_to_json(id: u32, dict: &Dictionary) -> Value {
    if id == NULL_ID {
        return Value::Null;
    }
    let value = dict.resolve(id);
    let typ = dict.resolve_type(id);
    match (value, typ) {
        (Some(v), Some(TermType::Iri)) => json!({ "type": "uri", "value": v }),
        (Some(v), Some(TermType::BlankNode)) => json!({ "type": "bnode", "value": v }),
        (Some(v), Some(TermType::Literal { datatype, lang })) => {
            let mut obj = json!({ "type": "literal", "value": v });
            if let Some(dt) = datatype {
                obj["datatype"] = json!(dt);
            }
            if let Some(l) = lang {
                obj["xml:lang"] = json!(l);
            }
            obj
        }
        (Some(v), _) => json!({ "type": "literal", "value": v }),
        (None, _) => json!({ "type": "literal", "value": format!("__id_{}", id) }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> TripleStore {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (
                "http://example.org/alice",
                "http://example.org/knows",
                "http://example.org/bob",
            ),
            (
                "http://example.org/bob",
                "http://example.org/knows",
                "http://example.org/charlie",
            ),
            ("http://example.org/bob", "http://example.org/age", "25"),
            (
                "http://example.org/charlie",
                "http://example.org/knows",
                "http://example.org/alice",
            ),
        ]);
        store
    }

    fn rows_of(store: &TripleStore, query: &str) -> Vec<Value> {
        let engine = HybridEngine::new();
        let result: Value =
            serde_json::from_str(&execute_sparql(store, &engine, query).unwrap()).unwrap();
        result["results"]["bindings"].as_array().unwrap().clone()
    }

    /// [`rows_of`] on the write-locked path, for what needs to intern a term.
    fn rows_of_mut(store: &mut TripleStore, query: &str) -> Vec<Value> {
        let engine = HybridEngine::new();
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(store, &engine, query).unwrap()).unwrap();
        result["results"]["bindings"].as_array().unwrap().clone()
    }

    const XSD_INT: &str = "http://www.w3.org/2001/XMLSchema#integer";

    /// A BGP lookup of a literal must respect the datatype: `"25"`
    /// (= xsd:string) must not match the same-named xsd:integer literal.
    #[test]
    fn bgp_literal_lookup_respects_datatype() {
        let mut store = TripleStore::new();
        let s_int = store
            .dict
            .insert_with_type("http://example.org/sInt", TermType::Iri);
        let s_str = store
            .dict
            .insert_with_type("http://example.org/sStr", TermType::Iri);
        let p = store
            .dict
            .insert_with_type("http://example.org/p", TermType::Iri);
        let o_int = store
            .dict
            .insert_with_type("25", TermType::literal_datatype(XSD_INT));
        let o_str = store.dict.insert_with_type("25", TermType::literal_plain());
        assert_ne!(o_int, o_str, "typed literals must have different IDs");
        store.insert_triple(s_int, p, o_int);
        store.insert_triple(s_str, p, o_str);

        // "25" without a type -> xsd:string -> only sStr.
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/p> \"25\" }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/sStr");

        // "25"^^xsd:integer -> only sInt.
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/p> \"25\"^^<http://www.w3.org/2001/XMLSchema#integer> }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/sInt");
    }

    /// SPARQL 1.1: division by zero is an error -> the row drops out in the
    /// FILTER (no INFINITY that would satisfy `> 0`).
    #[test]
    fn filter_division_by_zero_drops_row() {
        let mut store = TripleStore::new();
        let s = store
            .dict
            .insert_with_type("http://example.org/s", TermType::Iri);
        let p = store
            .dict
            .insert_with_type("http://example.org/v", TermType::Iri);
        let o = store
            .dict
            .insert_with_type("5", TermType::literal_datatype(XSD_INT));
        store.insert_triple(s, p, o);

        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/v> ?v FILTER(?v / 0 > 0) }",
        );
        assert_eq!(rows.len(), 0);
    }

    /// FILTER with `<` on IRIs must compare (previously: always an error -> empty).
    #[test]
    fn filter_iri_less_than_compares() {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[(
            "http://example.org/a",
            "http://example.org/p",
            "http://example.org/b",
        )]);
        // a < b lexically -> one row.
        let rows = rows_of(
            &store,
            "SELECT ?x ?y WHERE { ?x <http://example.org/p> ?y FILTER(?x < ?y) }",
        );
        assert_eq!(rows.len(), 1);
        // reversed: b < a is false -> empty.
        let rows = rows_of(
            &store,
            "SELECT ?x ?y WHERE { ?x <http://example.org/p> ?y FILTER(?y < ?x) }",
        );
        assert_eq!(rows.len(), 0);
    }

    /// isBlank distinguishes blank nodes from IRIs; the IRI object must not pass
    /// as a blank node, and it is output as `bnode`.
    #[test]
    fn isblank_distinguishes_blank_from_iri() {
        let mut store = TripleStore::new();
        let s_b = store
            .dict
            .insert_with_type("http://example.org/sB", TermType::Iri);
        let s_i = store
            .dict
            .insert_with_type("http://example.org/sI", TermType::Iri);
        let p = store
            .dict
            .insert_with_type("http://example.org/p", TermType::Iri);
        let blank = store.dict.insert_with_type("b0", TermType::BlankNode);
        let iri = store
            .dict
            .insert_with_type("http://example.org/o", TermType::Iri);
        store.insert_triple(s_b, p, blank);
        store.insert_triple(s_i, p, iri);

        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { ?s <http://example.org/p> ?o FILTER(isBlank(?o)) }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/sB");
        assert_eq!(rows[0]["o"]["type"], "bnode");

        // isIri is complementary: only the IRI object.
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/p> ?o FILTER(isIri(?o)) }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/sI");
    }

    #[test]
    fn optional_with_match() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?b ?age WHERE { ?a <http://example.org/knows> ?b . OPTIONAL { ?b <http://example.org/age> ?age } }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        // alice->bob has age 25; charlie->alice has no age
        assert_eq!(rows.len(), 3);
        let ages: Vec<Option<i64>> = rows
            .iter()
            .map(|r| {
                r.get("age")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.parse().unwrap())
            })
            .collect();
        assert!(ages.contains(&Some(25)));
        assert!(ages.contains(&None));
    }

    #[test]
    fn select_with_unknown_constant_returns_empty_not_panic() {
        let store = test_store();
        let engine = HybridEngine::new();
        // <…/zzz> does not occur in the store -> empty solution, no panic.
        let query = "SELECT ?p ?o WHERE { <http://example.org/zzz> ?p ?o }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        assert_eq!(result["results"]["bindings"].as_array().unwrap().len(), 0);
        // Head variables are preserved.
        let head: Vec<&str> = result["head"]["vars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(head, vec!["p", "o"]);
    }

    #[test]
    fn optional_multi_match_expands_rows() {
        // bob has two ages -> the left row alice->bob must expand to TWO output
        // rows; carol (no age) stays a single NULL row.
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (
                "http://example.org/alice",
                "http://example.org/knows",
                "http://example.org/bob",
            ),
            (
                "http://example.org/alice",
                "http://example.org/knows",
                "http://example.org/carol",
            ),
            ("http://example.org/bob", "http://example.org/age", "25"),
            ("http://example.org/bob", "http://example.org/age", "26"),
        ]);
        let engine = HybridEngine::new();
        let query = "SELECT ?b ?age WHERE { ?a <http://example.org/knows> ?b . OPTIONAL { ?b <http://example.org/age> ?age } }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        // bob×{25,26} = 2 rows + carol×NULL = 1 row.
        assert_eq!(rows.len(), 3);
        let ages: Vec<Option<String>> = rows
            .iter()
            .map(|r| {
                r.get("age")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(ages.contains(&Some("25".to_string())));
        assert!(ages.contains(&Some("26".to_string())));
        assert!(ages.contains(&None));
    }

    #[test]
    fn optional_without_match_returns_null() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?c WHERE { ?a <http://example.org/knows> ?b . OPTIONAL { ?b <http://example.org/unknown> ?c } }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert!(row.get("c").is_none() || row["c"].is_null());
        }
    }

    #[test]
    fn distinct_applies_after_projection() {
        // Two triples with predicate knows + one with age.
        // SELECT DISTINCT ?p must return {knows, age} = 2 rows,
        // NOT 3 (knows must not appear twice).
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT DISTINCT ?p WHERE { ?s ?p ?o }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            2,
            "DISTINCT ?p should dedup on the projected column"
        );
    }

    #[test]
    fn limit_after_projection() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT DISTINCT ?p WHERE { ?s ?p ?o } LIMIT 1";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn filter_numeric_comparison() {
        let mut store = TripleStore::new();
        let dt = "http://www.w3.org/2001/XMLSchema#integer";
        let alice = store.dict.insert("http://example.org/alice");
        let bob = store.dict.insert("http://example.org/bob");
        let age = store.dict.insert("http://example.org/age");
        let v30 = store
            .dict
            .insert_with_type("30", TermType::literal_datatype(dt));
        let v25 = store
            .dict
            .insert_with_type("25", TermType::literal_datatype(dt));
        store.insert_triple(alice, age, v30);
        store.insert_triple(bob, age, v25);
        let engine = HybridEngine::new();
        let query = "SELECT ?p ?a WHERE { ?p <http://example.org/age> ?a FILTER(?a > 26) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "only 30 > 26");
        assert_eq!(rows[0]["a"]["value"], "30");
    }

    #[test]
    fn filter_str_function() {
        let store = test_store();
        let engine = HybridEngine::new();
        // CONTAINS(STR(?b), "bob") -> only alice->bob.
        let query = "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b \
                     FILTER(CONTAINS(STR(?b), \"bob\")) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["b"]["value"], "http://example.org/bob");
    }

    #[test]
    fn filter_iri_inequality_and_logical() {
        let store = test_store(); // knows: alice->bob, bob->charlie, charlie->alice
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b \
                     FILTER(?b != <http://example.org/charlie>) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        // bob (->charlie) drops out; alice->bob and charlie->alice remain.
        assert_eq!(rows.len(), 2);
        for r in rows {
            assert_ne!(r["b"]["value"], "http://example.org/charlie");
        }
    }

    fn values_of<'a>(rows: &'a [Value], var: &str) -> Vec<&'a str> {
        rows.iter()
            .map(|r| {
                r.get(var)
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            })
            .collect()
    }

    #[test]
    fn order_by_ascending_on_iri() {
        let store = test_store(); // knows objects: bob, charlie, alice
        let engine = HybridEngine::new();
        let query = "SELECT ?b WHERE { ?a <http://example.org/knows> ?b } ORDER BY ?b";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(
            values_of(rows, "b"),
            vec![
                "http://example.org/alice",
                "http://example.org/bob",
                "http://example.org/charlie",
            ]
        );
    }

    #[test]
    fn order_by_desc_with_limit() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?b WHERE { ?a <http://example.org/knows> ?b } \
                     ORDER BY DESC(?b) LIMIT 2";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(
            values_of(rows, "b"),
            vec!["http://example.org/charlie", "http://example.org/bob"]
        );
    }

    #[test]
    fn order_by_numeric_not_lexical() {
        // 9 < 25 < 100 numerically; lexically it would be "100" < "25" < "9".
        let mut store = TripleStore::new();
        let dt = "http://www.w3.org/2001/XMLSchema#integer";
        let s = store.dict.insert("http://example.org/x");
        let age = store.dict.insert("http://example.org/age");
        for v in ["100", "9", "25"] {
            let o = store
                .dict
                .insert_with_type(v, TermType::literal_datatype(dt));
            store.insert_triple(s, age, o);
        }
        let engine = HybridEngine::new();
        let query = "SELECT ?a WHERE { ?s <http://example.org/age> ?a } ORDER BY ?a";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(values_of(rows, "a"), vec!["9", "25", "100"]);
    }

    #[test]
    fn order_by_with_distinct() {
        let store = test_store();
        let engine = HybridEngine::new();
        // DISTINCT ?p -> {knows, age}; ORDER BY ?p sorts the two predicates.
        let query = "SELECT DISTINCT ?p WHERE { ?s ?p ?o } ORDER BY ?p";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(
            values_of(rows, "p"),
            vec!["http://example.org/age", "http://example.org/knows"]
        );
    }

    #[test]
    fn union_combines_branches_same_var() {
        let store = test_store();
        let engine = HybridEngine::new();
        // Branch 1: knows objects (bob, charlie, alice); branch 2: age object (25).
        let query = "SELECT ?o WHERE { \
                     { ?s <http://example.org/knows> ?o } UNION \
                     { ?s <http://example.org/age> ?o } }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let mut vals = values_of(result["results"]["bindings"].as_array().unwrap(), "o");
        vals.sort();
        assert_eq!(
            vals,
            vec![
                "25",
                "http://example.org/alice",
                "http://example.org/bob",
                "http://example.org/charlie",
            ]
        );
    }

    #[test]
    fn union_aligns_differing_vars_with_null() {
        let store = test_store();
        let engine = HybridEngine::new();
        // Branch 1 binds only ?a, branch 2 only ?b -> columns must be NULL-aligned.
        let query = "SELECT ?a ?b WHERE { \
                     { ?a <http://example.org/knows> <http://example.org/bob> } UNION \
                     { ?b <http://example.org/knows> <http://example.org/alice> } }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // One row has a=alice (b NULL), the other b=charlie (a NULL).
        let a_only = rows.iter().any(|r| {
            r.get("a")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                == Some("http://example.org/alice")
                && (r.get("b").is_none() || r["b"].is_null())
        });
        let b_only = rows.iter().any(|r| {
            r.get("b")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                == Some("http://example.org/charlie")
                && (r.get("a").is_none() || r["a"].is_null())
        });
        assert!(a_only, "row with only ?a=alice is missing");
        assert!(b_only, "row with only ?b=charlie is missing");
    }

    fn path_store() -> TripleStore {
        // Chain alice -> bob -> carol -> dave (knows) + alice likes eve.
        let mut store = TripleStore::new();
        let k = "http://example.org/knows";
        let l = "http://example.org/likes";
        let e = "http://example.org/";
        store.ingest_str_triples(&[
            (&format!("{e}alice"), k, &format!("{e}bob")),
            (&format!("{e}bob"), k, &format!("{e}carol")),
            (&format!("{e}carol"), k, &format!("{e}dave")),
            (&format!("{e}alice"), l, &format!("{e}eve")),
        ]);
        store
    }

    fn obj_values(query: &str) -> Vec<String> {
        let store = path_store();
        let engine = HybridEngine::new();
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        let mut vals: Vec<String> = rows
            .iter()
            .filter_map(|r| {
                r.get("o")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim_start_matches("http://example.org/").to_string())
            })
            .collect();
        vals.sort();
        vals
    }

    #[test]
    fn path_one_or_more() {
        // alice knows+ ?o -> bob, carol, dave (transitive closure, without alice).
        assert_eq!(
            obj_values(
                "SELECT ?o WHERE { <http://example.org/alice> <http://example.org/knows>+ ?o }"
            ),
            vec!["bob", "carol", "dave"]
        );
    }

    #[test]
    fn path_zero_or_more_includes_self() {
        // alice knows* ?o -> alice (0 steps), bob, carol, dave.
        assert_eq!(
            obj_values(
                "SELECT ?o WHERE { <http://example.org/alice> <http://example.org/knows>* ?o }"
            ),
            vec!["alice", "bob", "carol", "dave"]
        );
    }

    #[test]
    fn path_zero_or_one() {
        // alice knows? ?o -> alice (0) + bob (1).
        assert_eq!(
            obj_values(
                "SELECT ?o WHERE { <http://example.org/alice> <http://example.org/knows>? ?o }"
            ),
            vec!["alice", "bob"]
        );
    }

    #[test]
    fn path_sequence() {
        // alice knows/knows ?o -> carol (2 steps).
        assert_eq!(
            obj_values(
                "SELECT ?o WHERE { <http://example.org/alice> <http://example.org/knows>/<http://example.org/knows> ?o }"
            ),
            vec!["carol"]
        );
    }

    #[test]
    fn path_alternative() {
        // alice (knows|likes) ?o -> bob, eve.
        assert_eq!(
            obj_values(
                "SELECT ?o WHERE { <http://example.org/alice> (<http://example.org/knows>|<http://example.org/likes>) ?o }"
            ),
            vec!["bob", "eve"]
        );
    }

    #[test]
    fn path_inverse() {
        // bob ^knows ?s -> who knows bob = alice (evaluated over the ?s column).
        let store = path_store();
        let engine = HybridEngine::new();
        let q = "SELECT ?s WHERE { <http://example.org/bob> ^<http://example.org/knows> ?s }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, q).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/alice");
    }

    #[test]
    fn path_both_variables_transitive() {
        // ?s knows+ ?o -> all reachable pairs: (alice,{bob,carol,dave}),
        // (bob,{carol,dave}), (carol,{dave}) = 6 pairs.
        let store = path_store();
        let engine = HybridEngine::new();
        let q = "SELECT ?s ?o WHERE { ?s <http://example.org/knows>+ ?o }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, q).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 6, "6 transitively reachable (s,o) pairs");
    }

    #[test]
    fn path_in_c2rpq_join() {
        // C2RPQ form: path join BGP. All triples in ONE ingest, since
        // ingest_str_triples rebuilds the indexes each time (does not append).
        let mut store = TripleStore::new();
        let k = "http://example.org/knows";
        let l = "http://example.org/likes";
        let e = "http://example.org/";
        store.ingest_str_triples(&[
            (&format!("{e}alice"), k, &format!("{e}bob")),
            (&format!("{e}bob"), k, &format!("{e}carol")),
            (&format!("{e}carol"), k, &format!("{e}dave")),
            (&format!("{e}alice"), l, &format!("{e}eve")),
            (&format!("{e}carol"), l, &format!("{e}zed")),
        ]);
        let engine = HybridEngine::new();
        // ?s knows+ ?mid . ?mid likes ?z  -> ?mid must be carol (carol likes zed),
        // reachable from alice and bob -> 2 hits.
        let q = "SELECT ?s ?z WHERE { ?s <http://example.org/knows>+ ?mid . \
                 ?mid <http://example.org/likes> ?z }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, q).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "alice/bob knows+ carol; carol likes zed");
        for r in rows {
            assert_eq!(r["z"]["value"], "http://example.org/zed");
        }
    }

    #[test]
    fn stream_produces_ndjson_lines() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b }";
        let select = evaluate_select(&store, &engine, query).unwrap();
        assert_eq!(select.rows.n_rows(), 3);
        assert_eq!(select.vars, vec!["a", "b"]);
    }

    #[test]
    fn chained_optional_referencing_prior_optional_var() {
        // Reproduces the WDBench `opts` q3 shape (a chained OPTIONAL):
        // an OPTIONAL whose pattern uses a variable bound only in an EARLIER
        // OPTIONAL (a "not well-designed" pattern).
        //   ?x1 P102 <O> .
        //   OPTIONAL { ?x1 P569 ?x2 }
        //   OPTIONAL { ?x1 P19  ?x3 }
        //   OPTIONAL { ?x1 P21  ?x4 }
        //   OPTIONAL { ?x3 P625 ?x5 }   <- ?x3 comes from the 2nd OPTIONAL
        let e = "http://ex/";
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            // base: s1, s2 haben P102 -> O
            (&format!("{e}s1"), &format!("{e}P102"), &format!("{e}O")),
            (&format!("{e}s2"), &format!("{e}P102"), &format!("{e}O")),
            (&format!("{e}s1"), &format!("{e}P569"), &format!("{e}v1")), // x2: s1 only
            (&format!("{e}s1"), &format!("{e}P19"), &format!("{e}m1")),  // x3: s1 -> {m1,m2}
            (&format!("{e}s1"), &format!("{e}P19"), &format!("{e}m2")),
            (&format!("{e}s1"), &format!("{e}P21"), &format!("{e}g")), // x4: s1 only
            (&format!("{e}m1"), &format!("{e}P625"), &format!("{e}c1")), // x5 via x3
            (&format!("{e}m2"), &format!("{e}P625"), &format!("{e}c2")),
            // NOISE: P625 from a subject NOT reachable via x3.
            // A cross-product would wrongly pull this in.
            (&format!("{e}noise"), &format!("{e}P625"), &format!("{e}nc")),
        ]);
        let engine = HybridEngine::new();
        let q = format!(
            "SELECT * WHERE {{ ?x1 <{e}P102> <{e}O> . \
             OPTIONAL {{ ?x1 <{e}P569> ?x2 }} \
             OPTIONAL {{ ?x1 <{e}P19> ?x3 }} \
             OPTIONAL {{ ?x1 <{e}P21> ?x4 }} \
             OPTIONAL {{ ?x3 <{e}P625> ?x5 }} }}"
        );
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, &q).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        // Left-deep, with SPARQL compatibility for the unbound ?x3:
        //   s1×{m1,m2} -> 2 rows with x5=c1/c2, and s2 (?x3 unbound, so its
        //   domain is disjoint from the right's) joins all three P625 triples.
        // This OPTIONAL chain is not well-designed — ?x3 comes from an earlier
        // OPTIONAL — so the noise triple genuinely belongs in the answer.
        // Cross-checked against rdflib 7.6.0, which returns the same 5 rows.
        assert_eq!(rows.len(), 5, "left-deep OPTIONAL chain");
        let pairs: Vec<(Option<&str>, Option<&str>)> = rows
            .iter()
            .map(|r| {
                let get = |k| r.get(k).and_then(|v: &Value| v.get("value")?.as_str());
                (get("x1"), get("x5"))
            })
            .collect();
        for expect in [
            (Some("http://ex/s1"), Some("http://ex/c1")),
            (Some("http://ex/s1"), Some("http://ex/c2")),
            (Some("http://ex/s2"), Some("http://ex/c1")),
            (Some("http://ex/s2"), Some("http://ex/c2")),
            (Some("http://ex/s2"), Some("http://ex/nc")),
        ] {
            assert!(pairs.contains(&expect), "missing {expect:?} in {pairs:?}");
        }
        // ?x3 is bound for s1, so the noise triple stays out of its rows.
        assert!(
            !pairs.contains(&(Some("http://ex/s1"), Some("http://ex/nc"))),
            "{pairs:?}"
        );
    }

    #[test]
    fn limit_on_multi_pattern_bgp_is_pushed_down() {
        // 2-pattern join `?s knows ?b . ?b knows ?x`: s->{b1..b5}, each bi->x.
        // Full result = 5 rows; LIMIT 3 must return exactly 3 valid rows
        // (pipelined DFS terminates early instead of materializing all 5).
        let e = "http://ex/";
        let k = format!("{e}knows");
        let mut triples: Vec<(String, String, String)> = Vec::new();
        for i in 1..=5 {
            triples.push((format!("{e}s"), k.clone(), format!("{e}b{i}")));
            triples.push((format!("{e}b{i}"), k.clone(), format!("{e}x")));
        }
        let refs: Vec<(&str, &str, &str)> = triples
            .iter()
            .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
            .collect();
        let mut store = TripleStore::new();
        store.ingest_str_triples(&refs);
        let engine = HybridEngine::new();
        let q = format!("SELECT ?s ?b ?x WHERE {{ ?s <{k}> ?b . ?b <{k}> ?x }} LIMIT 3");
        let r: Value = serde_json::from_str(&execute_sparql(&store, &engine, &q).unwrap()).unwrap();
        let rows = r["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "LIMIT 3 -> exactly 3 rows");
        for row in rows {
            assert_eq!(row["x"]["value"], "http://ex/x");
            assert!(
                row["b"]["value"]
                    .as_str()
                    .unwrap()
                    .starts_with("http://ex/b")
            );
        }
        // Without LIMIT: all 5.
        let q_all = format!("SELECT ?s ?b ?x WHERE {{ ?s <{k}> ?b . ?b <{k}> ?x }}");
        let r2: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, &q_all).unwrap()).unwrap();
        assert_eq!(r2["results"]["bindings"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn path_sequence_with_closure_rhs() {
        // WDBench paths bug: `<s> (k1/(k2)*) ?x`. spargebra decomposes this into
        // Join(<s> k1 _:b, _:b (k2)* ?x) — the blank-node node must act as a
        // variable, otherwise eval_path would wrongly return 0.
        // Data: s -k1-> m ; m -k2-> n1 -k2-> n2.  Expected ?x = {m,n1,n2}.
        let e = "http://ex/";
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (&format!("{e}s"), &format!("{e}k1"), &format!("{e}m")),
            (&format!("{e}m"), &format!("{e}k2"), &format!("{e}n1")),
            (&format!("{e}n1"), &format!("{e}k2"), &format!("{e}n2")),
        ]);
        let engine = HybridEngine::new();
        let q = format!("SELECT * WHERE {{ <{e}s> (<{e}k1>/(<{e}k2>)*) ?x }}");
        let r: Value = serde_json::from_str(&execute_sparql(&store, &engine, &q).unwrap()).unwrap();
        let mut xs: Vec<String> = r["results"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["x"]["value"].as_str().unwrap().to_string())
            .collect();
        xs.sort();
        assert_eq!(
            xs,
            vec![format!("{e}m"), format!("{e}n1"), format!("{e}n2")],
            "k1/(k2)* must return m (0×k2) + n1 + n2"
        );
        // The __bn_ variable must not be output.
        let head: Vec<&str> = r["head"]["vars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(head, vec!["x"], "only ?x, no __bn_ in SELECT *");
    }

    #[test]
    fn semijoin_over_blank_node_path_bound_object() {
        // c2rpq form: `?x (k1/(k2)*) <const>` (bound object). spargebra ->
        // Join(?x k1 _:b, _:b (k2)* <const>); the right side has ONLY the
        // join key as a variable (semi-join, new_arity=0). Previously hash_join
        // swallowed this (empty bucket) -> 0 rows. Now correct.
        // s -k1-> m -k2-> n1 -k2-> n2.
        let e = "http://ex/";
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (&format!("{e}s"), &format!("{e}k1"), &format!("{e}m")),
            (&format!("{e}m"), &format!("{e}k2"), &format!("{e}n1")),
            (&format!("{e}n1"), &format!("{e}k2"), &format!("{e}n2")),
        ]);
        let engine = HybridEngine::new();
        for target in ["m", "n1", "n2"] {
            // s reaches every target via k1 then k2* -> exactly {s}.
            let q = format!("SELECT * WHERE {{ ?x (<{e}k1>/(<{e}k2>)*) <{e}{target}> }}");
            let r: Value =
                serde_json::from_str(&execute_sparql(&store, &engine, &q).unwrap()).unwrap();
            let rows = r["results"]["bindings"].as_array().unwrap();
            assert_eq!(rows.len(), 1, "target {target}: exactly one ?x");
            assert_eq!(
                rows[0]["x"]["value"],
                format!("{e}s"),
                "target {target}: ?x=s"
            );
        }
    }

    #[test]
    fn semijoin_counts_multiple_right_matches() {
        // The semi-join must preserve the hit count: two ways a->c via b1/b2.
        // `?x knows/knows <c>` (bound object) -> a via b1 AND via b2 = 2 rows.
        let e = "http://ex/";
        let k = format!("{e}knows");
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (&format!("{e}a"), &k, &format!("{e}b1")),
            (&format!("{e}a"), &k, &format!("{e}b2")),
            (&format!("{e}b1"), &k, &format!("{e}c")),
            (&format!("{e}b2"), &k, &format!("{e}c")),
        ]);
        let engine = HybridEngine::new();
        let q = format!("SELECT * WHERE {{ ?x <{k}>/<{k}> <{e}c> }}");
        let r: Value = serde_json::from_str(&execute_sparql(&store, &engine, &q).unwrap()).unwrap();
        let rows = r["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "two paths a->c (via b1 and b2)");
        for row in rows {
            assert_eq!(row["x"]["value"], format!("{e}a"));
        }
    }

    #[test]
    fn ingests_and_queries_blank_nodes() {
        // Blank-node triples are loaded (not skipped), output as `bnode`, and
        // the same `_:b0` across multiple lines is the same node (document-scoped
        // identity -> join works).
        let path = std::env::temp_dir().join("trillian_bnode_query_test.nt");
        std::fs::write(
            &path,
            "<http://ex/alice> <http://ex/knows> _:b0 .\n\
             _:b0 <http://ex/name> \"Bob\" .\n",
        )
        .unwrap();
        let mut store = TripleStore::new();
        store.ingest_ntriples_file(path.to_str().unwrap()).unwrap();
        let engine = HybridEngine::new();

        // (1) Object is a blank node -> type=bnode.
        let q = "SELECT ?x WHERE { <http://ex/alice> <http://ex/knows> ?x }";
        let r: Value = serde_json::from_str(&execute_sparql(&store, &engine, q).unwrap()).unwrap();
        let rows = r["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "blank-node triple was loaded");
        assert_eq!(rows[0]["x"]["type"], "bnode");

        // (2) Join over the same blank node (both _:b0 = the same ID).
        let q2 = "SELECT ?n WHERE { <http://ex/alice> <http://ex/knows> ?b . \
                  ?b <http://ex/name> ?n }";
        let r2: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, q2).unwrap()).unwrap();
        let rows2 = r2["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows2.len(), 1, "join over the same blank node");
        assert_eq!(rows2[0]["n"]["value"], "Bob");

        let _ = std::fs::remove_file(&path);
    }

    /// sameTerm is exact RDF-term equality: `"1"^^xsd:integer` and
    /// `"1"^^xsd:double` are different terms, even though `=` promotes them.
    #[test]
    fn sameterm_distinguishes_datatypes() {
        let xsd_double = "http://www.w3.org/2001/XMLSchema#double";
        let mut store = TripleStore::new();
        let s1 = store.dict.insert_with_type("http://ex/s1", TermType::Iri);
        let s2 = store.dict.insert_with_type("http://ex/s2", TermType::Iri);
        let p = store.dict.insert_with_type("http://ex/p", TermType::Iri);
        let v_int = store
            .dict
            .insert_with_type("1", TermType::literal_datatype(XSD_INT));
        let v_dbl = store
            .dict
            .insert_with_type("1", TermType::literal_datatype(xsd_double));
        store.insert_triple(s1, p, v_int);
        store.insert_triple(s2, p, v_dbl);

        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://ex/p> ?v \
             FILTER(sameTerm(?v, \"1\"^^<http://www.w3.org/2001/XMLSchema#integer>)) }",
        );
        assert_eq!(rows.len(), 1, "sameTerm must match only the integer term");
        assert_eq!(rows[0]["s"]["value"], "http://ex/s1");

        // value-equality promotes numerically -> matches both.
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://ex/p> ?v \
             FILTER(?v = \"1\"^^<http://www.w3.org/2001/XMLSchema#integer>) }",
        );
        assert_eq!(rows.len(), 2, "= promotes numerics across datatypes");
    }

    /// SPARQL three-valued OR/AND: a `true`/`false` wins even when the other
    /// operand errors (and that operand is short-circuited).
    #[test]
    fn logical_or_and_three_valued_with_error() {
        let mut store = TripleStore::new();
        let s = store.dict.insert_with_type("http://ex/s", TermType::Iri);
        let p = store.dict.insert_with_type("http://ex/v", TermType::Iri);
        let o = store
            .dict
            .insert_with_type("5", TermType::literal_datatype(XSD_INT));
        store.insert_triple(s, p, o);
        let q = |f: &str| {
            rows_of(
                &store,
                &format!("SELECT ?s WHERE {{ ?s <http://ex/v> ?v FILTER({f}) }}"),
            )
            .len()
        };
        assert_eq!(q("?v = 5 || (?v / 0) > 0"), 1, "true || error -> true");
        assert_eq!(q("(?v / 0) > 0 || ?v = 5"), 1, "error || true -> true");
        assert_eq!(q("?v = 99 || (?v / 0) > 0"), 0, "false || error -> error");
        assert_eq!(q("?v = 99 && (?v / 0) > 0"), 0, "false && error -> false");
    }

    #[test]
    fn insert_and_delete_data() {
        let mut store = TripleStore::new();
        execute_update(
            &mut store,
            "INSERT DATA { <http://ex/a> <http://ex/knows> <http://ex/b> }",
            None,
        )
        .unwrap();
        assert_eq!(store.triple_count(), 1);
        let rows = rows_of(
            &store,
            "SELECT ?o WHERE { <http://ex/a> <http://ex/knows> ?o }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["o"]["value"], "http://ex/b");

        execute_update(
            &mut store,
            "DELETE DATA { <http://ex/a> <http://ex/knows> <http://ex/b> }",
            None,
        )
        .unwrap();
        assert_eq!(store.triple_count(), 0);
    }

    #[test]
    fn filter_in_strlen_bound() {
        let store = test_store(); // 3 knows edges
        let only_bob = rows_of(
            &store,
            "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b \
             FILTER(?b IN (<http://example.org/bob>)) }",
        );
        assert_eq!(only_bob.len(), 1);
        let long = rows_of(
            &store,
            "SELECT ?b WHERE { ?a <http://example.org/knows> ?b \
             FILTER(STRLEN(STR(?b)) > 10) }",
        );
        assert_eq!(long.len(), 3);
        let bound = rows_of(
            &store,
            "SELECT ?b WHERE { ?a <http://example.org/knows> ?b FILTER(BOUND(?b)) }",
        );
        assert_eq!(bound.len(), 3);
    }

    // ── Inference integration tests ──────────────────────────────────────

    #[test]
    fn inference_subclasof_returns_additional_results() {
        let mut store = TripleStore::new();
        // RDFS schema: Dog subClassOf Animal
        store.ingest_str_triples(&[
            (
                "http://example.org/Dog",
                "http://www.w3.org/2000/01/rdf-schema#subClassOf",
                "http://example.org/Animal",
            ),
            // Data: Fido is a Dog
            (
                "http://example.org/Fido",
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "http://example.org/Dog",
            ),
        ]);

        let engine = HybridEngine::new();

        // Without inference: only direct type matches
        let no_infer: Value = serde_json::from_str(
            &execute_sparql(
                &store,
                &engine,
                "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Animal> }",
            )
            .unwrap(),
        )
        .unwrap();
        let no_infer_rows = no_infer["results"]["bindings"].as_array().unwrap().len();
        assert_eq!(no_infer_rows, 0, "without inference, no direct Animal type");

        // With inference: Fido is a Dog and Dog subClassOf Animal -> Fido type Animal
        let with_infer: Value = serde_json::from_str(
            &execute_sparql_infer(
                &store,
                &engine,
                "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Animal> }",
            )
            .unwrap(),
        )
        .unwrap();
        let with_infer_rows: Vec<&Value> = with_infer["results"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(
            with_infer_rows.len(),
            1,
            "inference should find Fido as Animal"
        );
        assert_eq!(with_infer_rows[0]["s"]["value"], "http://example.org/Fido");
    }

    #[test]
    fn inference_subproperty_returns_additional_results() {
        let mut store = TripleStore::new();
        // RDFS schema: hasPet subPropertyOf hasAnimal
        store.ingest_str_triples(&[
            (
                "http://example.org/hasPet",
                "http://www.w3.org/2000/01/rdf-schema#subPropertyOf",
                "http://example.org/hasAnimal",
            ),
            // Data: Alice hasPet Fido
            (
                "http://example.org/Alice",
                "http://example.org/hasPet",
                "http://example.org/Fido",
            ),
        ]);

        let engine = HybridEngine::new();

        // Without inference: no direct hasAnimal triples
        let no_infer: Value = serde_json::from_str(
            &execute_sparql(
                &store,
                &engine,
                "SELECT ?s ?o WHERE { ?s <http://example.org/hasAnimal> ?o }",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            no_infer["results"]["bindings"].as_array().unwrap().len(),
            0,
            "no direct hasAnimal triples"
        );

        // With inference: hasPet subPropertyOf hasAnimal -> Alice hasAnimal Fido
        let with_infer: Value = serde_json::from_str(
            &execute_sparql_infer(
                &store,
                &engine,
                "SELECT ?s ?o WHERE { ?s <http://example.org/hasAnimal> ?o }",
            )
            .unwrap(),
        )
        .unwrap();
        let rows = with_infer["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "inference should find Alice hasAnimal Fido");
    }

    // -------------------------------------------------------------------
    // REGEX in FILTER
    // -------------------------------------------------------------------

    #[test]
    fn regex_filter_matches_case_insensitive() {
        let store = test_store(); // alice, bob, charlie
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b \
                     FILTER(REGEX(STR(?a), \"ALICE$\", \"i\")) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["a"]["value"], "http://example.org/alice");
    }

    #[test]
    fn regex_filter_no_match_drops_row() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b \
                     FILTER(REGEX(STR(?a), \"^nobody\")) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        assert_eq!(result["results"]["bindings"].as_array().unwrap().len(), 0);
    }

    /// An invalid pattern/flag is a type error per SPARQL semantics: the row
    /// is dropped rather than the query panicking or erroring out entirely.
    #[test]
    fn regex_filter_invalid_pattern_drops_row_not_panics() {
        let store = test_store();
        let engine = HybridEngine::new();
        let bad_pattern = "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b \
                     FILTER(REGEX(STR(?a), \"(unclosed\")) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, bad_pattern).unwrap()).unwrap();
        assert_eq!(result["results"]["bindings"].as_array().unwrap().len(), 0);

        let bad_flag = "SELECT ?a ?b WHERE { ?a <http://example.org/knows> ?b \
                     FILTER(REGEX(STR(?a), \"alice\", \"z\")) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, bad_flag).unwrap()).unwrap();
        assert_eq!(result["results"]["bindings"].as_array().unwrap().len(), 0);
    }

    // -------------------------------------------------------------------
    // BIND
    // -------------------------------------------------------------------

    #[test]
    fn contains_extend_detects_bind() {
        let with_bind = SparqlParser::new()
            .parse_query("SELECT ?y WHERE { ?a <http://example.org/p> ?x BIND(?x + 1 AS ?y) }")
            .unwrap();
        let without_bind = SparqlParser::new()
            .parse_query("SELECT ?x WHERE { ?a <http://example.org/p> ?x }")
            .unwrap();
        let SparqlQuery::Select { pattern, .. } = with_bind else {
            unreachable!()
        };
        assert!(contains_extend(&pattern));
        let SparqlQuery::Select { pattern, .. } = without_bind else {
            unreachable!()
        };
        assert!(!contains_extend(&pattern));
    }

    #[test]
    fn bind_computes_new_arithmetic_value() {
        let mut store = TripleStore::new();
        let dt = "http://www.w3.org/2001/XMLSchema#integer";
        let alice = store.dict.insert("http://example.org/alice");
        let age = store.dict.insert("http://example.org/age");
        let v25 = store
            .dict
            .insert_with_type("25", TermType::literal_datatype(dt));
        store.insert_triple(alice, age, v25);
        let engine = HybridEngine::new();
        let query = "SELECT ?p ?doubled WHERE { \
                     ?p <http://example.org/age> ?a BIND(?a * 2 AS ?doubled) }";
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(&mut store, &engine, query).unwrap())
                .unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["doubled"]["value"], "50");
        assert_eq!(
            rows[0]["doubled"]["datatype"],
            "http://www.w3.org/2001/XMLSchema#double"
        );
    }

    /// BIND is not a filter: a type error in the expression (here, an
    /// unbound operand) leaves the variable unbound rather than dropping
    /// the row — unlike FILTER, which would drop it.
    #[test]
    fn bind_leaves_variable_unbound_on_error() {
        let mut store = test_store(); // 3 knows-triples
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?y WHERE { \
                     ?a <http://example.org/knows> ?b BIND(?missing + 1 AS ?y) }";
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(&mut store, &engine, query).unwrap())
                .unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "rows are kept, not dropped");
        for row in rows {
            assert!(
                row.get("y").is_none(),
                "?y stays unbound and is omitted, not null"
            );
        }
    }

    /// The value BIND interns must be usable in a subsequent FILTER on the
    /// same bound variable.
    #[test]
    fn bind_value_usable_in_subsequent_filter() {
        let mut store = TripleStore::new();
        let dt = "http://www.w3.org/2001/XMLSchema#integer";
        let alice = store.dict.insert("http://example.org/alice");
        let bob = store.dict.insert("http://example.org/bob");
        let age = store.dict.insert("http://example.org/age");
        let v30 = store
            .dict
            .insert_with_type("30", TermType::literal_datatype(dt));
        let v10 = store
            .dict
            .insert_with_type("10", TermType::literal_datatype(dt));
        store.insert_triple(alice, age, v30);
        store.insert_triple(bob, age, v10);
        let engine = HybridEngine::new();
        let query = "SELECT ?p WHERE { \
                     ?p <http://example.org/age> ?a BIND(?a * 2 AS ?y) FILTER(?y > 40) }";
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(&mut store, &engine, query).unwrap())
                .unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "only alice: 30*2=60 > 40");
        assert_eq!(rows[0]["p"]["value"], "http://example.org/alice");
    }

    /// A BIND-computed value must intern to the *same* dictionary ID as an
    /// identical existing value, so a subsequent join on the bound variable
    /// against real triple data still matches (raw-ID equi-join, not just
    /// value equality).
    #[test]
    fn bind_computed_value_joins_with_existing_data() {
        let mut store = TripleStore::new();
        // Arithmetic results are always `Fv::Num`, which BIND interns as
        // xsd:double (see `intern_fv`/`Function::Datatype`) — so the age
        // literal and the baseline literal must share that datatype for the
        // interned ID to line up with the pre-existing one.
        let dt = "http://www.w3.org/2001/XMLSchema#double";
        let alice = store.dict.insert("http://example.org/alice");
        let age = store.dict.insert("http://example.org/age");
        let refnode = store.dict.insert("http://example.org/baseline");
        let has_baseline = store.dict.insert("http://example.org/hasBaseline");
        let v25 = store
            .dict
            .insert_with_type("25", TermType::literal_datatype(dt));
        store.insert_triple(alice, age, v25);
        store.insert_triple(refnode, has_baseline, v25);
        let engine = HybridEngine::new();
        let query = "SELECT ?p ?ref WHERE { \
                     ?p <http://example.org/age> ?a BIND(?a + 0 AS ?y) . \
                     ?ref <http://example.org/hasBaseline> ?y }";
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(&mut store, &engine, query).unwrap())
                .unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["ref"]["value"], "http://example.org/baseline");
    }

    // -----------------------------------------------------------------------
    // Sub-SELECT
    // -----------------------------------------------------------------------

    /// Sub-SELECT store: bob knows three people, alice knows only bob. The
    /// lopsided fan-out lets a LIMIT inside the sub-SELECT change which
    /// subjects survive, which is what the scoping tests below turn on.
    fn subselect_store() -> TripleStore {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (
                "http://example.org/bob",
                "http://example.org/knows",
                "http://example.org/c1",
            ),
            (
                "http://example.org/bob",
                "http://example.org/knows",
                "http://example.org/c2",
            ),
            (
                "http://example.org/bob",
                "http://example.org/knows",
                "http://example.org/c3",
            ),
            (
                "http://example.org/alice",
                "http://example.org/knows",
                "http://example.org/bob",
            ),
            ("http://example.org/bob", "http://example.org/age", "25"),
        ]);
        store
    }

    #[test]
    fn subselect_projects_and_keeps_duplicates() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { { SELECT ?s WHERE { ?s <http://example.org/knows> ?o } } }",
        );
        // No DISTINCT anywhere: bob keeps one row per person he knows.
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec![
                "http://example.org/alice",
                "http://example.org/bob",
                "http://example.org/bob",
                "http://example.org/bob",
            ]
        );
    }

    /// The outer projection must survive the sub-SELECT's own. Both levels
    /// carry a `Project` node, so peeling has to stop at the boundary instead
    /// of overwriting the outer variable list with the inner one.
    #[test]
    fn subselect_does_not_override_outer_projection() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { { SELECT * WHERE { ?s <http://example.org/knows> ?o } } }",
        );
        assert_eq!(rows.len(), 4);
        for r in &rows {
            assert!(r.get("s").is_some());
            assert!(r.get("o").is_none(), "?o is projected away by SELECT ?s");
        }
    }

    /// A variable the sub-SELECT does not project is out of scope outside it,
    /// so the outer pattern may reuse the name for an unrelated binding.
    #[test]
    fn subselect_hides_unprojected_variable() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { \
             { SELECT ?s WHERE { ?s <http://example.org/knows> ?o } } \
             ?s <http://example.org/age> ?o }",
        );
        // Only bob has an age; he appears three times in the sub-result.
        assert_eq!(rows.len(), 3);
        assert_eq!(values_of(&rows, "o"), vec!["25", "25", "25"]);
    }

    /// The sub-SELECT's LIMIT must be applied to *its* result, before the
    /// outer DISTINCT — not hoisted to the outer level, where DISTINCT would
    /// collapse the rows first and change the answer.
    #[test]
    fn subselect_limit_applies_before_outer_distinct() {
        let store = subselect_store();
        // ORDER BY ?o over {bob, c1, c2, c3} -> first two rows are
        // (alice, bob) and (bob, c1) -> DISTINCT ?s keeps both subjects.
        let rows = rows_of(
            &store,
            "SELECT DISTINCT ?s WHERE { \
             { SELECT ?s ?o WHERE { ?s <http://example.org/knows> ?o } ORDER BY ?o LIMIT 2 } }",
        );
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/alice", "http://example.org/bob"]
        );
    }

    #[test]
    fn subselect_order_by_with_limit_and_offset() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?o WHERE { \
             { SELECT ?o WHERE { ?s <http://example.org/knows> ?o } ORDER BY ?o LIMIT 2 OFFSET 1 } }",
        );
        assert_eq!(
            values_of(&rows, "o"),
            vec!["http://example.org/c1", "http://example.org/c2"]
        );
    }

    /// An outer LIMIT over a whole-body sub-SELECT is pushed down as a row cap;
    /// the sub-SELECT's ORDER BY must still see every row.
    #[test]
    fn outer_limit_over_subselect_still_orders_fully() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?o WHERE { \
             { SELECT ?o WHERE { ?s <http://example.org/knows> ?o } ORDER BY ?o } } LIMIT 2 OFFSET 1",
        );
        assert_eq!(
            values_of(&rows, "o"),
            vec!["http://example.org/c1", "http://example.org/c2"]
        );
    }

    #[test]
    fn subselect_nested_two_levels() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { { SELECT DISTINCT ?s WHERE { \
             { SELECT ?s WHERE { ?s <http://example.org/knows> ?o } } } ORDER BY ?s } }",
        );
        assert_eq!(
            values_of(&rows, "s"),
            vec!["http://example.org/alice", "http://example.org/bob"]
        );
    }

    #[test]
    fn subselect_as_union_branch() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { \
             { SELECT ?s WHERE { ?s <http://example.org/age> ?a } } \
             UNION { ?s <http://example.org/knows> <http://example.org/bob> } }",
        );
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/alice", "http://example.org/bob"]
        );
    }

    #[test]
    fn subselect_on_the_right_of_optional() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { \
             ?s <http://example.org/knows> <http://example.org/bob> \
             OPTIONAL { { SELECT ?o WHERE { ?x <http://example.org/knows> ?o } ORDER BY ?o LIMIT 1 } } }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/alice");
        assert_eq!(rows[0]["o"]["value"], "http://example.org/bob");
    }

    /// The motivating case for sub-SELECT: aggregate in the inner query, then
    /// join the aggregate back against the graph — impossible without it.
    #[test]
    fn subselect_aggregate_joins_back_against_the_graph() {
        let mut store = subselect_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?s ?c ?age WHERE { \
                     { SELECT ?s (COUNT(*) AS ?c) WHERE { ?s <http://example.org/knows> ?o } \
                       GROUP BY ?s } \
                     ?s <http://example.org/age> ?age }";
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(&mut store, &engine, query).unwrap())
                .unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "only bob has an age");
        assert_eq!(rows[0]["s"]["value"], "http://example.org/bob");
        assert_eq!(rows[0]["c"]["value"], "3");
        assert_eq!(rows[0]["age"]["value"], "25");
    }

    /// HAVING-style filtering on an aggregate from the enclosing query.
    #[test]
    fn subselect_aggregate_filtered_from_outside() {
        let mut store = subselect_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?s WHERE { \
                     { SELECT ?s (COUNT(*) AS ?c) WHERE { ?s <http://example.org/knows> ?o } \
                       GROUP BY ?s } \
                     FILTER(?c > 1) }";
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(&mut store, &engine, query).unwrap())
                .unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/bob");
    }

    /// A sub-SELECT containing BIND/GROUP BY must route the whole query to the
    /// write-locked path, since the computed values still need interning.
    #[test]
    fn query_needs_write_sees_through_subselect() {
        let store = test_store();
        assert!(query_needs_write(
            "SELECT ?y WHERE { { SELECT ?y WHERE { ?s ?p ?a BIND(?a + 1 AS ?y) } } }",
            &store
        ));
        assert!(query_needs_write(
            "SELECT ?c WHERE { { SELECT (COUNT(*) AS ?c) WHERE { ?s ?p ?o } } }",
            &store
        ));
        assert!(!query_needs_write(
            "SELECT ?s WHERE { { SELECT ?s WHERE { ?s ?p ?o } LIMIT 1 } }",
            &store
        ));
    }

    /// An outer `SELECT *` resolves against the sub-`SELECT`'s projection, not
    /// the inner BGP: `?o` is out of scope, so `*` must not pick it up.
    #[test]
    fn outer_select_star_resolves_against_subselect_projection() {
        let store = subselect_store();
        let rows = rows_of(
            &store,
            "SELECT * WHERE { \
             { SELECT DISTINCT ?s WHERE { ?s <http://example.org/knows> ?o } } }",
        );
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/alice", "http://example.org/bob"]
        );
        for r in &rows {
            assert!(
                r.get("o").is_none(),
                "?o is out of scope outside the sub-SELECT"
            );
        }
    }

    /// Both levels carry a `Slice`, and `SELECT *` gives the outer one nothing
    /// but its `Project`. The inner LIMIT must not overwrite the outer one.
    #[test]
    fn outer_select_star_keeps_its_own_limit_over_subselect() {
        let store = subselect_store();
        // Inner: ORDER BY ?o LIMIT 3 -> bob, c1, c2. Outer LIMIT 2 -> bob, c1.
        let rows = rows_of(
            &store,
            "SELECT * WHERE { \
             { SELECT ?o WHERE { ?s <http://example.org/knows> ?o } ORDER BY ?o LIMIT 3 } \
             } LIMIT 2",
        );
        assert_eq!(
            values_of(&rows, "o"),
            vec!["http://example.org/bob", "http://example.org/c1"]
        );
    }

    // -----------------------------------------------------------------------
    // MINUS
    // -----------------------------------------------------------------------

    /// `test_store()`: alice -> bob -> charlie -> alice (knows); only bob has
    /// an age. So MINUS on the age pattern takes bob out of the knows result.
    #[test]
    fn minus_removes_rows_matching_the_right_pattern() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
             MINUS { ?s <http://example.org/age> ?a } }",
        );
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/alice", "http://example.org/charlie"]
        );
    }

    /// The difference between `MINUS` and `FILTER NOT EXISTS`: a right pattern
    /// sharing no variable with the left side has a disjoint domain, so it is
    /// compatible with nothing and removes nothing — even though it matches.
    #[test]
    fn minus_with_disjoint_variables_removes_nothing() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
             MINUS { ?x <http://example.org/age> ?a } }",
        );
        assert_eq!(rows.len(), 3, "disjoint MINUS is a no-op");
    }

    /// A right pattern that matches nothing leaves the left side untouched.
    #[test]
    fn minus_over_an_empty_right_side_keeps_everything() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
             MINUS { ?s <http://example.org/height> ?h } }",
        );
        assert_eq!(rows.len(), 3);
    }

    /// MINUS is a test, not a source of bindings: its variables stay out of
    /// scope, so projecting one is the usual "not in pattern" error.
    #[test]
    fn minus_right_side_variables_stay_out_of_scope() {
        let store = test_store();
        let engine = HybridEngine::new();
        let err = execute_sparql(
            &store,
            &engine,
            "SELECT ?a WHERE { ?s <http://example.org/knows> ?o \
             MINUS { ?s <http://example.org/age> ?a } }",
        )
        .unwrap_err();
        assert!(err.contains("?a"), "unexpected error: {err}");
    }

    /// With two shared variables a row only cancels when both agree: bob is
    /// removed as a subject of `knows`, but only on the (bob, charlie) edge.
    #[test]
    fn minus_matches_on_all_shared_variables() {
        let mut store = test_store();
        // Incremental, since a second `ingest_str_triples` would rebuild the
        // index from scratch and drop what `test_store` put in.
        let bob = store
            .dict
            .insert_with_type("http://example.org/bob", TermType::Iri);
        let blocked = store
            .dict
            .insert_with_type("http://example.org/blocked", TermType::Iri);
        let charlie = store
            .dict
            .insert_with_type("http://example.org/charlie", TermType::Iri);
        store.insert_triple(bob, blocked, charlie);
        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { ?s <http://example.org/knows> ?o \
             MINUS { ?s <http://example.org/blocked> ?o } }",
        );
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/alice", "http://example.org/charlie"]
        );
    }

    /// An unbound column is not in the row's domain: bob has no nickname, and a
    /// `MINUS` on `?a` still cancels him because the sides agree on `?s`.
    #[test]
    fn minus_treats_an_unbound_left_column_as_compatible() {
        let mut store = test_store();
        // alice gets a nickname, bob only an age (from `test_store`).
        let alice = store
            .dict
            .insert_with_type("http://example.org/alice", TermType::Iri);
        let nick = store
            .dict
            .insert_with_type("http://example.org/nick", TermType::Iri);
        let al = store.dict.insert_with_type("Al", TermType::literal_plain());
        store.insert_triple(alice, nick, al);

        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
             OPTIONAL { ?s <http://example.org/nick> ?a } \
             MINUS { ?s <http://example.org/age> ?a } }",
        );
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/alice", "http://example.org/charlie"],
            "bob is cancelled even though ?a is unbound on the left"
        );
    }

    /// The same on the right: an unbound `?o` contradicts nothing, so the row
    /// still cancels on the shared `?s`.
    #[test]
    fn minus_treats_an_unbound_right_column_as_compatible() {
        let mut store = test_store();
        let alice = store
            .dict
            .insert_with_type("http://example.org/alice", TermType::Iri);
        let flag = store
            .dict
            .insert_with_type("http://example.org/flag", TermType::Iri);
        store.insert_triple(alice, flag, alice);

        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { ?s <http://example.org/knows> ?o \
             MINUS { ?s <http://example.org/flag> ?f \
                     OPTIONAL { ?s <http://example.org/nothing> ?o } } }",
        );
        let mut got = values_of(&rows, "s");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/bob", "http://example.org/charlie"],
            "alice is cancelled even though ?o is unbound on the right"
        );
    }

    /// MINUS binds nothing new, so it stays on the read-locked path.
    #[test]
    fn minus_does_not_need_the_write_path() {
        assert!(!query_needs_write(
            "SELECT ?s WHERE { ?s ?p ?o MINUS { ?s <http://example.org/age> ?a } }",
            &test_store()
        ));
    }

    // -----------------------------------------------------------------------
    // VALUES
    // -----------------------------------------------------------------------

    #[test]
    fn values_restricts_a_variable_to_the_listed_terms() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?o WHERE { \
             VALUES ?s { <http://example.org/alice> <http://example.org/charlie> } \
             ?s <http://example.org/knows> ?o }",
        );
        let mut got = values_of(&rows, "o");
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["http://example.org/alice", "http://example.org/bob"]
        );
    }

    /// A multi-column table constrains the tuple, not each variable on its own:
    /// (charlie, bob) is not an edge, so only the alice row survives.
    #[test]
    fn values_multi_variable_rows_constrain_the_tuple() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { \
             VALUES (?s ?o) { (<http://example.org/alice> <http://example.org/bob>) \
                              (<http://example.org/charlie> <http://example.org/bob>) } \
             ?s <http://example.org/knows> ?o }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/alice");
        assert_eq!(rows[0]["o"]["value"], "http://example.org/bob");
    }

    /// `VALUES` trailing the `WHERE` clause is the same inline table, joined in
    /// by the parser — it must reach evaluation just like the inline form.
    #[test]
    fn values_clause_after_the_where_block() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?o WHERE { ?s <http://example.org/knows> ?o } \
             VALUES ?s { <http://example.org/alice> }",
        );
        assert_eq!(values_of(&rows, "o"), vec!["http://example.org/bob"]);
    }

    /// A typed literal in `VALUES` must match on the datatype, not just the
    /// lexical form — `"25"` alone is an xsd:string and matches nothing.
    #[test]
    fn values_literal_respects_the_datatype() {
        let mut store = test_store();
        // The store has to hold the plain literal, or the read path rejects the
        // table before it ever gets to the join.
        store.dict.insert_with_type("25", TermType::literal_plain());
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { VALUES ?a { \"25\" } ?s <http://example.org/age> ?a }",
        );
        assert_eq!(rows.len(), 0, "plain \"25\" is an xsd:string");

        // The store's ages come from `ingest_str_triples`, so give it a typed one.
        let s = store
            .dict
            .insert_with_type("http://example.org/dave", TermType::Iri);
        let p = store
            .dict
            .insert_with_type("http://example.org/age", TermType::Iri);
        let o = store
            .dict
            .insert_with_type("25", TermType::literal_datatype(XSD_INT));
        store.insert_triple(s, p, o);
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { VALUES ?a { \"25\"^^<http://www.w3.org/2001/XMLSchema#integer> } \
             ?s <http://example.org/age> ?a }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/dave");
    }

    /// A term the store has never seen has no ID, so the read path says so
    /// rather than dropping the row. The write path answers it.
    #[test]
    fn values_rejects_a_term_the_store_does_not_know_on_the_read_path() {
        let mut store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?o WHERE { \
             VALUES ?s { <http://example.org/alice> <http://example.org/nobody> } \
             ?s <http://example.org/knows> ?o }";
        let err = execute_sparql(&store, &engine, query).unwrap_err();
        assert!(err.contains("nobody"), "unexpected error: {err}");

        let rows = rows_of_mut(&mut store, query);
        assert_eq!(values_of(&rows, "o"), vec!["http://example.org/bob"]);
    }

    /// Dropping the row is harmless only for a plain join; these three shapes
    /// are the ones it answered wrongly.
    #[test]
    fn an_unknown_values_term_is_kept_where_a_join_would_not_have_dropped_it() {
        let mut store = test_store();
        let unknown = "<http://example.org/nobody>";
        for query in [
            format!(
                "SELECT ?s ?o WHERE {{ VALUES ?s {{ {unknown} }} OPTIONAL {{ ?s <http://example.org/knows> ?o }} }}"
            ),
            format!(
                "SELECT ?s WHERE {{ {{ VALUES ?s {{ {unknown} }} }} UNION {{ ?s <http://example.org/knows> ?o }} }}"
            ),
            format!("SELECT ?s WHERE {{ VALUES ?s {{ {unknown} }} FILTER(isIRI(?s)) }}"),
        ] {
            let rows = rows_of_mut(&mut store, &query);
            assert!(
                values_of(&rows, "s").contains(&"http://example.org/nobody"),
                "lost the unknown term in: {query}"
            );
        }
    }

    /// A standalone `VALUES` is the table itself, so an unknown term has to come
    /// back verbatim — which needs an ID, hence the write-locked path.
    #[test]
    fn standalone_values_returns_unknown_terms_via_the_write_path() {
        let mut store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?s WHERE { VALUES ?s { <http://example.org/nobody> } }";
        let result: Value =
            serde_json::from_str(&execute_sparql_bind(&mut store, &engine, query).unwrap())
                .unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(values_of(rows, "s"), vec!["http://example.org/nobody"]);
    }

    /// ... which is why an unknown term routes `VALUES` to the write path; a
    /// table of known terms stays on the read lock.
    #[test]
    fn query_needs_write_only_for_an_unknown_values_term() {
        let store = test_store();
        assert!(query_needs_write(
            "SELECT ?s WHERE { VALUES ?s { <http://example.org/nobody> } }",
            &store
        ));
        assert!(query_needs_write(
            "SELECT ?s WHERE { ?s ?p ?o } VALUES ?s { <http://example.org/nobody> }",
            &store
        ));
        assert!(!query_needs_write(
            "SELECT ?s WHERE { VALUES ?s { <http://example.org/alice> } }",
            &store
        ));
        assert!(!query_needs_write(
            "SELECT ?s WHERE { ?s ?p ?o } VALUES ?s { <http://example.org/alice> }",
            &store
        ));
    }

    /// An UNDEF column is a wildcard: it joins with any value, while the bound
    /// column of the same row still constrains the match.
    #[test]
    fn values_undef_joins_as_a_wildcard() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { \
             VALUES (?s ?o) { (<http://example.org/alice> UNDEF) } \
             ?s <http://example.org/knows> ?o }",
        );
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["s"]["value"], "http://example.org/alice");
        assert_eq!(rows[0]["o"]["value"], "http://example.org/bob");
    }

    /// The merged row takes each column from whichever side binds it.
    #[test]
    fn undef_on_both_sides_merges_the_bound_halves() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { \
             VALUES (?s ?o) { (<http://example.org/alice> UNDEF) } \
             VALUES (?s ?o) { (UNDEF <http://example.org/bob>) } }",
        );
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["s"]["value"], "http://example.org/alice");
        assert_eq!(rows[0]["o"]["value"], "http://example.org/bob");
    }

    /// A right row unbound on the join key stays out of the hash and still
    /// matches every left row.
    #[test]
    fn an_unbound_right_join_key_matches_every_left_row() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s ?o ?x WHERE { \
             ?s <http://example.org/knows> ?o . \
             VALUES (?o ?x) { (UNDEF <http://example.org/alice>) } }",
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(
            rows.iter()
                .all(|r| r["x"]["value"] == "http://example.org/alice")
        );
    }

    /// The wildcard is per column: a right row unbound on one join key still has
    /// to agree on the other, so this OPTIONAL pads every row instead.
    #[test]
    fn an_unbound_column_does_not_widen_the_rest_of_the_key() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s ?o WHERE { \
             ?s <http://example.org/knows> ?o . \
             OPTIONAL { VALUES (?s ?o) { (<http://example.org/age> UNDEF) } } }",
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(
            rows.iter().all(|r| r["o"].get("value").is_some()),
            "{rows:?}"
        );
    }

    /// `VALUES` inside a sub-`SELECT` is scoped to it like any other pattern.
    #[test]
    fn values_inside_a_subselect() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?o WHERE { { SELECT ?o WHERE { \
             VALUES ?s { <http://example.org/bob> } ?s <http://example.org/knows> ?o } } }",
        );
        assert_eq!(values_of(&rows, "o"), vec!["http://example.org/charlie"]);
    }

    /// Both work under `?infer=rdfs`: the rewrite recurses into either side of
    /// a `MINUS` and leaves a `VALUES` table alone, and neither needs to intern
    /// a computed value, so the read-locked inference path carries them.
    #[test]
    fn minus_and_values_under_inference() {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (
                "http://example.org/Dog",
                "http://www.w3.org/2000/01/rdf-schema#subClassOf",
                "http://example.org/Animal",
            ),
            (
                "http://example.org/fido",
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "http://example.org/Dog",
            ),
            (
                "http://example.org/rex",
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "http://example.org/Dog",
            ),
            (
                "http://example.org/rex",
                "http://example.org/tagged",
                "http://example.org/yes",
            ),
        ]);
        let engine = HybridEngine::new();
        let infer_rows = |q: &str| -> Vec<Value> {
            let v: Value =
                serde_json::from_str(&execute_sparql_infer(&store, &engine, q).unwrap()).unwrap();
            v["results"]["bindings"].as_array().unwrap().clone()
        };

        // Both dogs are inferred Animals; MINUS takes the tagged one out.
        let rows = infer_rows(
            "SELECT ?s WHERE { \
             ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Animal> \
             MINUS { ?s <http://example.org/tagged> ?t } }",
        );
        assert_eq!(values_of(&rows, "s"), vec!["http://example.org/fido"]);

        let rows = infer_rows(
            "SELECT ?s WHERE { VALUES ?s { <http://example.org/rex> } \
             ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Animal> }",
        );
        assert_eq!(values_of(&rows, "s"), vec!["http://example.org/rex"]);
    }

    // -----------------------------------------------------------------------
    // Unsupported expressions
    // -----------------------------------------------------------------------

    /// Read as an error, an unimplemented expression makes `FILTER` drop every
    /// row — indistinguishable from a real "no solutions".
    #[test]
    fn unsupported_expressions_are_refused_rather_than_answered() {
        let store = test_store();
        let engine = HybridEngine::new();
        for (query, expected) in [
            (
                "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
                 FILTER NOT EXISTS { ?s <http://example.org/age> ?a } }",
                "EXISTS",
            ),
            (
                "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
                 FILTER EXISTS { ?s <http://example.org/age> ?a } }",
                "EXISTS",
            ),
            (
                "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
                 FILTER(SUBSTR(STR(?s), 1, 4) = \"http\") }",
                "SUBSTR",
            ),
            (
                "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
                 FILTER(COALESCE(?z, \"x\") = \"x\") }",
                "COALESCE",
            ),
        ] {
            let err = execute_sparql(&store, &engine, query).unwrap_err();
            assert!(err.contains(expected), "unexpected error: {err}");
        }
    }

    /// Same for `BIND`, which shares the evaluator.
    #[test]
    fn bind_with_an_unsupported_function_is_refused() {
        let mut store = test_store();
        let engine = HybridEngine::new();
        let err = execute_sparql_bind(
            &mut store,
            &engine,
            "SELECT ?x WHERE { ?s <http://example.org/knows> ?o BIND(ABS(-3) AS ?x) }",
        )
        .unwrap_err();
        assert!(err.contains("ABS"), "unexpected error: {err}");
    }

    /// The supported ones still evaluate, i.e. the check is not over-eager.
    #[test]
    fn supported_expressions_still_pass_the_check() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/knows> ?o \
             FILTER(isIRI(?s) && CONTAINS(STR(?o), \"example\") && !(STRLEN(STR(?s)) = 0)) }",
        );
        assert_eq!(rows.len(), 3);
    }

    // -----------------------------------------------------------------------
    // WAL alignment
    // -----------------------------------------------------------------------

    /// An intern the log never saw shifts every later ID, so the replay rebuilds
    /// the wrong triples. Queries intern too, so they have to log as well.
    ///
    /// Routed through [`AppState::needs_write`]/[`AppState::write_locked`], the
    /// wiring the handlers use and where the bug sat — repeating their body here
    /// would test a copy of it instead.
    #[test]
    fn a_term_interned_by_a_query_is_logged_so_the_replay_stays_aligned() {
        let path = std::env::temp_dir().join(format!(
            "trillian_wal_align_{}_{:p}.wal",
            std::process::id(),
            &0u8
        ));
        let _ = std::fs::remove_file(&path);
        let state = AppState::with_wal(
            test_store(),
            Some(Wal::open_append(path.to_str().unwrap()).unwrap()),
        );
        let dict_len = || state.store.read().unwrap().dict.len();

        // A query that mints an ID, taking the same route the handlers take.
        let query = "SELECT ?s WHERE { VALUES ?s { <http://example.org/ghost> } }";
        assert!(
            state.needs_write(query),
            "the query should take the write lock"
        );
        let first_new_id = dict_len();
        state
            .write_locked(|store| execute_sparql_bind(store, &state.engine, query))
            .unwrap();
        assert!(dict_len() > first_new_id, "the query interned nothing");

        // Then a logged update, whose IDs follow the ones the query took.
        {
            let mut store = state.store.write().unwrap();
            let mut wal = state.wal.as_ref().unwrap().lock().unwrap();
            execute_update(
                &mut store,
                "INSERT DATA { <http://example.org/x> <http://example.org/p> <http://example.org/y> }",
                Some(&mut wal),
            )
            .unwrap();
            wal.sync().unwrap();
        }

        let mut restored = test_store();
        Wal::replay(path.to_str().unwrap(), &mut restored).unwrap();
        let rows = rows_of(
            &restored,
            "SELECT ?o WHERE { <http://example.org/x> <http://example.org/p> ?o }",
        );
        assert_eq!(values_of(&rows, "o"), vec!["http://example.org/y"]);
        let _ = std::fs::remove_file(&path);
    }

    /// A query's interned terms have to reach disk, not sit in the writer's
    /// buffer: an unsynced tail is what tears the log when the process dies,
    /// and a torn record costs every update appended after it.
    #[test]
    fn a_term_interned_by_a_query_is_synced_not_left_buffered() {
        let path = std::env::temp_dir().join(format!(
            "trillian_wal_sync_{}_{:p}.wal",
            std::process::id(),
            &0u8
        ));
        let _ = std::fs::remove_file(&path);
        let state = AppState::with_wal(
            test_store(),
            Some(Wal::open_append(path.to_str().unwrap()).unwrap()),
        );

        let query = "SELECT ?s WHERE { VALUES ?s { <http://example.org/ghost> } }";
        state
            .write_locked(|store| execute_sparql_bind(store, &state.engine, query))
            .unwrap();

        // Nothing here synced, so the term is on disk only if the query path did.
        let mut restored = test_store();
        Wal::replay(path.to_str().unwrap(), &mut restored).unwrap();
        assert!(
            restored
                .dict
                .lookup_iri("http://example.org/ghost")
                .is_some(),
            "the query's term never reached disk"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The two combined: restrict with `VALUES`, then subtract with `MINUS`.
    #[test]
    fn values_and_minus_combined() {
        let store = test_store();
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { \
             VALUES ?s { <http://example.org/alice> <http://example.org/bob> } \
             ?s <http://example.org/knows> ?o \
             MINUS { ?s <http://example.org/age> ?a } }",
        );
        assert_eq!(values_of(&rows, "s"), vec!["http://example.org/alice"]);
    }
}
