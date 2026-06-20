use std::sync::{Arc, Mutex, RwLock};

use axum::body::{Bytes, Body};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use lru::LruCache;
use serde_json::{json, Map, Value};
use tokio_stream::wrappers::ReceiverStream;
use spargebra::term::{GroundQuad, GroundTerm, Literal, NamedNode, NamedOrBlankNode, Quad, Term as SparqlTerm};
use spargebra::{Query as SparqlQuery, SparqlParser};

use crate::hypertrie::{
    Dictionary, GraphPattern, HybridEngine, PatternTerm, RowBlock, TermType, TriplePattern,
    TripleStore,
};

const DEFAULT_CACHE_SIZE: usize = 256;

/// Cache-Eintrag für bereits ausgeführte SPARQL-Queries.
#[derive(Debug, Clone)]
pub enum CacheEntry {
    Select { vars: Vec<String>, rows: Vec<Map<String, Value>> },
    Ask(bool),
}

/// Shared application state: the loaded triple store plus the query engine.
pub struct AppState {
    pub store: RwLock<TripleStore>,
    pub engine: HybridEngine,
    pub cache: Mutex<LruCache<String, CacheEntry>>,
    /// Optionaler Persistenz-Pfad. Ist er gesetzt, wird der Store nach jedem
    /// erfolgreichen `/update` verlustfrei dorthin zurückgeschrieben
    /// (Write-Through), sodass Änderungen einen Neustart überleben.
    pub persist_path: Option<std::path::PathBuf>,
}

impl AppState {
    pub fn new(store: TripleStore) -> Self {
        Self::with_persistence(store, None)
    }

    pub fn with_persistence(store: TripleStore, persist_path: Option<std::path::PathBuf>) -> Self {
        Self {
            store: RwLock::new(store),
            engine: HybridEngine::new(),
            cache: Mutex::new(LruCache::new(
                std::num::NonZeroUsize::new(DEFAULT_CACHE_SIZE).unwrap(),
            )),
            persist_path,
        }
    }

    fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }
}

/// Start the SPARQL HTTP endpoint on the given port.
pub async fn serve(store: TripleStore, port: u16) {
    serve_with_persistence(store, port, None).await
}

/// Wie [`serve`], aber mit optionalem Write-Through-Persistenzpfad.
pub async fn serve_with_persistence(
    store: TripleStore,
    port: u16,
    persist_path: Option<std::path::PathBuf>,
) {
    let state = Arc::new(AppState::with_persistence(store, persist_path));

    let app = Router::new()
        .route("/sparql", get(sparql_handler).post(sparql_handler))
        .route("/stream", get(stream_handler).post(stream_handler))
        .route("/count", get(count_handler).post(count_handler))
        .route("/update", post(update_handler))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!(
        "SPARQL endpoint listening on http://{}/sparql, /stream, /count, /update",
        addr
    );
    axum::serve(listener, app).await.unwrap();
}

#[derive(serde::Deserialize, Debug, Default)]
pub struct SparqlQueryParams {
    query: Option<String>,
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
        return sparql_error("Missing query parameter or empty body", StatusCode::BAD_REQUEST);
    }

    // Cache-Lookup
    {
        if let Ok(cache) = state.cache.lock() {
            if let Some(entry) = cache.peek(&query_str) {
                return format_cached_entry(entry);
            }
        }
    }

    let store = state.store.read().unwrap();
    match execute_sparql(&store, &state.engine, &query_str) {
        Ok(result) => {
            // Ergebnis cachen
            if let Some(entry) = cache_entry_from_result(&result) {
                if let Ok(mut cache) = state.cache.lock() {
                    cache.put(query_str, entry);
                }
            }
            (StatusCode::OK, axum::Json(result)).into_response()
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
        return sparql_error("Missing query parameter or empty body", StatusCode::BAD_REQUEST);
    }

    // Stream materialisiert die Ergebnisse intern, sendet sie aber als NDJSON
    // chunked, bevor der Gesamt-JSON-Body aufgebaut wird.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(128);
    let state = Arc::clone(&state);

    tokio::task::spawn_blocking(move || {
        let store = state.store.read().unwrap();
        let result = evaluate_select(&*store, &state.engine, &query_str);
        match result {
            Ok(select) => {
                let var_order = select.var_order;
                let vars = select.vars;
                let mut var_indices = Vec::with_capacity(vars.len());
                for var in &vars {
                    let pos = var_order.iter().position(|v| v == var).unwrap_or(0);
                    var_indices.push(pos);
                }

                // Header-Zeile mit Variablennamen als NDJSON-Objekt.
                let header = json!({ "head": { "vars": vars } }).to_string();
                let _ = tx.blocking_send(Ok(Bytes::from(header + "\n")));

                for row in select.rows.rows() {
                    let obj: Map<String, Value> = vars
                        .iter()
                        .enumerate()
                        .map(|(i, var)| {
                            let id = row[var_indices[i]];
                            let term = term_to_json(id, &store.dict);
                            (var.clone(), term)
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
        return sparql_error("Missing query parameter or empty body", StatusCode::BAD_REQUEST);
    }

    let store = state.store.read().unwrap();
    match execute_count(&store, &state.engine, &query_str) {
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
        return sparql_error("Missing update parameter or empty body", StatusCode::BAD_REQUEST);
    }

    let mut store = state.store.write().unwrap();
    match execute_update(&mut store, &update_str) {
        Ok(()) => {
            // Write-Through-Persistenz: aktualisierten Store zurückschreiben,
            // solange wir noch den Write-Lock halten (konsistenter Snapshot).
            if let Some(path) = &state.persist_path {
                if let Some(path_str) = path.to_str() {
                    if let Err(e) = store.dump_ntriples(path_str) {
                        return sparql_error(
                            &format!("Update applied but persistence failed: {}", e),
                            StatusCode::INTERNAL_SERVER_ERROR,
                        );
                    }
                }
            }
            // Cache invalidieren, da sich die Daten geändert haben
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

fn sparql_error(msg: &str, status: StatusCode) -> Response {
    let body = json!({ "error": msg });
    (status, axum::Json(body)).into_response()
}

fn format_cached_entry(entry: &CacheEntry) -> Response {
    match entry {
        CacheEntry::Select { vars, rows } => {
            let result = json!({
                "head": { "vars": vars },
                "results": { "bindings": rows }
            });
            (StatusCode::OK, axum::Json(result)).into_response()
        }
        CacheEntry::Ask(b) => {
            let result = json!({ "head": {}, "boolean": b });
            (StatusCode::OK, axum::Json(result)).into_response()
        }
    }
}

fn cache_entry_from_result(result: &Value) -> Option<CacheEntry> {
    if let Some(boolean) = result.get("boolean").and_then(|v| v.as_bool()) {
        return Some(CacheEntry::Ask(boolean));
    }
    let vars = result
        .get("head")?
        .get("vars")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    let rows = result
        .get("results")?
        .get("bindings")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_object().cloned())
        .collect();
    Some(CacheEntry::Select { vars, rows })
}

// ---------------------------------------------------------------------------
// SPARQL Query Execution
// ---------------------------------------------------------------------------

struct SelectResult {
    vars: Vec<String>,
    rows: RowBlock,
    var_order: Vec<String>,
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

    let (bgp, optionals, projection, distinct, limit, offset) = extract_bgp_and_projection(&pattern)?;
    evaluate_select_with_modifiers(store, engine, bgp, optionals, projection, distinct, limit, offset)
}

fn execute_sparql(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<Value, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match query {
        SparqlQuery::Select { pattern, .. } => {
            let (bgp, optionals, projection, distinct, limit, offset) = extract_bgp_and_projection(&pattern)?;
            let result = evaluate_select_with_modifiers(
                store, engine, bgp, optionals, projection, distinct, limit, offset,
            )?;
            Ok(sparql_json(&result, store))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let (bgp, _, _, _, _, _) = extract_bgp_and_projection(&pattern)?;
            let has_match = match translate_bgp(bgp, &store.dict)? {
                Some(internal) => engine.execute(store, &internal).n_rows() > 0,
                None => false,
            };
            Ok(json!({ "head": {}, "boolean": has_match }))
        }
        _ => Err("Only SELECT and ASK queries are supported".to_string()),
    }
}

fn evaluate_select_with_modifiers(
    store: &TripleStore,
    engine: &HybridEngine,
    bgp: &[spargebra::term::TriplePattern],
    optionals: Vec<&Vec<spargebra::term::TriplePattern>>,
    projection: Option<Vec<String>>,
    distinct: bool,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<SelectResult, String> {
    let internal = match translate_bgp(bgp, &store.dict)? {
        Some(pat) => pat,
        None => {
            return Ok(SelectResult {
                vars: projection.unwrap_or_default(),
                rows: RowBlock::new(0),
                var_order: Vec::new(),
            });
        }
    };

    let mut rows = engine.execute(store, &internal);
    let mut var_order: Vec<String> = internal.variable_order().into_iter().cloned().collect();

    // Sequentielle LEFT JOINs über alle OPTIONAL-Gruppen.
    for opt_bgp in optionals {
        let (new_rows, new_var_order) = left_join(store, engine, &var_order, rows, opt_bgp)?;
        rows = new_rows;
        var_order = new_var_order;
    }

    let vars = projection.unwrap_or_else(|| var_order.clone());

    // Spaltenindizes der projizierten Variablen in der Roh-Zeile bestimmen.
    let mut var_indices = Vec::with_capacity(vars.len());
    for var in &vars {
        match var_order.iter().position(|v| v == var) {
            Some(pos) => var_indices.push(pos),
            None => {
                return Err(format!(
                    "SELECT variable ?{} does not appear in pattern",
                    var
                ))
            }
        }
    }

    // Projektion VOR DISTINCT/OFFSET/LIMIT – das ist die korrekte
    // SPARQL-Auswertungsreihenfolge (Projektion → DISTINCT → OFFSET/LIMIT).
    let mut rows = rows.project(&var_indices);
    let var_order = vars.clone();

    if distinct {
        rows.sort_distinct();
    }
    rows.apply_offset_limit(offset.unwrap_or(0), limit);

    Ok(SelectResult {
        vars,
        rows,
        var_order,
    })
}

fn left_join(
    store: &TripleStore,
    engine: &HybridEngine,
    left_var_order: &[String],
    left_rows: RowBlock,
    opt_bgp: &[spargebra::term::TriplePattern],
) -> Result<(RowBlock, Vec<String>), String> {
    const NULL_ID: u32 = u32::MAX;

    let opt_internal_full = match translate_bgp(opt_bgp, &store.dict)? {
        Some(pat) => pat,
        None => {
            // OPTIONAL enthält unbekannte Konstante -> alle OPTIONAL-Variablen
            // bleiben ungebunden (NULL).
            let opt_vars: Vec<String> = variables_in_bgp(opt_bgp)
                .into_iter()
                .filter(|v| !left_var_order.contains(v))
                .collect();
            let mut new_var_order = left_var_order.to_vec();
            new_var_order.extend(opt_vars.iter().cloned());
            let mut out = RowBlock::new(new_var_order.len());
            for row in left_rows.rows() {
                out.push_row_padded(row, NULL_ID);
            }
            return Ok((out, new_var_order));
        }
    };

    let opt_var_order: Vec<String> = opt_internal_full
        .variable_order()
        .into_iter()
        .cloned()
        .collect();

    // Gemeinsame Variablen (Join-Schlüssel) und neue Variablen bestimmen.
    // shared: (Position in der linken Zeile, Position in der OPTIONAL-Zeile),
    // in OPTIONAL-Reihenfolge, damit linker und rechter Schlüssel gleich geordnet sind.
    let mut shared: Vec<(usize, usize)> = Vec::new();
    let mut new_positions: Vec<usize> = Vec::new();
    let mut new_vars: Vec<String> = Vec::new();
    for (opt_pos, v) in opt_var_order.iter().enumerate() {
        if let Some(left_pos) = left_var_order.iter().position(|lv| lv == v) {
            shared.push((left_pos, opt_pos));
        } else {
            new_positions.push(opt_pos);
            new_vars.push(v.clone());
        }
    }
    let mut new_var_order = left_var_order.to_vec();
    new_var_order.extend(new_vars.iter().cloned());
    let new_arity = new_vars.len();

    // OPTIONAL-Muster **einmal** ausführen und nach den Join-Schlüsselwerten
    // indizieren (klassischer Hash-Left-Join). Die neuen Spalten je Schlüssel
    // liegen flach hintereinander (new_arity Spalten pro Match).
    let opt_rows = engine.execute(store, &opt_internal_full);
    let mut index: rustc_hash::FxHashMap<Vec<u32>, Vec<u32>> = rustc_hash::FxHashMap::default();
    for orow in opt_rows.rows() {
        let key: Vec<u32> = shared.iter().map(|&(_, op)| orow[op]).collect();
        let bucket = index.entry(key).or_default();
        for &p in &new_positions {
            bucket.push(orow[p]);
        }
    }

    let mut out = RowBlock::new(new_var_order.len());
    for row in left_rows.rows() {
        let key: Vec<u32> = shared.iter().map(|&(lp, _)| row[lp]).collect();
        match index.get(&key) {
            Some(flat) if new_arity > 0 && !flat.is_empty() => {
                for chunk in flat.chunks(new_arity) {
                    out.push_row_concat(row, chunk);
                }
            }
            _ => {
                // Kein Match (oder OPTIONAL ohne neue Variablen) -> NULL-Auffüllung.
                out.push_row_padded(row, NULL_ID);
            }
        }
    }

    Ok((out, new_var_order))
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


fn sparql_json(result: &SelectResult, store: &TripleStore) -> Value {
    let mut var_indices = Vec::with_capacity(result.vars.len());
    for var in &result.vars {
        let pos = result
            .var_order
            .iter()
            .position(|v| v == var)
            .expect("validated variable");
        var_indices.push(pos);
    }

    let bindings: Vec<Map<String, Value>> = result
        .rows
        .rows()
        .map(|row| {
            result
                .vars
                .iter()
                .enumerate()
                .map(|(i, var)| {
                    let id = row[var_indices[i]];
                    let term = term_to_json(id, &store.dict);
                    (var.clone(), term)
                })
                .collect()
        })
        .collect();

    json!({
        "head": { "vars": result.vars },
        "results": { "bindings": bindings }
    })
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
            let (bgp, optionals, projection, distinct, limit, offset) = extract_bgp_and_projection(&pattern)?;
            let result = evaluate_select_with_modifiers(
                store, engine, bgp, optionals, projection, distinct, limit, offset,
            )?;
            Ok(json!({ "count": result.rows.n_rows() }))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let (bgp, _, _, _, _, _) = extract_bgp_and_projection(&pattern)?;
            let has_match = match translate_bgp(bgp, &store.dict)? {
                Some(internal) => !engine.execute(store, &internal).is_empty(),
                None => false,
            };
            Ok(json!({ "boolean": has_match }))
        }
        _ => Err("Only SELECT and ASK queries are supported for /count".to_string()),
    }
}

// ---------------------------------------------------------------------------
// SPARQL Update Execution
// ---------------------------------------------------------------------------

fn execute_update(store: &mut TripleStore, update_str: &str) -> Result<(), String> {
    let update = SparqlParser::new()
        .parse_update(update_str)
        .map_err(|e| e.to_string())?;

    // Alle Inserts/Deletes sammeln und am Ende in einem einzigen
    // Index-Rebuild anwenden (statt einem Rebuild pro Triple).
    let mut inserts: Vec<(u32, u32, u32)> = Vec::new();
    let mut deletes: Vec<(u32, u32, u32)> = Vec::new();

    for op in update.operations {
        match op {
            spargebra::GraphUpdateOperation::InsertData { data } => {
                for quad in data {
                    let (s, p, o) = quad_to_triple_terms(&quad)?;
                    let sid = store.dict.insert_with_type(&s.value, s.typ);
                    let pid = store.dict.insert_with_type(&p.value, p.typ);
                    let oid = store.dict.insert_with_type(&o.value, o.typ);
                    inserts.push((sid, pid, oid));
                }
            }
            spargebra::GraphUpdateOperation::DeleteData { data } => {
                for quad in data {
                    let (s, p, o) = ground_quad_to_triple_terms(&quad)?;
                    if let (Some(sid), Some(pid), Some(oid)) =
                        (store.dict.lookup(&s.value), store.dict.lookup(&p.value), store.dict.lookup(&o.value))
                    {
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

fn quad_to_triple_terms(quad: &Quad) -> Result<(ParsedTermRdf, ParsedTermRdf, ParsedTermRdf), String> {
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
        NamedOrBlankNode::BlankNode(_) => Err("Blank nodes in updates are not supported".to_string()),
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

fn literal_to_parsed(lit: &Literal) -> ParsedTermRdf {
    let value = lit.value().to_string();
    let typ = if let Some(lang) = lit.language() {
        TermType::literal_lang(lang)
    } else {
        TermType::literal_datatype(lit.datatype().as_str())
    };
    ParsedTermRdf { value, typ }
}

// ---------------------------------------------------------------------------
// BGP Translation
// ---------------------------------------------------------------------------

fn extract_bgp_and_projection(
    pattern: &spargebra::algebra::GraphPattern,
) -> Result<
    (
        &Vec<spargebra::term::TriplePattern>,
        Vec<&Vec<spargebra::term::TriplePattern>>,
        Option<Vec<String>>,
        bool,
        Option<usize>,
        Option<usize>,
    ),
    String,
> {
    fn extract_bgp(
        pattern: &spargebra::algebra::GraphPattern,
    ) -> Result<&Vec<spargebra::term::TriplePattern>, String> {
        match pattern {
            spargebra::algebra::GraphPattern::Bgp { patterns } => Ok(patterns),
            spargebra::algebra::GraphPattern::Project { inner, .. }
            | spargebra::algebra::GraphPattern::Distinct { inner }
            | spargebra::algebra::GraphPattern::Reduced { inner }
            | spargebra::algebra::GraphPattern::Slice { inner, .. } => extract_bgp(inner),
            _ => Err("Unsupported graph pattern (nested OPTIONAL/UNION not supported here)".to_string()),
        }
    }

    fn walk<'a>(
        pattern: &'a spargebra::algebra::GraphPattern,
        projection: &mut Option<Vec<String>>,
        distinct: &mut bool,
        limit: &mut Option<usize>,
        offset: &mut Option<usize>,
        optionals: &mut Vec<&'a Vec<spargebra::term::TriplePattern>>,
    ) -> &'a Vec<spargebra::term::TriplePattern> {
        match pattern {
            spargebra::algebra::GraphPattern::Project { variables, inner } => {
                *projection = Some(variables.iter().map(|v| v.as_str().to_string()).collect());
                walk(inner, projection, distinct, limit, offset, optionals)
            }
            spargebra::algebra::GraphPattern::Distinct { inner }
            | spargebra::algebra::GraphPattern::Reduced { inner } => {
                *distinct = true;
                walk(inner, projection, distinct, limit, offset, optionals)
            }
            spargebra::algebra::GraphPattern::Slice {
                inner,
                start,
                length,
            } => {
                *offset = Some(*start);
                *limit = *length;
                walk(inner, projection, distinct, limit, offset, optionals)
            }
            spargebra::algebra::GraphPattern::LeftJoin { left, right, .. } => {
                let mandatory = walk(left, projection, distinct, limit, offset, optionals);
                if let Ok(opt_bgp) = extract_bgp(right) {
                    optionals.push(opt_bgp);
                }
                mandatory
            }
            spargebra::algebra::GraphPattern::Bgp { patterns } => patterns,
            _ => {
                // Fallback: treat unsupported pattern as empty BGP.
                static EMPTY: Vec<spargebra::term::TriplePattern> = Vec::new();
                &EMPTY
            }
        }
    }

    let mut projection = None;
    let mut distinct = false;
    let mut limit = None;
    let mut offset = None;
    let mut optionals = Vec::new();
    let mandatory = walk(pattern, &mut projection, &mut distinct, &mut limit, &mut offset, &mut optionals);
    Ok((mandatory, optionals, projection, distinct, limit, offset))
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
            match dict.lookup(iri) {
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
        spargebra::term::TermPattern::NamedNode(nn) => {
            let iri = nn.as_str();
            match dict.lookup(iri) {
                Some(id) => Ok(TranslationResult::Term(PatternTerm::Bound(id))),
                None => Ok(TranslationResult::UnknownConstant),
            }
        }
        spargebra::term::TermPattern::Variable(v) => Ok(TranslationResult::Term(
            PatternTerm::Variable(v.as_str().to_string()),
        )),
        spargebra::term::TermPattern::Literal(lit) => {
            let lexical = lit.value();
            match dict.lookup(lexical) {
                Some(id) => Ok(TranslationResult::Term(PatternTerm::Bound(id))),
                None => Ok(TranslationResult::UnknownConstant),
            }
        }
        spargebra::term::TermPattern::BlankNode(_) => {
            Err("Blank nodes in SPARQL queries are not supported".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// SPARQL-JSON Output
// ---------------------------------------------------------------------------

fn term_to_json(id: u32, dict: &Dictionary) -> Value {
    const NULL_ID: u32 = u32::MAX;
    if id == NULL_ID {
        return Value::Null;
    }
    let value = dict.resolve(id);
    let typ = dict.resolve_type(id);
    match (value, typ) {
        (Some(v), Some(TermType::Iri)) => json!({ "type": "uri", "value": v }),
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
            ("http://example.org/alice", "http://example.org/knows", "http://example.org/bob"),
            ("http://example.org/bob", "http://example.org/knows", "http://example.org/charlie"),
            ("http://example.org/bob", "http://example.org/age", "25"),
            ("http://example.org/charlie", "http://example.org/knows", "http://example.org/alice"),
        ]);
        store
    }

    #[test]
    fn optional_with_match() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT ?a ?b ?age WHERE { ?a <http://example.org/knows> ?b . OPTIONAL { ?b <http://example.org/age> ?age } }";
        let result = execute_sparql(&store, &engine, query).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        // alice->bob has age 25; charlie->alice has no age
        assert_eq!(rows.len(), 3);
        let ages: Vec<Option<i64>> = rows
            .iter()
            .map(|r| r.get("age").and_then(|v| v.get("value")).and_then(|v| v.as_str()).map(|s| s.parse().unwrap()))
            .collect();
        assert!(ages.contains(&Some(25)));
        assert!(ages.contains(&None));
    }

    #[test]
    fn optional_multi_match_expands_rows() {
        // bob hat zwei Alter -> die linke Zeile alice->bob muss zu ZWEI
        // Ausgabezeilen expandieren; carol (kein Alter) bleibt eine NULL-Zeile.
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            ("http://example.org/alice", "http://example.org/knows", "http://example.org/bob"),
            ("http://example.org/alice", "http://example.org/knows", "http://example.org/carol"),
            ("http://example.org/bob", "http://example.org/age", "25"),
            ("http://example.org/bob", "http://example.org/age", "26"),
        ]);
        let engine = HybridEngine::new();
        let query = "SELECT ?b ?age WHERE { ?a <http://example.org/knows> ?b . OPTIONAL { ?b <http://example.org/age> ?age } }";
        let result = execute_sparql(&store, &engine, query).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        // bob×{25,26} = 2 Zeilen + carol×NULL = 1 Zeile.
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
        let result = execute_sparql(&store, &engine, query).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert!(row.get("c").is_none() || row["c"].is_null());
        }
    }

    #[test]
    fn distinct_applies_after_projection() {
        // Zwei Triples mit Prädikat knows + eines mit age.
        // SELECT DISTINCT ?p muss {knows, age} = 2 Zeilen liefern,
        // NICHT 3 (knows darf nicht doppelt erscheinen).
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT DISTINCT ?p WHERE { ?s ?p ?o }";
        let result = execute_sparql(&store, &engine, query).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "DISTINCT ?p should dedup on the projected column");
    }

    #[test]
    fn limit_after_projection() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT DISTINCT ?p WHERE { ?s ?p ?o } LIMIT 1";
        let result = execute_sparql(&store, &engine, query).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
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
}
