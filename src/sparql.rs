use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use lru::LruCache;
use serde_json::{Map, Value, json};
use spargebra::algebra::{Expression, Function, PropertyPathExpression as Ppe};
use spargebra::term::{
    GroundQuad, GroundTerm, Literal, NamedNode, NamedOrBlankNode, Quad, Term as SparqlTerm,
};
use spargebra::{Query as SparqlQuery, SparqlParser};
use tokio_stream::wrappers::ReceiverStream;

use crate::hypertrie::{
    Dictionary, GraphPattern, HybridEngine, NULL_ID, PatternTerm, RowBlock, TermType,
    TriplePattern, TripleStore,
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
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("konnte nicht an {addr} binden: {e}"));
    println!(
        "SPARQL endpoint listening on http://{}/sparql, /stream, /count, /update",
        addr
    );
    axum::serve(listener, app)
        .await
        .expect("HTTP-Server unerwartet beendet");
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
        return sparql_error(
            "Missing query parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    // Cache-Lookup (fertiger JSON-Body).
    {
        if let Ok(cache) = state.cache.lock()
            && let Some(body) = cache.peek(&query_str)
        {
            return json_response(body.clone());
        }
    }

    let store = state.store.read().unwrap_or_else(|e| e.into_inner());
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
        return sparql_error(
            "Missing query parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    // Stream materialisiert die Ergebnisse intern, sendet sie aber als NDJSON
    // chunked, bevor der Gesamt-JSON-Body aufgebaut wird.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::convert::Infallible>>(128);
    let state = Arc::clone(&state);

    tokio::task::spawn_blocking(move || {
        let store = state.store.read().unwrap_or_else(|e| e.into_inner());
        let result = evaluate_select(&store, &state.engine, &query_str);
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
        return sparql_error(
            "Missing query parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    let store = state.store.read().unwrap_or_else(|e| e.into_inner());
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
        return sparql_error(
            "Missing update parameter or empty body",
            StatusCode::BAD_REQUEST,
        );
    }

    let mut store = state.store.write().unwrap_or_else(|e| e.into_inner());
    // WAL für die Dauer des Updates sperren (durabel protokollieren).
    let mut wal_guard = state
        .wal
        .as_ref()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
    match execute_update(&mut store, &update_str, wal_guard.as_deref_mut()) {
        Ok(()) => {
            // Write-Ahead-Log auf Platte zwingen, BEVOR wir Erfolg melden.
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

    let m = peel_modifiers(&pattern);
    evaluate_select_with_modifiers(store, engine, &m)
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
            let m = peel_modifiers(&pattern);
            let result = evaluate_select_with_modifiers(store, engine, &m)?;
            Ok(write_sparql_json(&result, store))
        }
        SparqlQuery::Ask { pattern, .. } => {
            // ASK über den vollen WHERE-Pfad (inkl. OPTIONAL/FILTER/UNION) -> ≥1 Lösung?
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

/// SELECT-Modifier (peeled von der Algebra), plus das innere WHERE-Pattern.
struct Modifiers<'a> {
    where_pat: &'a spargebra::algebra::GraphPattern,
    projection: Option<Vec<String>>,
    distinct: bool,
    order_by: Vec<(&'a Expression, bool)>, // (Ausdruck, absteigend?)
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Schält Project/Distinct/Reduced/Slice/OrderBy von der Algebra ab und liefert
/// das innere WHERE-Pattern (Bgp/Filter/LeftJoin/Join/Union) + die Modifier.
fn peel_modifiers(pattern: &spargebra::algebra::GraphPattern) -> Modifiers<'_> {
    use spargebra::algebra::{GraphPattern as GP, OrderExpression};
    let mut projection = None;
    let mut distinct = false;
    let mut order_by = Vec::new();
    let mut limit = None;
    let mut offset = None;
    let mut cur = pattern;
    loop {
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

/// Wertet ein WHERE-Pattern rekursiv aus: BGP, FILTER, OPTIONAL (LeftJoin),
/// Join, UNION. Liefert (Zeilen, Variablen-Reihenfolge).
fn eval_where(
    gp: &spargebra::algebra::GraphPattern,
    store: &TripleStore,
    engine: &HybridEngine,
) -> Result<(RowBlock, Vec<String>), String> {
    use spargebra::algebra::GraphPattern as GP;
    match gp {
        GP::Bgp { patterns } => eval_bgp(patterns, store, engine),
        GP::Filter { expr, inner } => {
            let (rows, vo) = eval_where(inner, store, engine)?;
            let mut kept = RowBlock::new(rows.n_vars());
            for row in rows.rows() {
                if row_passes(&[expr], row, &vo, store) {
                    kept.push_row(row);
                }
            }
            Ok((kept, vo))
        }
        GP::LeftJoin { left, right, .. } => {
            let (lr, lvo) = eval_where(left, store, engine)?;
            let (rr, rvo) = eval_where(right, store, engine)?;
            Ok(hash_join(lr, &lvo, rr, &rvo, true))
        }
        GP::Join { left, right } => {
            let (lr, lvo) = eval_where(left, store, engine)?;
            let (rr, rvo) = eval_where(right, store, engine)?;
            Ok(hash_join(lr, &lvo, rr, &rvo, false))
        }
        GP::Union { left, right } => {
            let (lr, lvo) = eval_where(left, store, engine)?;
            let (rr, rvo) = eval_where(right, store, engine)?;
            Ok(union_rows(lr, &lvo, rr, &rvo))
        }
        GP::Path {
            subject,
            path,
            object,
        } => eval_path(store, subject, path, object),
        _ => {
            Err("Unsupported WHERE pattern (only BGP/FILTER/OPTIONAL/UNION/Join/Path)".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Property Paths (SPARQL 1.1): /, ^, |, *, +, ?, !{…}
// ---------------------------------------------------------------------------
//
// Evaluierung als gerichtete Mengen-Propagation: ausgehend von einer bekannten
// Knotenmenge liefert `step_forward`/`step_backward` die über den Pfad
// erreichbaren Knoten. `*`/`+` sind transitive Hüllen (BFS bis Fixpunkt).
// Bei genau einem gebundenen Endpunkt ist das effizient (Closure nur vom
// gebundenen Knoten aus); bei zwei Variablen wird über alle Startknoten
// aufgezählt (korrekt, aber potenziell teuer – für `*` mit beiden Variablen
// die degenerierte Identitätsmenge über alle Knoten).

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
        spargebra::term::TermPattern::BlankNode(_) => None,
    }
}

/// Über `path` von `from` aus erreichbare Knoten (vorwärts).
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

/// Über `path` zu `from` führende Knoten (rückwärts).
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
            // rückwärts: erst b^-1, dann a^-1
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

/// Transitive Hülle (BFS bis Fixpunkt). `reflexive` schließt die Startmenge
/// ein (`*` vs `+`). `forward` wählt die Richtung.
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

    // Variablennamen für leere/Ergebnis-Spalten ableiten.
    let s_var = match subject {
        spargebra::term::TermPattern::Variable(v) => Some(v.as_str().to_string()),
        _ => None,
    };
    let o_var = match object {
        spargebra::term::TermPattern::Variable(v) => Some(v.as_str().to_string()),
        _ => None,
    };

    // Unbekannte Konstante auf einer Seite -> leere Lösung (mit Variablenspalten).
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
        // Subjekt gebunden, Objekt Variable: Vorwärts-Closure.
        (Some(PathEnd::Bound(s)), Some(PathEnd::Var(ov))) => {
            let mut from = FxHashSet::default();
            from.insert(s);
            let ends = step_forward(store, path, &from);
            let mut rows = RowBlock::new(1);
            for o in ends {
                rows.push_row(&[o]);
            }
            Ok((rows, vec![ov]))
        }
        // Objekt gebunden, Subjekt Variable: Rückwärts-Closure.
        (Some(PathEnd::Var(sv)), Some(PathEnd::Bound(o))) => {
            let mut from = FxHashSet::default();
            from.insert(o);
            let starts = step_backward(store, path, &from);
            let mut rows = RowBlock::new(1);
            for s in starts {
                rows.push_row(&[s]);
            }
            Ok((rows, vec![sv]))
        }
        // Beide gebunden: Existenzprüfung (0 Variablen, 1 leere Zeile bei Treffer).
        (Some(PathEnd::Bound(s)), Some(PathEnd::Bound(o))) => {
            let mut from = FxHashSet::default();
            from.insert(s);
            let ends = step_forward(store, path, &from);
            let mut rows = RowBlock::new(0);
            if ends.contains(&o) {
                rows.push_row(&[]);
            }
            Ok((rows, Vec::new()))
        }
        // Beide Variablen: über alle Startknoten aufzählen.
        (Some(PathEnd::Var(sv)), Some(PathEnd::Var(ov))) => {
            let same = sv == ov;
            // Startkandidaten: distinkte Subjekte; für reflexive Pfade zusätzlich
            // Objekte (Identität (x,x) gilt für jeden Knoten).
            let needs_all_nodes = path_is_reflexive(path);
            let mut starts = store.distinct_subjects();
            if needs_all_nodes {
                starts.extend(store.distinct_objects());
                starts.sort_unstable();
                starts.dedup();
            }
            let rows_vars = if same { vec![sv] } else { vec![sv, ov] };
            let mut rows = RowBlock::new(rows_vars.len());
            for s in starts {
                let mut from = FxHashSet::default();
                from.insert(s);
                let ends = step_forward(store, path, &from);
                for o in ends {
                    if same {
                        if o == s {
                            rows.push_row(&[s]);
                        }
                    } else {
                        rows.push_row(&[s, o]);
                    }
                }
            }
            Ok((rows, rows_vars))
        }
        // unreachable: None-Fälle oben behandelt
        _ => Ok((RowBlock::new(0), Vec::new())),
    }
}

/// Ob ein Pfad die leere (reflexive) Sequenz enthält (`*` oder `?` an der Wurzel).
fn path_is_reflexive(path: &Ppe) -> bool {
    matches!(path, Ppe::ZeroOrMore(_) | Ppe::ZeroOrOne(_))
}

fn eval_bgp(
    patterns: &[spargebra::term::TriplePattern],
    store: &TripleStore,
    engine: &HybridEngine,
) -> Result<(RowBlock, Vec<String>), String> {
    match translate_bgp(patterns, &store.dict)? {
        Some(internal) => {
            let vo: Vec<String> = internal.variable_order().into_iter().cloned().collect();
            Ok((engine.execute(store, &internal)?, vo))
        }
        None => {
            // Unbekannte Konstante -> leere Lösung mit den Pattern-Variablen.
            let vo = variables_in_bgp(patterns);
            Ok((RowBlock::new(vo.len()), vo))
        }
    }
}

/// Hash-Join zweier Ergebnis-Blöcke auf den gemeinsamen Variablen.
/// `left_outer = true` behält linke Zeilen ohne Match (NULL-aufgefüllt).
fn hash_join(
    left: RowBlock,
    lvo: &[String],
    right: RowBlock,
    rvo: &[String],
    left_outer: bool,
) -> (RowBlock, Vec<String>) {
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

    // Rechte Seite nach Join-Schlüssel indizieren (neue Spalten flach).
    let mut index: rustc_hash::FxHashMap<Vec<u32>, Vec<u32>> = rustc_hash::FxHashMap::default();
    for rrow in right.rows() {
        let key: Vec<u32> = shared.iter().map(|&(_, rp)| rrow[rp]).collect();
        let bucket = index.entry(key).or_default();
        for &p in &new_positions {
            bucket.push(rrow[p]);
        }
    }

    let mut out = RowBlock::new(new_var_order.len());
    for row in left.rows() {
        let key: Vec<u32> = shared.iter().map(|&(lp, _)| row[lp]).collect();
        match index.get(&key) {
            Some(flat) if !flat.is_empty() => {
                if new_arity == 0 {
                    out.push_row_padded(row, NULL_ID); // Match, aber keine neuen Spalten
                } else {
                    for chunk in flat.chunks(new_arity) {
                        out.push_row_concat(row, chunk);
                    }
                }
            }
            _ => {
                if left_outer {
                    out.push_row_padded(row, NULL_ID);
                }
            }
        }
    }
    (out, new_var_order)
}

/// UNION zweier Ergebnis-Blöcke: gemeinsame Variablenmenge, fehlende -> NULL.
fn union_rows(
    left: RowBlock,
    lvo: &[String],
    right: RowBlock,
    rvo: &[String],
) -> (RowBlock, Vec<String>) {
    let mut vo = lvo.to_vec();
    for v in rvo {
        if !vo.contains(v) {
            vo.push(v.clone());
        }
    }
    // Spaltenabbildung Quelle -> Ziel.
    let map_cols = |src_vo: &[String]| -> Vec<Option<usize>> {
        vo.iter()
            .map(|v| src_vo.iter().position(|x| x == v))
            .collect()
    };
    let lmap = map_cols(lvo);
    let rmap = map_cols(rvo);
    let mut out = RowBlock::new(vo.len());
    let emit = |src: &RowBlock, map: &[Option<usize>], out: &mut RowBlock| {
        let mut buf = vec![NULL_ID; map.len()];
        for row in src.rows() {
            for (i, m) in map.iter().enumerate() {
                buf[i] = m.map(|c| row[c]).unwrap_or(NULL_ID);
            }
            out.push_row(&buf);
        }
    };
    emit(&left, &lmap, &mut out);
    emit(&right, &rmap, &mut out);
    (out, vo)
}

/// Sortier-Schlüssel für ORDER BY (SPARQL-nahe Gesamtordnung).
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

fn evaluate_select_with_modifiers(
    store: &TripleStore,
    engine: &HybridEngine,
    m: &Modifiers,
) -> Result<SelectResult, String> {
    let (mut rows, var_order) = eval_where(m.where_pat, store, engine)?;

    // ORDER BY (auf den vollen Bindings, vor der Projektion).
    if !m.order_by.is_empty() {
        sort_rows(&mut rows, &var_order, &m.order_by, store);
    }

    // SELECT * : alle Variablen außer internen Blank-Node-Platzhaltern (__bn_),
    // die nur aus expandierten Sequenz-Pfaden stammen und nicht ausgegeben werden.
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
            rows.dedup_preserving_order(); // Sortierung der ORDER BY beibehalten
        }
    }
    rows.apply_offset_limit(m.offset.unwrap_or(0), m.limit);

    Ok(SelectResult {
        vars,
        rows,
        var_order,
    })
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
        (Some(v), Some(TermType::BlankNode)) => {
            out.push_str("{\"type\":\"bnode\",\"value\":");
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

/// Fügt einen Term ins Dictionary ein und protokolliert ihn im WAL, falls er
/// neu war (damit der Replay dieselben IDs vergibt).
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

/// Term-Typ eines geparsten SPARQL-Literals (für Dictionary-Lookup/-Insert).
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

/// Laufzeit-Wert eines FILTER-Ausdrucks.
#[derive(Debug, Clone)]
enum Fv {
    Iri(String),
    Blank(String), // Blank-Node-Label (isBlank != isIri)
    Str(String),   // einfaches Literal / xsd:string
    Num(f64),      // numerischer Datentyp
    Bool(bool),
    Lang(String, String),  // (Lexikal, Sprach-Tag)
    Typed(String, String), // (Lexikal, Datatype-IRI) – nicht numerisch/string
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
        TermType::Iri => Some(Fv::Iri(v.to_string())),
        TermType::BlankNode => Some(Fv::Blank(v.to_string())),
        TermType::Literal { datatype, lang } => {
            Some(classify(v, datatype.as_deref(), lang.as_deref()))
        }
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
        // SPARQL erlaubt Ordnungsvergleiche auf IRIs und (gleich-typisierten) Literalen.
        (Fv::Iri(x), Fv::Iri(y)) => Some(x.cmp(y)),
        (Fv::Blank(x), Fv::Blank(y)) => Some(x.cmp(y)),
        (Fv::Bool(x), Fv::Bool(y)) => Some(x.cmp(y)),
        // Gleicher Datentyp -> lexikalischer Vergleich; verschiedene -> unvergleichbar.
        (Fv::Typed(x, dx), Fv::Typed(y, dy)) if dx == dy => Some(x.cmp(y)),
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
            // SPARQL 1.1: Division durch Null ist ein Fehler -> Zeile fällt raus.
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
            Fv::Iri(_) | Fv::Blank(_) => return Err(()), // datatype() nur für Literale
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
    filters.iter().all(|f| ebv(f, row, vars, store) == Ok(true))
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
            // Lexikalwert **und** Datentyp/Sprach-Tag müssen passen: "25"^^xsd:integer
            // darf nicht "25"^^xsd:string oder den IRI 25 treffen.
            match dict.lookup_term(lit.value(), &literal_term_type(lit)) {
                Some(id) => Ok(TranslationResult::Term(PatternTerm::Bound(id))),
                None => Ok(TranslationResult::UnknownConstant),
            }
        }
        // Blank Nodes in Query-Mustern wirken wie nicht-distinguierte Variablen
        // (und sind genau das, was spargebra für expandierte Sequenz-Pfade
        // `p1/p2` als Zwischenknoten einsetzt). Mit stabilem internen Namen
        // mappen, sodass dasselbe `_:b` über mehrere Tripel hinweg joint.
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

    const XSD_INT: &str = "http://www.w3.org/2001/XMLSchema#integer";

    /// BGP-Lookup eines Literals muss den Datentyp respektieren: `"25"`
    /// (= xsd:string) darf nicht das gleichnamige xsd:integer-Literal treffen.
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
        assert_ne!(
            o_int, o_str,
            "typisierte Literale müssen verschiedene IDs haben"
        );
        store.insert_triple(s_int, p, o_int);
        store.insert_triple(s_str, p, o_str);

        // "25" ohne Typ -> xsd:string -> nur sStr.
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/p> \"25\" }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/sStr");

        // "25"^^xsd:integer -> nur sInt.
        let rows = rows_of(
            &store,
            "SELECT ?s WHERE { ?s <http://example.org/p> \"25\"^^<http://www.w3.org/2001/XMLSchema#integer> }",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "http://example.org/sInt");
    }

    /// SPARQL 1.1: Division durch Null ist ein Fehler -> die Zeile fällt im
    /// FILTER raus (kein INFINITY, das `> 0` erfüllen würde).
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

    /// FILTER mit `<` auf IRIs muss vergleichen (vorher: immer Fehler -> leer).
    #[test]
    fn filter_iri_less_than_compares() {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[(
            "http://example.org/a",
            "http://example.org/p",
            "http://example.org/b",
        )]);
        // a < b lexikalisch -> eine Zeile.
        let rows = rows_of(
            &store,
            "SELECT ?x ?y WHERE { ?x <http://example.org/p> ?y FILTER(?x < ?y) }",
        );
        assert_eq!(rows.len(), 1);
        // umgekehrt: b < a ist falsch -> leer.
        let rows = rows_of(
            &store,
            "SELECT ?x ?y WHERE { ?x <http://example.org/p> ?y FILTER(?y < ?x) }",
        );
        assert_eq!(rows.len(), 0);
    }

    /// isBlank unterscheidet Blank Nodes von IRIs; das IRI-Objekt darf nicht
    /// als Blank Node durchgehen, und es wird als `bnode` ausgegeben.
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

        // isIri ist komplementär: nur das IRI-Objekt.
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
        // <…/zzz> kommt im Store nicht vor -> leere Lösung, kein Panic.
        let query = "SELECT ?p ?o WHERE { <http://example.org/zzz> ?p ?o }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
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
        // Zwei Triples mit Prädikat knows + eines mit age.
        // SELECT DISTINCT ?p muss {knows, age} = 2 Zeilen liefern,
        // NICHT 3 (knows darf nicht doppelt erscheinen).
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
        // 9 < 25 < 100 numerisch; lexikalisch wäre es "100" < "25" < "9".
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
        // DISTINCT ?p -> {knows, age}; ORDER BY ?p sortiert die zwei Prädikate.
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
        // Branch 1: knows-Objekte (bob, charlie, alice); Branch 2: age-Objekt (25).
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
        // Branch 1 bindet nur ?a, Branch 2 nur ?b -> Spalten müssen mit NULL ausgerichtet werden.
        let query = "SELECT ?a ?b WHERE { \
                     { ?a <http://example.org/knows> <http://example.org/bob> } UNION \
                     { ?b <http://example.org/knows> <http://example.org/alice> } }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, query).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // Eine Zeile hat a=alice (b NULL), die andere b=charlie (a NULL).
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
        assert!(a_only, "Zeile mit nur ?a=alice fehlt");
        assert!(b_only, "Zeile mit nur ?b=charlie fehlt");
    }

    fn path_store() -> TripleStore {
        // Kette alice -> bob -> carol -> dave (knows) + alice likes eve.
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
        // alice knows+ ?o -> bob, carol, dave (transitive Hülle, ohne alice).
        assert_eq!(
            obj_values(
                "SELECT ?o WHERE { <http://example.org/alice> <http://example.org/knows>+ ?o }"
            ),
            vec!["bob", "carol", "dave"]
        );
    }

    #[test]
    fn path_zero_or_more_includes_self() {
        // alice knows* ?o -> alice (0 Schritte), bob, carol, dave.
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
        // alice knows/knows ?o -> carol (2 Schritte).
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
        // bob ^knows ?s -> wer kennt bob = alice (über ?o-Spalte ausgewertet? nein: ?s).
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
        // ?s knows+ ?o -> alle erreichbaren Paare: (alice,{bob,carol,dave}),
        // (bob,{carol,dave}), (carol,{dave}) = 6 Paare.
        let store = path_store();
        let engine = HybridEngine::new();
        let q = "SELECT ?s ?o WHERE { ?s <http://example.org/knows>+ ?o }";
        let result: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, q).unwrap()).unwrap();
        let rows = result["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 6, "6 transitiv erreichbare (s,o)-Paare");
    }

    #[test]
    fn path_in_c2rpq_join() {
        // C2RPQ-Form: Pfad join BGP. Alle Tripel in EINEM Ingest, da
        // ingest_str_triples die Indizes jeweils neu baut (nicht anhängt).
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
        // ?s knows+ ?mid . ?mid likes ?z  -> ?mid muss carol sein (carol likes zed),
        // erreichbar von alice und bob -> 2 Treffer.
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
        // Reproduziert die WDBench-`opts`-q3-Form (Rust 19 vs Tentris 6,77M):
        // ein OPTIONAL, dessen Muster eine Variable nutzt, die nur in einem
        // FRÜHEREN OPTIONAL gebunden wird ("nicht wohlgeformtes" Pattern).
        //   ?x1 P102 <O> .
        //   OPTIONAL { ?x1 P569 ?x2 }
        //   OPTIONAL { ?x1 P19  ?x3 }
        //   OPTIONAL { ?x1 P21  ?x4 }
        //   OPTIONAL { ?x3 P625 ?x5 }   <- ?x3 stammt aus dem 2. OPTIONAL
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
            // RAUSCHEN: P625 von einem NICHT über x3 erreichbaren Subjekt.
            // Ein Kreuzprodukt (Tentris-Verdacht) würde das fälschlich einziehen.
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
        // Korrekte SPARQL-Semantik (left-deep):
        //   s1×{m1,m2} -> 2 Zeilen mit x5=c1/c2; s2 -> 1 Zeile komplett NULL = 3.
        // KEIN Kreuzprodukt: das Rausch-Tripel (noise P625 nc) darf NICHT auftauchen.
        assert_eq!(rows.len(), 3, "left-deep OPTIONAL-Kette, kein Kreuzprodukt");
        let x5: Vec<Option<&str>> = rows
            .iter()
            .map(|r| {
                r.get("x5")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
            })
            .collect();
        assert!(x5.contains(&Some("http://ex/c1")));
        assert!(x5.contains(&Some("http://ex/c2")));
        assert!(x5.contains(&None)); // s2-Zeile: x3=NULL -> x5=NULL
        assert!(
            !x5.contains(&Some("http://ex/nc")),
            "kein Rausch-Tripel (Kreuzprodukt)"
        );
    }

    #[test]
    fn ingests_and_queries_blank_nodes() {
        // Blank-Node-Tripel werden geladen (nicht übersprungen), als `bnode`
        // ausgegeben, und dasselbe `_:b0` über mehrere Zeilen ist derselbe
        // Knoten (dokument-scoped Identität -> Join funktioniert).
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

        // (1) Objekt ist ein Blank Node -> type=bnode.
        let q = "SELECT ?x WHERE { <http://ex/alice> <http://ex/knows> ?x }";
        let r: Value = serde_json::from_str(&execute_sparql(&store, &engine, q).unwrap()).unwrap();
        let rows = r["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "Blank-Node-Tripel wurde geladen");
        assert_eq!(rows[0]["x"]["type"], "bnode");

        // (2) Join über denselben Blank Node (beide _:b0 = dieselbe ID).
        let q2 = "SELECT ?n WHERE { <http://ex/alice> <http://ex/knows> ?b . \
                  ?b <http://ex/name> ?n }";
        let r2: Value =
            serde_json::from_str(&execute_sparql(&store, &engine, q2).unwrap()).unwrap();
        let rows2 = r2["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows2.len(), 1, "Join über denselben Blank Node");
        assert_eq!(rows2[0]["n"]["value"], "Bob");

        let _ = std::fs::remove_file(&path);
    }
}
