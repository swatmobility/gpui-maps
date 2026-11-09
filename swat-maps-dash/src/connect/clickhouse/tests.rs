//! Task 1.7 with D3 and D8: the boundary, redaction, the probe, DESCRIBE,
//! the row cap, the URL goldens and cancel. Every test runs against a
//! recording stub transport; nothing touches a network, a Keychain or a
//! real credential. The two sentinels are the spec's canaries (C7).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::block_on;
use crate::connect::rowbinary::encode;
use crate::connect::{Connection, ConnectionKind, Credentials, Secret, StaticCredentials, TransportError};
use crate::hook::{reference::MinGroupHook, run_query, GuardedSource, Privacy, QueryError, Refusal, Schema, SourceRegistry};
use crate::spec::Page;
use crate::sql::{check_page, CheckOptions, GuardCode, QueryKey, VarValue};

const PW: &str = "CANARY-PW-7f3e";
const USER: &str = "CANARY-USER-91c2";

type Reply = Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync>;

struct Stub {
    log: Mutex<Vec<HttpRequest>>,
    reply: Reply,
}

impl Stub {
    fn new(reply: impl Fn(&HttpRequest) -> Result<HttpResponse, TransportError> + Send + Sync + 'static) -> Stub {
        Stub { log: Mutex::new(Vec::new()), reply: Box::new(reply) }
    }
    fn ok(body: Vec<u8>) -> Stub {
        Stub::new(move |_| Ok(HttpResponse { status: 200, body: body.clone() }))
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.log.lock().unwrap().clone()
    }
}

impl HttpTransport for Stub {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        let r = (self.reply)(&req);
        self.log.lock().unwrap().push(req);
        Box::pin(async move { r })
    }
}

fn conn() -> Connection {
    Connection::new("local", ConnectionKind::ClickHouse, "http://127.0.0.1:8123/").with_credentials(Arc::new(StaticCredentials(
        Credentials { user: Some(USER.into()), password: Some(Secret::new(PW)) },
    )))
}

fn one_row() -> Vec<u8> {
    let mut b = encode::header(&[("n", "UInt64")]);
    b.extend(1u64.to_le_bytes());
    b
}

fn select(sql: &str) -> CompiledQuery {
    CompiledQuery::forced_for_test(sql, Dialect::ClickHouse)
}

#[test]
fn read_only_enforced_at_boundary() {
    let stub = Stub::ok(one_row());
    let c = conn();
    // (a) a generated statement forced to a write; (b) and (c) sent directly.
    for sql in ["INSERT INTO t VALUES (1)", "DROP TABLE t", "SELECT 1; DROP TABLE t"] {
        let q = select(sql);
        match block_on(ClickHouse::send(&c, Statement::Query(&q), &stub)) {
            Err(SendError::Refused(r)) => assert_eq!(r.code(), "statement_not_read", "{sql}"),
            other => panic!("{sql}: {other:?}"),
        }
        match block_on(ClickHouse::send(&c, Statement::Describe(&q), &stub)) {
            Err(SendError::Refused(_)) => {}
            other => panic!("DESCRIBE of {sql}: {other:?}"),
        }
    }
    assert_eq!(stub.requests().len(), 0, "a refused statement sends zero requests");
    // The guard holds whatever the host declared: D2 makes `guarded()` the
    // only way to the text, so a read_only: false connection refuses too.
    let mut writable = conn();
    writable.read_only = false;
    assert!(block_on(ClickHouse::send(&writable, Statement::Query(&select("DROP TABLE t")), &stub)).is_err());
    assert_eq!(stub.requests().len(), 0);
    // The recorder works: a read reaches it.
    let t = block_on(ClickHouse::send(&c, Statement::Query(&select("SELECT 1 AS n")), &stub)).unwrap();
    assert_eq!(t.row_count(), 1);
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn every_request_carries_a_query_id_and_the_guarded_sql() {
    let stub = Stub::ok(one_row());
    block_on(ClickHouse::send(&conn(), Statement::Query(&select("SELECT 1 AS n")), &stub)).unwrap();
    let r = &stub.requests()[0];
    assert!(r.url.contains("&query_id=dash-"), "{}", r.url);
    assert_eq!(r.body, b"SELECT 1 AS n");
    assert_eq!(r.method, "POST");
    assert_eq!(r.header("X-ClickHouse-User").map(|h| h.expose()), Some(USER));
    assert_eq!(r.header("X-ClickHouse-Key").map(|h| h.expose()), Some(PW));
    assert!(!format!("{r:?}").contains(PW), "a request's Debug hides the key");
}

fn url_settings(url: &str) -> Vec<String> {
    url.split_once('?').unwrap().1.split('&').map(|kv| kv.split('=').next().unwrap().to_string()).collect()
}

#[test]
fn send_per_request_sends_readonly_and_the_cap() {
    let stub = Stub::ok(one_row());
    let q = select("SELECT 1 AS n").with_max_rows(500);
    block_on(ClickHouse::send(&conn(), Statement::Query(&q), &stub)).unwrap();
    let url = &stub.requests()[0].url;
    assert!(url.starts_with("http://127.0.0.1:8123/?default_format=RowBinaryWithNamesAndTypes&query_id="), "{url}");
    assert!(url.ends_with(
        "&readonly=2&max_execution_time=30&max_result_rows=501&result_overflow_mode=break&cancel_http_readonly_queries_on_client_close=1"
    ), "{url}");
}

#[test]
fn strict_sends_readonly_1_and_the_time_limit() {
    let stub = Stub::ok(one_row());
    let mut c = conn();
    c.ch_readonly_setting = ChReadonly::Strict;
    c.timeout = std::time::Duration::from_secs(25);
    let q = select("SELECT 1 AS n").with_max_rows(500);
    block_on(ClickHouse::send(&c, Statement::Query(&q), &stub)).unwrap();
    let url = &stub.requests()[0].url;
    assert!(url.contains("&readonly=1&max_execution_time=25&max_result_rows=501"), "{url}");
    assert!(!url.contains("readonly=2"), "{url}");
}

#[test]
fn server_profile_sends_no_settings() {
    let stub = Stub::ok(one_row());
    let mut c = conn();
    c.ch_readonly_setting = ChReadonly::ServerProfile;
    c.database = Some("bikeshare_demo".into());
    let (p, _) = Page::from_json(
        r#"{ "id": "p", "title": "P", "source": "s", "variables": [{ "key": "k", "control": "single" }],
             "filters": [{ "var": "k", "column": "c" }],
             "widgets": [{ "id": "w", "type": "bar", "sql": "SELECT c FROM t ${where}" }] }"#,
    )
    .unwrap();
    let values: BTreeMap<String, VarValue> = [("k".into(), VarValue::One(crate::sql::Scalar::Str("v".into())))].into_iter().collect();
    let q = check_page(&p, &CheckOptions::default()).compile(&p, &QueryKey::Widget("w".into()), &values, &BTreeMap::new()).unwrap().unwrap();
    block_on(ClickHouse::send(&c, Statement::Query(&q), &stub)).unwrap();
    assert_eq!(url_settings(&stub.requests()[0].url), ["database", "default_format", "query_id", "param_f_k"]);
}

#[test]
fn cap_over_ceiling_is_refused() {
    let stub = Stub::ok(one_row());
    let q = select("SELECT 1 AS n").with_max_rows(200_000);
    match block_on(ClickHouse::send(&conn(), Statement::Query(&q), &stub)) {
        Err(SendError::Failed(e)) => assert_eq!(e.code, "max_rows_over_ceiling"),
        other => panic!("{other:?}"),
    }
    assert!(stub.requests().is_empty());
    let mut explorer = conn();
    explorer.max_rows_ceiling = 1_000_000;
    assert!(block_on(ClickHouse::send(&explorer, Statement::Query(&q), &stub)).is_ok());
}

#[test]
fn cap_plus_one_rows_report_truncated() {
    let mut body = encode::header(&[("n", "UInt64")]);
    for i in 0..4u64 {
        body.extend(i.to_le_bytes());
    }
    let stub = Stub::ok(body);
    let t = block_on(ClickHouse::send(&conn(), Statement::Query(&select("SELECT n FROM t").with_max_rows(3)), &stub)).unwrap();
    assert_eq!(t.row_count(), 3);
    assert!(t.truncated());
}

#[test]
fn lowcardinality_nullable_string_url_params() {
    let (p, _) = Page::from_json(
        r#"{ "id": "p", "title": "P", "source": "s",
             "variables": [{ "key": "k", "control": "single", "options_sql": "SELECT DISTINCT k FROM t" },
                           { "key": "m", "control": "multi", "options_sql": "SELECT DISTINCT m FROM t" }],
             "filters": [{ "var": "k", "column": "k" }, { "var": "m", "column": "m" }],
             "widgets": [{ "id": "w", "type": "bar", "sql": "SELECT k FROM t ${where}" }] }"#,
    )
    .unwrap();
    let lc = "LowCardinality(Nullable(String))".to_string();
    let inferred: BTreeMap<String, String> = [("k".into(), lc.clone()), ("m".into(), lc)].into_iter().collect();
    let s = |v: &str| crate::sql::Scalar::Str(v.into());
    let values: BTreeMap<String, VarValue> =
        [("k".into(), VarValue::One(s("a'b"))), ("m".into(), VarValue::Many(vec![s("a"), s("b\\'c")]))].into_iter().collect();
    let q = check_page(&p, &CheckOptions::default()).compile(&p, &QueryKey::Widget("w".into()), &values, &inferred).unwrap().unwrap();
    let g = q.guarded(Dialect::ClickHouse).unwrap();
    assert_eq!(
        g.sql(),
        "SELECT k FROM t WHERE k = {f_k:LowCardinality(Nullable(String))} AND m IN {f_m:Array(LowCardinality(Nullable(String)))}"
    );
    let req = ClickHouse::request(&conn(), Statement::Query(&q), &QueryId("q1".into())).unwrap();
    assert!(req.url.contains("&param_f_k=a%27b&"), "{}", req.url);
    // ['a','b\\\'c'], escaped as ClickHouse reads an array parameter.
    assert!(req.url.ends_with("&param_f_m=%5B%27a%27%2C%27b%5C%5C%5C%5C%5C%5C%27c%27%5D"), "{}", req.url);
}

#[test]
fn secrets_never_logged() {
    let echo = |status: u16| {
        move |r: &HttpRequest| {
            let headers: Vec<String> = r.headers.iter().map(|(k, v)| format!("{k}: {}", v.expose())).collect();
            Ok(HttpResponse { status, body: format!("{} {} {}", r.url, headers.join(" "), "http://someone:x@127.0.0.1:8123/").into_bytes() })
        }
    };
    let auth = |r: &HttpRequest| {
        let user = r.header("X-ClickHouse-User").map(|h| h.expose().to_string()).unwrap_or_default();
        Ok(HttpResponse {
            status: 401,
            body: format!("Code: 516. DB::Exception: {user}: Authentication failed: password is incorrect. (AUTHENTICATION_FAILED)").into_bytes(),
        })
    };
    let stubs: Vec<Stub> = vec![
        Stub::new(echo(401)),
        Stub::new(echo(500)),
        Stub::new(auth),
        Stub::new(|_| Err(TransportError::Timeout)),
        Stub::new(|r: &HttpRequest| Err(TransportError::Io(format!("connect to {} failed with {PW}", r.url)))),
        Stub::ok(vec![1, 2, 3]),
    ];
    for stub in stubs {
        let e = match block_on(ClickHouse::send(&conn(), Statement::Query(&select("SELECT 1 AS n")), &stub)) {
            Err(SendError::Failed(e)) => e,
            other => panic!("{other:?}"),
        };
        for shown in [format!("{e}"), format!("{e:?}")] {
            for canary in [PW, USER, "someone:x"] {
                assert!(!shown.contains(canary), "{canary} leaked: {shown}");
            }
        }
        assert_eq!(e.connection, "local");
    }
}

#[test]
fn auth_errors_are_classified_and_redacted() {
    let stub = Stub::new(|_| {
        Ok(HttpResponse {
            status: 401,
            body: format!("Code: 516. DB::Exception: {USER}: Authentication failed. (AUTHENTICATION_FAILED)").into_bytes(),
        })
    });
    match block_on(ClickHouse::send(&conn(), Statement::Query(&select("SELECT 1 AS n")), &stub)) {
        Err(SendError::Failed(e)) => {
            assert_eq!(e.code, "auth_rejected");
            assert_eq!(e.server_code, Some(516));
            assert!(e.message.contains("***") && !e.message.contains(USER), "{}", e.message);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn readonly_probe() {
    let answer = |v: u64| {
        let mut b = encode::header(&[("readonly", "UInt64")]);
        b.extend(v.to_le_bytes());
        Stub::ok(b)
    };
    let mut c = conn();
    c.ch_readonly_setting = ChReadonly::ServerProfile;
    assert_eq!(block_on(ClickHouse::readonly_probe(&c, &answer(0))), ServerReadonly::No);
    let one = answer(1);
    assert_eq!(block_on(ClickHouse::readonly_probe(&c, &one)), ServerReadonly::Yes);
    assert_eq!(url_settings(&one.requests()[0].url), ["default_format", "query_id"], "the probe sends no settings");
    let refused = Stub::new(|_| Ok(HttpResponse { status: 403, body: b"Code: 497. DB::Exception: x. (ACCESS_DENIED)".to_vec() }));
    assert_eq!(block_on(ClickHouse::readonly_probe(&c, &refused)), ServerReadonly::Unknown);
    // With SendPerRequest the probe still sends no readonly setting, so it
    // reads the server's own value.
    let one = answer(1);
    block_on(ClickHouse::readonly_probe(&conn(), &one));
    assert!(!one.requests()[0].url.contains("readonly=2"));
}

/// A stub server for an aggregate-only source: refuses DESCRIBE, answers
/// every query with a group of 7 and a `card_id` column.
fn card_server() -> Stub {
    Stub::new(|r: &HttpRequest| {
        if r.body.starts_with(b"DESCRIBE") {
            return Ok(HttpResponse { status: 403, body: b"Code: 497. DB::Exception: no. (ACCESS_DENIED)".to_vec() });
        }
        let mut b = encode::header(&[("card_id", "String"), ("riders", "UInt64")]);
        encode::string(b"CANARY-CARD", &mut b);
        b.extend(7_777_777u64.to_le_bytes());
        Ok(HttpResponse { status: 200, body: b })
    })
}

fn card_registry() -> SourceRegistry {
    let source = ClickHouseSource {
        connection: Arc::new(conn()),
        schema: Schema { columns: vec![("riders".into(), "UInt64".into())], forbidden_columns: vec!["card_id".into()] },
    };
    let mut reg = SourceRegistry::new();
    reg.register(
        "cards",
        GuardedSource::new(Arc::new(source), Arc::new(MinGroupHook::new("riders", &[], 10)), Privacy::AggregateOnly).threshold("min_group", 10),
    );
    reg
}

#[test]
fn describe_refused() {
    let stub = card_server();
    let q = select("SELECT card_id, riders FROM trips");
    match block_on(run_query(&card_registry(), "cards", &q, &stub, "en")) {
        Err(QueryError::Refused(Refusal::ForbiddenColumn { column })) => assert_eq!(column.as_str(), "card_id"),
        other => panic!("{other:?}"),
    }
    let reqs = stub.requests();
    assert_eq!(reqs.len(), 2, "DESCRIBE was tried, refused, and the query still ran");
    assert!(reqs[0].body.starts_with(b"DESCRIBE (SELECT card_id"));
}

#[test]
fn describe_refuses_before_any_row_is_read() {
    // A server that does describe: the forbidden column is refused before
    // the query runs.
    let stub = Stub::new(|r: &HttpRequest| {
        let mut b = encode::header(&[("name", "String"), ("type", "String")]);
        encode::string(b"card_id", &mut b);
        encode::string(b"String", &mut b);
        assert!(r.body.starts_with(b"DESCRIBE"), "only DESCRIBE is sent");
        Ok(HttpResponse { status: 200, body: b })
    });
    let q = select("SELECT card_id FROM trips");
    assert!(matches!(block_on(run_query(&card_registry(), "cards", &q, &stub, "en")), Err(QueryError::Refused(_))));
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn a_denied_table_function_is_refused_at_send_and_allowed_per_connection() {
    let stub = Stub::ok(one_row());
    let q = select("SELECT n FROM remote('10.0.0.1', db.t)");
    match block_on(ClickHouse::send(&conn(), Statement::Query(&q), &stub)) {
        Err(SendError::Refused(Refusal::Guard { code: GuardCode::TableFunction("remote") })) => {}
        other => panic!("{other:?}"),
    }
    assert!(stub.requests().is_empty());
    let mut c = conn();
    c.allowed_table_functions = vec!["remote".into()];
    assert!(block_on(ClickHouse::send(&c, Statement::Query(&q), &stub)).is_ok());
}

#[test]
fn cancel_by_query_id() {
    let id = QueryId("dash-1-2-3".into());
    let req = ClickHouse::cancel_request(&conn(), &id).unwrap();
    assert_eq!(req.body, b"KILL QUERY WHERE query_id = {qid:String} ASYNC");
    assert!(req.url.ends_with("?default_format=TabSeparated&param_qid=dash-1-2-3"), "{}", req.url);
    assert!(!req.url.contains("readonly"));

    let found = Stub::ok(b"waiting\tdash-1-2-3\tdefault\tSELECT 1\n".to_vec());
    assert_eq!(block_on(ClickHouse::cancel(&conn(), &id, &found)), CancelOutcome::Requested);
    let none = Stub::ok(Vec::new());
    assert_eq!(block_on(ClickHouse::cancel(&conn(), &id, &none)), CancelOutcome::NotRunning);
    let refused = Stub::new(|_| Ok(HttpResponse { status: 403, body: b"Code: 497. DB::Exception: not allowed. (ACCESS_DENIED)".to_vec() }));
    match block_on(ClickHouse::cancel(&conn(), &id, &refused)) {
        CancelOutcome::Refused(e) => assert_eq!(e.code, "access_denied"),
        other => panic!("a refused kill is never a cancel: {other:?}"),
    }
    // The cancelled query itself comes back as 394: cancelled, not failed.
    let cancelled = Stub::new(|_| Ok(HttpResponse { status: 500, body: b"Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)".to_vec() }));
    match block_on(ClickHouse::send(&conn(), Statement::Query(&select("SELECT 1 AS n")), &cancelled)) {
        Err(SendError::Failed(e)) => assert!(is_cancelled(&e)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn rowbinary_5k_rows_decode_under_2ms() {
    let mut b = encode::header(&[("mesh", "String"), ("moves", "UInt64"), ("share", "Float64")]);
    for i in 0..5000u64 {
        encode::string(format!("53394{i:05}").as_bytes(), &mut b);
        b.extend(i.to_le_bytes());
        b.extend((i as f64 / 7.0).to_le_bytes());
    }
    let start = std::time::Instant::now();
    let t = crate::connect::rowbinary::decode(&b, 100_000).unwrap();
    let took = start.elapsed();
    assert_eq!(t.row_count(), 5000);
    // The 2 ms budget is for release; a debug build gets 10x.
    assert!(took.as_millis() < 20, "dash.decode: {took:?}");
}

/// The Test-connection statements (explorer-sql P2 2.4): `version()` and
/// `SELECT 1` go through the guard and carry the read-only settings, so a
/// server profile that refuses them answers 164 on the first request.
#[test]
fn version_and_ping_for_test_connection() {
    let mut b = encode::header(&[("version", "String")]);
    encode::string(b"25.8.1.1", &mut b);
    let stub = Stub::ok(b);
    assert_eq!(block_on(ClickHouse::version(&conn(), &stub)).unwrap(), "25.8.1.1");
    let req = &stub.requests()[0];
    assert_eq!(req.body, b"SELECT version() AS version");
    assert!(req.url.contains("readonly=2"), "{}", req.url);
    assert!(!req.url.contains("max_result_rows"), "a probe reads no rows cap: {}", req.url);

    let ping = Stub::ok(one_row());
    let mut c = conn();
    c.database = Some("synthetic_db".into());
    block_on(ClickHouse::ping(&c, &ping)).unwrap();
    assert_eq!(ping.requests()[0].body, b"SELECT 1 AS ok");
    assert!(ping.requests()[0].url.contains("database=synthetic_db"));

    let settings = Stub::new(|_| {
        Ok(HttpResponse { status: 500, body: b"Code: 164. DB::Exception: Cannot modify 'readonly' setting in readonly mode. (READONLY)".to_vec() })
    });
    match block_on(ClickHouse::version(&conn(), &settings)) {
        Err(SendError::Failed(e)) => assert_eq!(e.code, "settings_rejected"),
        other => panic!("{other:?}"),
    }
}

fn grant_rows(lines: &[&str]) -> Vec<u8> {
    let mut b = encode::header(&[("GRANTS FOR synthetic_user", "String")]);
    for l in lines {
        encode::string(l.as_bytes(), &mut b);
    }
    b
}

#[test]
fn grants_classify_each_case() {
    let g = |ls: &[&str]| classify_grants(&ls.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(g(&["GRANT SELECT ON synthetic_db.* TO synthetic_user"]), Grants::SelectOnly);
    assert_eq!(g(&["GRANT SELECT(a, b), SHOW TABLES ON synthetic_db.t TO synthetic_user"]), Grants::SelectOnly);
    assert_eq!(g(&["GRANT SELECT ON *.* TO u", "GRANT INSERT ON synthetic_db.t TO u"]), Grants::CanWrite);
    assert_eq!(g(&["GRANT ALL ON *.* TO default WITH GRANT OPTION"]), Grants::CanWrite);
    assert_eq!(g(&["GRANT SELECT, ALTER UPDATE ON db.t TO u"]), Grants::CanWrite);
    assert_eq!(g(&["GRANT CREATE TABLE, DROP TABLE ON db.* TO u"]), Grants::CanWrite);
    assert_eq!(g(&["GRANT default_role TO u"]), Grants::Unknown, "a role's privileges are not shown");
    assert_eq!(g(&[]), Grants::Unknown);
    // A column named like a privilege is not one.
    assert_eq!(g(&["GRANT SELECT(insert, drop) ON db.t TO u"]), Grants::SelectOnly);
}

#[test]
fn grants_probe_sends_a_fixed_statement_and_falls_back() {
    let stub = Stub::ok(grant_rows(&["GRANT SELECT ON synthetic_db.* TO synthetic_user"]));
    assert_eq!(block_on(ClickHouse::grants(&conn(), &stub)), Grants::SelectOnly);
    let r = &stub.requests()[0];
    assert_eq!(r.body, b"SHOW GRANTS FINAL");
    assert_eq!(url_settings(&r.url), ["default_format", "query_id"], "no settings, so a read-only profile answers");
    assert!(!r.url.contains(PW) && !r.url.contains(USER));

    // An older server rejects FINAL: one retry without it.
    let old = Stub::new(|r: &HttpRequest| {
        if r.body.ends_with(b"FINAL") {
            Ok(HttpResponse { status: 400, body: b"Code: 62. DB::Exception: Syntax error. (SYNTAX_ERROR)".to_vec() })
        } else {
            Ok(HttpResponse { status: 200, body: grant_rows(&["GRANT INSERT ON db.t TO u"]) })
        }
    });
    assert_eq!(block_on(ClickHouse::grants(&conn(), &old)), Grants::CanWrite);
    assert_eq!(old.requests().len(), 2);
    assert_eq!(old.requests()[1].body, b"SHOW GRANTS");

    // Not allowed to see its grants: unknown, one request.
    let denied = Stub::new(|_| Ok(HttpResponse { status: 403, body: b"Code: 497. DB::Exception: no. (ACCESS_DENIED)".to_vec() }));
    assert_eq!(block_on(ClickHouse::grants(&conn(), &denied)), Grants::Unknown);
    assert_eq!(denied.requests().len(), 1);
}

#[test]
fn a_bound_query_sends_its_values_as_params_and_tuning_as_settings() {
    use crate::sql::{Param, ParamValue, Scalar};
    let params = vec![Param { name: "bbox_w".into(), ty: "Float64".into(), value: ParamValue::One(Scalar::Float(103.5)) }];
    let q = CompiledQuery::bound("SELECT id FROM t WHERE lon >= {bbox_w:Float64}", params.clone(), Dialect::ClickHouse, &Default::default(), 10).unwrap();
    let tuning = Tuning { use_query_cache: true, max_rows_to_read: Some(5_000_000), max_bytes_to_read: Some(1 << 30) };
    let req = ClickHouse::request_tuned(&conn(), Statement::Query(&q), &QueryId::next(), &tuning).unwrap();
    assert!(req.url.contains("param_bbox_w=103.5"), "{}", req.url);
    for s in ["use_query_cache=1", "max_rows_to_read=5000000", "max_bytes_to_read=1073741824", "readonly=2"] {
        assert!(req.url.contains(s), "{s} in {}", req.url);
    }
    assert!(!String::from_utf8_lossy(&req.body).contains("103.5"), "never spliced");
    // A placeholder without a value, an untyped one, and a write are refused.
    assert!(CompiledQuery::bound("SELECT id FROM t WHERE x = {other:Float64}", params.clone(), Dialect::ClickHouse, &Default::default(), 10).is_err());
    assert!(CompiledQuery::bound("SELECT id FROM t WHERE x = {bbox_w}", params.clone(), Dialect::ClickHouse, &Default::default(), 10).is_err());
    assert!(CompiledQuery::bound("DROP TABLE t", params, Dialect::ClickHouse, &Default::default(), 10).is_err());
    // The plain request sends none of the tuning.
    let plain = ClickHouse::request(&conn(), Statement::Query(&q), &QueryId::next()).unwrap();
    assert!(!plain.url.contains("use_query_cache") && !plain.url.contains("max_rows_to_read"));
}
