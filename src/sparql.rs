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
use spargebra::algebra::{Expression, Function};
use spargebra::term::{GroundQuad, GroundTerm, Literal, NamedNode, NamedOrBlankNode, Quad, Term as SparqlTerm};
use spargebra::{Query as SparqlQuery, SparqlParser};

use crate::hypertrie::{
    Dictionary, GraphPattern, HybridEngine, PatternTerm, RowBlock, TermType, TriplePattern,
    TripleStore,
};
use crate::wal::Wal;

const DEFAULT_CACHE_SIZE: usize = 256;

/// Shared application state: the loaded triple store plus the query engine.
pub struct AppState {
    pub store: RwLock<TripleStore>,
    pub engine: HybridEngine,
    /// Query-String -> fertig serialisierter JSON-Antwort-Body.
    pub cache: Mutex<LruCache<String, String>>,
    /// Optionales Write-Ahead-Log. Ist es gesetzt, werden Updates durabel
    /// (append + fsync) protokolliert, sodass sie einen Neustart überleben.
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
}

/// Start the SPARQL HTTP endpoint on the given port (ohne Durabilität).
pub async fn serve(store: TripleStore, port: u16) {
    serve_durable(store, port, None).await
}

/// Wie [`serve`], aber mit optionalem Write-Ahead-Log für durable Updates.
pub async fn serve_durable(store: TripleStore, port: u16, wal: Option<crate::wal::Wal>) {
    let state = Arc::new(AppState::with_wal(store, wal));

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

    // Cache-Lookup (fertiger JSON-Body).
    {
        if let Ok(cache) = state.cache.lock() {
            if let Some(body) = cache.peek(&query_str) {
                return json_response(body.clone());
            }
        }
    }

    let store = state.store.read().unwrap();
    match execute_sparql(&store, &state.engine, &query_str) {
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
    // WAL für die Dauer des Updates sperren (durabel protokollieren).
    let mut wal_guard = state.wal.as_ref().map(|m| m.lock().unwrap());
    match execute_update(&mut store, &update_str, wal_guard.as_deref_mut()) {
        Ok(()) => {
            // Write-Ahead-Log auf Platte zwingen, BEVOR wir Erfolg melden.
            if let Some(w) = wal_guard.as_deref_mut() {
                if let Err(e) = w.sync() {
                    return sparql_error(
                        &format!("Update applied but WAL sync failed: {}", e),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
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

fn sparql_error(msg: &str, status: StatusCode) -> Response {
    let body = json!({ "error": msg });
    (status, axum::Json(body)).into_response()
}

/// Baut die HTTP-Antwort aus einem bereits serialisierten JSON-Body.
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

/// Profiling: führt eine SELECT-Query `runs`-mal aus und gibt die Median-Zeiten
/// der Phasen Parse / Eval (Plan+Join) / Serialize (SPARQL-JSON) aus. Trennt so,
/// wo die Zeit großer Queries hingeht.
pub fn profile_query(store: &TripleStore, engine: &HybridEngine, query_str: &str, runs: usize) {
    use std::time::Instant;
    let (mut parse, mut eval, mut ser) = (Vec::new(), Vec::new(), Vec::new());
    let mut rows = 0usize;
    for _ in 0..runs {
        let t = Instant::now();
        let query = SparqlParser::new().parse_query(query_str).expect("parse");
        parse.push(t.elapsed().as_secs_f64() * 1000.0);
        let SparqlQuery::Select { pattern, .. } = query else {
            eprintln!("profile_query: nur SELECT");
            return;
        };
        let (bgp, opt, proj, dist, lim, off, filt) = extract_bgp_and_projection(&pattern).unwrap();
        let t = Instant::now();
        let result =
            evaluate_select_with_modifiers(store, engine, bgp, opt, proj, dist, lim, off, filt).unwrap();
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
    println!("=== Profil ({} runs, {} rows) ===", runs, rows);
    println!("  parse:     {:.3} ms", p);
    println!("  eval:      {:.3} ms  (Plan + Join + Materialisierung)", e);
    println!("  serialize: {:.3} ms  (SPARQL-JSON)", s);
    println!("  gesamt:    {:.3} ms", p + e + s);
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

    let (bgp, optionals, projection, distinct, limit, offset, filters) = extract_bgp_and_projection(&pattern)?;
    evaluate_select_with_modifiers(
        store, engine, bgp, optionals, projection, distinct, limit, offset, filters,
    )
}

fn execute_sparql(
    store: &TripleStore,
    engine: &HybridEngine,
    query_str: &str,
) -> Result<String, String> {
    let query = SparqlParser::new()
        .parse_query(query_str)
        .map_err(|e| e.to_string())?;

    match query {
        SparqlQuery::Select { pattern, .. } => {
            let (bgp, optionals, projection, distinct, limit, offset, filters) =
                extract_bgp_and_projection(&pattern)?;
            let result = evaluate_select_with_modifiers(
                store, engine, bgp, optionals, projection, distinct, limit, offset, filters,
            )?;
            Ok(write_sparql_json(&result, store))
        }
        SparqlQuery::Ask { pattern, .. } => {
            // ASK über den vollen WHERE-Pfad (inkl. OPTIONAL/FILTER) -> ≥1 Lösung?
            let (bgp, optionals, _, _, _, _, filters) = extract_bgp_and_projection(&pattern)?;
            let result = evaluate_select_with_modifiers(
                store, engine, bgp, optionals, None, false, None, None, filters,
            )?;
            Ok(format!("{{\"head\":{{}},\"boolean\":{}}}", result.rows.n_rows() > 0))
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
    filters: Vec<&Expression>,
) -> Result<SelectResult, String> {
    let internal = match translate_bgp(bgp, &store.dict)? {
        Some(pat) => pat,
        None => {
            // Unbekannte Konstante im BGP -> leere Lösung. var_order = vars,
            // damit die Projektion (sparql_json) konsistent auflösen kann.
            let vars = projection.unwrap_or_default();
            return Ok(SelectResult {
                rows: RowBlock::new(vars.len()),
                var_order: vars.clone(),
                vars,
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

    // FILTER anwenden (auf die vollen Bindings, vor der Projektion).
    if !filters.is_empty() {
        let mut kept = RowBlock::new(rows.n_vars());
        for row in rows.rows() {
            if row_passes(&filters, row, &var_order, store) {
                kept.push_row(row);
            }
        }
        rows = kept;
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


/// Hängt einen JSON-String-Literal (mit Escaping) an den Puffer an.
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

/// Hängt das SPARQL-JSON-Term-Objekt für eine ID an (uri/literal+datatype/lang).
fn append_term(out: &mut String, id: u32, dict: &Dictionary) {
    match (dict.resolve(id), dict.resolve_type(id)) {
        (Some(v), Some(TermType::Iri)) => {
            out.push_str("{\"type\":\"uri\",\"value\":");
            append_json_str(out, v);
            out.push('}');
        }
        (Some(v), Some(TermType::Literal { datatype, lang })) => {
            out.push_str("{\"type\":\"literal\",\"value\":");
            append_json_str(out, v);
            if let Some(dt) = datatype {
                out.push_str(",\"datatype\":");
                append_json_str(out, dt);
            }
            if let Some(l) = lang {
                out.push_str(",\"xml:lang\":");
                append_json_str(out, l);
            }
            out.push('}');
        }
        (Some(v), _) => {
            out.push_str("{\"type\":\"literal\",\"value\":");
            append_json_str(out, v);
            out.push('}');
        }
        (None, _) => {
            out.push_str("{\"type\":\"literal\",\"value\":\"__id_");
            out.push_str(&id.to_string());
            out.push_str("\"}");
        }
    }
}

/// Serialisiert das Ergebnis **direkt als JSON-String** – ohne pro Zeile eine
/// `serde_json::Map`/`Value` zu allokieren (das war ~95 % der Zeit großer Queries).
fn write_sparql_json(result: &SelectResult, store: &TripleStore) -> String {
    const NULL_ID: u32 = u32::MAX;
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
                continue; // ungebundene (OPTIONAL-)Variable: weglassen
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
            let (bgp, optionals, projection, distinct, limit, offset, filters) =
                extract_bgp_and_projection(&pattern)?;
            let result = evaluate_select_with_modifiers(
                store, engine, bgp, optionals, projection, distinct, limit, offset, filters,
            )?;
            Ok(json!({ "count": result.rows.n_rows() }))
        }
        SparqlQuery::Ask { pattern, .. } => {
            let (bgp, optionals, _, _, _, _, filters) = extract_bgp_and_projection(&pattern)?;
            let result = evaluate_select_with_modifiers(
                store, engine, bgp, optionals, None, false, None, None, filters,
            )?;
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

    // Alle Inserts/Deletes sammeln und am Ende in einem einzigen
    // Index-Rebuild anwenden; parallel ins WAL protokollieren.
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
                    if let (Some(sid), Some(pid), Some(oid)) =
                        (store.dict.lookup(&s.value), store.dict.lookup(&p.value), store.dict.lookup(&o.value))
                    {
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

/// Fügt einen Term ins Dictionary ein und protokolliert ihn im WAL, falls er
/// neu war (damit der Replay dieselben IDs vergibt).
fn insert_term_logged(
    store: &mut TripleStore,
    t: &ParsedTermRdf,
    wal: Option<&mut Wal>,
) -> Result<u32, String> {
    let before = store.dict.len();
    let id = store.dict.insert_with_type(&t.value, t.typ.clone());
    if store.dict.len() > before {
        if let Some(w) = wal {
            w.log_term(&t.value, &t.typ).map_err(|e| e.to_string())?;
        }
    }
    Ok(id)
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

// ---------------------------------------------------------------------------
// FILTER: SPARQL-Ausdrucks-Evaluator
// ---------------------------------------------------------------------------

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// Laufzeit-Wert eines FILTER-Ausdrucks.
#[derive(Debug, Clone)]
enum Fv {
    Iri(String),
    Str(String),          // einfaches Literal / xsd:string
    Num(f64),             // numerischer Datentyp
    Bool(bool),
    Lang(String, String), // (Lexikal, Sprach-Tag)
    Typed(String, String),// (Lexikal, Datatype-IRI) – nicht numerisch/string
}

fn is_numeric_dt(dt: &str) -> bool {
    matches!(
        dt.strip_prefix(XSD),
        Some(
            "integer" | "decimal" | "double" | "float" | "int" | "long" | "short"
                | "byte" | "nonNegativeInteger" | "positiveInteger" | "nonPositiveInteger"
                | "negativeInteger" | "unsignedInt" | "unsignedLong" | "unsignedShort"
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
        Some(dt) if dt == format!("{XSD}string") => Fv::Str(lex.to_string()),
        Some(dt) if dt == format!("{XSD}boolean") => Fv::Bool(lex == "true" || lex == "1"),
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
        TermType::Iri | TermType::BlankNode => Some(Fv::Iri(v.to_string())),
        TermType::Literal { datatype, lang } => Some(classify(v, datatype.as_deref(), lang.as_deref())),
    }
}

fn literal_to_fv(lit: &Literal) -> Fv {
    classify(lit.value(), Some(lit.datatype().as_str()), lit.language())
}

/// Numerischer Wert, falls der Ausdruck einen liefert.
fn as_num(fv: &Fv) -> Option<f64> {
    match fv {
        Fv::Num(n) => Some(*n),
        Fv::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Lexikalischer Vergleichs-String (für =/< auf Strings).
fn as_str<'a>(fv: &'a Fv) -> Option<&'a str> {
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
        (Fv::Bool(x), Fv::Bool(y)) => Some(x == y),
        (Fv::Str(x), Fv::Str(y)) => Some(x == y),
        (Fv::Lang(x, lx), Fv::Lang(y, ly)) => Some(x == y && lx == ly),
        (Fv::Typed(x, dx), Fv::Typed(y, dy)) => Some(x == y && dx == dy),
        _ => None, // unvergleichbar -> Fehler
    }
}

fn fv_cmp(a: &Fv, b: &Fv) -> Option<std::cmp::Ordering> {
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return x.partial_cmp(&y);
    }
    match (a, b) {
        (Fv::Str(x), Fv::Str(y)) => Some(x.cmp(y)),
        (Fv::Lang(x, _), Fv::Lang(y, _)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn eval(expr: &Expression, row: &[u32], vars: &[String], store: &TripleStore) -> Result<Fv, ()> {
    const NULL_ID: u32 = u32::MAX;
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
            let ea = ebv(a, row, vars, store);
            let eb = ebv(b, row, vars, store);
            if ea == Ok(true) || eb == Ok(true) {
                Ok(Fv::Bool(true))
            } else if ea.is_err() || eb.is_err() {
                Err(())
            } else {
                Ok(Fv::Bool(false))
            }
        }
        Expression::And(a, b) => {
            let ea = ebv(a, row, vars, store);
            let eb = ebv(b, row, vars, store);
            if ea == Ok(false) || eb == Ok(false) {
                Ok(Fv::Bool(false))
            } else if ea.is_err() || eb.is_err() {
                Err(())
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
            let (x, y) = (eval(a, row, vars, store)?, eval(b, row, vars, store)?);
            Ok(Fv::Bool(fv_equal(&x, &y).unwrap_or(false)))
        }
        Expression::Greater(a, b) => cmp_op(a, b, row, vars, store, |o| o == std::cmp::Ordering::Greater),
        Expression::GreaterOrEqual(a, b) => cmp_op(a, b, row, vars, store, |o| o != std::cmp::Ordering::Less),
        Expression::Less(a, b) => cmp_op(a, b, row, vars, store, |o| o == std::cmp::Ordering::Less),
        Expression::LessOrEqual(a, b) => cmp_op(a, b, row, vars, store, |o| o != std::cmp::Ordering::Greater),
        Expression::Add(a, b) => num_op(a, b, row, vars, store, |x, y| x + y),
        Expression::Subtract(a, b) => num_op(a, b, row, vars, store, |x, y| x - y),
        Expression::Multiply(a, b) => num_op(a, b, row, vars, store, |x, y| x * y),
        Expression::Divide(a, b) => num_op(a, b, row, vars, store, |x, y| x / y),
        Expression::UnaryPlus(a) => Ok(Fv::Num(as_num(&eval(a, row, vars, store)?).ok_or(())?)),
        Expression::UnaryMinus(a) => Ok(Fv::Num(-as_num(&eval(a, row, vars, store)?).ok_or(())?)),
        Expression::Bound(v) => {
            let col = vars.iter().position(|x| x == v.as_str());
            Ok(Fv::Bool(col.is_some_and(|c| row[c] != NULL_ID)))
        }
        Expression::In(e, list) => {
            let x = eval(e, row, vars, store)?;
            for item in list {
                if let Ok(y) = eval(item, row, vars, store) {
                    if fv_equal(&x, &y) == Some(true) {
                        return Ok(Fv::Bool(true));
                    }
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
        _ => Err(()), // nicht unterstützt -> Fehler (Zeile fällt raus)
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
            Fv::Iri(s) | Fv::Str(s) | Fv::Lang(s, _) | Fv::Typed(s, _) => s,
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
            Fv::Iri(_) => return Err(()),
        })),
        Function::StrLen => Ok(Fv::Num(as_str(&arg(0)?).ok_or(())?.chars().count() as f64)),
        Function::UCase => Ok(Fv::Str(as_str(&arg(0)?).ok_or(())?.to_uppercase())),
        Function::LCase => Ok(Fv::Str(as_str(&arg(0)?).ok_or(())?.to_lowercase())),
        Function::Contains => str2(&arg(0)?, &arg(1)?, |a, b| a.contains(b)),
        Function::StrStarts => str2(&arg(0)?, &arg(1)?, |a, b| a.starts_with(b)),
        Function::StrEnds => str2(&arg(0)?, &arg(1)?, |a, b| a.ends_with(b)),
        Function::IsIri | Function::IsBlank => Ok(Fv::Bool(matches!(arg(0)?, Fv::Iri(_)))),
        Function::IsLiteral => Ok(Fv::Bool(!matches!(arg(0)?, Fv::Iri(_)))),
        Function::IsNumeric => Ok(Fv::Bool(matches!(arg(0)?, Fv::Num(_)))),
        _ => Err(()),
    }
}

fn str2(a: &Fv, b: &Fv, f: impl Fn(&str, &str) -> bool) -> Result<Fv, ()> {
    Ok(Fv::Bool(f(as_str(a).ok_or(())?, as_str(b).ok_or(())?)))
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Effective Boolean Value eines Ausdrucks.
fn ebv(expr: &Expression, row: &[u32], vars: &[String], store: &TripleStore) -> Result<bool, ()> {
    match eval(expr, row, vars, store)? {
        Fv::Bool(b) => Ok(b),
        Fv::Num(n) => Ok(n != 0.0 && !n.is_nan()),
        Fv::Str(s) => Ok(!s.is_empty()),
        _ => Err(()),
    }
}

/// Behält eine Zeile, wenn **alle** FILTER-Ausdrücke EBV true ergeben.
fn row_passes(filters: &[&Expression], row: &[u32], vars: &[String], store: &TripleStore) -> bool {
    filters
        .iter()
        .all(|f| ebv(f, row, vars, store) == Ok(true))
}

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
        Vec<&Expression>,
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
        filters: &mut Vec<&'a Expression>,
    ) -> &'a Vec<spargebra::term::TriplePattern> {
        match pattern {
            spargebra::algebra::GraphPattern::Filter { expr, inner } => {
                filters.push(expr);
                walk(inner, projection, distinct, limit, offset, optionals, filters)
            }
            spargebra::algebra::GraphPattern::Project { variables, inner } => {
                *projection = Some(variables.iter().map(|v| v.as_str().to_string()).collect());
                walk(inner, projection, distinct, limit, offset, optionals, filters)
            }
            spargebra::algebra::GraphPattern::Distinct { inner }
            | spargebra::algebra::GraphPattern::Reduced { inner } => {
                *distinct = true;
                walk(inner, projection, distinct, limit, offset, optionals, filters)
            }
            spargebra::algebra::GraphPattern::Slice {
                inner,
                start,
                length,
            } => {
                *offset = Some(*start);
                *limit = *length;
                walk(inner, projection, distinct, limit, offset, optionals, filters)
            }
            spargebra::algebra::GraphPattern::LeftJoin { left, right, .. } => {
                let mandatory = walk(left, projection, distinct, limit, offset, optionals, filters);
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
    let mut filters = Vec::new();
    let mandatory = walk(
        pattern,
        &mut projection,
        &mut distinct,
        &mut limit,
        &mut offset,
        &mut optionals,
        &mut filters,
    );
    Ok((mandatory, optionals, projection, distinct, limit, offset, filters))
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
        let result: Value = serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
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
    fn select_with_unknown_constant_returns_empty_not_panic() {
        let store = test_store();
        let engine = HybridEngine::new();
        // <…/zzz> kommt im Store nicht vor -> leere Lösung, kein Panic.
        let query = "SELECT ?p ?o WHERE { <http://example.org/zzz> ?p ?o }";
        let result: Value = serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        assert_eq!(result["results"]["bindings"].as_array().unwrap().len(), 0);
        // Head-Variablen bleiben erhalten.
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
        let result: Value = serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
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
        let result: Value = serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
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
        let result: Value = serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "DISTINCT ?p should dedup on the projected column");
    }

    #[test]
    fn limit_after_projection() {
        let store = test_store();
        let engine = HybridEngine::new();
        let query = "SELECT DISTINCT ?p WHERE { ?s ?p ?o } LIMIT 1";
        let result: Value = serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
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
        let v30 = store.dict.insert_with_type("30", TermType::literal_datatype(dt));
        let v25 = store.dict.insert_with_type("25", TermType::literal_datatype(dt));
        store.insert_triple(alice, age, v30);
        store.insert_triple(bob, age, v25);
        let engine = HybridEngine::new();
        let query =
            "SELECT ?p ?a WHERE { ?p <http://example.org/age> ?a FILTER(?a > 26) }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "nur 30 > 26");
        assert_eq!(rows[0]["a"]["value"], "30");
    }

    #[test]
    fn filter_str_function() {
        let store = test_store();
        let engine = HybridEngine::new();
        // CONTAINS(STR(?b), "bob") -> nur alice->bob.
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
        // bob (->charlie) fällt raus; alice->bob und charlie->alice bleiben.
        assert_eq!(rows.len(), 2);
        for r in rows {
            assert_ne!(r["b"]["value"], "http://example.org/charlie");
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
}
